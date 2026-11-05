//! Handing a secret to a child on an inherited descriptor, never in its
//! environment or arguments (OWL-96); and, for the local web UI (OWL-467),
//! a listening socket on `127.0.0.1` the daemon binds and keeps, handed to
//! the UI child it starts, with the reading side of both.
//!
//! The `unsafe` of this module on Unix is two blocks: the `pre_exec`
//! closure of [`inherit`], which every handover uses, and the
//! `OwnedFd::from_raw_fd` of [`adopt`], which every adoption uses.

use std::io;
use std::net::TcpListener;
use std::process::Command;

/// The most bytes [`hand_over_on_descriptor`] hands over, its closing newline
/// included: `PIPE_BUF` on macOS, so the one write is atomic, and less than
/// any pipe's capacity, so it never waits for a reader that is not there yet.
#[cfg(unix)]
const MAX_FRAME: usize = 512;

/// Makes `command`'s child, and it alone, inherit a copy of `fd`, and
/// returns that copy's number for the caller to name to the child. The copy
/// lies above the standard streams, which the child's own are copied onto
/// before `pre_exec` runs, and is close-on-exec in this process, so no other
/// process this one starts inherits it; it is closed when `command` is
/// dropped: drop it right after the spawn.
#[cfg(unix)]
fn inherit(command: &mut Command, fd: std::os::fd::BorrowedFd<'_>) -> io::Result<i32> {
    use rustix::io::{FdFlags, fcntl_dupfd_cloexec, fcntl_setfd};
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;

    let copy: OwnedFd = fcntl_dupfd_cloexec(fd, 3)?;
    let number = copy.as_raw_fd();
    // SAFETY: the closure runs in the child between fork and exec, where
    // only async-signal-safe calls are allowed: it makes one `fcntl` call on
    // a descriptor the closure owns, and neither allocates nor locks.
    unsafe {
        command.pre_exec(move || fcntl_setfd(&copy, FdFlags::empty()).map_err(io::Error::from));
    }
    Ok(number)
}

/// Takes ownership of the descriptor `fd` a parent handed this process,
/// named in its arguments: refused below 3, the standard streams, and when
/// it is not open.
#[cfg(unix)]
fn adopt(fd: i32) -> io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::{FromRawFd, OwnedFd};

    if fd < 3 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("descriptor {fd} is a standard stream: a handed-over one is 3 or above"),
        ));
    }
    if std::fs::metadata(format!("/dev/fd/{fd}")).is_err() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("descriptor {fd} is not open: nothing was handed over on it"),
        ));
    }
    // SAFETY: `fd` is open, checked above, and above the standard streams.
    // It comes from this process's arguments, where the parent that handed
    // it over named it; the callers adopt each number once, at the start,
    // before this process opens anything of its own, so no other owner of
    // it exists in this process.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

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
    use std::io::Write;
    use std::os::fd::{AsFd, OwnedFd};

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
    inherit(command, OwnedFd::from(reader).as_fd())
}

/// Native Windows runs no agent (decision D9): there is nothing to hand over.
#[cfg(not(unix))]
pub fn hand_over_on_descriptor(command: &mut Command, secret: &[u8]) -> io::Result<i32> {
    let _ = (command, secret);
    Err(unsupported())
}

/// Reads the secret a parent handed this process on the descriptor `fd`
/// ([`hand_over_on_descriptor`]): what precedes the newline. The descriptor
/// is closed after. A descriptor that holds no newline within 511 bytes is
/// refused, and the error never holds what it read.
#[cfg(unix)]
pub fn take_handed_over(fd: i32) -> io::Result<Vec<u8>> {
    use std::io::Read;

    let mut file = std::fs::File::from(adopt(fd)?);
    let mut frame = Vec::with_capacity(MAX_FRAME);
    let mut chunk = [0; MAX_FRAME];
    while !frame.contains(&b'\n') && frame.len() < MAX_FRAME {
        let read = file.read(&mut chunk[..MAX_FRAME - frame.len()])?;
        if read == 0 {
            break;
        }
        frame.extend_from_slice(&chunk[..read]);
    }
    match frame.iter().position(|byte| *byte == b'\n') {
        Some(end) => {
            frame.truncate(end);
            Ok(frame)
        }
        None => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("descriptor {fd} held no line of at most 511 bytes"),
        )),
    }
}

/// Native Windows runs no daemon (decision D9).
#[cfg(not(unix))]
pub fn take_handed_over(fd: i32) -> io::Result<Vec<u8>> {
    let _ = fd;
    Err(unsupported())
}

/// Binds `127.0.0.1:port` and listens, while no process tree can start, so
/// that no tree forks between the socket's creation and its close-on-exec
/// flag on a system that sets it after, as macOS (OWL-467). Port 0 picks a
/// free one.
#[cfg(unix)]
pub fn bind_loopback(port: u16) -> io::Result<TcpListener> {
    super::signals::while_no_tree_starts(|| {
        TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
    })
}

/// Native Windows runs no daemon (decision D9).
#[cfg(not(unix))]
pub fn bind_loopback(port: u16) -> io::Result<TcpListener> {
    let _ = port;
    Err(unsupported())
}

/// Makes `command`'s child, and it alone, inherit a copy of `listener`, and
/// returns its number for the caller to name to the child (OWL-467). This
/// process's copies stay close-on-exec, so no watch, step or agent run it
/// starts inherits the socket; the child's copy is closed here when
/// `command` is dropped: drop it right after the spawn.
#[cfg(unix)]
pub fn hand_over_listener(command: &mut Command, listener: &TcpListener) -> io::Result<i32> {
    use std::os::fd::AsFd;
    inherit(command, listener.as_fd())
}

/// Native Windows runs no daemon (decision D9).
#[cfg(not(unix))]
pub fn hand_over_listener(command: &mut Command, listener: &TcpListener) -> io::Result<i32> {
    let _ = (command, listener);
    Err(unsupported())
}

/// The listening socket a parent handed this process on the descriptor
/// `fd` ([`hand_over_listener`]), made close-on-exec again so no process
/// this one starts inherits it, and blocking. A descriptor that is not a
/// socket bound on `127.0.0.1` is refused, and closed.
#[cfg(unix)]
pub fn adopt_listener(fd: i32) -> io::Result<TcpListener> {
    use rustix::io::{FdFlags, fcntl_setfd};

    let listener = TcpListener::from(adopt(fd)?);
    fcntl_setfd(&listener, FdFlags::CLOEXEC)?;
    let address = listener.local_addr().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("descriptor {fd} is not a listening socket: {error}"),
        )
    })?;
    if address.ip() != std::net::IpAddr::from(std::net::Ipv4Addr::LOCALHOST) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("descriptor {fd} listens on {address}, not on 127.0.0.1"),
        ));
    }
    listener.set_nonblocking(false)?;
    Ok(listener)
}

/// Native Windows runs no daemon (decision D9).
#[cfg(not(unix))]
pub fn adopt_listener(fd: i32) -> io::Result<TcpListener> {
    let _ = fd;
    Err(unsupported())
}

#[cfg(not(unix))]
fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "a descriptor is handed over only on Unix",
    )
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

    /// OWL-467: the loopback listener binds on 127.0.0.1.
    #[test]
    fn a_loopback_listener_binds_on_127_0_0_1() {
        let listener = bind_loopback(0).unwrap();
        let address = listener.local_addr().unwrap();
        assert_eq!(address.ip(), std::net::Ipv4Addr::LOCALHOST);
        assert_ne!(address.port(), 0);
    }

    /// OWL-467: the child the listener is handed to has it; a process
    /// started meanwhile does not.
    #[test]
    fn only_the_child_gets_the_listener() {
        let listener = bind_loopback(0).unwrap();
        let mut command = sh(r#"[ -e "/dev/fd/$FD" ] && echo open || echo closed"#, 0);
        let number = hand_over_listener(&mut command, &listener).unwrap();
        assert!(number >= 3, "{number}");
        command.env("FD", number.to_string());
        let other = output(&mut sh(
            r#"[ -e "/dev/fd/$FD" ] && echo open || echo closed"#,
            number,
        ));
        assert_eq!(other, "closed\n");
        assert_eq!(output(&mut command), "open\n");
    }

    const CHILD: &str = "OWLSHIFT_TEST_HANDOVER_CHILD";

    /// The child's half of [`a_child_adopts_both_and_hands_neither_on`]:
    /// run only as that child, a pass otherwise.
    #[test]
    fn handover_child() {
        use std::io::{Read, Write};
        let Ok(numbers) = std::env::var(CHILD) else {
            return;
        };
        let (listener, secret) = numbers.split_once(',').unwrap();
        let (listener, secret): (i32, i32) = (listener.parse().unwrap(), secret.parse().unwrap());
        let adopted = adopt_listener(listener).unwrap();
        let read = take_handed_over(secret).unwrap();
        // On a line of its own: the test harness prints the test's name
        // first.
        println!("\nsecret={}", String::from_utf8(read).unwrap());
        let (mut stream, _) = adopted.accept().unwrap();
        let mut ping = [0; 4];
        stream.read_exact(&mut ping).unwrap();
        stream.write_all(b"pong").unwrap();
        println!("got={}", String::from_utf8_lossy(&ping));
        let seen = Command::new("sh")
            .args([
                "-c",
                r#"for n in "$L" "$S"; do [ -e "/dev/fd/$n" ] && echo "fd$n=open" || echo "fd$n=closed"; done"#,
            ])
            .env("L", listener.to_string())
            .env("S", secret.to_string())
            .output()
            .unwrap();
        print!("{}", String::from_utf8(seen.stdout).unwrap());
    }

    /// OWL-467: a child given the listener and a secret on one command
    /// adopts both, accepts a connection on the listener and reads the
    /// secret without its newline; a process it starts has neither.
    #[test]
    fn a_child_adopts_both_and_hands_neither_on() {
        use std::io::{Read, Write};
        let listener = bind_loopback(0).unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "process::handover::tests::handover_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .stdout(Stdio::piped());
        let l = hand_over_listener(&mut command, &listener).unwrap();
        let s = hand_over_on_descriptor(&mut command, SECRET.as_bytes()).unwrap();
        command.env(CHILD, format!("{l},{s}"));
        let child = command.spawn().unwrap();
        drop(command);
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.write_all(b"ping").unwrap();
        let mut pong = [0; 4];
        stream.read_exact(&mut pong).unwrap();
        assert_eq!(&pong, b"pong");
        let output = child.wait_with_output().unwrap();
        let seen = String::from_utf8(output.stdout).unwrap();
        assert!(output.status.success(), "{seen}");
        for line in [
            format!("secret={SECRET}"),
            "got=ping".to_owned(),
            format!("fd{l}=closed"),
            format!("fd{s}=closed"),
        ] {
            assert!(seen.lines().any(|seen| seen == line), "{line} in\n{seen}");
        }
    }

    /// OWL-467: a standard stream, a descriptor that is not a socket, and a
    /// descriptor that holds no line are refused, the error never holding
    /// what was read.
    #[test]
    fn a_wrong_descriptor_is_refused() {
        use std::io::Write;
        use std::os::fd::IntoRawFd;
        assert!(adopt_listener(1).is_err());
        assert!(take_handed_over(2).is_err());
        let (reader, _writer) = io::pipe().unwrap();
        assert!(adopt_listener(reader.into_raw_fd()).is_err());
        let (reader, mut writer) = io::pipe().unwrap();
        writer.write_all(b"abc").unwrap();
        drop(writer);
        let error = take_handed_over(reader.into_raw_fd()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(!error.to_string().contains("abc"), "{error}");
    }
}
