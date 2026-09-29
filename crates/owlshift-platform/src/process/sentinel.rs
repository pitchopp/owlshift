//! The sentinel: a process that outlives Owlshift to stop the process trees
//! still live when Owlshift ended, however it ended (OWL-86).
//!
//! It is not a deadline watcher, as the executor's `Watchdog` is: it acts
//! once, when the process that started it is gone. [`stop_trees_when_killed`]
//! starts it and says what it guarantees.
//!
//! The sentinel is `/bin/sh` running [`SCRIPT`], in a process group of its
//! own, so nothing that ends Owlshift's group reaches it. Its standard input
//! is a pipe whose write end only Owlshift holds: std creates it
//! close-on-exec, so no process Owlshift starts inherits it. Owlshift writes
//! one line per event, `+ <group>` when a tree becomes live and `- <group>`
//! when it no longer is. When Owlshift ends, the system closes the write end,
//! the sentinel reads the end of its input, kills every group still live,
//! and ends.
//!
//! [`stop_trees_when_killed`]: super::stop_trees_when_killed

use std::io::{self, Write};
use std::os::unix::process::CommandExt;
use std::process::{ChildStdin, Command, Stdio};

use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use rustix::process::Pid;

/// What the sentinel runs: it keeps the groups of the `+` lines less those
/// of the `-` lines, and at the end of its input kills each group left, as
/// `ProcessTree::kill` does. POSIX sh, builtins only, since it runs with an
/// empty environment: dash on Debian and Ubuntu, bash on macOS.
const SCRIPT: &str = r#"trees=
while read -r sign group; do
  case $sign in
    +) trees="$trees $group" ;;
    -) left=
       for tree in $trees; do
         [ "$tree" = "$group" ] || left="$left $tree"
       done
       trees=$left ;;
  esac
done
for tree in $trees; do
  kill -s KILL -- "-$tree" 2>/dev/null
done
"#;

/// A running sentinel: the write end of its input.
///
/// The sentinel is never waited on. It ends once Owlshift has; one killed
/// by someone else stays a zombie until Owlshift ends.
#[derive(Debug)]
pub(super) struct Sentinel {
    input: ChildStdin,
}

impl Sentinel {
    /// Starts a sentinel that knows of no tree yet.
    pub(super) fn start() -> io::Result<Self> {
        let mut child = Command::new("/bin/sh")
            .args(["-c", SCRIPT, "owlshift-sentinel"])
            .env_clear()
            .current_dir("/")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()?;
        let input = child.stdin.take().expect("the input is piped");
        // A sentinel that stops reading must never block a spawn, a dropped
        // tree or the signal handler, which all write under the lock of the
        // live trees: once the pipe is full, a line is lost instead.
        let flags = fcntl_getfl(&input)?;
        fcntl_setfl(&input, flags | OFlags::NONBLOCK)?;
        Ok(Self { input })
    }

    /// Tells the sentinel a tree is live.
    pub(super) fn announce(&mut self, group: Pid) {
        self.send('+', group);
    }

    /// Tells the sentinel a tree is no longer live.
    pub(super) fn withdraw(&mut self, group: Pid) {
        self.send('-', group);
    }

    /// Best effort: a line that cannot be written is lost, whether the
    /// sentinel is gone or its pipe is full. A write this short to a pipe is
    /// whole or nothing, so the sentinel never reads half a line.
    fn send(&mut self, sign: char, group: Pid) {
        let line = format!("{sign} {}\n", group.as_raw_nonzero());
        let _ = self.input.write_all(line.as_bytes());
    }
}
