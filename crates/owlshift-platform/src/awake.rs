//! Keeping the machine from idle sleep while a step runs (OWL-439,
//! architecture section 5, "The background service", "Keep awake").
//!
//! [`KeepAwake::hold`] keeps an idle-sleep assertion for this process's
//! life, held by a process of its own beside it, never around it: on macOS
//! `caffeinate -i -w <pid>` (`caffeinate`, on Unix), which ends within a
//! second of this process's end; on Linux `systemd-inhibit` (`inhibit`), asking
//! `idle:sleep`, then `idle`, then nothing, each refusal returned for the
//! caller to warn of, never holding the caller back. The inhibitor's command
//! reads a pipe whose only writer is this process, so this process's end,
//! however it comes, ends the command and with it the inhibitor (checked on
//! 2026-10-25, build plan, "P7 · Background service", L5). Dropping a
//! [`KeepAwake`] ends its holder at once. Elsewhere nothing is held, and
//! nothing is said.

use std::process::Child;

/// An idle-sleep assertion asked of the system and not held, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// What was asked: `idle:sleep` or `idle`.
    pub asked: String,
    /// Why it is not held: what the system said, or why its program could
    /// not start or did not answer.
    pub reason: String,
}

/// An idle-sleep assertion held for this process, or none: its holder is
/// stopped and reaped when it is dropped.
#[derive(Debug)]
pub struct KeepAwake {
    holder: Option<Child>,
}

impl KeepAwake {
    /// No assertion.
    pub fn none() -> Self {
        Self { holder: None }
    }

    /// An idle-sleep assertion for this process's life, `why` naming it
    /// where the system shows it (Linux), with what was refused on the way.
    /// Called before this process starts any other child, so that no
    /// child it starts at the same moment inherits the holder's pipe before
    /// it is marked close-on-exec (macOS has no `pipe2`).
    pub fn hold(why: &str) -> (Self, Vec<Refusal>) {
        imp::hold(why)
    }

    /// Whether an assertion's holder was started.
    pub fn is_held(&self) -> bool {
        self.holder.is_some()
    }

    /// The process id of the assertion's holder.
    #[cfg(all(test, unix))]
    fn holder(&self) -> Option<u32> {
        self.holder.as_ref().map(Child::id)
    }
}

impl Drop for KeepAwake {
    /// The holder's pipe is closed first, which ends the inhibitor's
    /// command whatever becomes of the inhibitor; then it is killed and
    /// reaped.
    fn drop(&mut self) {
        if let Some(mut holder) = self.holder.take() {
            drop(holder.stdin.take());
            let _ = holder.kill();
            let _ = holder.wait();
        }
    }
}

#[cfg(unix)]
pub use imp::{ANSWER, caffeinate, inhibit};

#[cfg(unix)]
mod imp {
    use std::io::{BufRead, BufReader, Read};
    use std::process::{Command, Stdio};
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
    use std::thread;
    use std::time::Duration;

    use super::{KeepAwake, Refusal};
    use crate::process::read_capped;

    /// How long an inhibitor is given to say whether it holds. logind
    /// answered at once, a grant or a refusal alike, in the container of
    /// 2026-10-25 (build plan, "P7 · Background service", L5); a bus that
    /// does not answer costs a step this, per assertion asked, never more.
    pub const ANSWER: Duration = Duration::from_secs(5);

    /// What is asked of `systemd-inhibit`, in order: from a user unit or a
    /// session with no seat logind refuses `idle:sleep` and grants `idle`
    /// (L5).
    const ASKED: [&str; 2] = ["idle:sleep", "idle"];

    /// The inhibitor's command: it says it holds, then reads its standard
    /// input, the pipe this process alone writes, to its end.
    const HOLDING: &str = "echo held; exec cat >/dev/null";

    /// The most bytes of an inhibitor's error output kept for its reason.
    const REASON_CAP: usize = 4 * 1024;

    #[cfg(target_os = "macos")]
    pub fn hold(_why: &str) -> (KeepAwake, Vec<Refusal>) {
        caffeinate(Command::new("/usr/bin/caffeinate"), std::process::id())
    }

    #[cfg(target_os = "linux")]
    pub fn hold(why: &str) -> (KeepAwake, Vec<Refusal>) {
        inhibit(|| Command::new("systemd-inhibit"), why, ANSWER)
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    pub fn hold(_why: &str) -> (KeepAwake, Vec<Refusal>) {
        (KeepAwake::none(), Vec::new())
    }

    /// `command`, `caffeinate`, asked to prevent idle sleep until `pid`
    /// ends (`-i -w <pid>`); a program that cannot start is one refusal.
    /// That it then holds is not checked: `pmset` shows it (L5).
    pub fn caffeinate(mut command: Command, pid: u32) -> (KeepAwake, Vec<Refusal>) {
        command
            .args(["-i", "-w", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        match command.spawn() {
            Ok(holder) => (
                KeepAwake {
                    holder: Some(holder),
                },
                Vec::new(),
            ),
            Err(error) => (
                KeepAwake::none(),
                vec![Refusal {
                    asked: "idle".to_owned(),
                    reason: format!("`caffeinate` could not start: {error}"),
                }],
            ),
        }
    }

    /// An inhibitor each `command()` starts, `systemd-inhibit`, asked
    /// `idle:sleep`, then `idle` once that is refused, each given `answer`
    /// to say it holds; with what was refused. A program that cannot start
    /// is one refusal, and nothing more is asked.
    pub fn inhibit(
        command: impl Fn() -> Command,
        why: &str,
        answer: Duration,
    ) -> (KeepAwake, Vec<Refusal>) {
        let mut refusals = Vec::new();
        for what in ASKED {
            let mut inhibitor = command();
            inhibitor
                .arg(format!("--what={what}"))
                .arg("--who=Owlshift")
                .arg(format!("--why={why}"))
                .arg("--mode=block")
                .args(["sh", "-c", HOLDING])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let refused = |reason: String| Refusal {
                asked: what.to_owned(),
                reason,
            };
            let mut holder = match inhibitor.spawn() {
                Ok(holder) => holder,
                Err(error) => {
                    refusals.push(refused(format!("it could not start: {error}")));
                    break;
                }
            };
            let said = first_line(holder.stdout.take());
            let errors = read_capped(holder.stderr.take(), REASON_CAP);
            let said = said.recv_timeout(answer);
            if matches!(&said, Ok(Some(line)) if line.trim() == "held") {
                let held = KeepAwake {
                    holder: Some(holder),
                };
                return (held, refusals);
            }
            // Refused, it ends by itself, its command never run; silent,
            // it is stopped. Either way it is reaped before the next ask.
            drop(KeepAwake {
                holder: Some(holder),
            });
            let reason = match said {
                Err(RecvTimeoutError::Timeout) => format!("no answer within {answer:?}"),
                _ => errors
                    .recv_timeout(Duration::from_secs(1))
                    .ok()
                    .and_then(Result::ok)
                    .and_then(|bytes| {
                        String::from_utf8_lossy(&bytes)
                            .lines()
                            .map(str::trim)
                            .find(|line| !line.is_empty())
                            .map(str::to_owned)
                    })
                    .unwrap_or_else(|| "it ended without holding".to_owned()),
            };
            refusals.push(refused(reason));
        }
        (KeepAwake::none(), refusals)
    }

    /// Reads `stream`'s first line on a thread of its own and sends it, or
    /// `None` when it closed first; the stream is closed once read.
    fn first_line(stream: Option<impl Read + Send + 'static>) -> Receiver<Option<String>> {
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let line = stream.and_then(|stream| {
                let mut line = String::new();
                match BufReader::new(stream).read_line(&mut line) {
                    Ok(read) if read > 0 => Some(line),
                    _ => None,
                }
            });
            // No receiver once the answer came too late.
            let _ = sender.send(line);
        });
        receiver
    }
}

#[cfg(not(unix))]
mod imp {
    use super::{KeepAwake, Refusal};

    pub fn hold(_why: &str) -> (KeepAwake, Vec<Refusal>) {
        (KeepAwake::none(), Vec::new())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::io::Read;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::test_proc::is_alive;

    /// A stand-in for `systemd-inhibit`, run by `sh`: it records its
    /// arguments in `asked` beside it; refuses, as logind does, each
    /// `--what` that `REFUSE` names; says nothing while `SILENT` is set;
    /// otherwise runs its command in a child of its own, as the real one
    /// forks it, on its own standard input. The real inhibitor's release at
    /// the end of that input is L5's live result (2026-10-25), not this.
    const FAKE: &str = r#"echo "$*" >> "$(dirname "$0")/asked"
[ -n "$SILENT" ] && exec sleep 5
case " $REFUSE " in *" ${1#--what=} "*) echo "Failed to inhibit: Access denied" >&2; exit 1;; esac
shift 4
exec 3<&0
"$@" <&3 3<&- &
wait $!
"#;

    fn fake(dir: &Path) -> PathBuf {
        let script = dir.join("systemd-inhibit");
        fs::write(&script, FAKE).unwrap();
        script
    }

    fn fake_command(script: &Path, refuse: &str, silent: bool) -> Command {
        let mut command = Command::new("sh");
        command.arg(script).env("REFUSE", refuse);
        if silent {
            command.env("SILENT", "1");
        }
        command
    }

    fn gone_within(pid: u32, bound: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < bound {
            if !is_alive(pid) {
                return true;
            }
            thread::sleep(Duration::from_millis(50));
        }
        false
    }

    fn reasons(refusals: &[Refusal]) -> Vec<(&str, &str)> {
        refusals
            .iter()
            .map(|refusal| (refusal.asked.as_str(), refusal.reason.as_str()))
            .collect()
    }

    /// `idle:sleep` first, `idle` once it is refused, then nothing: one
    /// refusal each, with what the inhibitor said; a program that cannot
    /// start is one refusal and nothing more is asked; a silent one is
    /// given its bound. What is held ends once dropped.
    #[test]
    fn the_inhibitor_falls_back_and_each_refusal_is_told() {
        let dir = tempfile::tempdir().unwrap();
        let script = fake(dir.path());
        let ask = |refuse: &str, silent: bool, answer: Duration| {
            inhibit(
                || fake_command(&script, refuse, silent),
                "a step of OWL-1",
                answer,
            )
        };

        let (held, refusals) = ask("", false, ANSWER);
        assert!(held.is_held() && refusals.is_empty(), "{refusals:?}");
        let asked = fs::read_to_string(dir.path().join("asked")).unwrap();
        assert_eq!(
            asked,
            "--what=idle:sleep --who=Owlshift --why=a step of OWL-1 --mode=block \
             sh -c echo held; exec cat >/dev/null\n"
        );
        let holder = held.holder().unwrap();
        drop(held);
        assert!(gone_within(holder, Duration::from_secs(5)));

        let denied = "Failed to inhibit: Access denied";
        let (held, refusals) = ask("idle:sleep", false, ANSWER);
        assert!(held.is_held());
        assert_eq!(reasons(&refusals), [("idle:sleep", denied)]);

        let (held, refusals) = ask("idle:sleep idle", false, ANSWER);
        assert!(!held.is_held());
        assert_eq!(
            reasons(&refusals),
            [("idle:sleep", denied), ("idle", denied)]
        );

        let silent = Duration::from_millis(200);
        let (held, refusals) = ask("", true, silent);
        assert!(!held.is_held());
        let late = "no answer within 200ms";
        assert_eq!(reasons(&refusals), [("idle:sleep", late), ("idle", late)]);

        let (held, refusals) = inhibit(|| Command::new(dir.path().join("absent")), "x", ANSWER);
        assert!(!held.is_held());
        assert_eq!(refusals.len(), 1);
        assert_eq!(refusals[0].asked, "idle:sleep");
        assert!(refusals[0].reason.starts_with("it could not start: "));
    }

    /// Holds an assertion as `HOLD_WITH` says, `fake` (the stand-in at
    /// `FAKE`) or `system` (this system's own), writes the holder's pid to
    /// `pids` in its working directory, then waits to be killed.
    #[test]
    #[ignore = "helper, run by the test below"]
    fn helper_hold() {
        if !std::env::args().any(|arg| arg == "--exact") {
            return;
        }
        let (held, _) = match std::env::var("HOLD_WITH").unwrap().as_str() {
            "fake" => {
                let script = PathBuf::from(std::env::var_os("FAKE").unwrap());
                inhibit(|| fake_command(&script, "", false), "test", ANSWER)
            }
            _ => KeepAwake::hold("test"),
        };
        fs::write("pids.partial", held.holder().unwrap().to_string()).unwrap();
        fs::rename("pids.partial", "pids").unwrap();
        thread::sleep(Duration::from_secs(20));
    }

    /// OWL-439's acceptance: a process killed outright while it holds an
    /// assertion leaves nothing: the holder ends with it, and no holder
    /// keeps its output open.
    #[test]
    fn nothing_outlives_the_process_that_holds_it() {
        let dir = tempfile::tempdir().unwrap();
        let script = fake(dir.path());
        let mut ways = vec!["fake"];
        if cfg!(target_os = "macos") {
            ways.push("system");
        }
        for way in ways {
            let cwd = tempfile::tempdir().unwrap();
            let mut helper = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "awake::tests::helper_hold",
                    "--ignored",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .current_dir(cwd.path())
                .env("HOLD_WITH", way)
                .env("FAKE", &script)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let pids = cwd.path().join("pids");
            let start = Instant::now();
            while !pids.exists() && start.elapsed() < Duration::from_secs(10) {
                thread::sleep(Duration::from_millis(50));
            }
            let holder: u32 = fs::read_to_string(&pids).unwrap().parse().unwrap();
            assert!(is_alive(holder), "{way}: no holder");

            let mut output = helper.stdout.take().unwrap();
            helper.kill().unwrap();
            helper.wait().unwrap();
            assert!(gone_within(holder, Duration::from_secs(5)), "{way}");
            let (sender, closed) = mpsc::channel();
            thread::spawn(move || {
                let _ = sender.send(output.read_to_end(&mut Vec::new()).is_ok());
            });
            assert_eq!(
                closed.recv_timeout(Duration::from_secs(3)),
                Ok(true),
                "{way}"
            );
        }
    }
}
