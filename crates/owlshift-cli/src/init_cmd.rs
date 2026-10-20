//! `owlshift init`: writes the commented project file, then stores the
//! secrets `owlshift do` needs, asking for them on a terminal only, with
//! hidden input (`owlshift_runner::init`): the tracker's under the accounts
//! of the project's GitHub repository, read from `origin`, the Claude Code
//! token for agent runs once per machine (OWL-308). The forge's comes from
//! signing in to GitHub through Owlshift's GitHub App
//! (`owlshift_runner::github_sign_in`, OWL-336), or with `--github-token`
//! from a personal token asked for like the others.

use std::io::{self, IsTerminal};
use std::process::ExitCode;

use clap::ValueEnum;
use owlshift_adapters::forge::Repo;
use owlshift_adapters::forge::github_app::{DeviceFlow, HttpLogin};
use owlshift_contracts::config::TrackerKind;
use owlshift_platform::keychain::{Keychain, SERVICE, Secret};
use owlshift_runner::config::{Effective, FileState};
use owlshift_runner::forge::GitHubApp;
use owlshift_runner::github_sign_in::{self, Ask, GitHub, Outcome, SystemWait};
use owlshift_runner::init::{
    AppStored, GITHUB_TOKEN, InitOptions, SecretsReport, Wanted, Written, project_file,
    required_secrets, store_app_credentials, store_secrets, wanted_secrets, write_project_file,
};
use owlshift_runner::project::ProjectDirs;
use owlshift_runner::secrets::{ForgeAccount, TrackerAccounts};
use owlshift_runner::tracker::{self, app_credentials};
use owlshift_runner::{on_demand, project};

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
    /// Ask again for this project's secrets already stored, and replace
    /// them, signing in to GitHub again; a secret shared by every project of
    /// this machine is never replaced, and an empty answer keeps reading it.
    #[arg(long)]
    replace_secrets: bool,
    /// Ask for a personal GitHub token instead of signing in through
    /// Owlshift's GitHub App, the expert option: it removes the project's
    /// GitHub App token, so the personal token is the one used.
    #[arg(long, conflicts_with = "skip_secrets")]
    github_token: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum TrackerChoice {
    Linear,
    Markdown,
}

pub fn run(args: &Args, config: &Effective) -> ExitCode {
    let (tracker, file) = match &config.project {
        FileState::Loaded { path, config, .. } => {
            println!(
                "{} exists: kept as it is. Its values win over --tracker, --team and --gate.",
                path.display()
            );
            (config.tracker.kind, path)
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
                         `stack.gate`, or `owlshift do` refuses to start a ticket."
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
            (options.tracker, path)
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
    // The project's secrets are named by its GitHub repository (OWL-308).
    let root = file.parent().unwrap_or(file);
    let repo = project::origin_url(&project::runner_git(), root)
        .and_then(|url| on_demand::check_origin(&url));
    let (mut wanted, unnamed) =
        match wanted_secrets(&keychain, &required_secrets(tracker), repo.as_ref().ok()) {
            Ok(found) => found,
            Err(error) => return fail(&error),
        };
    // Since OWL-336 the GitHub credential comes from the sign-in below; the
    // personal token is asked for with `--github-token` only. Without a
    // repository it stays among the secrets that cannot be named.
    if !args.github_token {
        wanted.retain(|one| one.spec != GITHUB_TOKEN);
    }
    let terminal = io::stdin().is_terminal();
    let mut asked = false;
    let mut ask = |one: &Wanted| {
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
        println!("{}: {}.", one.spec.label, one.spec.help);
        if let Some(shared) = &one.shared {
            println!(
                "This project has none of its own and reads the one every project of this \
                 machine shares (account `{shared}`): leave it empty to keep using that one."
            );
        }
        rpassword::prompt_password(format!("{}: ", one.spec.label))
            .ok()
            .map(Secret::new)
    };
    let report = store_secrets(&keychain, &wanted, args.replace_secrets, &mut ask);
    let report = match report {
        Ok(report) => report,
        Err(error) => return fail(&error.to_string()),
    };
    // The Linear app goes with the project's own key (OWL-308).
    let app_ready = match (tracker, &repo) {
        (TrackerKind::Linear, Ok(repo)) => linear_app(
            &keychain,
            &TrackerAccounts::project(repo),
            args.replace_secrets,
            &mut ask,
        ),
        _ => true,
    };
    for one in &report.stored {
        println!("Stored the {one}.");
    }
    for one in &report.kept {
        println!("Kept the {one} already stored.");
    }
    for one in &report.kept_shared {
        println!(
            "Kept reading the {} under the account `{}` every project of this machine shares: \
             `owlshift init --replace-secrets` stores one for this project.",
            one.spec.label,
            one.shared.as_deref().unwrap_or_default()
        );
    }
    for one in &report.refused {
        eprintln!(
            "owlshift: not stored: the {one}: what was given holds a space or a control \
             character, as a secret pasted across lines does."
        );
    }
    if let Err(reason) = &repo {
        for spec in &unnamed {
            eprintln!(
                "owlshift: not stored: the {}: Owlshift keeps it under a keychain account named \
                 by the project's GitHub repository, and {reason}.",
                spec.label
            );
        }
    }
    let github_ready = match &repo {
        Ok(repo) if args.github_token => personal_token_chosen(&keychain, repo, &report),
        Ok(repo) => {
            let app = GitHubApp::from_personal(match &config.personal {
                FileState::Loaded { config, .. } => Some(config),
                _ => None,
            });
            // A stored token is refreshed under the project's account lock,
            // as `owlshift do` refreshes it (OWL-384). Without a data
            // directory no `do` runs to share it with, so any lock file does.
            let lock = crate::data_dir().map_or_else(
                |_| std::env::temp_dir().join("owlshift-github-app.lock"),
                |dir| ProjectDirs::new(&dir, repo).account_lock_file(),
            );
            let flow = DeviceFlow::new(HttpLogin::new(), &app.client_id);
            let github = GitHub::live(flow, repo, lock);
            let ask = Ask {
                replace: args.replace_secrets,
                terminal,
            };
            let outcome = github_sign_in::step(
                &keychain,
                repo,
                &app,
                &github,
                &SystemWait,
                ask,
                &mut |line| println!("{line}"),
            );
            said_sign_in(&outcome, repo);
            outcome.is_ok()
        }
        // Said above, with the other secrets that cannot be named.
        Err(_) => true,
    };
    if report.missing.is_empty()
        && report.refused.is_empty()
        && unnamed.is_empty()
        && app_ready
        && github_ready
    {
        return next_steps();
    }
    for one in &report.missing {
        eprintln!("owlshift: missing: the {one}: {}.", one.spec.help);
    }
    if !unnamed.is_empty() {
        eprintln!(
            "owlshift: set the remote `origin` to the project's github.com repository, then run \
             `owlshift init` again."
        );
    } else if terminal {
        eprintln!(
            "owlshift: run `owlshift init` again to store it (`--replace-secrets` replaces one \
             this project has stored)."
        );
    } else {
        eprintln!(
            "owlshift: `owlshift init` asks for secrets on a terminal only: run it again in one."
        );
    }
    ExitCode::FAILURE
}

/// With `--github-token`: once the project holds a personal token of its
/// own, stored or kept, removes its GitHub App token, which would otherwise
/// be used first, and says so; `false` when that removal fails. With no
/// personal token of its own, nothing is removed, and the token's absence is
/// in `report` already.
fn personal_token_chosen(keychain: &Keychain, repo: &Repo, report: &SecretsReport) -> bool {
    let own = |found: &[Wanted]| found.iter().any(|one| one.spec == GITHUB_TOKEN);
    if !(own(&report.stored) || own(&report.kept)) {
        return true;
    }
    match github_sign_in::remove_app_token(keychain, repo) {
        Ok(true) => {
            println!(
                "Removed the GitHub App's token this project had stored (account `{}`): the \
                 personal token is the one used.",
                ForgeAccount::project_app(repo).token
            );
            true
        }
        Ok(false) => true,
        Err(error) => {
            eprintln!("owlshift: {error}");
            false
        }
    }
}

/// Says how the GitHub sign-in ended for the project of `repo`; an end that
/// fails `init` goes to standard error.
fn said_sign_in(outcome: &Outcome, repo: &Repo) {
    let account = ForgeAccount::project_app(repo).token;
    match outcome {
        Outcome::Kept => println!(
            "Kept the GitHub App's token already stored (keychain service `{SERVICE}`, account \
             `{account}`)."
        ),
        Outcome::Stored => println!(
            "Signed in to GitHub: stored the GitHub App's token (keychain service `{SERVICE}`, \
             account `{account}`)."
        ),
        Outcome::NeedsTerminal => eprintln!(
            "owlshift: missing: the GitHub sign-in (keychain service `{SERVICE}`, account \
             `{account}`): `owlshift init` signs in to GitHub through Owlshift's GitHub App on a \
             terminal, or `owlshift init --github-token` stores a personal token."
        ),
        Outcome::Denied => eprintln!(
            "owlshift: the GitHub sign-in was cancelled on GitHub: nothing was stored. Run \
             `owlshift init` to sign in again."
        ),
        Outcome::Expired => eprintln!(
            "owlshift: the GitHub sign-in code expired before it was entered: nothing was stored. \
             Run `owlshift init` for a new code."
        ),
        Outcome::NotInstalled { link } => eprintln!(
            "owlshift: Owlshift's GitHub App is not installed on github.com/{repo}: nothing was \
             stored. Install it at {link}, give it this repository, then run `owlshift init` \
             again. On an organisation's repository, the installation may be a request waiting \
             for an owner of the organisation to approve it."
        ),
        Outcome::Failed(reason) => eprintln!("owlshift: the GitHub sign-in failed: {reason}."),
    }
}

/// Stores the Linear app's pair under the project's accounts `own`, or
/// not, and says what Owlshift writes as (OWL-157); `false` when `init` must
/// fail: half a pair given, one pasted wrong, or half a pair left in the
/// keychain. The app goes with the project's own key: without one, the
/// project reads the shared key and the shared app, and nothing is asked
/// (OWL-308).
fn linear_app(
    keychain: &Keychain,
    own: &TrackerAccounts,
    replace: bool,
    ask: &mut dyn FnMut(&Wanted) -> Option<Secret>,
) -> bool {
    let shared = TrackerAccounts::shared();
    let contains = |account: &str| keychain.contains(account).map_err(|e| e.to_string());
    let found = (|| {
        Ok::<_, String>((
            contains(&own.key)?,
            contains(&own.app_id)? || contains(&own.app_secret)?,
            contains(&shared.app_id)? || contains(&shared.app_secret)?,
        ))
    })();
    let (own_key, own_app, shared_app) = match found {
        Ok(found) => found,
        Err(error) => {
            eprintln!("owlshift: {error}");
            return false;
        }
    };
    if !own_key {
        println!(
            "The Linear app goes with the Linear API key: this project has no key of its own, so \
             it is not asked for."
        );
        // The shared pair it may read is still checked: half of it would
        // refuse the next `owlshift do`.
        return usable_app(keychain, &shared);
    }
    if !own_app && shared_app {
        println!(
            "A Linear app is stored under the accounts every project of this machine shares; it \
             is not used with this project's own key. Give this project's app below, or leave it \
             empty to write through the key."
        );
    }
    let stored = match store_app_credentials(keychain, own, replace, ask) {
        Ok(stored) => stored,
        Err(error) => {
            eprintln!("owlshift: {error}");
            return false;
        }
    };
    match stored {
        AppStored::Stored => println!(
            "Stored the Linear app's client ID and secret: Owlshift comments as its app user."
        ),
        AppStored::Kept => println!("Kept the Linear app's client ID and secret already stored."),
        AppStored::Skipped => println!(
            "No Linear app: Owlshift comments through the Linear API key, and Linear does not \
             notify that key's holder of those comments."
        ),
        AppStored::Incomplete => {
            eprintln!(
                "owlshift: not stored: the Linear app's client ID goes with its secret; neither \
                 was stored."
            );
            return false;
        }
        AppStored::Refused => {
            eprintln!(
                "owlshift: not stored: the Linear app's client ID or secret holds a space or a \
                 control character, as a secret pasted across lines does; neither was stored."
            );
            return false;
        }
    }
    usable_app(keychain, own)
}

/// Whether the Linear app's pair under `accounts` is whole or absent, as
/// `owlshift do` needs it; half a pair is said and fails `init`.
fn usable_app(keychain: &Keychain, accounts: &TrackerAccounts) -> bool {
    match tracker::read(keychain, accounts)
        .map_err(|error| error.to_string())
        .and_then(|found| app_credentials(&found).map_err(|error| error.to_string()))
    {
        Ok(_) => true,
        Err(error) => {
            eprintln!("owlshift: {error}");
            false
        }
    }
}

fn next_steps() -> ExitCode {
    println!(
        "Next: `owlshift doctor` checks git, the harnesses and the Claude Code token for agent runs; `owlshift do TICKET` runs \
         a ticket."
    );
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use owlshift_adapters::forge::Repo;
    use owlshift_runner::tracker::{
        LINEAR_ACCOUNT, LINEAR_APP_ID_ACCOUNT, LINEAR_APP_SECRET_ACCOUNT,
    };

    use super::*;

    /// A project reading the shared key is asked for no app, but half a
    /// shared pair still fails `init`, as it would refuse `owlshift do`.
    #[test]
    fn a_half_shared_app_fails_a_project_without_its_own_key() {
        let keychain = Keychain::in_memory();
        let own = TrackerAccounts::project(&Repo::parse("pitchopp/owlshift").unwrap());
        for account in [LINEAR_ACCOUNT, LINEAR_APP_ID_ACCOUNT] {
            keychain.store(account, &Secret::new("x")).unwrap();
        }
        let mut ask = |_: &Wanted| -> Option<Secret> { panic!("no app is asked for") };
        assert!(!linear_app(&keychain, &own, false, &mut ask));

        keychain
            .store(LINEAR_APP_SECRET_ACCOUNT, &Secret::new("y"))
            .unwrap();
        assert!(linear_app(&keychain, &own, false, &mut ask));
    }

    /// Criterion 6 (OWL-383): `--github-token` stores the personal token under
    /// the project's own account and removes the app token, which would be
    /// used first; with no personal token given, the app token stays.
    #[test]
    fn a_personal_token_with_github_token_removes_the_app_token() {
        let repo = Repo::parse("pitchopp/owlshift").unwrap();
        let keychain = Keychain::in_memory();
        let app = ForgeAccount::project_app(&repo).token;
        keychain
            .store(&app, &Secret::new("{\"v\":1,\"access_token\":\"ghu_x\"}"))
            .unwrap();
        let wanted = wanted_secrets(&keychain, &[GITHUB_TOKEN], Some(&repo))
            .unwrap()
            .0;

        let report = store_secrets(&keychain, &wanted, false, &mut |_| None).unwrap();
        assert_eq!(report.missing.len(), 1);
        assert!(personal_token_chosen(&keychain, &repo, &report));
        assert!(keychain.contains(&app).unwrap());

        let report = store_secrets(&keychain, &wanted, false, &mut |_| {
            Some(Secret::new("github_pat_x"))
        })
        .unwrap();
        assert!(personal_token_chosen(&keychain, &repo, &report));
        assert!(!keychain.contains(&app).unwrap());
        assert_eq!(
            keychain
                .read(&ForgeAccount::project(&repo).token)
                .unwrap()
                .unwrap()
                .expose(),
            "github_pat_x"
        );
    }
}
