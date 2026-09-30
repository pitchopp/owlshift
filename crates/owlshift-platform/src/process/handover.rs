//! Handing a secret to a child on an inherited descriptor, never in its
//! environment or arguments (OWL-96).

use std::io;
use std::process::Command;

/// The most bytes [`hand_over_on_descriptor`] hands over, its closing newline
/// included: `PIPE_BUF` on macOS, so the one write is atomic, and less than
/// any pipe's capacity, so it never waits for a reader that is not there yet.
#[cfg(unix)]
const MAX_FRAME: usize = 512;

/// Puts `secret`, then a newline, in a pipe whose read end `command`'s child
/// inherits, and returns that descriptor's number for the caller to name to
/// the child, as `CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR` does to Claude
/// Code. The secret is in no process's environment or arguments, so no
/// process can read it from another's exec block (`/proc/<pid>/environ`,
/// `sysctl(KERN_PROCARGS2)`); the child reads it once, and a process it starts
/// afterwards finds the pipe drained.
///
/// The write end is closed before the spawn. The runner's own copy of the
/// read end is close-on-exec, so no other process the runner starts inherits
/// it, and it is closed when `command` is dropped: drop it right after the
/// spawn. On a system with no `pipe2`, as macOS, a pipe is made close-on-exec
/// just after it is created; it is made while no process tree can start
/// ([`super::ProcessTree`]), so no harness or gate command is forked in that
/// moment. A process another thread starts with a bare `Command::spawn` then
/// could still inherit it, as for the sentinel's pipe.
///
/// A secret holding a newline, or longer than 511 bytes, is refused; the
/// error never holds it.
#[cfg(unix)]
pub fn hand_over_on_descriptor(command: &mut Command, secret: &[u8]) -> io::Result<i32> {
    use rustix::io::{FdFlags, fcntl_dupfd_cloexec, fcntl_setfd};
    use std::io::Write;
    use std::os::fd::{AsFd, AsRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;

    if secret.contains(&b'\n') || secret.len() >= MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the secret to hand over holds a newline or is longer than 511 bytes",
        ));
    }
    let (reader, mut writer) = super::signals::while_no_tree_starts(io::pipe)?;
    let mut frame = Vec::with_capacity(secret.len() + 1);
    frame.extend_from_slice(secret);
    frame.push(b'\n');
    writer.write_all(&frame)?;
    drop(writer);
    // Above the standard streams, which the child's own are copied onto
    // before `pre_exec` runs; still close-on-exec in the runner.
    let reader: OwnedFd = fcntl_dupfd_cloexec(OwnedFd::from(reader).as_fd(), 3)?;
    let number = reader.as_raw_fd();
    // SAFETY: the closure runs in the child between fork and exec, where
    // only async-signal-safe calls are allowed: it makes one `fcntl` call on
    // a descriptor the closure owns, and neither allocates nor locks. It is
    // the only `unsafe` of this crate on Unix.
    unsafe {
        command.pre_exec(move || fcntl_setfd(&reader, FdFlags::empty()).map_err(io::Error::from));
    }
    Ok(number)
}

/// Native Windows runs no agent (decision D9): there is nothing to hand over.
#[cfg(not(unix))]
pub fn hand_over_on_descriptor(command: &mut Command, secret: &[u8]) -> io::Result<i32> {
    let _ = (command, secret);
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "a secret is handed over on a descriptor only on Unix",
    ))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::Stdio;

    /// Made up.
    const SECRET: &str = "sk-ant-oat01-owl96-FAKE-secret";

    fn sh(line: &str, number: i32) -> Command {
        let mut command = Command::new("sh");
        command
            .args(["-c", line])
            .env("FD", number.to_string())
            .stdout(Stdio::piped());
        command
    }

    fn output(command: &mut Command) -> String {
        let output = command.output().unwrap();
        String::from_utf8(output.stdout).unwrap()
    }

    /// The child reads exactly the secret, once; its environment does not
    /// hold it; and a process the runner starts while the handed-over
    /// command still lives does not have the descriptor.
    #[test]
    fn only_the_child_gets_the_secret_and_only_once() {
        let mut command = sh(
            r#"read -r got < "/dev/fd/$FD"; printf 'got=%s\n' "$got"; printf 'rest=%s\n' "$(cat "/dev/fd/$FD")"; env"#,
            0,
        );
        let number = hand_over_on_descriptor(&mut command, SECRET.as_bytes()).unwrap();
        assert!(number >= 3, "{number}");
        command.env("FD", number.to_string());

        let other = output(&mut sh(
            r#"[ -e "/dev/fd/$FD" ] && echo open || echo closed"#,
            number,
        ));
        assert_eq!(other, "closed\n");

        let seen = output(&mut command);
        drop(command);
        let mut lines = seen.lines();
        assert_eq!(lines.next(), Some(format!("got={SECRET}").as_str()));
        assert_eq!(lines.next(), Some("rest="));
        assert!(lines.all(|line| !line.contains(SECRET)), "{seen}");
    }

    /// A secret that would break the one-line frame, or not fit one atomic
    /// write, is refused, and the error does not show it.
    #[test]
    fn a_secret_holding_a_newline_or_too_long_is_refused() {
        let long = "x".repeat(MAX_FRAME);
        for secret in ["SENTINEL\nowl96", long.as_str()] {
            let error =
                hand_over_on_descriptor(&mut Command::new("true"), secret.as_bytes()).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert!(!error.to_string().contains("SENTINEL"), "{error}");
        }
        let longest = "x".repeat(MAX_FRAME - 1);
        assert!(hand_over_on_descriptor(&mut Command::new("true"), longest.as_bytes()).is_ok());
    }
}
