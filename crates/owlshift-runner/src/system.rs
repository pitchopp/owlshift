//! The runner's view of the host: finding programs and running them. Behind
//! a trait so that checks can be tested without the real CLIs installed.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use owlshift_platform::process::OUTPUT_CAP;
pub use owlshift_platform::process::{Captured, RunError};

/// How long a probe such as `git --version` may take. C8 measured about
/// 0.1 s for the harness status commands.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

pub trait System {
    /// Finds a program on the `PATH`.
    fn locate(&self, program: &str) -> Option<PathBuf>;
    /// Runs a program with no input, within [`PROBE_TIMEOUT`].
    fn run(&self, program: &Path, args: &[&str], cwd: Option<&Path>) -> Result<Captured, RunError>;
}

/// The machine Owlshift runs on.
pub struct HostSystem;

impl System for HostSystem {
    fn locate(&self, program: &str) -> Option<PathBuf> {
        owlshift_platform::process::find_executable(program)
    }

    fn run(&self, program: &Path, args: &[&str], cwd: Option<&Path>) -> Result<Captured, RunError> {
        owlshift_platform::process::run(program, args, cwd, PROBE_TIMEOUT)
    }
}

/// Runs a command the caller prepared (program, arguments, directory,
/// environment), writes `input` on its standard input and captures its
/// output, each stream up to [`OUTPUT_CAP`] bytes. At the deadline the
/// command is killed; a process it started that still holds the output open
/// is left behind rather than waited on.
///
/// The same work as `owlshift_platform::process::run`, which takes neither
/// an environment nor input; fold the two together once that module's
/// process work (OWL-31) has landed.
pub(crate) fn run_command(
    mut command: Command,
    input: &[u8],
    timeout: Duration,
) -> Result<Captured, RunError> {
    let deadline = Instant::now() + timeout;
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(RunError::Io)?;
    let stdout = read_capped(child.stdout.take());
    let stderr = read_capped(child.stderr.take());
    if let Some(mut stdin) = child.stdin.take() {
        // A program that exits without reading its input closes the pipe;
        // its exit status and output tell the rest.
        let _ = stdin.write_all(input);
    }
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                // Best effort: the child may have exited in between.
                let _ = child.kill();
                let _ = child.wait();
                return Err(RunError::TimedOut);
            }
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(RunError::Io(error));
            }
        }
    };
    let receive = |stream: Receiver<Vec<u8>>| {
        stream
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| RunError::TimedOut)
    };
    Ok(Captured {
        code: status.code(),
        stdout: receive(stdout)?,
        stderr: receive(stderr)?,
    })
}

/// Reads a stream to its end on its own thread, keeping the first
/// [`OUTPUT_CAP`] bytes, and sends them once the stream closes.
fn read_capped(stream: Option<impl Read + Send + 'static>) -> Receiver<Vec<u8>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut kept = Vec::new();
        if let Some(mut stream) = stream {
            let mut buffer = [0u8; 8192];
            loop {
                match stream.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        let room = OUTPUT_CAP - kept.len();
                        kept.extend_from_slice(&buffer[..n.min(room)]);
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        }
        // The receiver is gone when the run timed out: nothing to report.
        let _ = sender.send(kept);
    });
    receiver
}

/// The first version number in a program's `--version` output, rebuilt from
/// its digits and dots only, so nothing else the program printed can pass.
///
/// `git version 2.54.0 (Apple Git-157)`, `2.1.283 (Claude Code)` and
/// `codex-cli 0.154.0` give `2.54.0`, `2.1.283` and `0.154.0`.
pub(crate) fn version_of(output: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(output);
    Some(rebuild(version_token(&text)?))
}

/// [`version_of`], only when the version holds nothing but digits and dots:
/// `2.1.283-beta.1` gives `None`, since it is not the release `2.1.283`.
pub(crate) fn exact_version_of(output: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(output);
    let token = version_token(&text)?;
    let version = rebuild(token);
    (version == token).then_some(version)
}

fn version_token(text: &str) -> Option<&str> {
    text.split_whitespace()
        .find(|token| token.starts_with(|c: char| c.is_ascii_digit()))
}

fn rebuild(token: &str) -> String {
    let mut version = String::new();
    for part in token.split('.') {
        let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() {
            break;
        }
        if !version.is_empty() {
            version.push('.');
        }
        version.push_str(&digits);
        if digits.len() != part.len() {
            break;
        }
    }
    version
}

#[cfg(test)]
pub(crate) mod fake {
    //! A scripted host for tests.

    use std::collections::HashMap;

    use super::*;

    pub(crate) enum Answer {
        Exit(i32, &'static str, &'static str),
        TimedOut,
    }

    #[derive(Default)]
    pub(crate) struct FakeSystem {
        located: HashMap<String, PathBuf>,
        answers: HashMap<String, Answer>,
    }

    impl FakeSystem {
        /// Puts a program on the fake `PATH`.
        pub(crate) fn install(mut self, program: &str) -> Self {
            self.located.insert(
                program.to_owned(),
                PathBuf::from(format!("/fake/bin/{program}")),
            );
            self
        }

        /// Scripts the answer to `program args…`, written as one string.
        pub(crate) fn answer(mut self, command: &str, answer: Answer) -> Self {
            self.answers.insert(command.to_owned(), answer);
            self
        }
    }

    impl System for FakeSystem {
        fn locate(&self, program: &str) -> Option<PathBuf> {
            self.located.get(program).cloned()
        }

        fn run(
            &self,
            program: &Path,
            args: &[&str],
            _cwd: Option<&Path>,
        ) -> Result<Captured, RunError> {
            let name = program.file_name().unwrap().to_string_lossy();
            let command = std::iter::once(name.as_ref())
                .chain(args.iter().copied())
                .collect::<Vec<_>>()
                .join(" ");
            match self.answers.get(&command) {
                Some(Answer::Exit(code, stdout, stderr)) => Ok(Captured {
                    code: Some(*code),
                    stdout: stdout.as_bytes().to_vec(),
                    stderr: stderr.as_bytes().to_vec(),
                }),
                Some(Answer::TimedOut) => Err(RunError::TimedOut),
                None => panic!("unscripted command: {command}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_rebuilt_from_digits() {
        assert_eq!(
            version_of(b"git version 2.54.0 (Apple Git-157)\n").as_deref(),
            Some("2.54.0")
        );
        assert_eq!(
            version_of(b"2.1.283 (Claude Code)\n").as_deref(),
            Some("2.1.283")
        );
        assert_eq!(
            version_of(b"codex-cli 0.154.0\n").as_deref(),
            Some("0.154.0")
        );
        assert_eq!(
            version_of(b"123@example.com 4.5-sk-x").as_deref(),
            Some("123")
        );
        assert_eq!(version_of(b"no version here"), None);
    }

    #[test]
    fn an_exact_version_has_no_suffix() {
        assert_eq!(
            exact_version_of(b"2.1.283 (Claude Code)\n").as_deref(),
            Some("2.1.283")
        );
        assert_eq!(exact_version_of(b"2.1.283-beta.1 (Claude Code)\n"), None);
        assert_eq!(exact_version_of(b"no version here"), None);
    }
}
