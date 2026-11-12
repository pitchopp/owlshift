//! The Linear tracker's [`Kind`]: a team's issues, read and written through
//! a Linear API key and, when one is stored, the Linear app Owlshift writes
//! as (decision D8, OWL-157).

use std::path::Path;

use owlshift_adapters::forge::Repo;
use owlshift_adapters::tracker::linear::{
    AdmissionFacts, ApiKey, AppUser, LinearTracker, StateMismatch, find_state,
};
use owlshift_adapters::tracker::{Capability, ErrorKind, PROPOSED_LABEL, Tracker};
use owlshift_contracts::config::{self, Admit, ProjectConfig};
use owlshift_contracts::ids::TicketId;
use owlshift_platform::keychain::{Keychain, SERVICE, Secret};

use super::{AppError, ForgeCredential, Kind, Opener, Probe, Probed, StatesError};
use crate::doctor::{Check, DOES_NOT_BLOCK, KEYCHAIN_FIX, Section, Step, configured_states};
use crate::init::{LINEAR_KEY, SecretSpec};
use crate::secrets::{Source, TrackerAccounts, TrackerSecrets};
use crate::system::System;
use crate::ui::reply::Key;

pub struct Linear;

impl Kind for Linear {
    fn name(&self) -> &'static str {
        "linear"
    }

    fn capabilities(&self) -> &'static [Capability] {
        LinearTracker::CAPABILITIES
    }

    fn secret_specs(&self) -> &'static [SecretSpec] {
        &[LINEAR_KEY]
    }

    fn project_file_lines(&self, team: Option<&str>) -> Result<String, String> {
        let quote = |text: &str| toml::Value::String(text.to_owned()).to_string();
        let team = team
            .map(str::trim)
            .filter(|team| !team.is_empty())
            .ok_or("a Linear project needs its team key: pass --team, such as --team OWL")?;
        Ok(format!(
            "\
# Where the tickets live: \"linear\", or \"markdown\" for tickets kept in
# this repository, one folder per ticket under tickets/.
kind = \"linear\"
# The Linear team key: the prefix of its ticket ids, OWL in OWL-12.
team = {}
",
            quote(team)
        ))
    }

    /// On Linear, a ticket id starts with its team's key (`OWL` in
    /// `OWL-12`), compared without regard to case, as Linear reads
    /// identifiers.
    fn check_ticket(&self, tracker: &config::Tracker, ticket: &TicketId) -> Result<(), String> {
        let Some(team) = &tracker.team else {
            return Ok(());
        };
        let key = ticket.as_str().rsplit_once('-').map(|(key, _)| key);
        if key.is_some_and(|key| key.eq_ignore_ascii_case(team)) {
            Ok(())
        } else {
            Err(format!(
                "{ticket} is not a ticket of team {team}, the project's `tracker.team`: \
                 `owlshift do` runs this project's tickets only"
            ))
        }
    }

    /// Opens [`super::linear()`] with the project's team, which a Linear
    /// project names; its set is read whenever the kind is Linear.
    fn opener<'a>(
        &self,
        tracker: &'a config::Tracker,
        _root: &'a Path,
        secrets: Option<&'a TrackerSecrets<Secret>>,
        _forge: ForgeCredential<'a>,
    ) -> Result<Opener<'a>, String> {
        let team = tracker
            .team
            .as_deref()
            .ok_or("a Linear project names its team in `tracker.team`")?;
        let secrets = secrets.ok_or("the Linear key and app were not read from the keychain")?;
        Ok(Opener::new(move || {
            Ok(Box::new(super::linear(secrets, team)?) as Box<dyn Tracker>)
        }))
    }

    /// `tracker states`, then `tracker identity`.
    fn doctor_lines(
        &self,
        system: &dyn System,
        config: &ProjectConfig,
        accounts: Option<&TrackerAccounts>,
    ) -> Vec<Check> {
        tracker_states_check(system, config, accounts)
            .into_iter()
            .chain(tracker_identity_check(system, config, accounts))
            .collect()
    }

    fn admission_facts(
        &self,
        system: &dyn System,
        config: &ProjectConfig,
        accounts: Option<&TrackerAccounts>,
    ) -> Option<Result<AdmissionFacts, StatesError>> {
        admission_facts(system, config, accounts)
    }

    fn proposals_inbox(
        &self,
        config: &ProjectConfig,
        facts: Option<&Result<AdmissionFacts, StatesError>>,
    ) -> Option<Check> {
        proposals_inbox_check(config, facts)
    }

    /// The key `owlshift init` stored for the project of `repo`, its own
    /// accounts or the shared ones ([`crate::secrets::tracker`]), read from
    /// the system keychain. The Linear app's pair is never used: a reply goes
    /// through the key, whose account is the operator's.
    fn reply_key(&self, repo: &Repo) -> Result<Key, String> {
        let keychain =
            Keychain::system().map_err(|e| format!("the keychain cannot be read: {e}"))?;
        let mut look = |account: &str| keychain.read(account).map_err(|e| e.to_string());
        let found = crate::secrets::tracker(repo, &mut look)?;
        let Some(key) = found.key else {
            return Err(format!(
                "no Linear API key is stored for this project (account `{}`): run `owlshift init`",
                found.accounts.key
            ));
        };
        let tracker = LinearTracker::new(ApiKey::new(key.expose()));
        let account = tracker
            .key_account()
            .map_err(|e| format!("the Linear key's account could not be read: {e}"))?;
        let reader = LinearTracker::new(ApiKey::new(key.expose()));
        Ok(Key {
            account,
            tracker: Box::new(tracker),
            names: Box::new(move |id: &str| reader.account_name(id).map_err(|e| e.to_string())),
        })
    }

    /// The app Owlshift writes as, so that Linear notifies the decider
    /// (decision D8).
    fn stores_app(&self) -> bool {
        true
    }
}

/// Whether each state named under `[tracker].states` is exactly one
/// workflow state of the project's Linear team (OWL-147): the rule by which
/// the stage moves. A name that matches none or several only keeps the stage
/// from moving, which never stops `owlshift do`, so it is a warning; what
/// keeps `do` from reading the ticket at all (no key, a rejected key, a
/// keychain it cannot read, a team Linear does not know) is a failure. None
/// for a Markdown project or none loaded: only a Linear project opens the
/// keychain and the network here, once each.
fn tracker_states_check(
    system: &dyn System,
    config: &ProjectConfig,
    accounts: Option<&TrackerAccounts>,
) -> Option<Check> {
    const SUBJECT: &str = "tracker states";
    let Some(team) = &config.tracker.team else {
        return None;
    };
    // Unresolved, the `secrets` line says why.
    let accounts = accounts?;
    let place = format!(
        "in the system keychain (service `{SERVICE}`, account `{}`)",
        accounts.key
    );
    let fail = |detail: String, why: &str, fix: Vec<Step>| {
        Some(Check::fail(Section::Project, SUBJECT, detail, why, fix))
    };
    let names = match linear_states(system, accounts, team) {
        Ok(names) => names,
        Err(StatesError::NoKey) => {
            return fail(
                format!("not checked: no Linear API key {place}"),
                "`owlshift do` reads the ticket with the Linear API key Owlshift keeps in the \
                 system keychain, and doctor reads the team's workflow states with it.",
                vec![Step::run_noting(
                    "owlshift init",
                    "paste the key when asked",
                )],
            );
        }
        Err(StatesError::Keychain(reason)) => {
            return fail(
                format!("not checked: could not read the Linear API key {place}: {reason}"),
                "`owlshift do` reads the Linear API key in the system keychain before each run.",
                vec![Step::act(KEYCHAIN_FIX)],
            );
        }
        Err(StatesError::Tracker(error)) => {
            return match error.kind {
                ErrorKind::Unauthorized => fail(
                    format!("not checked: Linear refused the API key stored {place}: {error}"),
                    "`owlshift do` reads the ticket with this key, so Linear would refuse it too.",
                    vec![Step::run_noting(
                        "owlshift init --replace-secrets",
                        "paste a valid Linear API key when asked",
                    )],
                ),
                ErrorKind::NotFound => fail(
                    format!("not checked: {error}, which `[tracker].team` names (`{team}`)"),
                    "`owlshift do` runs the tickets of the team `[tracker].team` names, and moves \
                     their stage among that team's workflow states.",
                    vec![Step::act(
                        "Set `team` under `[tracker]` in owlshift.toml to the key your team's \
                         issue identifiers start with, such as `OWL` for `OWL-12`",
                    )],
                ),
                ErrorKind::Unreachable | ErrorKind::Other => Some(Check::warn(
                    Section::Project,
                    SUBJECT,
                    format!("not checked: {error}"),
                    format!(
                        "{DOES_NOT_BLOCK} doctor could not read the workflow states of team \
                         `{team}`. A name under `[tracker].states` that matches none only keeps \
                         the ticket's visible stage from moving."
                    ),
                )),
            };
        }
    };
    let mut configured = configured_states(&config.tracker.states);
    if let Admit::State(name) = &config.tracker.admit {
        configured.push(("admit.state".to_owned(), name.clone()));
    }
    let mut problems = Vec::new();
    let mut ambiguous = false;
    for (key, name) in &configured {
        match find_state(names.iter().map(String::as_str), name) {
            Ok(_) => {}
            Err(StateMismatch::Missing) => {
                problems.push(format!("no state named {name:?} (`{key}`)"));
            }
            Err(StateMismatch::Ambiguous) => {
                ambiguous = true;
                problems.push(format!("several named {name:?} (`{key}`)"));
            }
        }
    }
    let keys: Vec<&str> = configured.iter().map(|(key, _)| key.as_str()).collect();
    if problems.is_empty() {
        return Some(Check::ok(
            Section::Project,
            SUBJECT,
            format!(
                "each of {} is one workflow state of team `{team}`",
                keys.join(", ")
            ),
        ));
    }
    let mut why = format!(
        "{DOES_NOT_BLOCK} a move to a state that matches none, or several, fails with a \
         `stage_not_moved` warning event, and the ticket's visible stage stays where it was. Set \
         each under `[tracker].states` in owlshift.toml to one of the team's states, exactly, \
         case included: {}.",
        names.join(", ")
    );
    if ambiguous {
        why.push_str(" A name several states share must first be made unique in Linear.");
    }
    Some(Check::warn(
        Section::Project,
        SUBJECT,
        format!("team `{team}` has {}", problems.join("; ")),
        why,
    ))
}

/// What Linear says of the admission and proposals setup of a Linear
/// project: the labels it names, its admitters, and its proposals inbox
/// (OWL-155). None for a Markdown project or none loaded.
fn admission_facts(
    system: &dyn System,
    config: &ProjectConfig,
    secrets: Option<&TrackerAccounts>,
) -> Option<Result<AdmissionFacts, StatesError>> {
    let Some(team) = &config.tracker.team else {
        return None;
    };
    let secrets = secrets?;
    let tracker = &config.tracker;
    let mut labels = vec![PROPOSED_LABEL];
    if let Admit::Label(label) = &tracker.admit {
        labels.push(label);
    }
    labels.extend(tracker.exclude.as_deref());
    let accounts: Vec<&str> = tracker
        .admitters
        .iter()
        .flatten()
        .map(String::as_str)
        .collect();
    Some(linear_admission(
        system,
        secrets,
        team,
        &tracker.states.ready,
        &labels,
        &accounts,
    ))
}

/// Whether a follow-up Owlshift proposes can be filed (OWL-155): the team's
/// triage inbox, or the label fallback `agent:proposed` on its first backlog
/// state, chosen as the adapter chooses (`propose`). A warning when neither
/// works. None unless the team's facts were read; a tracker error is the
/// admission line's to report.
fn proposals_inbox_check(
    config: &ProjectConfig,
    facts: Option<&Result<AdmissionFacts, StatesError>>,
) -> Option<Check> {
    const SUBJECT: &str = "proposals inbox";
    let team = config.tracker.team.as_deref()?;
    let Some(Ok(found)) = facts else {
        return None;
    };
    let labelled = match &found.inbox {
        Ok(false) => {
            return Some(Check::ok(
                Section::Project,
                SUBJECT,
                format!("team `{team}` files proposals in its triage inbox"),
            ));
        }
        Ok(true) if found.labels.iter().any(|l| l == PROPOSED_LABEL) => {
            return Some(Check::ok(
                Section::Project,
                SUBJECT,
                format!(
                    "team `{team}` has no triage inbox: proposals go to its first backlog state \
                     with the label {PROPOSED_LABEL:?}"
                ),
            ));
        }
        Ok(true) => true,
        Err(_) => false,
    };
    let detail = match &found.inbox {
        Err(reason) => reason.clone(),
        Ok(_) => format!(
            "team `{team}` has no triage inbox, and neither it nor the workspace has the label \
             {PROPOSED_LABEL:?} that marks a proposal in its backlog"
        ),
    };
    let fix = if labelled {
        format!("Create the label {PROPOSED_LABEL:?} in Linear, on the team or the workspace.")
    } else {
        "Turn Triage on for the team in Linear (team settings), or give it a backlog state \
         other than the ready state."
            .to_owned()
    };
    Some(Check::warn(
        Section::Project,
        SUBJECT,
        detail,
        format!(
            "{DOES_NOT_BLOCK} Owlshift cannot file the follow-ups it proposes, and `owlshift \
             watch --drain` refuses to start, until a proposals inbox exists. {fix}"
        ),
    ))
}

/// Who Owlshift writes as on Linear (OWL-157): the Linear app's user,
/// checked as `owlshift do` checks it when it opens the tracker, or the API
/// key's account. No app is a warning: Owlshift then writes through the key,
/// as before. An app `do` would refuse fails: half of it stored, Linear
/// refusing it or not answering, since `do` never falls back to the key.
/// None for a Markdown project or none loaded.
fn tracker_identity_check(
    system: &dyn System,
    config: &ProjectConfig,
    accounts: Option<&TrackerAccounts>,
) -> Option<Check> {
    const SUBJECT: &str = "tracker identity";
    let Some(team) = &config.tracker.team else {
        return None;
    };
    // Unresolved, the `secrets` line says why.
    let accounts = accounts?;
    let place = format!(
        "in the system keychain (service `{SERVICE}`, accounts `{}` and `{}`)",
        accounts.app_id, accounts.app_secret
    );
    let why = "With the Linear app stored, `owlshift do` writes as its app user, checked before \
               any work, and refuses rather than write through the API key.";
    let shared = accounts.source == Source::Shared;
    let store_both = || {
        vec![Step::run_noting(
            "owlshift init --replace-secrets",
            if shared {
                "give this project a Linear key of its own, then its Linear app's client ID and \
                 secret, when asked"
            } else {
                "give the Linear app's client ID and secret when asked"
            },
        )]
    };
    let fail =
        |detail: String, fix: Vec<Step>| Check::fail(Section::Project, SUBJECT, detail, why, fix);
    Some(match linear_app(system, accounts, team) {
        Ok(Some(user)) => Check::ok(
            Section::Project,
            SUBJECT,
            format!("Owlshift writes on Linear as the app user `{}`", user.name),
        ),
        Ok(None) => Check::warn(
            Section::Project,
            SUBJECT,
            format!("no Linear app {place}: Owlshift writes through the API key"),
            format!(
                "{DOES_NOT_BLOCK} Linear does not notify a person of what their own API key \
                 writes, so a decider who holds the key learns of a question from this \
                 machine's desktop notification alone. A workspace admin can create an OAuth \
                 application with client credentials in Linear's API settings; `owlshift init` \
                 stores its client ID and secret."
            ),
        ),
        Err(AppError::NoKey { .. }) => Check::info(
            Section::Project,
            SUBJECT,
            "not checked: no Linear API key, which the Linear app's check needs",
        ),
        Err(AppError::Keychain(reason)) => fail(
            format!("not checked: could not read the Linear app's credentials {place}: {reason}"),
            vec![Step::act(KEYCHAIN_FIX)],
        ),
        Err(AppError::Incomplete { missing, .. }) => fail(
            format!(
                "half of the Linear app is stored: nothing under service `{SERVICE}`, account \
                 `{missing}`"
            ),
            store_both(),
        ),
        Err(AppError::Tracker { error, .. }) => fail(
            format!("the Linear app stored {place} cannot be used: {error}"),
            match error.kind {
                // The shared entries may serve another project: never
                // advise deleting them (OWL-308).
                ErrorKind::Unreachable | ErrorKind::Other if shared => vec![Step::act(
                    "Check that Linear is reachable from this machine, then run `owlshift \
                     doctor` again",
                )],
                ErrorKind::Unreachable | ErrorKind::Other => vec![Step::act(
                    "Check that Linear is reachable from this machine, then run `owlshift \
                     doctor` again; or delete both of the app's keychain entries to write \
                     through the API key",
                )],
                ErrorKind::Unauthorized | ErrorKind::NotFound => store_both(),
            },
        ),
    })
}

/// The workflow states of the Linear team `team`, as the host reads them.
fn linear_states(
    system: &dyn System,
    accounts: &TrackerAccounts,
    team: &str,
) -> Result<Vec<String>, StatesError> {
    match system.tracker_probe(&Probe::LinearStates { accounts, team }) {
        Probed::States(answer) => answer,
        other => unreachable!("the host answered a Linear states probe with {other:?}"),
    }
}

/// The admission facts of the Linear team `team`, as the host reads them.
fn linear_admission(
    system: &dyn System,
    accounts: &TrackerAccounts,
    team: &str,
    ready: &str,
    labels: &[&str],
    people: &[&str],
) -> Result<AdmissionFacts, StatesError> {
    let probe = Probe::LinearAdmission {
        accounts,
        team,
        ready,
        labels,
        people,
    };
    match system.tracker_probe(&probe) {
        Probed::Admission(answer) => answer,
        other => unreachable!("the host answered a Linear admission probe with {other:?}"),
    }
}

/// The Linear app user Owlshift writes as, as the host checks it.
fn linear_app(
    system: &dyn System,
    accounts: &TrackerAccounts,
    team: &str,
) -> Result<Option<AppUser>, AppError> {
    match system.tracker_probe(&Probe::LinearApp { accounts, team }) {
        Probed::App(answer) => answer,
        other => unreachable!("the host answered a Linear app probe with {other:?}"),
    }
}
