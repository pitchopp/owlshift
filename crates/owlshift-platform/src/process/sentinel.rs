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
//! close-on-exec, so no process Owlshift starts inherits it. Nor does it
//! inherit any other descriptor Owlshift has open when it starts (OWL-671):
//! one Owlshift was handed and has not adopted yet, as the web UI's socket,
//! would stay open in the sentinel until the sentinel ends. One line per
//! event: `+ <group>` when a tree becomes live and `- <group>` when it no
//! longer is. When Owlshift ends, the system closes the write end, the
//! sentinel reads the end of its input, kills every group still live, and
//! ends.
//!
//! A tree's start has no window a kill could slip through (OWL-673): its
//! root writes `? <group>` itself, between fork and exec ([`Sentinel::arm`]),
//! and Owlshift then confirms it with `+ <group>`, or writes `!` when the
//! start failed, which forgets the group of that unconfirmed `?`. Until it
//! executes its program, the root holds the write end too, so a kill of
//! Owlshift at any moment of the start leaves the sentinel reading that `?`
//! before the end of its input.
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
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, waitid};

/// What the sentinel runs: it keeps the groups of the `?` and `+` lines less
/// those of the `-` lines, and at the end of its input kills each group
/// left, as `ProcessTree::kill` does, without its search for what left the
/// group (OWL-451). POSIX sh, builtins only, since it runs with an empty
/// environment: dash on Debian and Ubuntu, bash on macOS.
///
/// A `?` is a start not yet confirmed (OWL-673): its group is kept as a live
/// one, and pending until the next line, Owlshift's about that start. A `+`
/// of the same group confirms it, kept once; a `!` forgets it. Every line
/// but a `?` ends the pending. The `?` and `!` patterns are quoted:
/// unquoted, `?` matches any one character.
///
/// It first ignores SIGHUP (OWL-93): when Owlshift's end orphans the
/// sentinel's process group while the sentinel is stopped, the system sends
/// the group SIGHUP, then SIGCONT, and the sentinel must live on to read the
/// end of its input. It then closes its output, which only a test reads, to
/// tell it so.
const SCRIPT: &str = r#"trap '' HUP
exec >&-
trees=
started=
forget() {
  left=
  for tree in $trees; do
    [ "$tree" = "$1" ] || left="$left $tree"
  done
  trees=$left
}
while read -r sign group; do
  case $sign in
    '?') trees="$trees $group"; started=$group ;;
    +) [ "$group" = "$started" ] || trees="$trees $group"; started= ;;
    -) forget "$group"; started= ;;
    '!') [ -z "$started" ] || forget "$started"; started= ;;
  esac
done
for tree in $trees; do
  kill -s KILL -- "-$tree" 2>/dev/null
done
"#;

/// The longest line to the sentinel: a sign, a space, the 10 digits of the
/// largest pid, a newline.
const LINE: usize = 13;

/// One line to the sentinel, `<sign> <group>` or a sign alone, built on the
/// stack: a tree's root writes its own between fork and exec, where nothing
/// may allocate, lock or panic (OWL-673). No step here has a panic path: the
/// bytes are filled from the end through an iterator, and read back with
/// `get`. Owlshift's own lines are built the same way, so that a root's
/// `?` and Owlshift's `+` name its group in the same bytes.
pub(super) struct Line {
    bytes: [u8; LINE],
    start: usize,
}

impl Line {
    pub(super) fn new(sign: u8, group: Option<Pid>) -> Self {
        const DIGITS: &[u8; 10] = b"0123456789";
        let mut bytes = [0; LINE];
        let mut slots = bytes.iter_mut().rev();
        let mut put = |byte| {
            if let Some(slot) = slots.next() {
                *slot = byte;
            }
        };
        put(b'\n');
        if let Some(group) = group {
            // A pid is positive: 10 digits at most.
            let mut number = group.as_raw_nonzero().get().unsigned_abs();
            loop {
                put(DIGITS.get((number % 10) as usize).copied().unwrap_or(b'0'));
                number /= 10;
                if number == 0 {
                    break;
                }
            }
            put(b' ');
        }
        put(sign);
        let start = slots.len();
        Self { bytes, start }
    }

    pub(super) fn as_bytes(&self) -> &[u8] {
        self.bytes.get(self.start..).unwrap_or_default()
    }
}

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
/// input piped, its outputs dropped, and no other descriptor of this
/// process ([`inherit_none`]).
fn sh(script: &str, name: &str) -> Command {
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", script, name])
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    inherit_none(&mut command);
    command
}

/// Makes `command`'s child inherit none of this process's descriptors but
/// its standard streams (OWL-671). A descriptor handed to this process stays
/// inheritable until the process adopts it, and every process starts its
/// sentinel first: the web UI's sentinel held the UI's socket, so the port
/// stayed bound until that sentinel had ended too.
///
/// Each descriptor above the standard streams open now, as `/dev/fd` lists
/// them, is made close-on-exec in the child just before it executes, never
/// in this process, whose later children may need one. One opened after the
/// listing, by another thread and not close-on-exec, is still inherited;
/// std opens its own close-on-exec. When `/dev/fd` cannot be read, as in a
/// container with no `/proc`, nothing is listed and the child inherits as
/// before: a sentinel that starts matters more than one that holds nothing.
fn inherit_none(command: &mut Command) {
    use rustix::io::{FdFlags, fcntl_setfd};
    use std::os::fd::BorrowedFd;

    let open: Vec<i32> = std::fs::read_dir("/dev/fd")
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
                .filter(|fd| *fd > 2)
                .collect()
        })
        .unwrap_or_default();
    // SAFETY: the closure runs in the child between fork and exec, after
    // std has set its standard streams, where only async-signal-safe calls
    // are allowed: one `fcntl` for each number of a list made before the
    // fork, neither allocating nor locking. A number no longer open there,
    // as `/dev/fd`'s own, fails alone and is skipped: borrowing it closes
    // nothing.
    unsafe {
        command.pre_exec(move || {
            for &fd in &open {
                let _ = fcntl_setfd(BorrowedFd::borrow_raw(fd), FdFlags::CLOEXEC);
            }
            Ok(())
        });
    }
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
        self.send(&Line::new(b'+', Some(group)));
    }

    /// Tells the sentinel a tree is no longer live.
    pub(super) fn withdraw(&mut self, group: Pid) {
        self.send(&Line::new(b'-', Some(group)));
    }

    /// Makes the next start of `command`, a tree's root in a process group
    /// of its own, announce itself to the sentinel between fork and exec:
    /// `? <group>`, written by the root, which holds the sentinel's input
    /// until it executes its program (OWL-673). The start is then Owlshift's
    /// to confirm ([`Start::confirm`]) or, dropped unconfirmed, to withdraw.
    /// `None` once a line to the sentinel was lost: it is killed and told
    /// nothing more.
    ///
    /// The root writes its line only for the start armed here: once that
    /// start is confirmed or withdrawn, the closure left on `command` writes
    /// nothing, should `command` be started again, by this or any other way.
    /// Adding a closure makes std fork the root instead of using
    /// `posix_spawn`, about half a millisecond more per start on macOS.
    pub(super) fn arm(&mut self, command: &mut Command) -> Option<Start<'_>> {
        if self.lost {
            return None;
        }
        let armed = Arc::new(AtomicBool::new(true));
        let in_root = Arc::clone(&armed);
        let input = self.input.as_raw_fd();
        // SAFETY: the closure runs in the root between fork and exec, after
        // std has made it a group leader and reset SIGPIPE, where only
        // async-signal-safe calls are allowed: an atomic load through a
        // pointer made before the fork, then `announce_self`, which says
        // what it calls. The descriptor is the sentinel's input, open until
        // this process ends: the sentinel is never dropped once it runs.
        unsafe {
            command.pre_exec(move || {
                if in_root.load(Ordering::SeqCst) {
                    announce_self(input);
                }
                Ok(())
            });
        }
        Some(Start {
            sentinel: self,
            armed,
            confirmed: false,
        })
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
    fn send(&mut self, line: &Line) {
        if self.lost {
            return;
        }
        if let Err(error) = self.input.write_all(line.as_bytes())
            && error.kind() == io::ErrorKind::WouldBlock
        {
            self.lost = true;
            let _ = self.process.kill();
        }
    }
}

/// A start [`Sentinel::arm`] armed, under the lock of the live trees: its
/// root may have announced itself. Dropped unconfirmed, as when the spawn
/// failed, it writes `!`, which forgets that root's group: std reaped the
/// root of a failed start before the spawn returned, so its pid may be
/// another's by the time the sentinel acts.
pub(super) struct Start<'a> {
    sentinel: &'a mut Sentinel,
    armed: Arc<AtomicBool>,
    confirmed: bool,
}

impl Start<'_> {
    /// Confirms the start, its root's group `group`: `+ <group>`, which also
    /// announces the tree should the root's own line have been lost.
    pub(super) fn confirm(mut self, group: Pid) {
        self.armed.store(false, Ordering::SeqCst);
        self.sentinel.announce(group);
        self.confirmed = true;
    }
}

impl Drop for Start<'_> {
    fn drop(&mut self) {
        self.armed.store(false, Ordering::SeqCst);
        if !self.confirmed {
            self.sentinel.send(&Line::new(b'!', None));
        }
    }
}

/// What an armed root runs between fork and exec: writes `? <its pid>`,
/// its group, to the sentinel's input `input`, a non-blocking pipe, so a
/// full one loses the line rather than block (Owlshift's `+` then tells the
/// sentinel instead, or is lost too and has it killed). SIGPIPE, which std
/// set back to its default before the closures, is ignored around the
/// write: a sentinel gone would otherwise kill the root before its program
/// runs, and std would report a start. Without that, the line is not
/// written.
///
/// Async-signal-safe calls only, nothing allocated, locked or able to panic:
/// `getpid`, `sigaction` twice, `write`, on a [`Line`] built on the stack.
/// A zeroed `sigaction` has an empty mask and no flags, on macOS and Linux.
fn announce_self(input: RawFd) {
    use std::os::fd::BorrowedFd;

    let line = Line::new(b'?', Some(rustix::process::getpid()));
    // SAFETY: each call is async-signal-safe and given valid pointers to
    // locals; `input` is the sentinel's input, open in this process until
    // it executes its program.
    unsafe {
        let mut ignore: libc::sigaction = std::mem::zeroed();
        ignore.sa_sigaction = libc::SIG_IGN;
        let mut before: libc::sigaction = std::mem::zeroed();
        if libc::sigaction(libc::SIGPIPE, &ignore, &mut before) != 0 {
            return;
        }
        let _ = rustix::io::write(BorrowedFd::borrow_raw(input), line.as_bytes());
        libc::sigaction(libc::SIGPIPE, &before, std::ptr::null_mut());
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
/// that joins it, then a sentinel running `SCRIPT`, tells the sentinel of
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
    /// or unreaped, and writes its verdict to `verdict`, then on a line of
    /// its own how many milliseconds the probe took. The helper runs alone,
    /// on one thread, so no other test's process is a child here, and none
    /// can inherit the test sentinel's input: on macOS std makes a pipe
    /// close-on-exec only after creating it.
    fn probe_alone(probe: impl FnOnce() -> SentinelProbe) {
        if helper_requested() {
            let started = Instant::now();
            let verdict = probe();
            let took = started.elapsed().as_millis();
            let left = wait(WaitOptions::NOHANG);
            assert!(matches!(left, Err(Errno::CHILD)), "left: {left:?}");
            std::fs::write("verdict", format!("{verdict:?}\n{took}")).unwrap();
        }
    }

    /// The variable that gives a helper its probe's bound, in milliseconds.
    const BOUND: &str = "OWLSHIFT_TEST_PROBE_BOUND_MS";

    /// The probe's bound the test gave this helper.
    fn bound_given() -> Duration {
        let ms = std::env::var(BOUND).expect(BOUND);
        Duration::from_millis(ms.parse().unwrap())
    }

    /// The verdict of the helper `name`, run in a process of its own, with
    /// the probe's bound `bound` if it has one.
    fn verdict_of(name: &str, bound: Option<Duration>) -> String {
        let dir = tempfile::tempdir().unwrap();
        let mut helper = Command::new(std::env::current_exe().unwrap());
        helper
            .args(["--exact", name, "--ignored", "--test-threads=1"])
            .current_dir(dir.path());
        if let Some(bound) = bound {
            helper.env(BOUND, bound.as_millis().to_string());
        }
        let output = helper.output().unwrap();
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
        probe_alone(|| probe_sentinel(bound_given()));
    }

    #[test]
    #[ignore = "helper, run by the tests below"]
    fn helper_probe_a_sentinel_that_stops_nothing() {
        probe_alone(|| probe_with("while read -r sign group; do :; done", bound_given()));
    }

    /// OWL-90: this host's `/bin/sh`, running the sentinel's script, stops a
    /// group it was told of when its input ends, and the probe leaves nothing
    /// behind. The probe's bound is not what it checks: `wait::REACHED`
    /// (OWL-672), which a working sentinel never waits out.
    #[test]
    fn the_sentinel_stops_a_group_when_its_input_ends() {
        let verdict = verdict_of(
            "process::sentinel::tests::helper_probe_the_sentinel",
            Some(crate::wait::REACHED),
        );
        assert!(verdict.starts_with("Works"), "{verdict}");
    }

    #[test]
    #[ignore = "helper, run by the tests below"]
    fn helper_start_a_sentinel_beside_a_handed_over_descriptor() {
        use rustix::io::{FdFlags, fcntl_setfd};
        use std::io::Read;

        if !helper_requested() {
            return;
        }
        let (mut reader, writer) = io::pipe().unwrap();
        // Inheritable, as a descriptor handed to this process that it has
        // not adopted yet.
        fcntl_setfd(&writer, FdFlags::empty()).unwrap();
        let reaped = Reaped(Sentinel::start().unwrap());
        drop(writer);
        // Its end is read at once when no other process holds the write end.
        let flags = fcntl_getfl(&reader).unwrap();
        fcntl_setfl(&reader, flags | OFlags::NONBLOCK).unwrap();
        let verdict = match reader.read(&mut [0]) {
            Ok(0) => "held by none".to_owned(),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                "the sentinel holds the write end".to_owned()
            }
            other => format!("{other:?}"),
        };
        drop(reaped);
        std::fs::write("verdict", verdict).unwrap();
    }

    /// OWL-671: a sentinel holds no descriptor of the process that started
    /// it but its input: one that process was handed and has not made
    /// close-on-exec yet, as the web UI's socket, is not kept open by it.
    #[test]
    fn a_sentinel_keeps_no_descriptor_handed_to_its_starter() {
        let verdict = verdict_of(
            "process::sentinel::tests::helper_start_a_sentinel_beside_a_handed_over_descriptor",
            None,
        );
        assert_eq!(verdict, "held by none");
    }

    /// A sentinel that stops nothing fails the probe at its bound, and the
    /// probe still stops and reaps the group itself.
    ///
    /// The bound is the case, and the verdict on the group comes only once
    /// the test sentinel has ended, which load delayed past 300 ms in 12 of
    /// 1307 runs (OWL-672): a probe cut before then runs again with twice the
    /// bound, `wait::REACHED` at most (build plan, "Waits in tests"), so a
    /// probe that is always cut early fails after about four minutes.
    #[test]
    fn a_sentinel_that_stops_nothing_fails_and_leaves_nothing() {
        let mut bound = Duration::from_millis(300);
        let (verdict, took) = loop {
            let written = verdict_of(
                "process::sentinel::tests::helper_probe_a_sentinel_that_stops_nothing",
                Some(bound),
            );
            let (verdict, took) = written.split_once('\n').unwrap();
            let cut_early = verdict.starts_with("Fails(\"the test sentinel still ran");
            if cut_early && bound < crate::wait::REACHED {
                eprintln!("{verdict}: run again with twice the bound");
                bound = (bound * 2).min(crate::wait::REACHED);
                continue;
            }
            break (
                verdict.to_owned(),
                Duration::from_millis(took.parse().unwrap()),
            );
        };
        let expected = format!("Fails(\"the test group still ran {} ms", bound.as_millis());
        assert!(verdict.starts_with(&expected), "{verdict}");
        // It waited on the group its whole bound, and gave its verdict then,
        // not waiting past it: the rest is the probe's own stops and reaps,
        // which load stretches (its 5 s ceiling before OWL-672 was one more
        // deadline the passing path had to beat).
        assert!(took >= bound, "{verdict} after {took:?}");
        assert!(
            took < bound + crate::wait::REACHED,
            "{verdict} after {took:?}"
        );
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

    /// Its status once `done` holds, or the last one read within
    /// `wait::REACHED`, a state the test only needs reached (OWL-672).
    fn status_once(sentinel: &mut Sentinel, done: fn(&SentinelStatus) -> bool) -> SentinelStatus {
        let deadline = Instant::now() + crate::wait::REACHED;
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

    /// OWL-673: a line built on the stack, for the shortest and the longest
    /// group, and a sign alone.
    #[test]
    fn a_line_names_its_group_in_full() {
        let line = |sign, group: Option<i32>| {
            let line = Line::new(sign, group.map(|raw| Pid::from_raw(raw).unwrap()));
            String::from_utf8(line.as_bytes().to_vec()).unwrap()
        };
        assert_eq!(line(b'?', Some(1)), "? 1\n");
        assert_eq!(line(b'+', Some(10)), "+ 10\n");
        assert_eq!(line(b'-', Some(i32::MAX)), "- 2147483647\n");
        assert_eq!(line(b'!', None), "!\n");
    }

    /// OWL-673: the script keeps a tree whose start was confirmed, or never
    /// answered, as when Owlshift died during it, once in its list; it
    /// forgets a start Owlshift said failed, and a `!` with no start pending
    /// forgets nothing. Its list at the end of its input is written to a
    /// file after its kills: the groups are above any pid (99,999 on macOS,
    /// 4,194,304 at most on Linux), so each kill fails and reaches no one.
    #[test]
    fn the_script_keeps_a_started_tree_and_forgets_a_failed_start() {
        let dir = tempfile::tempdir().unwrap();
        let list = dir.path().join("trees");
        let script = format!("{SCRIPT}echo $trees > '{}'\n", list.display());
        let Sentinel {
            process, mut input, ..
        } = Sentinel::start_running(&script, "owlshift-test-sentinel").unwrap();
        let _started = Started {
            sentinel: Some(process),
            members: Vec::new(),
        };
        let group = |raw| Some(Pid::from_raw(raw).unwrap());
        for line in [
            Line::new(b'?', group(9_000_001)),
            Line::new(b'!', None),
            Line::new(b'?', group(9_000_002)),
            Line::new(b'+', group(9_000_002)),
            Line::new(b'+', group(9_000_003)),
            Line::new(b'-', group(9_000_003)),
            Line::new(b'+', group(9_000_005)),
            Line::new(b'!', None),
            Line::new(b'?', group(9_000_006)),
            Line::new(b'+', group(9_000_006)),
            Line::new(b'!', None),
            Line::new(b'?', group(9_000_004)),
        ] {
            input.write_all(line.as_bytes()).unwrap();
        }
        drop(input);
        let mut written = String::new();
        crate::wait::until("the sentinel to write its list", || {
            written = std::fs::read_to_string(&list).unwrap_or_default();
            written.ends_with('\n')
        });
        assert_eq!(written, "9000002 9000005 9000006 9000004\n");
    }

    /// OWL-673: what a start writes to the sentinel. A root announces itself
    /// before its program runs, then Owlshift withdraws it, its program not
    /// found, or confirms it; a command started again once its start ended
    /// writes nothing more. The sentinel here logs each line it reads.
    #[test]
    fn a_start_is_announced_by_its_root_then_confirmed_or_withdrawn() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("lines");
        let script = format!(
            "while read -r line; do echo \"$line\" >> '{}'; done",
            log.display()
        );
        let mut reaped =
            Reaped(Sentinel::start_running(&script, "owlshift-test-sentinel").unwrap());
        let sentinel = &mut reaped.0;

        let mut missing = Command::new("owlshift-test-no-such-program");
        missing.process_group(0);
        let start = sentinel.arm(&mut missing).unwrap();
        assert!(missing.spawn().is_err());
        drop(start);

        let mut runs = Command::new("true");
        runs.process_group(0);
        let start = sentinel.arm(&mut runs).unwrap();
        let mut child = runs.spawn().unwrap();
        let group = Pid::from_child(&child);
        start.confirm(group);
        child.wait().unwrap();
        // Its closure stays on the command, disarmed.
        runs.spawn().unwrap().wait().unwrap();
        sentinel.withdraw(group);

        let mut lines = Vec::new();
        crate::wait::until("the sentinel to log the last line", || {
            let logged = std::fs::read_to_string(&log).unwrap_or_default();
            lines = logged.lines().map(str::to_owned).collect();
            lines.last().is_some_and(|line| line.starts_with('-'))
        });
        let failed = lines[0].strip_prefix("? ").map(str::parse::<u32>);
        assert!(matches!(failed, Some(Ok(_))), "{lines:?}");
        assert_eq!(
            lines[1..],
            [
                "!".to_owned(),
                format!("? {group}"),
                format!("+ {group}"),
                format!("- {group}"),
            ],
            "{lines:?}"
        );
    }
}
