//! The web UI on a bench data directory, for a person to drive the overview
//! in a browser (OWL-508; `docs/design/live-checks/owl-508-overview.sh`). It
//! works on a data directory only: no Linear, no GitHub and no daemon.
//!
//! - `owlshift-ui-bench serve <data> [--port 18466]` serves the real web UI
//!   (`owlshift_runner::ui::server`) on `127.0.0.1`, prints the login link,
//!   and prints a fresh one each time a line is read on its standard input,
//!   since a code lasts a minute. It refreshes the overview every second.
//! - `owlshift-ui-bench link <data>` prints one link, which logs in only
//!   while a `serve` runs on the same token.
//! - `owlshift-ui-bench touch-ready <data>` rewrites `ready.json` as read
//!   now, with one more ticket in the column, `DEMO-8`.
//! - `owlshift-ui-bench append-event <data>` appends a park of `DEMO-2` at
//!   the present time to the event log.
//!
//! The link holds a one-time code that logs in: keep it out of chats and
//! pull requests.

use std::io::BufRead as _;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;
use owlshift_runner::config::FileState;
use owlshift_runner::ui::auth::{self, RunSecret};
use owlshift_runner::ui::server::{self, Options};
use owlshift_runner::ui::token;
use owlshift_testkit::ui_bench;

const DEFAULT_PORT: u16 = 18466;

const USAGE: &str = "usage: owlshift-ui-bench serve <data> [--port N] | link <data> | \
                     touch-ready <data> | append-event <data>";

fn link(data: &Path, port: u16) -> Result<String, String> {
    let token = token::read_token(data).map_err(|error| {
        format!(
            "{} cannot be read: {error}",
            token::token_file(data).display()
        )
    })?;
    let code = auth::mint_code(&token, server::now_ms())
        .map_err(|error| format!("no login code could be made: {error}"))?;
    Ok(format!("http://127.0.0.1:{port}/#login={code}"))
}

fn serve(data: &Path, port: u16) -> Result<(), String> {
    ui_bench::seeded(data)?;
    let listener = TcpListener::bind(("127.0.0.1", port))
        .map_err(|error| format!("127.0.0.1:{port} cannot be bound: {error}"))?;
    let options = Options {
        refresh: Duration::from_secs(1),
        overview_refresh: Duration::from_secs(1),
        // The bench reads nothing of the person's own configuration.
        personal: Arc::new(|| FileState::Absent(PathBuf::from("/nowhere/owlshift.toml"))),
        ..Options::default()
    };
    let run = RunSecret::new().map_err(|error| format!("no run secret: {error}"))?;
    let serving = server::start(data, listener, run, None, options)
        .map_err(|error| format!("the web UI did not start: {error}"))?;
    let port = serving.port();
    println!("The web UI serves {} on 127.0.0.1:{port}.", data.display());
    println!("Open this link within a minute (it holds a one-time code):");
    println!("{}", link(data, port)?);
    println!("Press Enter for a fresh link; end with Ctrl-C.");
    for line in std::io::stdin().lock().lines() {
        if line.is_err() {
            break;
        }
        println!("{}", link(data, port)?);
    }
    // Standard input closed: serve until killed.
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let (command, rest) = args.split_first().ok_or(USAGE)?;
    let (data, options) = rest.split_first().ok_or(USAGE)?;
    let data = Path::new(data);
    match (command.as_str(), options) {
        ("serve", []) => serve(data, DEFAULT_PORT),
        ("serve", [flag, port]) if flag == "--port" => {
            let port = port
                .parse()
                .map_err(|_| format!("{port} is not a port number"))?;
            serve(data, port)
        }
        ("link", []) => {
            println!("{}", link(data, DEFAULT_PORT)?);
            Ok(())
        }
        ("touch-ready", []) => {
            ui_bench::rewrite_ready(data, Timestamp::now(), true)?;
            println!("ready.json now lists {}.", ui_bench::ADDED);
            Ok(())
        }
        ("append-event", []) => {
            let event = ui_bench::parked_event(ui_bench::PARKED_LIVE, Timestamp::now());
            ui_bench::append(data, &[event])?;
            println!("The log records {} parked.", ui_bench::PARKED_LIVE);
            Ok(())
        }
        _ => Err(USAGE.to_owned()),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("owlshift-ui-bench: {why}");
            ExitCode::FAILURE
        }
    }
}
