//! Opening the project's tracker adapter with its credentials.
//!
//! The runner is the only holder of tracker credentials: it reads them from
//! the system keychain and hands them to the adapter, never to an agent
//! (architecture section 8). The token of the Linear app lives in the
//! runner's memory alone, for one command (OWL-157). Which accounts hold a
//! project's credentials is [`crate::secrets`]'s to say (OWL-308).
//!
//! It is also where the runner chooses between its trackers (OWL-533,
//! build plan "P9 · Open-source readiness", part 2): [`of`] gives the
//! [`Kind`] of the project's tracker, which holds what differs by tracker,
//! and `doctor`, `init`, `owlshift do` and the UI reach a tracker through
//! it. Each kind is a file of its own under `tracker/`.

mod github;
mod linear;
mod markdown;

use std::any::Any;
use std::fmt;
use std::path::Path;

use owlshift_adapters::forge::Repo;
use owlshift_adapters::forge::github::GitHubForge;
use owlshift_adapters::tracker;
use owlshift_adapters::tracker::github::{GitHubTracker, IssuesAccess};
use owlshift_adapters::tracker::linear::app::{AppTransport, ClientCredentials, HttpOAuth};
use owlshift_adapters::tracker::linear::{AdmissionFacts, ApiKey, AppUser, LinearTracker};
use owlshift_adapters::tracker::{AccountFact, Capability, ErrorKind};
use owlshift_contracts::config::{self, ProjectConfig, TrackerKind};
use owlshift_contracts::ids::TicketId;
use owlshift_platform::keychain::{Keychain, SERVICE, Secret};

use crate::doctor::Check;
use crate::init::SecretSpec;
use crate::refusal::Refusal;
use crate::secrets::{ForgeSecret, Lookup, SecretSet, Source, TrackerAccounts, TrackerSecrets};
use crate::system::System;

/// What differs by tracker, for one kind of tracker a project file names
/// (`[tracker].kind`): [`of`] gives it.
pub trait Kind: Sync {
    /// Its name in a project file, `kind = "…"`.
    fn name(&self) -> &'static str;

    /// The capabilities its adapter declares (architecture section 6).
    fn capabilities(&self) -> &'static [Capability];

    /// The secrets of the tracker that `owlshift do` reads from the
    /// keychain, in the order `init` asks for them: none when it needs none
    /// of its own.
    fn secret_specs(&self) -> &'static [SecretSpec];

    /// The keychain set of the tracker that `owlshift do` reads, by the rule
    /// of [`crate::secrets::tracker`]: none when it needs no secret of its
    /// own. Every account of [`Kind::secret_specs`] is one of its entries.
    fn secret_set(&self) -> Option<&'static SecretSet>;

    /// What doctor's `secrets` line says of the set [`Kind::secret_set`]
    /// declares, as `found` (presence only): one part per thing stored or
    /// missing, naming its accounts, never a value.
    fn secrets_detail(&self, found: &TrackerSecrets<()>) -> Vec<String>;

    /// The lines `init` writes under `[tracker]` of a new project file, the
    /// team key being `team` as `--team` gave it; the error says what is
    /// missing.
    fn project_file_lines(&self, team: Option<&str>) -> Result<String, String>;

    /// Refuses `ticket` when its id says it is not one of the project's,
    /// before anything is read ([`crate::on_demand::check_team`]).
    fn check_ticket(&self, tracker: &config::Tracker, ticket: &TicketId) -> Result<(), String>;

    /// What opens the adapter of the project whose `[tracker]` is `tracker`,
    /// checked out at `root`, with the tracker secrets [`keychain_set`]
    /// read, the set [`Kind::secret_set`] declares, and the project's forge
    /// credential, which a tracker of the
    /// forge's own reaches it with (OWL-535): the error says what the
    /// project file or the keychain lacks.
    fn opener<'a>(
        &self,
        tracker: &'a config::Tracker,
        root: &'a Path,
        secrets: Option<&'a TrackerSecrets<Secret>>,
        forge: ForgeCredential<'a>,
    ) -> Result<Opener<'a>, String>;

    /// What `owlshift doctor` reports on the tracker itself, after the
    /// secrets and forge lines, for the loaded project `config` whose
    /// tracker set is stored under `accounts` (`None` when doctor could not
    /// tell, which its `secrets` line says) and whose repository is `repo`
    /// (`None` when `origin` names none): its own lines, and what it says of
    /// the project's admission and proposals setup (OWL-155), read once for
    /// doctor's `drain admission` and `proposals inbox` lines (`None` when
    /// the tracker is not asked).
    fn doctor(
        &self,
        system: &dyn System,
        config: &ProjectConfig,
        accounts: Option<&TrackerAccounts>,
        repo: Option<&Repo>,
    ) -> Doctored;

    /// Doctor's `proposals inbox` line (OWL-155): whether a follow-up
    /// Owlshift proposes can be filed, given what [`Kind::doctor`]
    /// read.
    fn proposals_inbox(
        &self,
        config: &ProjectConfig,
        facts: Option<&Result<AdmissionFacts, StatesError>>,
    ) -> Option<Check>;

    /// The operator's key that the UI posts a decider's reply through, for
    /// the project of `repo`, opened with its account read once
    /// ([`crate::ui::reply::open_key`]); the error says why there is none.
    fn reply_key(&self, repo: &Repo) -> Result<crate::ui::reply::Key, String>;

    /// Whether `owlshift init` stores the client ID and secret of an app the
    /// tracker writes as (OWL-157).
    fn stores_app(&self) -> bool;

    /// The `admit`, `admitters` and `states` lines `init` writes under
    /// `[tracker]` of a new project file, `admitter` being the account it
    /// signed in as, when it asked the tracker (OWL-537).
    fn admission_lines(&self, admitter: Option<&str>) -> String {
        let _ = admitter;
        ADMISSION_LINES.to_owned()
    }

    /// What `owlshift init` asks of the tracker before it writes the
    /// project file, for every tracker (OWL-537; build plan "P9 ·
    /// Open-source readiness", part 6): whether it answers, with the
    /// credentials `init` just stored, and what it sets up on it. The
    /// error says why it did not answer.
    fn connect(&self, setup: &Setup<'_>) -> Result<Connected, String>;

    /// The words of doctor's `drain admission` line for this tracker.
    fn admission_words(&self) -> &'static AdmissionWords {
        &LINEAR_WORDS
    }
}

/// The lines `init` has written under `[tracker]` since P4 for Linear and
/// Markdown: a label to admit, Linear's default states.
pub(crate) const ADMISSION_LINES: &str = "\
# How a person admits a ticket to Owlshift's queue: { label = \"...\" },
# \"ready\" (the move to the ready state below, which { state = \"...\" }
# would repeat) or \"delegation\". `owlshift do TICKET` does not use it: it
# runs the ticket it is given.
admit = { label = \"agent\" }
# The tracker states that show where a ticket stands.
states = { ready = \"Todo\", working = \"In Progress\", needs_input = \"Needs Input\", review = \"In Review\" }
";

/// What [`Kind::doctor`] reports.
#[derive(Default)]
pub struct Doctored {
    pub lines: Vec<Check>,
    pub facts: Option<Result<AdmissionFacts, StatesError>>,
}

/// What [`Kind::connect`] works with: the project's `[tracker]`, the
/// repository `origin` names (or why it names none), the keychain `init`
/// stored the secrets in, and how the GitHub App's token is refreshed.
pub struct Setup<'a> {
    pub tracker: &'a config::Tracker,
    pub repo: Result<&'a Repo, &'a str>,
    pub keychain: &'a Keychain,
    pub access: &'a crate::forge::AppAccess,
}

/// What [`Kind::connect`] found and did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Connected {
    /// The account the credential signs in as, written as the admitter.
    pub admitter: Option<String>,
    /// The labels created, in order.
    pub created: Vec<String>,
}

/// The tracker's words in doctor's `drain admission` line (OWL-537).
pub struct AdmissionWords {
    /// After "the admission label \"x\"".
    pub label_missing: &'static str,
    pub label_fix: &'static str,
    /// After "admitter \"id\"".
    pub no_account: &'static str,
    pub app_account: &'static str,
    pub deactivated: &'static str,
    pub accounts_fix: &'static str,
    pub admitters_unset_fix: &'static str,
    pub delegation_fix: &'static str,
    /// The scope of an ok line once the tracker was asked.
    pub checked: &'static str,
}

/// Linear's words, which a Markdown project reads too.
pub const LINEAR_WORDS: AdmissionWords = AdmissionWords {
    label_missing: "exists on neither team nor workspace",
    label_fix: "Create the label in Linear, on the team or the workspace, or set the name as Linear \
                has it: names match exactly, case included.",
    no_account: "is no account of the workspace",
    app_account: "is an app user, which never admits",
    deactivated: "is a deactivated account",
    accounts_fix: "Set `admitters` to the ids of active people of the workspace, as Linear gives \
                   them (a user's `id`).",
    admitters_unset_fix: "List under `admitters` the Linear user ids whose gesture admits a \
                          ticket; with none, no ticket is ever admitted.",
    delegation_fix: "Decision D8's app takes no delegation scope, so nothing can be delegated to \
                     Owlshift: set `admit` under `[tracker]` to `{ label = … }` or `\"ready\"`.",
    checked: "labels and admitters exist in Linear",
};

/// The kind of the tracker a project file names: the one place the runner
/// chooses between its trackers.
pub fn of(kind: TrackerKind) -> &'static dyn Kind {
    match kind {
        TrackerKind::Linear => &linear::Linear,
        TrackerKind::Markdown => &markdown::Markdown,
        TrackerKind::Github => &github::Github,
    }
}

/// The project's forge credential, as `owlshift do` read it, for a tracker
/// reached through it (OWL-535).
#[derive(Clone, Copy)]
pub struct ForgeCredential<'a> {
    pub secret: &'a ForgeSecret<Secret>,
    pub repo: &'a Repo,
    pub access: &'a crate::forge::AppAccess,
}

/// The tracker set of the project of `repo`, each account looked up by
/// `look` as [`crate::secrets::tracker`] says: `None` when the project's
/// tracker needs no secret of its own, and then no account is looked up.
pub fn keychain_set<T>(
    config: &ProjectConfig,
    repo: &Repo,
    look: &mut Lookup<'_, T>,
) -> Result<Option<TrackerSecrets<T>>, String> {
    of(config.tracker.kind)
        .secret_set()
        .map(|set| crate::secrets::tracker(repo, set, look))
        .transpose()
}

/// What `owlshift doctor` asks of the project's tracker over the network,
/// through [`System::tracker_probe`]: each kind defines its own probes, in
/// its file, and reads their answers with [`ask`] (OWL-544).
pub trait Probe: Any + fmt::Debug + Sync {
    /// The answer, read with the secrets stored in `keychain`, over the
    /// network.
    fn read(&self, keychain: &Keychain) -> Probed;

    /// The answer of a keychain that could not be read, for `reason`.
    fn keychain_error(&self, reason: String) -> Probed;

    /// The answer of the test host, as the kind scripts it
    /// ([`crate::system::fake::FakeSystem::script`]); never the keychain or
    /// the network.
    #[cfg(test)]
    fn fake(&self, host: &crate::system::fake::FakeSystem) -> Probed;
}

/// The answer to a [`Probe`], of the type its kind reads it as ([`ask`]).
pub struct Probed(Box<dyn Any + Send>);

impl Probed {
    pub fn new<A: Any + Send>(answer: A) -> Self {
        Self(Box::new(answer))
    }

    /// The answer, when it is an `A`.
    pub fn take<A: Any>(self) -> Option<A> {
        self.0.downcast().ok().map(|answer| *answer)
    }
}

/// No value: an answer may carry what a tracker said.
impl fmt::Debug for Probed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Probed(..)")
    }
}

/// The answer of `system` to `probe`, of the type the probe's kind reads.
pub(crate) fn ask<A: Any>(system: &dyn System, probe: &dyn Probe) -> A {
    system
        .tracker_probe(probe)
        .take()
        .unwrap_or_else(|| unreachable!("the host answered {probe:?} with another type"))
}

/// What doctor reads of a GitHub Issues project (OWL-537): whether the
/// credential reads the issues, then, when it does, the repository's labels
/// and which admitters are GitHub accounts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GithubFacts {
    pub access: IssuesAccess,
    pub labels: Option<Vec<String>>,
    pub accounts: Option<Vec<AccountFact>>,
}

/// Opens the project's tracker adapter, afresh on each call: `owlshift do`
/// opens it again on each try while the tracker does not answer (OWL-479).
pub struct Opener<'a> {
    open: Box<dyn Fn() -> Result<Box<dyn tracker::Tracker>, Refusal> + 'a>,
}

impl<'a> Opener<'a> {
    fn new(open: impl Fn() -> Result<Box<dyn tracker::Tracker>, Refusal> + 'a) -> Self {
        Self {
            open: Box::new(open),
        }
    }

    /// The adapter, opened now.
    pub fn open(&self) -> Result<Box<dyn tracker::Tracker>, Refusal> {
        (self.open)()
    }
}

/// The keychain account, under [`SERVICE`], that every project of a machine
/// shared for the Linear API key before OWL-308, still read for a project
/// with no tracker account of its own; a project's own account ends with
/// this name ([`crate::secrets`]).
pub const LINEAR_ACCOUNT: &str = "linear";

/// The shared keychain account of the client ID of the Linear OAuth app
/// Owlshift writes as (decision D8, OWL-157). Optional, and stored with
/// [`LINEAR_APP_SECRET_ACCOUNT`] or not at all; per project as the key is.
pub const LINEAR_APP_ID_ACCOUNT: &str = "linear-app-client-id";

/// The shared keychain account of that app's client secret.
pub const LINEAR_APP_SECRET_ACCOUNT: &str = "linear-app-client-secret";

/// The Linear kind's keychain set ([`Kind::secret_set`]): its API key and
/// its app's client ID and secret, read whole from one place.
pub static LINEAR_SECRETS: SecretSet = SecretSet {
    name: "the Linear key and app",
    entries: &[
        LINEAR_ACCOUNT,
        LINEAR_APP_ID_ACCOUNT,
        LINEAR_APP_SECRET_ACCOUNT,
    ],
};

/// The Linear adapter, with the API key of the tracker set `secrets`:
/// `owlshift do` reads the ticket with it and hands it to the
/// [writer](crate::writer); `owlshift init` stores the key.
///
/// When the set holds the Linear app's client ID and secret too, the
/// adapter requests the app's token now, before any work, and writes
/// through it, checked to act as an app user of the key's workspace that
/// sees team `team`. Anything that keeps the app from being used refuses:
/// Owlshift never falls back to the key, which would keep Linear from
/// notifying a decider who holds it. The token is revoked when the adapter
/// is dropped.
///
/// Every refusal is lasting but a Linear that did not answer the token's
/// request, which a restart may mend (OWL-435, [`Refusal`]).
pub fn linear(secrets: &TrackerSecrets<Secret>, team: &str) -> Result<LinearTracker, Refusal> {
    let Some(key) = secrets.get(LINEAR_ACCOUNT) else {
        return Err(format!(
            "no Linear API key in the system keychain: run `owlshift init` in a terminal, or \
             store one under service `{SERVICE}`, account `{}`",
            secrets.accounts.of(LINEAR_ACCOUNT)
        )
        .into());
    };
    let tracker = LinearTracker::new(ApiKey::new(key.expose()));
    match app_credentials(secrets).map_err(|e| e.to_string())? {
        None => Ok(tracker),
        Some(credentials) => open_app(tracker, credentials, &secrets.accounts, team)
            .map(|(tracker, _)| tracker)
            .map_err(app_refusal),
    }
}

/// The refusal of a Linear app that could not be used: waiting for the
/// network when Linear gave no answer at all (OWL-479), passing when it
/// answered with an error a retry may clear, lasting otherwise.
fn app_refusal(error: AppError) -> Refusal {
    match &error {
        AppError::Tracker { error: answer, .. } => match answer.kind {
            ErrorKind::Unreachable => Refusal::unreachable(error.to_string()),
            ErrorKind::Other => Refusal::passing(error.to_string()),
            ErrorKind::Unauthorized | ErrorKind::NotFound => error.to_string().into(),
        },
        _ => error.to_string().into(),
    }
}

/// Why the Linear app could not be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AppError {
    /// The keychain could not be read, for this reason.
    Keychain(String),
    /// One of the app's two entries is stored without the other: the account
    /// missing, and whether the set is the project's or the shared one.
    Incomplete { missing: String, source: Source },
    /// No Linear API key is stored under this account, which reads and the
    /// app's check need.
    NoKey { account: String },
    /// Linear refused the app stored under these accounts, or did not
    /// answer.
    Tracker {
        error: tracker::Error,
        accounts: TrackerAccounts,
    },
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Keychain(reason) => write!(
                f,
                "could not read the Linear app's credentials in the system keychain: {reason}"
            ),
            Self::Incomplete { missing, source } => write!(
                f,
                "only half of the Linear app's credentials is stored: nothing under service \
                 `{SERVICE}`, account `{missing}`. {}",
                match source {
                    Source::Project =>
                        "Run `owlshift init --replace-secrets` to store its \
                                        client ID and secret together",
                    Source::Shared => SHARED_FIX,
                }
            ),
            Self::NoKey { account } => write!(
                f,
                "no Linear API key in the system keychain (service `{SERVICE}`, account \
                 `{account}`): run `owlshift init`"
            ),
            Self::Tracker { error, accounts } => {
                write!(
                    f,
                    "the Linear app Owlshift writes as cannot be used: {error}. Owlshift does \
                     not write through the API key instead, so that its questions keep reaching \
                     the decider: "
                )?;
                match accounts.source {
                    Source::Project => write!(
                        f,
                        "fix the app, run `owlshift init --replace-secrets` to store other \
                         credentials, or delete both of the app's keychain entries (service \
                         `{SERVICE}`, accounts `{}` and `{}`) to write through the API key",
                        accounts.of(LINEAR_APP_ID_ACCOUNT),
                        accounts.of(LINEAR_APP_SECRET_ACCOUNT)
                    ),
                    Source::Shared => write!(f, "fix the app, or {SHARED_FIX}"),
                }
            }
        }
    }
}

/// The fix for the shared tracker set, whose entries other projects of the
/// machine may use and which Owlshift never tells anyone to delete.
const SHARED_FIX: &str = "run `owlshift init --replace-secrets` in this repository to give the \
     project a Linear key and app of its own: the shared entries serve every project of this \
     machine";

/// The Linear app's client ID and secret of `secrets`: `None` when neither
/// is stored, [`AppError::Incomplete`] when only one is.
pub fn app_credentials(
    secrets: &TrackerSecrets<Secret>,
) -> Result<Option<ClientCredentials>, AppError> {
    let accounts = &secrets.accounts;
    let incomplete = |missing: &str| AppError::Incomplete {
        missing: missing.to_owned(),
        source: accounts.source,
    };
    match (
        secrets.get(LINEAR_APP_ID_ACCOUNT),
        secrets.get(LINEAR_APP_SECRET_ACCOUNT),
    ) {
        (None, None) => Ok(None),
        (Some(id), Some(secret)) => Ok(Some(ClientCredentials::new(id.expose(), secret.expose()))),
        (Some(_), None) => Err(incomplete(accounts.of(LINEAR_APP_SECRET_ACCOUNT))),
        (None, Some(_)) => Err(incomplete(accounts.of(LINEAR_APP_ID_ACCOUNT))),
    }
}

/// The tracker set stored under `accounts`, each entry read once, in the
/// set's order.
pub fn read(
    keychain: &Keychain,
    accounts: &TrackerAccounts,
) -> Result<TrackerSecrets<Secret>, String> {
    let values = accounts
        .accounts
        .iter()
        .map(|account| keychain.read(account).map_err(|error| error.to_string()))
        .collect::<Result<_, _>>()?;
    Ok(TrackerSecrets {
        accounts: accounts.clone(),
        values,
    })
}

/// The app user the Linear adapter writes as, checked exactly as
/// [`linear`] checks it, with the set stored under `accounts`: `None` when
/// no app is stored. Its token is revoked before this returns. What
/// `owlshift doctor` reports (OWL-157).
pub fn linear_app_user(
    keychain: &Keychain,
    accounts: &TrackerAccounts,
    team: &str,
) -> Result<Option<AppUser>, AppError> {
    let secrets = read(keychain, accounts).map_err(AppError::Keychain)?;
    let Some(credentials) = app_credentials(&secrets)? else {
        return Ok(None);
    };
    let key = secrets.get(LINEAR_ACCOUNT).ok_or_else(|| AppError::NoKey {
        account: accounts.of(LINEAR_ACCOUNT).to_owned(),
    })?;
    open_app(
        LinearTracker::new(ApiKey::new(key.expose())),
        credentials,
        accounts,
        team,
    )
    .map(|(_, user)| Some(user))
}

/// Requests the app's token, then checks it against the key's workspace and
/// the team.
fn open_app(
    tracker: LinearTracker,
    credentials: ClientCredentials,
    accounts: &TrackerAccounts,
    team: &str,
) -> Result<(LinearTracker, AppUser), AppError> {
    let refused = |error| AppError::Tracker {
        error,
        accounts: accounts.clone(),
    };
    let app = AppTransport::connect(HttpOAuth::new(credentials)).map_err(refused)?;
    tracker.writing_as_app(app, team).map_err(refused)
}

/// Why `owlshift doctor` could not read the Linear team's workflow states.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StatesError {
    /// No Linear API key is stored.
    NoKey,
    /// The keychain could not be read, for this reason.
    Keychain(String),
    /// Linear answered with an error, or did not answer.
    Tracker(tracker::Error),
}

/// The names of the workflow states of the Linear team `team`, read with
/// the key stored under `accounts` (OWL-147). The key is read once: on
/// macOS each read may ask to allow the access.
pub fn linear_team_states(
    keychain: &Keychain,
    accounts: &TrackerAccounts,
    team: &str,
) -> Result<Vec<String>, StatesError> {
    match read_key(keychain, accounts) {
        Ok(Some(key)) => LinearTracker::new(key)
            .team_states(team)
            .map_err(StatesError::Tracker),
        Ok(None) => Err(StatesError::NoKey),
        Err(reason) => Err(StatesError::Keychain(reason)),
    }
}

/// What `owlshift doctor` checks of the admission and proposals setup of the
/// Linear team `team` (OWL-155), read with the key stored under `accounts`:
/// the labels named `labels`, the accounts `people` and the inbox for
/// proposals, given the ready state `ready`. The key is read once more, as
/// [`linear_team_states`] reads it.
pub fn linear_admission(
    keychain: &Keychain,
    accounts: &TrackerAccounts,
    team: &str,
    ready: &str,
    labels: &[&str],
    people: &[&str],
) -> Result<AdmissionFacts, StatesError> {
    match read_key(keychain, accounts) {
        Ok(Some(key)) => LinearTracker::new(key)
            .admission_facts(team, ready, labels, people)
            .map_err(StatesError::Tracker),
        Ok(None) => Err(StatesError::NoKey),
        Err(reason) => Err(StatesError::Keychain(reason)),
    }
}

/// What doctor reads of the GitHub Issues project of `repo` whose
/// `[tracker]` is `tracker` (OWL-537), with the forge token stored for it,
/// read as doctor's other forge probes read it, without its refresh: one
/// issue list, then, when it reads, the labels and each admitter.
pub fn github_facts(
    keychain: &Keychain,
    repo: &Repo,
    tracker: &config::Tracker,
) -> Result<GithubFacts, StatesError> {
    let mut read = |account: &str| keychain.read(account).map_err(|error| error.to_string());
    let found = crate::secrets::forge(repo, &mut read).map_err(StatesError::Keychain)?;
    let token =
        crate::forge::token_at(&found, jiff::Timestamp::now()).map_err(StatesError::Keychain)?;
    let issues = GitHubTracker::new(GitHubForge::new(token, repo.clone()), tracker);
    let access = issues.issues_access().map_err(StatesError::Tracker)?;
    if access != IssuesAccess::Granted {
        return Ok(GithubFacts {
            access,
            labels: None,
            accounts: None,
        });
    }
    let labels = issues.labels().map_err(StatesError::Tracker)?;
    let ids: Vec<&str> = tracker
        .admitters
        .iter()
        .flatten()
        .map(String::as_str)
        .collect();
    let accounts = issues.accounts(&ids).map_err(StatesError::Tracker)?;
    Ok(GithubFacts {
        access,
        labels: Some(labels),
        accounts: Some(accounts),
    })
}

fn read_key(keychain: &Keychain, accounts: &TrackerAccounts) -> Result<Option<ApiKey>, String> {
    keychain
        .read(accounts.of(LINEAR_ACCOUNT))
        .map(|key| key.map(|key| ApiKey::new(key.expose())))
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use owlshift_adapters::forge::Repo;
    use owlshift_adapters::tracker::ErrorKind;

    use super::*;

    fn own() -> TrackerAccounts {
        TrackerAccounts::project(&Repo::parse("pitchopp/owlshift").unwrap(), &LINEAR_SECRETS)
    }

    /// A kind's name is the value `[tracker].kind` takes for it, and the
    /// lines `init` writes say it.
    #[test]
    fn each_kind_is_named_as_the_project_file_names_it() {
        for kind in [
            TrackerKind::Linear,
            TrackerKind::Markdown,
            TrackerKind::Github,
        ] {
            let name = of(kind).name();
            assert_eq!(serde_json::to_value(kind).unwrap(), name, "{kind:?}");
            let lines = of(kind).project_file_lines(Some("OWL")).unwrap();
            assert!(lines.contains(&format!("kind = \"{name}\"")), "{lines}");
        }
    }

    /// OWL-544: each kind's secrets are entries of the set it declares, and
    /// a kind declares a set exactly when it has secrets, so that
    /// [`keychain_set`] looks no account up for a kind with none.
    #[test]
    fn each_kind_reads_the_set_it_declares() {
        for kind in [
            TrackerKind::Linear,
            TrackerKind::Markdown,
            TrackerKind::Github,
        ] {
            let kind = of(kind);
            let specs = kind.secret_specs();
            assert_eq!(
                kind.secret_set().is_none(),
                specs.is_empty(),
                "{}",
                kind.name()
            );
            if let Some(set) = kind.secret_set() {
                for spec in specs {
                    assert!(set.entries.contains(&spec.account), "{}", spec.account);
                }
            }
        }
        assert_eq!(of(TrackerKind::Linear).secret_set(), Some(&LINEAR_SECRETS));
    }

    /// OWL-544: a probe of a kind of its own, with a script of its own, is
    /// answered through the host by the kind-neutral API alone.
    #[test]
    fn a_probe_of_another_kind_is_answered_through_the_host() {
        use crate::system::fake::FakeSystem;

        type Acme = Result<u32, String>;
        #[derive(Debug)]
        struct AcmeProbe;
        #[derive(Default)]
        struct AcmeScript {
            answer: Option<u32>,
        }
        impl Probe for AcmeProbe {
            fn read(&self, _keychain: &Keychain) -> Probed {
                Probed::new::<Acme>(Err("no network in tests".to_owned()))
            }

            fn keychain_error(&self, reason: String) -> Probed {
                Probed::new::<Acme>(Err(reason))
            }

            fn fake(&self, host: &FakeSystem) -> Probed {
                if let Err(reason) = host.secret_stored("acme") {
                    return self.keychain_error(reason);
                }
                let answer = host.scripted::<AcmeScript>().and_then(|s| s.answer);
                Probed::new::<Acme>(answer.ok_or_else(|| "not scripted".to_owned()))
            }
        }

        let host = FakeSystem::default().script(|s: &mut AcmeScript| s.answer = Some(7));
        assert_eq!(ask::<Acme>(&host, &AcmeProbe), Ok(7));
        assert_eq!(
            ask::<Acme>(&FakeSystem::default(), &AcmeProbe),
            Err("not scripted".to_owned())
        );
        let locked = FakeSystem::default()
            .script(|s: &mut AcmeScript| s.answer = Some(7))
            .keychain_fails("locked");
        assert_eq!(ask::<Acme>(&locked, &AcmeProbe), Err("locked".to_owned()));
        // An answer is never shown, and only its own type takes it.
        let probed = Probed::new::<Acme>(Ok(7));
        assert_eq!(format!("{probed:?}"), "Probed(..)");
        assert_eq!(probed.take::<u32>(), None);
    }

    /// OWL-537: `init` asks a Linear project's tracker with the key it
    /// stored, and a Markdown project's not at all; with no key, or no
    /// repository to name it by, the tracker does not answer.
    #[test]
    fn init_asks_linear_with_its_stored_key_and_markdown_nothing() {
        let project = |kind: &str, team: &str| {
            ProjectConfig::parse(&format!(
                "requires = \">=0.1\"\n[tracker]\nkind = \"{kind}\"\n{team}\
                 admit = {{ label = \"agent\" }}\n\
                 states = {{ ready = \"Todo\", working = \"Doing\", needs_input = \"Asked\", review = \"Review\" }}\n\
                 [stack]\ngate = [\"make\"]\n[pipeline]\ndefault = \"trivial\"\n\
                 plan_approval = \"never\"\n[models]\n[policy]\nalways_human = []\n"
            ))
            .unwrap()
            .tracker
        };
        let repo = Repo::parse("pitchopp/owlshift").unwrap();
        let keychain = Keychain::in_memory();
        let dir = tempfile::tempdir().unwrap();
        let access = crate::forge::AppAccess {
            keychain: std::sync::Arc::new(Keychain::in_memory()),
            lock: dir.path().join("lock"),
            app: crate::forge::GitHubApp::from_personal(None),
        };
        let linear = project("linear", "team = \"OWL\"\n");
        let setup = |tracker, repo| Setup {
            tracker,
            repo,
            keychain: &keychain,
            access: &access,
        };
        let error = of(TrackerKind::Linear)
            .connect(&setup(&linear, Ok(&repo)))
            .unwrap_err();
        assert!(
            error.contains("no Linear API key")
                && error.contains("`github.com/pitchopp/owlshift/linear`"),
            "{error}"
        );
        let error = of(TrackerKind::Linear)
            .connect(&setup(&linear, Err("`origin` is not set")))
            .unwrap_err();
        assert!(error.ends_with("`origin` is not set"), "{error}");

        let markdown = project("markdown", "");
        assert_eq!(
            of(TrackerKind::Markdown).connect(&setup(&markdown, Err("no origin"))),
            Ok(Connected::default())
        );
    }

    /// OWL-479: a Linear that gave no answer waits for the network, one
    /// that answered with an error a retry may clear restarts, a refusal
    /// lasts.
    #[test]
    fn an_app_that_could_not_be_used_is_refused_by_what_mends_it() {
        let of = |kind| {
            app_refusal(AppError::Tracker {
                error: tracker::Error::new(kind, "Linear's token endpoint: io: x"),
                accounts: own(),
            })
        };
        assert!(of(ErrorKind::Unreachable).unanswered());
        let passing = of(ErrorKind::Other);
        assert!(!passing.lasting() && !passing.unanswered());
        assert!(of(ErrorKind::Unauthorized).lasting());
        assert!(
            app_refusal(AppError::NoKey {
                account: "k".into()
            })
            .lasting()
        );
    }

    #[test]
    fn the_linear_key_comes_from_the_accounts_given() {
        let keychain = Keychain::in_memory();
        let missing = linear(&read(&keychain, &own()).unwrap(), "OWL").unwrap_err();
        assert!(missing.lasting(), "{missing}");
        let missing = missing.message;
        assert!(
            missing.contains("service `owlshift`, account `github.com/pitchopp/owlshift/linear`"),
            "{missing}"
        );
        assert_eq!(
            linear_team_states(&keychain, &own(), "OWL"),
            Err(StatesError::NoKey)
        );

        keychain
            .store(own().of(LINEAR_ACCOUNT), &Secret::new("lin_api_test"))
            .unwrap();
        assert!(linear(&read(&keychain, &own()).unwrap(), "OWL").is_ok());
        // The shared account is another entry.
        assert_eq!(
            read(&keychain, &TrackerAccounts::shared(&LINEAR_SECRETS))
                .unwrap()
                .get(LINEAR_ACCOUNT),
            None
        );
    }

    /// The app is the pair or nothing: half of it is refused, naming the
    /// entry missing, and no stored value shows in a message.
    #[test]
    fn the_apps_credentials_are_both_stored_or_neither() {
        const SENTINEL: &str = "SENTINEL_never_shown";
        let keychain = Keychain::in_memory();
        let accounts = own();
        let set = || read(&keychain, &accounts).unwrap();
        assert!(app_credentials(&set()).unwrap().is_none());
        assert_eq!(linear_app_user(&keychain, &accounts, "OWL"), Ok(None));

        keychain
            .store(accounts.of(LINEAR_APP_ID_ACCOUNT), &Secret::new(SENTINEL))
            .unwrap();
        assert_eq!(
            app_credentials(&set()).unwrap_err(),
            AppError::Incomplete {
                missing: accounts.of(LINEAR_APP_SECRET_ACCOUNT).to_owned(),
                source: Source::Project,
            }
        );
        assert_eq!(
            linear_app_user(&keychain, &accounts, "OWL"),
            Err(AppError::Incomplete {
                missing: accounts.of(LINEAR_APP_SECRET_ACCOUNT).to_owned(),
                source: Source::Project,
            })
        );
        keychain
            .store(accounts.of(LINEAR_ACCOUNT), &Secret::new("lin_api_test"))
            .unwrap();
        let refused = linear(&set(), "OWL").unwrap_err();
        assert!(refused.lasting(), "{refused}");
        let refused = refused.message;
        assert!(
            refused.contains("`github.com/pitchopp/owlshift/linear-app-client-secret`"),
            "{refused}"
        );
        assert!(!refused.contains("SENTINEL"), "{refused}");

        keychain.delete(accounts.of(LINEAR_APP_ID_ACCOUNT)).unwrap();
        keychain
            .store(
                accounts.of(LINEAR_APP_SECRET_ACCOUNT),
                &Secret::new(SENTINEL),
            )
            .unwrap();
        assert_eq!(
            app_credentials(&set()).unwrap_err(),
            AppError::Incomplete {
                missing: accounts.of(LINEAR_APP_ID_ACCOUNT).to_owned(),
                source: Source::Project,
            }
        );
        keychain
            .store(accounts.of(LINEAR_APP_ID_ACCOUNT), &Secret::new(SENTINEL))
            .unwrap();
        let both = format!("{:?}", app_credentials(&set()).unwrap().unwrap());
        assert!(!both.contains("SENTINEL"), "{both}");
    }

    /// Deleting the app's entries is advised for the project's own entries
    /// only: shared ones may serve another project of the machine.
    #[test]
    fn deleting_the_app_is_never_advised_for_shared_entries() {
        let error = tracker::Error::new(ErrorKind::Unauthorized, "Linear: refused");
        let own_text = AppError::Tracker {
            error: error.clone(),
            accounts: own(),
        }
        .to_string();
        assert!(
            own_text.contains(
                "delete both of the app's keychain entries (service `owlshift`, accounts \
                 `github.com/pitchopp/owlshift/linear-app-client-id` and"
            ),
            "{own_text}"
        );
        for shared in [
            AppError::Tracker {
                error,
                accounts: TrackerAccounts::shared(&LINEAR_SECRETS),
            },
            AppError::Incomplete {
                missing: LINEAR_APP_SECRET_ACCOUNT.to_owned(),
                source: Source::Shared,
            },
        ] {
            let text = shared.to_string();
            assert!(!text.contains("delete"), "{text}");
            assert!(
                text.contains("`owlshift init --replace-secrets` in this repository"),
                "{text}"
            );
        }
    }
}
