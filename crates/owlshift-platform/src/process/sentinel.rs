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
//! The writes never block: a line that finds the pipe full is lost. A
//! sentinel that lost a line holds a list that may miss a `-`, so Owlshift
//! kills it at once, before the drop or the stop that wrote the line
//! returns (OWL-93): it then stops nothing, not even a tree whose handle was
//! dropped. Its script ignores SIGHUP, so a sentinel stopped when Owlshift
//! ends, which the system then hangs up and continues, still reads the end
//! of its input.
//!
//! [`probe_sentinel`] checks, with a sentinel and a process group of its
//! own, that this host's `/bin/sh` does so (OWL-90).
//!
//! [`stop_trees_when_killed`]: super::stop_trees_when_killed

use std::io::{self, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, waitid};

/// What the sentinel runs: it keeps the groups of the `+` lines less those
/// of the `-` lines, and at the end of its input kills each group left, as
/// `ProcessTree::kill` does. POSIX sh, builtins only, since it runs with an
/// empty environment: dash on Debian and Ubuntu, bash on macOS.
///
/// It first ignores SIGHUP (OWL-93): when Owlshift's end orphans the
/// sentinel's process group while the sentinel is stopped, the system sends
/// the group SIGHUP, then SIGCONT, and the sentinel must live on to read the
/// end of its input. It then closes its output, which only a test reads, to
/// tell it so.
const SCRIPT: &str = r#"trap '' HUP
exec >&-
trees=
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

/// A running sentinel: the write end of its input, and the process, so
/// that its end can be seen (OWL-88).
///
/// Nothing waits for the sentinel to end: it ends once Owlshift has. One
/// that ends first, killed by someone else, stays a zombie until its
/// [`status`](Self::status) is read.
#[derive(Debug)]
pub(super) struct Sentinel {
    process: Child,
    input: ChildStdin,
    /// Whether a line to it was lost, its pipe full, and so it was killed
    /// (OWL-93). Nothing more is written to it then.
    lost: bool,
}

/// How a sentinel killed for a lost line ended.
const LOST: &str = "killed by Owlshift once its input was full, a line to it lost";

/// Whether the sentinel protects the live trees from a hard kill
/// (`stop_trees_when_killed`), as far as this process can tell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SentinelStatus {
    /// None runs: `stop_trees_when_killed` was not called, or failed.
    NotRunning,
    /// It runs, and gives its best effort.
    Running {
        /// Its process id.
        pid: u32,
    },
    /// It runs but is stopped, as by SIGSTOP, and reads nothing until it is
    /// continued (OWL-91). When this process ends, the system continues it,
    /// and it stops the live trees then (OWL-93), unless its input filled
    /// first: this process kills it then, and it ends.
    Stopped {
        /// Its process id.
        pid: u32,
    },
    /// It ended, as said: nothing protects the trees any more. Owlshift
    /// never starts another, and kills it itself once a line to it is lost
    /// (OWL-93).
    Ended(String),
}

/// `/bin/sh` running `script` as `name`, with an empty environment, its
/// input piped and its outputs dropped.
fn sh(script: &str, name: &str) -> Command {
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", script, name])
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

impl Sentinel {
    /// Starts a sentinel that knows of no tree yet.
    pub(super) fn start() -> io::Result<Self> {
        Self::start_running(SCRIPT, "owlshift-sentinel")
    }

    /// Starts `script` as a sentinel named `name`: [`SCRIPT`] but in the
    /// test of a sentinel that never stops anything.
    fn start_running(script: &str, name: &str) -> io::Result<Self> {
        let mut command = sh(script, name);
        // Its output stays in `process`, never read but by the tests: the
        // script writes nothing to it, it closes it.
        command.stdout(Stdio::piped()).process_group(0);
        let mut process = command.spawn()?;
        let input = process.stdin.take().expect("the input is piped");
        // A sentinel that stops reading must never block a spawn, a dropped
        // tree or the signal handler, which all write under the lock of the
        // live trees: once the pipe is full, a line is lost instead.
        let flags = fcntl_getfl(&input)?;
        fcntl_setfl(&input, flags | OFlags::NONBLOCK)?;
        Ok(Self {
            process,
            input,
            lost: false,
        })
    }

    /// Whether it still runs, and is not stopped, without waiting: a
    /// sentinel that ended is reaped here.
    pub(super) fn status(&mut self) -> SentinelStatus {
        let pid = self.process.id();
        let state = self.process.try_wait();
        if self.lost {
            // Killed by SIGKILL, which nothing catches: it runs no more,
            // reaped or not yet.
            return SentinelStatus::Ended(match state {
                Ok(Some(status)) => format!("{LOST} ({status})"),
                _ => LOST.to_owned(),
            });
        }
        match state {
            Ok(None) if self.is_stopped() => SentinelStatus::Stopped { pid },
            Ok(None) => SentinelStatus::Running { pid },
            Ok(Some(status)) => SentinelStatus::Ended(status.to_string()),
            Err(error) => SentinelStatus::Ended(format!("its state cannot be read: {error}")),
        }
    }

    /// Whether it is stopped (OWL-91), asked only while it is not reaped, so
    /// its pid is its own. The wait takes nothing: `NOWAIT` leaves the stop
    /// to be seen again, and an end, not asked for, is left to `try_wait`.
    fn is_stopped(&self) -> bool {
        let options = WaitIdOptions::STOPPED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT;
        // An error tells nothing: `try_wait` just said it runs, so it counts
        // as running, the answer before OWL-91.
        matches!(
            waitid(WaitId::Pid(Pid::from_child(&self.process)), options),
            Ok(Some(status)) if status.stopped()
        )
    }

    /// Waits until its script closed its output, which it does once it
    /// ignores SIGHUP, or until it ended.
    #[cfg(test)]
    pub(super) fn wait_until_it_ignores_hangups(&mut self) {
        use std::io::Read;

        let mut output = self.process.stdout.take().expect("the output is piped");
        let mut rest = Vec::new();
        output.read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty(), "the sentinel wrote {rest:?}");
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
    ///
    /// A line lost to a full pipe leaves the sentinel with a list that may
    /// miss a `-`, and once continued it would kill a tree whose handle was
    /// dropped. So it is killed here, under the lock of the live trees,
    /// before the drop or the stop that sent the line returns (OWL-93): a
    /// hard kill of Owlshift before then ends it while that tree is still
    /// live. The kill, by its pid, which stays its own until it is reaped,
    /// fails only for a sentinel that ended already.
    fn send(&mut self, sign: char, group: Pid) {
        if self.lost {
            return;
        }
        let line = format!("{sign} {}\n", group.as_raw_nonzero());
        if let Err(error) = self.input.write_all(line.as_bytes())
            && error.kind() == io::ErrorKind::WouldBlock
        {
            self.lost = true;
            let _ = self.process.kill();
        }
    }
}

/// Whether a sentinel works on this host: whether one stops a process group
/// it was told of when its input ends (OWL-90).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SentinelProbe {
    /// It stopped the group, this long after its input ended.
    Works {
        /// From the end of its input to the group's last member reaped.
        elapsed: Duration,
    },
    /// The test's processes could not start, as said.
    CannotStart(String),
    /// The sentinel did not stop the group, as said.
    Fails(String),
}

/// What each member of the test group runs: it waits for a line on its
/// input, which never comes, since the probe holds the write end until it
/// is done. `read` is a shell builtin, so the empty environment is enough.
const MEMBER: &str = "read -r line";

/// Checks that a sentinel stops a process group it was told of when its
/// input ends, within `bound`.
///
/// It starts a group of two processes of its own, the leader and a member
/// that joins it, then a sentinel running [`SCRIPT`], tells the sentinel of
/// the group and closes its input. The group is stopped when both are killed
/// by `SIGKILL`. The sentinel of this process, if any, is never told of that
/// group, and everything the probe starts is reaped before it returns.
pub fn probe_sentinel(bound: Duration) -> SentinelProbe {
    probe_with(SCRIPT, bound)
}

fn probe_with(script: &str, bound: Duration) -> SentinelProbe {
    // Declared first, so that its drop reaps what was started on every way
    // out, a panic included.
    let mut started = Started::default();
    let leader = match sh(MEMBER, "owlshift-probe-member").process_group(0).spawn() {
        Ok(leader) => leader,
        Err(error) => return SentinelProbe::CannotStart(format!("a test process: {error}")),
    };
    // The leader's own pid, and it stays reserved while the leader is not
    // reaped: the group can be no one else's.
    let group = Pid::from_child(&leader);
    started.members.push(leader);
    match sh(MEMBER, "owlshift-probe-member")
        .process_group(group.as_raw_nonzero().get())
        .spawn()
    {
        Ok(member) => started.members.push(member),
        Err(error) => return SentinelProbe::CannotStart(format!("a test process: {error}")),
    }
    // Started after the members, so that none of them holds its input open.
    let mut sentinel = match Sentinel::start_running(script, "owlshift-probe-sentinel") {
        Ok(sentinel) => sentinel,
        Err(error) => return SentinelProbe::CannotStart(format!("a test sentinel: {error}")),
    };
    sentinel.announce(group);
    let Sentinel { process, input, .. } = sentinel;
    started.sentinel = Some(process);
    drop(input);
    let ended = Instant::now();
    let deadline = ended + bound;
    let bound_ms = bound.as_millis();

    // The sentinel is reaped before the members: once it is gone nothing can
    // signal the group any more, and only then may reaping the members free
    // the group's id.
    let sentinel = started.sentinel.as_mut().expect("the sentinel was kept");
    match wait_until(sentinel, deadline) {
        Ok(Some(_)) => {}
        Ok(None) => {
            return SentinelProbe::Fails(format!(
                "the test sentinel still ran {bound_ms} ms after its input ended"
            ));
        }
        Err(error) => {
            return SentinelProbe::Fails(format!(
                "the test sentinel's state cannot be read: {error}"
            ));
        }
    }
    // Only `try_wait` before the verdict: `wait` would close a member's
    // input, and it would end by itself.
    for member in &mut started.members {
        match wait_until(member, deadline) {
            Ok(Some(status)) if status.signal() == Some(Signal::KILL.as_raw()) => {}
            Ok(Some(status)) => {
                return SentinelProbe::Fails(format!(
                    "a process of the test group ended otherwise ({status})"
                ));
            }
            Ok(None) => {
                return SentinelProbe::Fails(format!(
                    "the test group still ran {bound_ms} ms after the test sentinel's input ended"
                ));
            }
            Err(error) => {
                return SentinelProbe::Fails(format!(
                    "the test group's state cannot be read: {error}"
                ));
            }
        }
    }
    SentinelProbe::Works {
        elapsed: ended.elapsed(),
    }
}

/// What [`probe_with`] started. Dropping it kills by pid what still runs,
/// the sentinel first, and reaps all of it. It never signals a group: a
/// process that is not reaped keeps its pid, so the signal cannot reach
/// anyone else, which a group id no member holds any more could.
#[derive(Default)]
struct Started {
    sentinel: Option<Child>,
    members: Vec<Child>,
}

impl Drop for Started {
    fn drop(&mut self) {
        for child in self.sentinel.iter_mut().chain(&mut self.members) {
            // Whatever the kill does, the wait ends: it closes the child's
            // input, and each of these ends at the end of its input.
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// The child's status once it ended, reaping it; `None` if it still runs at
/// `deadline`.
fn wait_until(child: &mut Child, deadline: Instant) -> io::Result<Option<ExitStatus>> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        thread::sleep(super::POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use rustix::io::Errno;
    use rustix::process::{WaitOptions, kill_process, wait};

    fn helper_requested() -> bool {
        std::env::args().any(|arg| arg == "--exact")
    }

    /// Runs `probe` in this helper process, checks it left no child, running
    /// or unreaped, and writes its verdict to `verdict`. The helper runs
    /// alone, on one thread, so no other test's process is a child here, and
    /// none can inherit the test sentinel's input: on macOS std makes a pipe
    /// close-on-exec only after creating it.
    fn probe_alone(probe: impl FnOnce() -> SentinelProbe) {
        if helper_requested() {
            let verdict = probe();
            let left = wait(WaitOptions::NOHANG);
            assert!(matches!(left, Err(Errno::CHILD)), "left: {left:?}");
            std::fs::write("verdict", format!("{verdict:?}")).unwrap();
        }
    }

    /// The verdict of the helper `name`, run in a process of its own.
    fn verdict_of(name: &str) -> String {
        let dir = tempfile::tempdir().unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--ignored", "--test-threads=1"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::fs::read_to_string(dir.path().join("verdict")).unwrap_or_else(|_| {
            panic!(
                "no verdict from {name}: {}\n{}",
                output.status,
                String::from_utf8_lossy(&output.stdout)
            )
        })
    }

    #[test]
    #[ignore = "helper, run by the tests below"]
    fn helper_probe_the_sentinel() {
        probe_alone(|| probe_sentinel(Duration::from_secs(5)));
    }

    #[test]
    #[ignore = "helper, run by the tests below"]
    fn helper_probe_a_sentinel_that_stops_nothing() {
        probe_alone(|| {
            probe_with(
                "while read -r sign group; do :; done",
                Duration::from_millis(300),
            )
        });
    }

    /// OWL-90: this host's `/bin/sh`, running the sentinel's script, stops a
    /// group it was told of when its input ends, and the probe leaves nothing
    /// behind.
    #[test]
    fn the_sentinel_stops_a_group_when_its_input_ends() {
        let verdict = verdict_of("process::sentinel::tests::helper_probe_the_sentinel");
        assert!(verdict.starts_with("Works"), "{verdict}");
    }

    /// A sentinel that stops nothing fails the probe within its bound, and
    /// the probe still stops and reaps the group itself.
    #[test]
    fn a_sentinel_that_stops_nothing_fails_and_leaves_nothing() {
        let started = Instant::now();
        let verdict =
            verdict_of("process::sentinel::tests::helper_probe_a_sentinel_that_stops_nothing");
        assert!(
            verdict.starts_with("Fails(\"the test group still ran 300 ms"),
            "{verdict}"
        );
        assert!(started.elapsed() < Duration::from_secs(5), "{verdict}");
    }

    /// A sentinel killed and reaped when dropped, however the test ends.
    struct Reaped(Sentinel);

    impl Drop for Reaped {
        fn drop(&mut self) {
            // SIGKILL ends it even stopped, and std knows once it is reaped,
            // so neither call can reach another process.
            let _ = self.0.process.kill();
            let _ = self.0.process.wait();
        }
    }

    /// Its status once `done` holds, or the last one read within 5 s.
    fn status_once(sentinel: &mut Sentinel, done: fn(&SentinelStatus) -> bool) -> SentinelStatus {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let status = sentinel.status();
            if done(&status) || Instant::now() >= deadline {
                return status;
            }
            thread::sleep(super::super::POLL);
        }
    }

    /// OWL-91: a stopped sentinel is told apart from a running one until it
    /// is continued, and its end is still read and reaped. It runs in this
    /// process, unlike the probes: nothing here waits for the end of its
    /// input, so another test's child inheriting the pipe on macOS changes
    /// nothing.
    #[test]
    fn a_stopped_sentinel_is_told_apart_until_it_is_continued() {
        let mut reaped = Reaped(Sentinel::start().unwrap());
        let sentinel = &mut reaped.0;
        let pid = sentinel.process.id();
        let id = Pid::from_child(&sentinel.process);

        kill_process(id, Signal::STOP).unwrap();
        let stopped = status_once(sentinel, |s| matches!(s, SentinelStatus::Stopped { .. }));
        assert_eq!(stopped, SentinelStatus::Stopped { pid });
        // Seen again: the wait took nothing.
        assert_eq!(sentinel.status(), SentinelStatus::Stopped { pid });

        kill_process(id, Signal::CONT).unwrap();
        let continued = status_once(sentinel, |s| matches!(s, SentinelStatus::Running { .. }));
        assert_eq!(continued, SentinelStatus::Running { pid });

        kill_process(id, Signal::STOP).unwrap();
        sentinel.process.kill().unwrap();
        let ended = status_once(sentinel, |s| matches!(s, SentinelStatus::Ended(_)));
        assert_eq!(ended, SentinelStatus::Ended("signal: 9 (SIGKILL)".into()));
    }

    /// OWL-93: a stopped sentinel sent more lines than its pipe holds loses
    /// one, and is killed for it at once, which its status says.
    #[test]
    fn a_sentinel_that_loses_a_line_is_killed() {
        let mut reaped = Reaped(Sentinel::start().unwrap());
        let sentinel = &mut reaped.0;
        kill_process(Pid::from_child(&sentinel.process), Signal::STOP).unwrap();
        let stopped = status_once(sentinel, |s| matches!(s, SentinelStatus::Stopped { .. }));
        assert!(
            matches!(stopped, SentinelStatus::Stopped { .. }),
            "{stopped:?}"
        );

        // Pid 1 leads no tree, so each line changes nothing; 256 KiB of
        // them, more than a pipe holds.
        let none = Pid::from_raw(1).unwrap();
        for _ in 0..64 * 1024 {
            sentinel.withdraw(none);
        }
        assert!(sentinel.lost, "no line was lost");
        let ended = status_once(
            sentinel,
            |s| matches!(s, SentinelStatus::Ended(how) if how.ends_with(')')),
        );
        assert_eq!(
            ended,
            SentinelStatus::Ended(format!("{LOST} (signal: 9 (SIGKILL))"))
        );
    }
}
