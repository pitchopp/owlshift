//! `owlshift init`: writes the commented project file, then stores the
//! secrets `owlshift do` needs, asking for them on a terminal only, with
//! hidden input (`owlshift_runner::init`).

use std::io::{self, IsTerminal};
use std::process::ExitCode;

use clap::ValueEnum;
use owlshift_contracts::config::TrackerKind;
use owlshift_platform::keychain::{Keychain, Secret};
use owlshift_runner::config::{Effective, FileState};
use owlshift_runner::init::{
    InitOptions, Written, project_file, required_secrets, store_secrets, write_project_file,
};

use crate::fail;

#[derive(clap::Args)]
pub struct Args {
    /// Where the tickets live.
    #[arg(long, value_enum, default_value_t = TrackerChoice::Linear)]
    tracker: TrackerChoice,
    /// The Linear team key, the prefix of its ticket ids: OWL in OWL-12.
    #[arg(long)]
    team: Option<String>,
    /// One command of the project's gate (lint, formatter, tests), run from
    /// the repository root; repeat it for each command, in order.
    #[arg(long = "gate", value_name = "COMMAND")]
    gate: Vec<String>,
    /// Write the project file only, and store no secret.
    #[arg(long, conflicts_with = "replace_secrets")]
    skip_secrets: bool,
    /// Ask again for the secrets already stored, and replace them.
    #[arg(long)]
    replace_secrets: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum TrackerChoice {
    Linear,
    Markdown,
}

pub fn run(args: &Args, config: &Effective) -> ExitCode {
    let tracker = match &config.project {
        FileState::Loaded { path, config, .. } => {
            println!(
                "{} exists: kept as it is. Its values win over --tracker, --team and --gate.",
                path.display()
            );
            config.tracker.kind
        }
        FileState::Absent(path) => {
            let options = InitOptions {
                tracker: match args.tracker {
                    TrackerChoice::Linear => TrackerKind::Linear,
                    TrackerChoice::Markdown => TrackerKind::Markdown,
                },
                team: args.team.clone(),
                gate: args.gate.clone(),
            };
            let text = match project_file(&options) {
                Ok(text) => text,
                Err(error) => return fail(&error),
            };
            let root = path.parent().unwrap_or(path);
            match write_project_file(root, &text) {
                Ok(Written::Created) => println!(
                    "Wrote {}: review it, then commit it.{}",
                    path.display(),
                    if options.gate.is_empty() {
                        " Its gate is empty: list your lint, formatter and test commands in \
                         `stack.gate`, or the build role stops with `blocked`."
                    } else {
                        ""
                    }
                ),
                Ok(Written::Kept) => {
                    return fail(&format!(
                        "{} appeared while `owlshift init` ran: run it again",
                        path.display()
                    ));
                }
                Err(error) => return fail(&format!("{}: {error}", path.display())),
            }
            options.tracker
        }
        FileState::Invalid { path, error } => {
            return fail(&format!(
                "{} is invalid: {error}\nFix it, then run `owlshift init` again to store the \
                 secrets.",
                path.display()
            ));
        }
        FileState::NotApplicable(reason) => {
            return fail(&format!(
                "`owlshift init` runs in a git repository: {reason}"
            ));
        }
        FileState::Unavailable(reason) => return fail(reason),
    };

    if args.skip_secrets {
        println!("No secret stored (--skip-secrets).");
        return next_steps();
    }
    let keychain = match Keychain::system() {
        Ok(keychain) => keychain,
        Err(error) => return fail(&error.to_string()),
    };
    let terminal = io::stdin().is_terminal();
    let specs = required_secrets(tracker);
    let mut asked = false;
    let report = store_secrets(&keychain, &specs, args.replace_secrets, &mut |spec| {
        if !terminal {
            return None;
        }
        if !asked {
            println!(
                "Owlshift keeps these in the system keychain, never in a file. Input is hidden; \
                 leave one empty to skip it."
            );
            asked = true;
        }
        println!("{}: {}.", spec.label, spec.help);
        rpassword::prompt_password(format!("{}: ", spec.label))
            .ok()
            .map(Secret::new)
    });
    let report = match report {
        Ok(report) => report,
        Err(error) => return fail(&error.to_string()),
    };
    for spec in &report.stored {
        println!("Stored the {spec}.");
    }
    for spec in &report.kept {
        println!("Kept the {spec} already stored.");
    }
    if report.missing.is_empty() {
        return next_steps();
    }
    for spec in &report.missing {
        eprintln!("owlshift: missing: the {spec}: {}.", spec.help);
    }
    if terminal {
        eprintln!("owlshift: run `owlshift init` again to store it.");
    } else {
        eprintln!(
            "owlshift: `owlshift init` asks for secrets on a terminal only: run it again in one."
        );
    }
    ExitCode::FAILURE
}

fn next_steps() -> ExitCode {
    println!(
        "Next: `owlshift doctor` checks git and the harness logins; `owlshift do TICKET` runs \
         a ticket."
    );
    ExitCode::SUCCESS
}
