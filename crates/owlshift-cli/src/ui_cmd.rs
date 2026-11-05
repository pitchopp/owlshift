//! `owlshift ui` (OWL-467, architecture section 5, "The local web UI"):
//! opens the web UI the daemon serves, logged in. It proves the listener
//! under the token first, then mints a one-time code and opens
//! `http://127.0.0.1:<port>/#login=<code>`, or prints the link with
//! `--print`. `--new-token` replaces the token first, which ends every
//! session.
//!
//! `owlshift ui --daemon` is the UI itself, the daemon's child: it serves on
//! the socket the daemon handed it, with the daemon's run secret, both on
//! descriptors the daemon names.

use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, ExitCode, ExitStatus, Stdio};

use owlshift_runner::config::Effective;
use owlshift_runner::daemon;
use owlshift_runner::events::printable;
use owlshift_runner::ui::auth::{self, RunSecret};
use owlshift_runner::ui::probe::{self, Probe};
use owlshift_runner::ui::server::{self, Options};
use owlshift_runner::ui::token;

use crate::service_cmd::ON_WINDOWS;
use crate::{data_dir, fail, own_build};

/// Why `form` of `owlshift ui` is refused on this platform, `windows` when
/// it is native Windows, as the daemon is.
fn refused_on(windows: bool, form: &str) -> Option<String> {
    windows.then(|| format!("`{form}`: {ON_WINDOWS}"))
}

/// `owlshift ui --daemon --listener-fd <n> --run-secret-fd <m>`: serves on
/// the socket and with the run secret the daemon handed over, until the
/// process ends.
pub fn run_daemon(listener_fd: i32, run_secret_fd: i32) -> ExitCode {
    if let Some(refusal) = refused_on(cfg!(windows), "owlshift ui --daemon") {
        return fail(&refusal);
    }
    let data = match data_dir() {
        Ok(data) => data,
        Err(error) => return fail(&error),
    };
    let run = match owlshift_platform::process::take_handed_over(run_secret_fd) {
        Ok(bytes) => match std::str::from_utf8(&bytes)
            .ok()
            .and_then(RunSecret::from_hex)
        {
            Some(run) => run,
            None => return fail("the run secret handed over is not 64 hexadecimal digits"),
        },
        Err(error) => return fail(&format!("the run secret cannot be read: {error}")),
    };
    let listener = match owlshift_platform::process::adopt_listener(listener_fd) {
        Ok(listener) => listener,
        Err(error) => return fail(&format!("the socket handed over cannot be used: {error}")),
    };
    match server::serve(&data, listener, run, Some(own_build()), Options::default()) {
        Ok(never) => match never {},
        Err(error) => fail(&error.to_string()),
    }
}

/// The link that logs a browser in with `code`.
pub fn login_link(port: u16, code: &str) -> String {
    format!("http://127.0.0.1:{port}/#login={code}")
}

/// The program that opens the link on macOS.
const OSASCRIPT: &str = "/usr/bin/osascript";

/// How the person's browser is asked to open a link: a command, and what
/// it reads on its standard input.
#[derive(Debug)]
pub struct Opener {
    /// The command to run.
    pub command: Command,
    /// What the command reads on its standard input, when anything.
    pub input: Option<String>,
}

/// The opener of `link` on this platform ([`opener_for`]).
pub fn opener(link: &str) -> Opener {
    opener_for(cfg!(target_os = "macos"), link)
}

/// The opener of `link`, on macOS when `macos`. On macOS a confined run
/// reads the arguments of the user's other processes (architecture section
/// 5, "Who may drive it"), so the link, its one-time code included, never
/// is one: `osascript` reads `open location "<link>"` on its standard
/// input, as the decider chose on OWL-467 (round 4, Q1). On Linux bwrap
/// gives a confined run its own process namespace, which hides every other
/// process, and `xdg-open` takes the link as its argument.
pub fn opener_for(macos: bool, link: &str) -> Opener {
    if macos {
        let quoted = link.replace('\\', "\\\\").replace('"', "\\\"");
        Opener {
            command: Command::new(OSASCRIPT),
            input: Some(format!("open location \"{quoted}\"\n")),
        }
    } else {
        let mut command = Command::new("xdg-open");
        command.arg(link);
        Opener {
            command,
            input: None,
        }
    }
}

/// Runs `opener`, its input written then closed, and waits for its end.
/// Its standard output goes nowhere; its errors reach the terminal.
fn open_with(opener: Opener) -> io::Result<ExitStatus> {
    let Opener { mut command, input } = opener;
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::null());
    let mut child = command.spawn()?;
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        let written = stdin.write_all(input.as_bytes());
        drop(stdin);
        if let Err(error) = written {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    }
    child.wait()
}

/// What `owlshift ui` says of a listener that did not prove itself, or
/// `None` once it did.
fn not_proved(probe: &Probe, data: &Path, port: u16) -> Option<String> {
    let log = daemon::service_dir(data).join("daemon.log");
    let held = format!(
        "port {port} of 127.0.0.1, `[ui] port` of the personal file, is held by a program that \
         did not prove itself the web UI: free the port, which the daemon tries again, or set \
         another `[ui] port` and restart the service"
    );
    match probe {
        Probe::Proved(_) => None,
        Probe::Refused => Some(format!(
            "nothing listens on 127.0.0.1:{port}: the daemon binds it again at its next try; {} \
             says why",
            log.display()
        )),
        Probe::Silent { recorded: true } => Some(format!(
            "the web UI on 127.0.0.1:{port} does not answer: the daemon starts it again after an \
             end; {} says why",
            log.display()
        )),
        Probe::Silent { recorded: false } => Some(format!(
            "{held}; it took the connection and said nothing. {} says why",
            log.display()
        )),
        Probe::NotProved(why) => Some(format!("{held} ({why})")),
        Probe::Unread(why) => Some(format!(
            "the web UI on 127.0.0.1:{port} could not be checked: {why}"
        )),
    }
}

/// `owlshift ui [--print] [--new-token]`.
pub fn run(config: &Effective, print: bool, new_token: bool) -> ExitCode {
    if let Some(refusal) = refused_on(cfg!(windows), "owlshift ui") {
        return fail(&refusal);
    }
    let data = match data_dir() {
        Ok(data) => data,
        Err(error) => return fail(&error),
    };
    if new_token {
        if let Err(error) = token::new_token(&data) {
            return fail(&format!(
                "{} cannot be replaced: {error}",
                token::token_file(&data).display()
            ));
        }
        eprintln!("A new token: every session of the web UI ended.");
    }
    match daemon::running(&data) {
        Ok(Some(_)) => {}
        Ok(None) => return fail("no daemon runs, so no web UI: `owlshift start` starts both"),
        Err(error) => return fail(&format!("whether the daemon runs cannot be read: {error}")),
    }
    let port = probe::port_to_check(&data, &config.personal);
    if let Some(why) = not_proved(&probe::probe(&data, port), &data, port) {
        return fail(&why);
    }
    let code = match token::read_token(&data)
        .and_then(|token| auth::mint_code(&token, server::now_ms()))
    {
        Ok(code) => code,
        Err(error) => return fail(&format!("no login code could be made: {error}")),
    };
    let link = login_link(port, &code);
    if print {
        println!("{link}");
        return ExitCode::SUCCESS;
    }
    let opened = open_with(opener(&link));
    match opened {
        Ok(status) if status.success() => {
            println!("The web UI opens in your browser, on 127.0.0.1:{port}.");
        }
        Ok(status) => {
            eprintln!(
                "owlshift: the browser did not open ({status}): open this link, valid for a minute:"
            );
            println!("{}", printable(&link));
        }
        Err(error) => {
            eprintln!(
                "owlshift: the browser did not open ({error}): open this link, valid for a minute:"
            );
            println!("{}", printable(&link));
        }
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn on_macos_no_argument_holds_the_login_code() {
        let code = "00112233445566778899aabbccddeeff.1762329600000.ab";
        let link = login_link(18466, code);
        let Opener { command, input } = opener_for(true, &link);
        assert_eq!(command.get_program(), OSASCRIPT);
        assert_eq!(command.get_args().count(), 0);
        assert_eq!(
            input.as_deref(),
            Some(format!("open location \"http://127.0.0.1:18466/#login={code}\"\n").as_str())
        );
        // A quote or a backslash, which no link holds, stays inside the
        // AppleScript string.
        let Opener { input, .. } = opener_for(true, r#"a"b\c"#);
        assert_eq!(input.as_deref(), Some("open location \"a\\\"b\\\\c\"\n"));
    }

    #[test]
    fn elsewhere_xdg_open_takes_the_login_link() {
        let code = "00112233445566778899aabbccddeeff.1762329600000.ab";
        let Opener { command, input } = opener_for(false, &login_link(18466, code));
        assert_eq!(command.get_program(), "xdg-open");
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(
            args,
            [format!("http://127.0.0.1:18466/#login={code}").as_str()]
        );
        assert_eq!(input, None);
    }

    #[cfg(unix)]
    #[test]
    fn an_opener_reads_its_input_on_its_standard_input() {
        // `cat` ends only once its input is closed.
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "cat >/dev/null; exit 5"]);
        let status = open_with(Opener {
            command,
            input: Some("open location \"x\"\n".into()),
        })
        .unwrap();
        assert_eq!(status.code(), Some(5));
        // `sh` reads its script on standard input, as `osascript` does.
        let status = open_with(Opener {
            command: Command::new("/bin/sh"),
            input: Some("exit 7\n".into()),
        })
        .unwrap();
        assert_eq!(status.code(), Some(7));
    }

    #[test]
    fn both_forms_are_refused_on_native_windows_only() {
        for form in ["owlshift ui", "owlshift ui --daemon"] {
            let refusal = refused_on(true, form).unwrap();
            assert!(
                refusal.contains(form) && refusal.contains("WSL2"),
                "{refusal}"
            );
            assert_eq!(refused_on(false, form), None);
        }
    }

    #[test]
    fn each_failed_check_names_the_port_and_says_what_to_do() {
        let data = Path::new("/data");
        assert_eq!(not_proved(&Probe::Proved(None), data, 18466), None);
        let held = not_proved(&Probe::NotProved("it answered 404".into()), data, 18466).unwrap();
        assert!(
            held.contains("port 18466") && held.contains("`[ui] port`"),
            "{held}"
        );
        let silent = not_proved(&Probe::Silent { recorded: false }, data, 18466).unwrap();
        assert!(
            silent.contains("port 18466") && silent.contains("`[ui] port`"),
            "{silent}"
        );
        let restarting = not_proved(&Probe::Silent { recorded: true }, data, 18466).unwrap();
        assert!(restarting.contains("daemon.log"), "{restarting}");
        let refused = not_proved(&Probe::Refused, data, 18466).unwrap();
        assert!(refused.contains("127.0.0.1:18466") && refused.contains("daemon.log"));
        let unread = not_proved(&Probe::Unread("cut short".into()), data, 18466).unwrap();
        assert!(unread.contains("127.0.0.1:18466") && unread.contains("cut short"));
    }
}
