//! The `owlshift` binary.

mod do_cmd;
mod init_cmd;
mod logs_cmd;

use std::process::ExitCode;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use owlshift_contracts::format::{
    BRIEF_FORMAT, CLAIM_FORMAT, EVENT_FORMAT, FOOTER_FORMAT, RESULT_FORMAT, TICKET_STATE_FORMAT,
};
use owlshift_runner::config::{Effective, OWLSHIFT_VERSION};
use owlshift_runner::doctor;
use owlshift_runner::events::printable;
use owlshift_runner::system::HostSystem;

/// Works a team's backlog with coding agents, and asks a human on the ticket
/// when a decision is theirs.
#[derive(Parser)]
#[command(name = "owlshift", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check whether this machine is ready: git, the harness CLIs and their
    /// logins, the configuration files.
    Doctor,
    /// Read the configuration.
    #[command(subcommand)]
    Config(ConfigCommand),
    /// Write a commented owlshift.toml for this repository, then store the
    /// tracker and forge secrets `owlshift do` needs in the system keychain.
    Init(init_cmd::Args),
    /// Run one ticket to a verified pull request, in the foreground.
    Do {
        /// The ticket, such as OWL-12.
        ticket: String,
    },
    /// Print the events `owlshift do` recorded, oldest first.
    Logs {
        /// Only this ticket's events.
        ticket: Option<String>,
        /// Keep printing new events as they are recorded, until Ctrl-C.
        #[arg(long, short)]
        follow: bool,
    },
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Print the effective configuration and the file each value comes from.
    Show,
}

/// `--version`: the binary's version, then the version of every format it
/// reads and writes.
fn long_version() -> String {
    format!(
        "{OWLSHIFT_VERSION}\nformats: brief {BRIEF_FORMAT}, result {RESULT_FORMAT}, \
         event {EVENT_FORMAT}, claim {CLAIM_FORMAT}, ticket state {TICKET_STATE_FORMAT}, \
         comment footer {FOOTER_FORMAT}"
    )
}

fn main() -> ExitCode {
    // Before any probe or run starts: each runs in a process group of its
    // own, out of reach of the terminal's Ctrl-C, so Owlshift stops it itself.
    if let Err(error) = owlshift_platform::process::stop_trees_on_signal() {
        eprintln!("owlshift: cannot watch for Ctrl-C, a running probe would outlive it: {error}");
    }
    let matches = Cli::command().long_version(long_version()).get_matches();
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|error| error.exit());

    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            eprintln!("owlshift: cannot read the current directory: {error}");
            return ExitCode::FAILURE;
        }
    };
    let system = HostSystem;
    let config = Effective::load(
        &system,
        &cwd,
        owlshift_platform::paths::personal_config_file(),
    );

    match cli.command {
        Command::Doctor => {
            let report = doctor::run(&system, &config);
            print!("{report}");
            exit_code(report.ready())
        }
        Command::Config(ConfigCommand::Show) => {
            print!("{config}");
            exit_code(config.is_valid())
        }
        Command::Init(args) => init_cmd::run(&args, &config),
        Command::Do { ticket } => do_cmd::run(&system, &config, &ticket),
        Command::Logs { ticket, follow } => logs_cmd::run(ticket.as_deref(), follow),
    }
}

/// Prints `message` on standard error, safe for a terminal, and fails.
fn fail(message: &str) -> ExitCode {
    eprintln!("owlshift: {}", printable(message));
    ExitCode::FAILURE
}

fn exit_code(success: bool) -> ExitCode {
    if success {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
