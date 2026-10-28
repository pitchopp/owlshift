//! The `owlshift` binary.

mod do_cmd;
mod forget_cmd;
mod gate_cmd;
mod init_cmd;
mod logs_cmd;
mod pause_cmd;
mod service_cmd;
mod status_cmd;
mod unquarantine_cmd;
mod why_cmd;

use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use owlshift_adapters::forge::Repo;
use owlshift_contracts::format::{
    BRIEF_FORMAT, CLAIM_FORMAT, EVENT_FORMAT, FOOTER_FORMAT, QUESTIONS_FORMAT, RESULT_FORMAT,
    TICKET_STATE_FORMAT,
};
use owlshift_runner::config::{Effective, OWLSHIFT_VERSION};
use owlshift_runner::doctor;
use owlshift_runner::events::printable;
use owlshift_runner::system::HostSystem;
use owlshift_runner::watch::stop::CHECKED_STOP_BOUND;

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
    /// logins, the agent runs' isolation and Claude Code token, the
    /// configuration files. Says why each problem matters and how to fix it.
    Doctor {
        /// Print the report as JSON, for scripts.
        #[arg(long)]
        json: bool,
    },
    /// Read the configuration.
    #[command(subcommand)]
    Config(ConfigCommand),
    /// Write a commented owlshift.toml for this repository, then store its
    /// tracker and forge secrets and the agent runs' Claude Code token
    /// `owlshift do` needs in the system keychain.
    Init(init_cmd::Args),
    /// Run one ticket to a verified pull request, in the foreground. Its
    /// questions are posted on the ticket.
    Do {
        /// The ticket, such as OWL-12.
        ticket: String,
    },
    /// Continue a ticket once its decider has answered on it: once their
    /// reply has been left unedited for 10 minutes, or ends with `go`, check
    /// the answers, then ask again what is missing or run on to a verified
    /// pull request. Restarts a parked ticket.
    Continue {
        /// The ticket, such as OWL-12.
        ticket: String,
        /// Restart a parked ticket and return without running it:
        /// `owlshift watch` drives it from its next pass. On a ticket that
        /// is not parked, runs nothing and says what watch does with it.
        #[arg(long, conflicts_with = "step")]
        to_watch: bool,
        /// The step `owlshift watch --drain` runs in this process, as its
        /// envelope (OWL-399): watch's own, never typed.
        #[arg(long, hide = true, value_name = "ENVELOPE")]
        step: Option<String>,
        /// The step's served project, its watch's (OWL-434): watch's own,
        /// never typed.
        #[arg(long, hide = true, requires = "step", value_name = "OWNER/REPO")]
        served: Option<String>,
    },
    /// Watch this project's tickets whose questions wait, in the foreground,
    /// and continue each one as `owlshift continue` would once its decider's
    /// reply counts; checked every minute. A first Ctrl-C ends it once the
    /// steps under way finish, a second (or SIGTERM) stops them and checks
    /// their runs, a third (or SIGQUIT) stops everything at once. It also resumes a
    /// ticket at Intake, Design, Design review, Build or Verify that an
    /// answered round (round > 0) left shown working or needing input, a
    /// refusal after the answer check included. A parked ticket, or one left
    /// at Build before any question round, still needs `owlshift continue`;
    /// `owlshift continue --to-watch` restarts a park and leaves it to watch.
    Watch {
        /// Also take admitted tickets in from the ready column, intake
        /// first, and dispatch them to Build one at a time.
        #[arg(long)]
        drain: bool,
        /// Watch this served project, found in the list of served projects
        /// rather than from the current directory, on the project file of
        /// its default branch (OWL-434): the service's own, never typed.
        #[arg(long, hide = true, requires = "drain", value_name = "OWNER/REPO")]
        served: Option<String>,
    },
    /// Start over what Owlshift keeps on a ticket between commands, when it
    /// has no room left or cannot be read: its asks and decisions go, their
    /// comments staying on the ticket; its round count and the refusals its
    /// next runs are told stay. Refused while another command works the
    /// project.
    Forget {
        /// The ticket, such as OWL-12.
        ticket: String,
    },
    /// Run the project's gate once, as a Build's gate runs it: in a fresh
    /// worktree of origin's default branch, confined, with the variables
    /// the project declares and you allow. Prints each command's last lines,
    /// status and time, then removes the worktree. The gate is the project
    /// file's at that commit, or yours when it has none. Refused while
    /// another command works the project.
    Gate {
        /// Check this commit instead: a full commit id, or a branch of
        /// origin such as `origin/fix-gate`.
        #[arg(long, value_name = "COMMIT")]
        at: Option<String>,
    },
    /// Clear this project's quarantine: the marker a run whose isolation
    /// check did not pass leaves, which makes every command refuse the
    /// project. Shows what the marker says, then clears it once you type the
    /// project's name (owner/repo) at a terminal. The dedicated checkout and
    /// the worktrees stay as they are. Refused while another command works
    /// the project.
    Unquarantine,
    /// Say why this project's admitted tickets are not running: blockers,
    /// the ticket or pull request that holds what they would touch, the
    /// caps, a harness at its usage limit, the quarantine, and the people's
    /// tickets left out for want of zones; then the machine's run slots and
    /// the harnesses limited. With a ticket, its reasons over time. Read
    /// only, takes no lock, and works while `owlshift watch` runs.
    Why {
        /// Only this ticket, such as OWL-12, with its reasons over time.
        ticket: Option<String>,
    },
    /// Serve this project with the background service, and start the service
    /// for this login session when it does not run: it keeps one `owlshift
    /// watch --drain` per served project alive, without a terminal, each on
    /// the project file of its default branch. Its output goes to
    /// `service/daemon.log` in the data directory.
    Start {
        /// Also install the service to start at each login, as a user
        /// LaunchAgent (macOS) or a systemd user unit (Linux): `owlshift stop
        /// --at-login` or `owlshift uninstall` removes it.
        #[arg(long)]
        at_login: bool,
    },
    /// Stop the background service: each watch ends once its steps under way
    /// finish, then the service exits. With --forget, stop serving this
    /// project only.
    Stop {
        /// Stop serving the project of the current directory, the service
        /// going on with the others.
        #[arg(long)]
        forget: bool,
        /// Also remove the service installed at login, once the daemon has
        /// ended: this waits for the steps under way to finish.
        #[arg(long, conflicts_with = "forget")]
        at_login: bool,
    },
    /// Pause every project of this machine, or only OWNER/REPO: from each
    /// watch's next pass, no ticket is dispatched and no step starts, nor a
    /// merge on green; the steps under way finish, and answers, pull
    /// requests and stages are still read. The service keeps running.
    Pause {
        /// Pause only this project, such as acme/web.
        #[arg(value_name = "OWNER/REPO")]
        repo: Option<String>,
        /// Why, shown by `owlshift status` and the digest.
        #[arg(long, value_name = "TEXT")]
        reason: Option<String>,
    },
    /// Lift the pause of every project of this machine, or of OWNER/REPO
    /// only: each watch goes on at its next pass.
    Resume {
        /// Lift only this project's pause, such as acme/web.
        #[arg(value_name = "OWNER/REPO")]
        repo: Option<String>,
    },
    /// Say what the background service serves and does: the daemon, each
    /// served project's watch (running, restarting and why, paused and why,
    /// draining) and the file it runs on, the tickets waiting for a person,
    /// the steps under way, each harness's usage and reset, the latest
    /// digest. Read only, takes no lock.
    Status,
    /// Remove what Owlshift installed on this machine, as its install
    /// manifest lists it: the service at login and the keychain entries
    /// `owlshift init` stored. A daemon that runs is stopped first, its runs
    /// checked. Repositories, tickets and branches are never touched.
    Uninstall {
        /// Also remove the data directory (the dedicated clones, worktrees,
        /// run logs and events) and the personal file.
        #[arg(long)]
        purge: bool,
    },
    /// The background service itself, in the foreground: `owlshift start`
    /// runs it. One per data directory; a second exits at once. Ctrl-C ends
    /// it once its watches drained, a second (or SIGTERM) stops their steps
    /// checking their runs, a third stops everything now.
    Daemon,
    /// Print the events `owlshift do`, `continue`, `watch` and `forget`
    /// recorded, oldest first.
    Logs {
        /// Only this ticket's events.
        ticket: Option<String>,
        /// Only the last N events, after the ticket filter (at least 1).
        #[arg(long, value_name = "N")]
        last: Option<NonZeroUsize>,
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
         ticket questions {QUESTIONS_FORMAT}, comment footer {FOOTER_FORMAT}"
    )
}

fn main() -> ExitCode {
    // Before any probe or run starts: on Unix each runs in a process group of
    // its own, out of reach of the terminal's Ctrl-C, and on Windows one that
    // ignores Ctrl-C would outlive Owlshift, so Owlshift stops them itself.
    if let Err(error) = owlshift_platform::process::stop_trees_on_signal() {
        eprintln!("owlshift: cannot watch for Ctrl-C, a running probe would outlive it: {error}");
    }
    // And when Owlshift is killed outright, a crash included, which runs no
    // handler: a sentinel process stops them. First, before any other thread
    // starts a process.
    #[cfg(unix)]
    if let Err(error) = owlshift_platform::process::stop_trees_when_killed() {
        eprintln!(
            "owlshift: cannot start its sentinel, a running probe would outlive a hard kill: {error}"
        );
    }
    let matches = Cli::command().long_version(long_version()).get_matches();
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|error| error.exit());
    // `owlshift watch` and the steps it runs in children stop by degrees
    // (OWL-161), as soon as the command is known: a step's child before it
    // takes its run slot or writes anything.
    if let Some(bound) = by_degrees(&cli.command)
        && let Err(error) = owlshift_platform::process::stop_by_degrees(bound)
    {
        eprintln!("owlshift: cannot stop by degrees, a signal stops everything at once: {error}");
    }

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

    let served = match &cli.command {
        Command::Watch {
            served: Some(repo), ..
        }
        | Command::Continue {
            served: Some(repo), ..
        } => match Repo::parse(repo) {
            Ok(repo) => Some(repo),
            Err(error) => return fail(&format!("--served: {error}")),
        },
        _ => None,
    };
    let served = served.as_ref();

    match cli.command {
        Command::Doctor { json } => {
            let report = doctor::run(&system, &config);
            if json {
                println!("{:#}", report.to_json());
            } else {
                print!("{}", report.render(&doctor::render::Style::for_stdout()));
            }
            exit_code(report.ready())
        }
        Command::Config(ConfigCommand::Show) => {
            print!("{config}");
            exit_code(config.is_valid())
        }
        Command::Init(args) => init_cmd::run(&args, &config),
        Command::Do { ticket } => do_cmd::run(&system, &config, do_cmd::Mode::Do(&ticket), None),
        Command::Continue {
            ticket,
            step: None,
            to_watch: false,
            ..
        } => do_cmd::run(&system, &config, do_cmd::Mode::Continue(&ticket), None),
        Command::Continue {
            ticket,
            step: None,
            to_watch: true,
            ..
        } => do_cmd::run(&system, &config, do_cmd::Mode::ToWatch(&ticket), None),
        Command::Continue {
            ticket,
            step: Some(envelope),
            ..
        } => do_cmd::run(
            &system,
            &config,
            do_cmd::Mode::Step(&ticket, &envelope),
            served,
        ),
        Command::Watch { drain: false, .. } => {
            do_cmd::run(&system, &config, do_cmd::Mode::Watch, None)
        }
        Command::Watch { drain: true, .. } => {
            do_cmd::run(&system, &config, do_cmd::Mode::Drain, served)
        }
        Command::Forget { ticket } => forget_cmd::run(&config, &ticket),
        Command::Gate { at } => gate_cmd::run(&system, &config, at.as_deref()),
        Command::Unquarantine => unquarantine_cmd::run(&config),
        Command::Why { ticket } => why_cmd::run(&config, ticket.as_deref()),
        Command::Start { at_login } => service_cmd::start(&config, at_login),
        Command::Stop { forget, at_login } => service_cmd::stop(&config, forget, at_login),
        Command::Pause { repo, reason } => pause_cmd::pause(repo.as_deref(), reason.as_deref()),
        Command::Resume { repo } => pause_cmd::resume(repo.as_deref()),
        Command::Status => status_cmd::run(&config),
        Command::Uninstall { purge } => service_cmd::uninstall(purge),
        Command::Daemon => service_cmd::daemon(&system, &config),
        Command::Logs {
            ticket,
            last,
            follow,
        } => logs_cmd::run(ticket.as_deref(), last, follow),
    }
}

/// Whether `command` stops by degrees, and with which bound after a checked
/// stop (OWL-161, architecture section 5, "Three ways to stop"): `owlshift
/// watch`, stopped now [`CHECKED_STOP_BOUND`] after it; a step's child, with
/// no bound of its own, its watch's applying; the daemon likewise, each of
/// its watches carrying the bound (OWL-435). `None`: every signal stops the
/// command at once.
fn by_degrees(command: &Command) -> Option<Option<Duration>> {
    match command {
        Command::Watch { .. } => Some(Some(CHECKED_STOP_BOUND)),
        Command::Continue { step: Some(_), .. } | Command::Daemon => Some(None),
        _ => None,
    }
}

/// The data directory, where the event log and each project's files live.
fn data_dir() -> Result<PathBuf, String> {
    owlshift_platform::paths::data_dir().ok_or_else(|| {
        "this system has no data directory: set OWLSHIFT_DATA_DIR to an absolute path".to_owned()
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    /// OWL-161: `watch`, served or not, stops by degrees with the checked
    /// stop's bound, a step's child by degrees with none; every other
    /// command, a typed `continue` included, keeps stopping at once.
    #[test]
    fn only_watch_and_its_steps_stop_by_degrees() {
        let of = |args: &[&str]| {
            let cli = Cli::try_parse_from([&["owlshift"], args].concat()).unwrap();
            by_degrees(&cli.command)
        };
        let watch = Some(Some(CHECKED_STOP_BOUND));
        assert_eq!(of(&["watch"]), watch);
        assert_eq!(of(&["watch", "--drain"]), watch);
        assert_eq!(of(&["watch", "--drain", "--served", "o/r"]), watch);
        assert_eq!(of(&["continue", "OWL-1", "--step", "{}"]), Some(None));
        // OWL-435: the daemon with no bound of its own, its watches'.
        assert_eq!(of(&["daemon"]), Some(None));
        for args in [
            &["start"][..],
            &["start", "--at-login"],
            &["stop"],
            &["stop", "--forget"],
            &["stop", "--at-login"],
            &["uninstall"],
            &["uninstall", "--purge"],
            &["do", "OWL-1"][..],
            &["continue", "OWL-1"],
            &["continue", "OWL-1", "--to-watch"],
            &["doctor"],
            &["gate"],
            &["forget", "OWL-1"],
            &["unquarantine"],
            // OWL-438.
            &["pause"],
            &["pause", "acme/web", "--reason", "lunch"],
            &["resume"],
            &["resume", "acme/web"],
            &["status"],
        ] {
            assert_eq!(of(args), None, "{args:?}");
        }
        // OWL-437: removing the service while forgetting one project is no
        // one command.
        assert!(Cli::try_parse_from(["owlshift", "stop", "--forget", "--at-login"]).is_err());
    }
}
