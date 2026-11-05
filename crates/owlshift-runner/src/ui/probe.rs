//! Whether the web UI listens, proved (OWL-467): `owlshift ui` and `owlshift
//! doctor` send `POST /api/proof` with a fresh challenge and check the
//! answer under the token. Only a holder of the token answers it, so a code
//! is minted only for a listener that proved itself the web UI.
//!
//! The request is written by hand over a plain `TcpStream` to `127.0.0.1`,
//! within [`TIMEOUT`]: no proxy, no dependency, and the bytes it sends are
//! the challenge alone, never the token.

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::Path;
use std::time::Duration;

use owlshift_contracts::config::PersonalConfig;
use owlshift_contracts::service::UiRecord;
use serde_json::{Value, json};

use super::auth::{self, CHALLENGE_LEN};
use super::{record, token};
use crate::config::FileState;

/// How long the probe waits to connect, and then for each read.
pub const TIMEOUT: Duration = Duration::from_secs(2);

/// The most bytes of an answer the probe reads.
const MAX_ANSWER: u64 = 64 * 1024;

/// What the probe found on the port.
#[derive(Clone, Debug, PartialEq)]
pub enum Probe {
    /// The web UI answered with its proof; its record, when it reads.
    Proved(Option<UiRecord>),
    /// Nothing listens.
    Refused,
    /// The connection was taken and nothing answered in time. `recorded`:
    /// a web UI recorded itself in `ui.json`, so this is the daemon holding
    /// the port while it starts the UI again; without a record, another
    /// program may hold it.
    Silent { recorded: bool },
    /// Something answered without the web UI's proof: why.
    NotProved(String),
    /// The probe could not be made, or the answer not read whole: why.
    Unread(String),
}

/// The port to check: the one the web UI recorded in `ui.json`, else `[ui]
/// port` of the personal file.
pub fn port_to_check(data: &Path, personal: &FileState<PersonalConfig>) -> u16 {
    match record::read(data) {
        Ok(Some(record)) => record.port,
        _ => super::port_of(personal),
    }
}

/// Probes `127.0.0.1:<port>` for the web UI of the data directory `data`.
pub fn probe(data: &Path, port: u16) -> Probe {
    probe_within(data, port, TIMEOUT)
}

/// [`probe`], waiting `timeout` at most for each step.
pub fn probe_within(data: &Path, port: u16, timeout: Duration) -> Probe {
    let token = match token::read_token_if_any(data) {
        Ok(token) => token,
        Err(error) => {
            return Probe::Unread(format!(
                "{} cannot be read: {error}",
                token::token_file(data).display()
            ));
        }
    };
    let challenge: [u8; CHALLENGE_LEN] = match auth::random() {
        Ok(challenge) => challenge,
        Err(error) => return Probe::Unread(format!("no challenge could be made: {error}")),
    };
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = match TcpStream::connect_timeout(&address, timeout) {
        Ok(stream) => stream,
        Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => return Probe::Refused,
        Err(error) if error.kind() == io::ErrorKind::TimedOut => return silent(data),
        Err(error) => return Probe::Unread(format!("cannot connect: {error}")),
    };
    let body = json!({ "challenge": auth::hex(&challenge) }).to_string();
    let request = format!(
        "POST /api/proof HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: \
         application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let sent = stream
        .set_read_timeout(Some(timeout))
        .and_then(|()| stream.set_write_timeout(Some(timeout)))
        .and_then(|()| stream.write_all(request.as_bytes()));
    if let Err(error) = sent {
        return Probe::NotProved(format!(
            "it took the connection, then not the request: {error}"
        ));
    }
    let mut answer = Vec::new();
    let read = (&mut stream).take(MAX_ANSWER).read_to_end(&mut answer);
    let complete = match read {
        Ok(_) => true,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) =>
        {
            if answer.is_empty() {
                return silent(data);
            }
            false
        }
        Err(error) if answer.is_empty() => {
            return Probe::NotProved(format!("it closed the connection: {error}"));
        }
        Err(error) => return Probe::Unread(format!("its answer stopped midway: {error}")),
    };
    if answer.is_empty() {
        return Probe::NotProved("it closed the connection without an answer".to_owned());
    }
    let proof = match proof_of(&answer, complete) {
        Ok(proof) => proof,
        Err(Answer::Cut) => return Probe::Unread("its answer was cut short".to_owned()),
        Err(Answer::Not(why)) => return Probe::NotProved(why),
    };
    let Some(token) = token else {
        return Probe::NotProved(format!(
            "it answered, but there is no token at {} to check it with",
            token::token_file(data).display()
        ));
    };
    if auth::check_proof(&token, &challenge, &proof) {
        Probe::Proved(record::read(data).ok().flatten())
    } else {
        Probe::NotProved("its answer is not the web UI's proof".to_owned())
    }
}

fn silent(data: &Path) -> Probe {
    Probe::Silent {
        recorded: matches!(record::read(data), Ok(Some(_))),
    }
}

/// Why an answer holds no proof.
enum Answer {
    /// It ended before its head or its body did.
    Cut,
    /// It is whole and no proof: why.
    Not(String),
}

/// The proof of a whole HTTP answer, `complete` when it was read to its
/// end.
fn proof_of(answer: &[u8], complete: bool) -> Result<String, Answer> {
    let Some(split) = answer.windows(4).position(|four| four == b"\r\n\r\n") else {
        return Err(Answer::Cut);
    };
    let head = String::from_utf8_lossy(&answer[..split]);
    let mut body = &answer[split + 4..];
    let status = head.lines().next().unwrap_or("");
    if !status.starts_with("HTTP/1.") {
        return Err(Answer::Not("its answer is not HTTP".to_owned()));
    }
    let header = |name: &str| {
        head.lines().skip(1).find_map(|line| {
            let (field, value) = line.split_once(':')?;
            field
                .trim()
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
    };
    let joined;
    if header("Transfer-Encoding").is_some_and(|coding| coding.eq_ignore_ascii_case("chunked")) {
        joined = dechunk(body).ok_or(Answer::Cut)?;
        body = &joined;
    } else if let Some(length) = header("Content-Length") {
        let length: usize = length
            .parse()
            .map_err(|_| Answer::Not("its answer's length is not a number".to_owned()))?;
        if body.len() < length {
            return Err(Answer::Cut);
        }
        body = &body[..length];
    } else if !complete {
        return Err(Answer::Cut);
    }
    if status.split(' ').nth(1) != Some("200") {
        return Err(Answer::Not(format!("it answered {status:?}")));
    }
    let value: Value = serde_json::from_slice(body)
        .map_err(|_| Answer::Not("its answer is not the web UI's JSON".to_owned()))?;
    value
        .get("proof")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| Answer::Not("its answer holds no proof".to_owned()))
}

/// The body of a chunked answer, `None` when it ends before its last chunk.
fn dechunk(mut body: &[u8]) -> Option<Vec<u8>> {
    let mut joined = Vec::new();
    loop {
        let end = body.windows(2).position(|pair| pair == b"\r\n")?;
        let size = std::str::from_utf8(&body[..end]).ok()?;
        let size = usize::from_str_radix(size.split(';').next()?.trim(), 16).ok()?;
        if size == 0 {
            return Some(joined);
        }
        let chunk = body.get(end + 2..end + 2 + size)?;
        joined.extend_from_slice(chunk);
        body = body.get(end + 2 + size + 2..)?;
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::thread;

    use super::*;
    use crate::ui::auth::RunSecret;
    use crate::ui::server::{self, Options};

    /// A listener that answers each connection with `answer` and closes.
    fn answering(answer: &'static [u8]) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut buffer = [0u8; 4096];
                let _ = stream.read(&mut buffer);
                let _ = stream.write_all(answer);
            }
        });
        port
    }

    #[test]
    fn the_web_ui_proves_itself() {
        let data = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let options = Options {
            personal: Arc::new(|| FileState::Absent("/nowhere/owlshift.toml".into())),
            ..Options::default()
        };
        let ui = server::start(
            data.path(),
            listener,
            RunSecret::new().unwrap(),
            None,
            options,
        )
        .unwrap();
        match probe(data.path(), ui.port()) {
            Probe::Proved(Some(record)) => assert_eq!(record.port, ui.port()),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            port_to_check(data.path(), &FileState::Absent("/x".into())),
            ui.port()
        );
    }

    #[test]
    fn a_wrong_proof_or_no_token_is_not_proved() {
        let data = tempfile::tempdir().unwrap();
        let port = answering(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 15\r\n\r\n{\"proof\":\"00\"}\n",
        );
        // No token file: any answer is not proved.
        match probe(data.path(), port) {
            Probe::NotProved(why) => assert!(why.contains("no token"), "{why}"),
            other => panic!("{other:?}"),
        }
        token::ensure_token(data.path()).unwrap();
        match probe(data.path(), port) {
            Probe::NotProved(why) => assert!(why.contains("not the web UI's proof"), "{why}"),
            other => panic!("{other:?}"),
        }
        let plain = answering(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
        assert!(matches!(probe(data.path(), plain), Probe::NotProved(_)));
        let closed = answering(b"");
        assert!(matches!(probe(data.path(), closed), Probe::NotProved(_)));
    }

    #[test]
    fn a_half_answer_is_unread() {
        let data = tempfile::tempdir().unwrap();
        token::ensure_token(data.path()).unwrap();
        let port = answering(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{\"pro");
        match probe(data.path(), port) {
            Probe::Unread(why) => assert!(why.contains("cut short"), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_listener_that_never_answers_is_silent() {
        let data = tempfile::tempdir().unwrap();
        // Bound and never accepted: the connection waits in its queue.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let within = Duration::from_millis(300);
        assert_eq!(
            probe_within(data.path(), port, within),
            Probe::Silent { recorded: false }
        );
        record::write(
            data.path(),
            &UiRecord {
                format: owlshift_contracts::format::Format,
                pid: 1,
                port,
                since: jiff::Timestamp::now(),
                build: None,
            },
        )
        .unwrap();
        assert_eq!(
            probe_within(data.path(), port, within),
            Probe::Silent { recorded: true }
        );
        drop(listener);
    }

    #[test]
    fn a_freed_port_is_refused() {
        let data = tempfile::tempdir().unwrap();
        let port = {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        // Windows tries a refused loopback connection again for about two
        // seconds, as long as `TIMEOUT`, before it says so; the web UI, like
        // the daemon, never runs there.
        assert_eq!(
            probe_within(data.path(), port, Duration::from_secs(10)),
            Probe::Refused
        );
    }

    #[test]
    fn the_port_to_check_is_the_records_else_the_personal_files() {
        let data = tempfile::tempdir().unwrap();
        let personal = FileState::from_text(
            "/home/ada/.config/owlshift/owlshift.toml".into(),
            "[ui]\nport = 18500\n",
            PersonalConfig::parse,
        );
        assert_eq!(port_to_check(data.path(), &personal), 18500);
        std::fs::create_dir_all(crate::daemon::service_dir(data.path())).unwrap();
        std::fs::write(record::file(data.path()), "{").unwrap();
        assert_eq!(port_to_check(data.path(), &personal), 18500);
    }
}
