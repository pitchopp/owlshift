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
//! it. Each kind is a file of its own under `tracker/`, which declares its
//! keychain secret set ([`Kind::secret_set`]) and defines doctor's probes
//! of its tracker ([`Probe`], OWL-544).

mod github;
mod linear;
mod markdown;

/// The reply key of a GitHub Issues project from two opened forges of its
/// credential (OWL-552): what `Kind::reply_key` builds once it has opened
/// them, public so that a bench builds the same key over a fake.
pub use github::key_of as github_key;
/// The Linear workspace's teams `init` offers to pick from (OWL-596).
pub use linear::workspace_teams as linear_teams;

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
use owlshift_contracts::config::{self, PersonalConfig, ProjectConfig, TrackerKind};
use owlshift_contracts::ids::TicketId;
use owlshift_platform::keychain::{Keychain, SERVICE, Secret};
use owlshift_platform::service::sh_quote;

use crate::config::FileState;
use crate::doctor::{Check, Step};
use crate::init::SecretSpec;
use crate::refusal::{PICKED_UP_AGAIN, Refusal, resumed};
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
    /// the tracker is not asked). A fix that runs `owlshift init` names
    /// `folder`, the one the service's pause names (OWL-658).
    fn doctor(
        &self,
        system: &dyn System,
        config: &ProjectConfig,
        accounts: Option<&TrackerAccounts>,
        repo: Option<&Repo>,
        folder: &Path,
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
    /// the project of `repo` served from `data` on `config`, opened with its
    /// account read once ([`crate::ui::reply::open_key`]); the error says why
    /// there is none. Linear's is the API key `init` stored, GitHub Issues'
    /// the forge credential, which needs the personal file's GitHub App
    /// settings (`personal`) to refresh the App's token (OWL-552).
    fn reply_key(
        &self,
        data: &Path,
        repo: &Repo,
        config: &ProjectConfig,
        personal: &FileState<PersonalConfig>,
    ) -> Result<crate::ui::reply::Key, String>;

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

    /// The accounts `init` signed in as, with the credentials it just
    /// stored, for the entry it prints when `[people]` does not list them
    /// (OWL-582): the tracker key's account and the forge credential's
    /// login, each read as the UI's reply key reads it (`key_account`) and
    /// left out when it cannot be read or is an app's. Nothing on a tracker
    /// whose key has no account.
    fn signed_in(&self, setup: &Setup<'_>) -> SignedIn {
        let _ = setup;
        SignedIn::default()
    }

    /// This machine's operator for doctor's `people` line (OWL-582): the
    /// account of the tracker key `init` stored (build plan "P10 · Teams",
    /// "The operator"), read as the UI's reply key reads it
    /// (`key_account`), for the loaded project `config`, with the tracker
    /// set stored under `accounts` or the credential of `repo`. `None` when
    /// it cannot be asked, which the `secrets` line says, and on a tracker
    /// whose key has no account.
    fn operator(
        &self,
        system: &dyn System,
        config: &ProjectConfig,
        accounts: Option<&TrackerAccounts>,
        repo: Option<&Repo>,
    ) -> Option<Operator> {
        let _ = (system, config, accounts, repo);
        None
    }
}

/// The operator's account as [`Kind::operator`] read it, or why not.
pub type Operator = Result<tracker::KeyAccount, String>;

/// The accounts [`Kind::signed_in`] read, each when it could.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SignedIn {
    /// The tracker account's identifier.
    pub tracker: Option<String>,
    /// The forge login.
    pub forge: Option<String>,
}

/// The account of the forge credential `init` stored for the project, as
/// `GET user` gives it (`key_account`); `None` when it cannot be read.
pub(crate) fn forge_account(setup: &Setup<'_>) -> Option<tracker::KeyAccount> {
    let repo = setup.repo.ok()?;
    let mut read = |account: &str| setup.keychain.read(account).map_err(|e| e.to_string());
    let found = crate::secrets::forge(repo, &mut read).ok()?;
    let forge = crate::forge::github(&found, repo.clone(), setup.access).ok()?;
    GitHubTracker::new(forge, setup.tracker).key_account().ok()
}

/// The login of [`forge_account`], unless it is an app's.
pub(crate) fn forge_login(setup: &Setup<'_>) -> Option<String> {
    forge_account(setup)
        .filter(|account| !account.app)
        .map(|account| account.name)
}

/// The lines `init` has written under `[tracker]` since P4 for Linear and
/// Markdown: a label to admit, Linear's default states; and since OWL-594
/// the admission keys it leaves unset, commented out.
pub(crate) const ADMISSION_LINES: &str = "\
# How a person admits a ticket to Owlshift's queue (`owlshift do TICKET`
# ignores it): { label = \"...\" }, or \"ready\", the move to states.ready, which
# { state = \"...\" } would repeat; \"delegation\" is not built. Required.
admit = { label = \"agent\" }
# The accounts whose admission counts, as the tracker reports them (a Linear
# user's id, the name on the Markdown tracker), or handles of [people].
# Default: none, and `owlshift watch --drain` refuses to start.
# admitters = [\"<account id>\"]
# A label that keeps a ticket out of the drain, whatever else it carries;
# never the admission label. Default: none, no ticket is kept out.
# exclude = \"no-agent\"

[tracker.states]
# The tracker states that show where a ticket stands. Required, but for
# `parked`, the state of a ticket Owlshift stopped. Default for `parked`:
# none, such a ticket shows needs_input, its PARKED comment saying why.
ready = \"Todo\"
working = \"In Progress\"
needs_input = \"Needs Input\"
review = \"In Review\"
# parked = \"Parked\"
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
/// its file, and reads their answers with `ask` (OWL-544).
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

/// The answer to a [`Probe`], of the type its kind reads it as (`ask`).
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
        return Err(Refusal::no_linear_key(format!(
            "no Linear API key in the system keychain: run `owlshift init` in a terminal, or \
             store one under service `{SERVICE}`, account `{}`",
            secrets.accounts.of(LINEAR_ACCOUNT)
        )));
    };
    let tracker = LinearTracker::new(ApiKey::new(key.expose()));
    let accounts = &secrets.accounts;
    match app_credentials(secrets).map_err(|error| app_refusal(error, accounts, team))? {
        None => Ok(tracker),
        Some(credentials) => open_app(tracker, credentials, accounts, team)
            .map(|(tracker, _)| tracker)
            .map_err(|error| app_refusal(error, accounts, team)),
    }
}

/// What a person reads when no Linear API key is stored for the project of
/// `repo` (OWL-658): what stops, the command and the folder to run it in,
/// `checkout` shell-quoted, where to make the key, and what the command
/// keeps. `owlshift init` asks for a secret it does not find, keeps those
/// already stored and an existing project file. When `resumes`, the daemon
/// tries the project again by itself, and the notice says so.
pub fn linear_key_notice(repo: &Repo, checkout: &Path, resumes: bool) -> String {
    let notice = format!(
        "Owlshift has no Linear API key for {repo}, so none of its tickets move. Give it one: \
         `cd {}` then `owlshift init`, and paste {MAKE_A_KEY}; your owlshift.toml, your other \
         stored keys and your tickets are kept.",
        sh_quote(&checkout.display().to_string())
    );
    resumed(notice, resumes, PICKED_UP_AGAIN)
}

/// The step of `owlshift doctor`'s `tracker states` line for no Linear API
/// key: the [`linear_key_notice`]'s fix, in `folder` (OWL-658).
pub(crate) fn linear_key_step(folder: &Path) -> Step {
    Step::run_noting(
        "owlshift init",
        &format!(
            "in {}: paste {MAKE_A_KEY}; your owlshift.toml, your other stored keys and your \
             tickets are kept",
            folder.display()
        ),
    )
}

/// Where a person makes the Linear API key `owlshift init` asks for, as its
/// own question says ([`crate::init::LINEAR_KEY`]).
const MAKE_A_KEY: &str = "a personal API key you create in Linear's settings under Security & \
     access";

/// The refusal of the Linear app stored under `accounts` that could not be
/// used for team `team`: waiting for the network when Linear gave no answer
/// at all (OWL-479), passing when it answered with an error a retry may
/// clear, lasting otherwise. A lasting one a person mends is worded by its
/// [`AppFault`] once its command knows its folder (OWL-658).
fn app_refusal(error: AppError, accounts: &TrackerAccounts, team: &str) -> Refusal {
    let lasting = |kind| {
        Refusal::linear_app(
            error.to_string(),
            AppFault {
                kind,
                accounts: accounts.clone(),
            },
        )
    };
    match &error {
        AppError::Tracker { error: answer, .. } => match answer.kind {
            ErrorKind::Unreachable => Refusal::unreachable(error.to_string()),
            ErrorKind::Other | ErrorKind::NoGesture => Refusal::passing(error.to_string()),
            ErrorKind::Unauthorized => lasting(AppFaultKind::Refused),
            ErrorKind::NotFound => lasting(AppFaultKind::NoTeam(team.to_ascii_uppercase())),
        },
        AppError::Incomplete { .. } => lasting(AppFaultKind::Half),
        AppError::Keychain(_) => Refusal::keychain(error.to_string()),
        AppError::NoKey { .. } => Refusal::no_linear_key(error.to_string()),
    }
}

/// Why the Linear app a project stores keeps its tickets from moving, and
/// under which accounts, for the notice a person reads (OWL-658).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppFault {
    pub kind: AppFaultKind,
    pub accounts: TrackerAccounts,
}

/// What is wrong with the Linear app: each kind has its own fix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AppFaultKind {
    /// One of its two entries is stored without the other.
    Half,
    /// Linear refused it: its token's request, or the API key the app is
    /// checked against (both answer `Unauthorized`).
    Refused,
    /// It does not see the team of this key: an app reaches the
    /// workspace's public teams only.
    NoTeam(String),
}

impl AppFault {
    /// What a person reads for the project of `repo` whose command runs from
    /// `checkout`: what stops, then each fix with what it keeps. A whole
    /// pair of the project's own that Linear refused is stored again with
    /// `owlshift init --replace-secrets`, which asks again for each stored
    /// secret and signs in to GitHub again; half a pair and a shared set are
    /// mended by `owlshift init`, which asks for what is missing. The
    /// project's own entries may be deleted to write through the API key;
    /// shared ones never are (OWL-308). When `resumes`, the notice says the
    /// daemon picks the project up again.
    pub fn notice(&self, repo: &Repo, checkout: &Path, resumes: bool) -> String {
        let cd = format!("`cd {}`", sh_quote(&checkout.display().to_string()));
        let shared = self.accounts.source == Source::Shared;
        let from = if shared {
            ", from the keychain entries every project of this machine shares"
        } else {
            ""
        };
        let head = match &self.kind {
            AppFaultKind::Half => format!(
                "Only half of the Linear app Owlshift writes as is stored for {repo}{from}, so \
                 none of its tickets move."
            ),
            AppFaultKind::Refused => format!(
                "Linear refused the app Owlshift writes as for {repo}, or the API key it checks \
                 the app with{from}, so none of its tickets move."
            ),
            AppFaultKind::NoTeam(team) => format!(
                "The Linear app Owlshift writes as for {repo}{from} does not see team {team}, as \
                 an app reaches the workspace's public teams only, so none of its tickets move."
            ),
        };
        let fixes = match (&self.kind, shared) {
            (AppFaultKind::NoTeam(team), true) => format!(
                "Either {}. Or give the project a Linear key and app of its own: {cd} then \
                 `owlshift init`, {}.",
                public_team(team),
                OWN_SET_GIVEN
            ),
            (_, true) => format!(
                "Give the project a Linear key and app of its own: {cd} then `owlshift init`, \
                 {OWN_SET_GIVEN}."
            ),
            (AppFaultKind::Half, false) => format!(
                "Either store it whole: {cd} then `owlshift init`, {HALF_GIVEN}. Or {}.",
                self.write_through_key()
            ),
            (AppFaultKind::Refused, false) => format!(
                "Either store them again: {cd} then `owlshift init --replace-secrets`, \
                 {REFUSED_GIVEN}. Or, {}.",
                self.write_through_key()
            ),
            (AppFaultKind::NoTeam(team), false) => format!(
                "Either {}. Or {}.",
                public_team(team),
                self.write_through_key()
            ),
        };
        resumed(format!("{head} {fixes}"), resumes, PICKED_UP_AGAIN)
    }

    /// The steps of `owlshift doctor`'s `tracker identity` line for this
    /// fault, in `folder`: the notice's fixes, in the same words.
    pub(crate) fn steps(&self, folder: &Path) -> Vec<Step> {
        let shared = self.accounts.source == Source::Shared;
        let in_folder = |words: &str| format!("in {}: {words}", folder.display());
        let mut steps = Vec::new();
        if let AppFaultKind::NoTeam(team) = &self.kind {
            steps.push(Step::act(capitalized(&public_team(team))));
        }
        if shared {
            steps.push(Step::run_noting(
                "owlshift init",
                &in_folder(&format!(
                    "give the project a Linear key and app of its own: {OWN_SET_GIVEN}"
                )),
            ));
            return steps;
        }
        match &self.kind {
            AppFaultKind::Half => {
                steps.push(Step::run_noting("owlshift init", &in_folder(HALF_GIVEN)));
            }
            AppFaultKind::Refused => steps.push(Step::run_noting(
                "owlshift init --replace-secrets",
                &in_folder(REFUSED_GIVEN),
            )),
            AppFaultKind::NoTeam(_) => {}
        }
        steps.push(Step::act(format!("Or {}", self.write_through_key())));
        steps
    }

    /// The choice of writing through the API key: the project's own two
    /// entries to delete, and what it costs.
    fn write_through_key(&self) -> String {
        let (id, secret) = (
            self.accounts.of(LINEAR_APP_ID_ACCOUNT),
            self.accounts.of(LINEAR_APP_SECRET_ACCOUNT),
        );
        let (lead, which) = match self.kind {
            AppFaultKind::Half => ("write through the Linear API key", ", whichever is there"),
            AppFaultKind::Refused => ("if the API key still works, write through it", ""),
            AppFaultKind::NoTeam(_) => ("write through the Linear API key", ""),
        };
        format!(
            "{lead}: delete the keychain entries `{id}` and `{secret}` (service \
             `{SERVICE}`){which}; everything else is kept, but Linear then does not notify the \
             key's holder of Owlshift's questions"
        )
    }
}

/// What `owlshift init` asks once half the app's pair is stored, and what it
/// keeps.
const HALF_GIVEN: &str = "and give the app's client ID and secret; your owlshift.toml, your \
     other stored keys and your tickets are kept";

/// What `owlshift init --replace-secrets` asks for a refused app, and what
/// it keeps: it asks again for every stored secret and signs in to GitHub
/// again.
const REFUSED_GIVEN: &str = "and give the app's client ID and secret, and a new API key if \
     Linear revoked it; it asks again for each stored key, where Enter keeps it, and signs in \
     to GitHub again; your owlshift.toml and your tickets are kept";

/// What `owlshift init` asks of a project that reads the shared set: its
/// own key, the shared one kept by an empty answer, then its own app.
const OWN_SET_GIVEN: &str = "paste this project's Linear API key, then its app's client ID and \
     secret, or leave those empty to write through the key; the shared entries stay for the \
     other projects, and your owlshift.toml and your tickets are kept";

/// The fix of an app that does not see team `team`.
fn public_team(team: &str) -> String {
    format!("make team {team} a public team in Linear; nothing needs storing again")
}

/// `text` with its first letter in upper case.
fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
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
                        "Run `owlshift init` to store its client ID and secret together",
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
/// machine may use and which Owlshift never tells anyone to delete. `owlshift
/// init` asks for the project's own key, which the shared one does not
/// count as, then its own app (OWL-658: `--replace-secrets` is not needed).
const SHARED_FIX: &str = "run `owlshift init` in this repository to give the project a Linear \
     key and app of its own: the shared entries serve every project of this machine";

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
    let token = crate::forge::token_at(&found, jiff::Timestamp::now())
        .map_err(|error| StatesError::Keychain(error.into()))?;
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

/// The account of the Linear key stored under `accounts`: this machine's
/// operator, for doctor's `people` line (OWL-582).
pub fn linear_operator(keychain: &Keychain, accounts: &TrackerAccounts) -> Operator {
    match read_key(keychain, accounts)? {
        Some(key) => LinearTracker::new(key)
            .key_account()
            .map_err(|error| error.to_string()),
        None => Err("no Linear API key is stored for this project".to_owned()),
    }
}

/// The account of the forge credential stored for the GitHub Issues project
/// of `repo`: this machine's operator, for doctor's `people` line (OWL-582).
/// Read as [`github_facts`] opens the credential.
pub fn github_operator(keychain: &Keychain, repo: &Repo, tracker: &config::Tracker) -> Operator {
    let mut read = |account: &str| keychain.read(account).map_err(|error| error.to_string());
    let found = crate::secrets::forge(repo, &mut read)?;
    let token = crate::forge::token_at(&found, jiff::Timestamp::now())?;
    GitHubTracker::new(GitHubForge::new(token, repo.clone()), tracker)
        .key_account()
        .map_err(|error| error.to_string())
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
            app_refusal(
                AppError::Tracker {
                    error: tracker::Error::new(kind, "Linear's token endpoint: io: x"),
                    accounts: own(),
                },
                &own(),
                "owl",
            )
        };
        assert!(of(ErrorKind::Unreachable).unanswered());
        let passing = of(ErrorKind::Other);
        assert!(!passing.lasting() && !passing.unanswered());
        assert!(of(ErrorKind::Unauthorized).lasting());
        assert!(of(ErrorKind::NotFound).lasting());
        assert!(
            app_refusal(
                AppError::NoKey {
                    account: "k".into()
                },
                &own(),
                "owl"
            )
            .lasting()
        );

        // OWL-658: only a lasting refusal is worded for a person; a passing
        // one, and one waiting for the network, keep their words.
        let repo = Repo::parse("pitchopp/owlshift").unwrap();
        let folder = Path::new("/work/owlshift");
        let fault = |kind| AppFault {
            kind,
            accounts: own(),
        };
        for (kind, fault_kind) in [
            (ErrorKind::Unauthorized, AppFaultKind::Refused),
            (ErrorKind::NotFound, AppFaultKind::NoTeam("OWL".into())),
        ] {
            let said = of(kind).in_checkout(&repo, folder, true);
            assert_eq!(said.message, fault(fault_kind).notice(&repo, folder, true));
            assert!(said.detail.unwrap().contains("cannot be used"));
        }
        for kind in [ErrorKind::Unreachable, ErrorKind::Other] {
            let said = of(kind).in_checkout(&repo, folder, true);
            assert_eq!(said.detail, None, "{said}");
            assert!(said.message.contains("cannot be used"), "{said}");
        }
    }

    /// OWL-658: no Linear API key is said with the folder quoted, where to
    /// make the key, and what `owlshift init` keeps; never the keychain
    /// entry, which the technical words before it keep.
    #[test]
    fn no_linear_key_is_said_with_the_command_and_its_folder() {
        let repo = Repo::parse("acme/web").unwrap();
        let folder = Path::new("/Users/ada/My Projects/web");
        assert_eq!(
            linear_key_notice(&repo, folder, true),
            "Owlshift has no Linear API key for acme/web, so none of its tickets move. Give it \
             one: `cd '/Users/ada/My Projects/web'` then `owlshift init`, and paste a personal \
             API key you create in Linear's settings under Security & access; your \
             owlshift.toml, your other stored keys and your tickets are kept. Owlshift picks the \
             project up again within a minute."
        );
        let typed = linear_key_notice(&repo, Path::new("/tmp/ada's web"), false);
        assert!(typed.contains(r"`cd '/tmp/ada'\''s web'`"), "{typed}");
        assert!(typed.ends_with("your tickets are kept."), "{typed}");

        let keychain = Keychain::in_memory();
        let accounts = TrackerAccounts::project(&repo, &LINEAR_SECRETS);
        let refused = linear(&read(&keychain, &accounts).unwrap(), "WEB").unwrap_err();
        assert!(refused.lasting(), "{refused}");
        let technical = refused.message.clone();
        let said = refused.in_checkout(&repo, folder, false);
        assert_eq!(said.message, linear_key_notice(&repo, folder, false));
        assert_eq!(said.detail, Some(technical));
        assert!(!said.message.contains("service `"), "{said}");
    }

    /// OWL-658: the Linear app's notices, one per fault and source. The
    /// project's own pair: half of it is stored by `owlshift init`, a whole
    /// one Linear refused by `--replace-secrets`, which says it asks again
    /// and signs in again; both may be deleted to write through the key. A
    /// shared set is never told to be deleted.
    #[test]
    fn each_app_fault_is_said_with_its_fixes() {
        let repo = Repo::parse("acme/web").unwrap();
        let folder = Path::new("/work/web");
        let own = TrackerAccounts::project(&repo, &LINEAR_SECRETS);
        let shared = TrackerAccounts::shared(&LINEAR_SECRETS);
        let notice = |kind, accounts: &TrackerAccounts, resumes| {
            AppFault {
                kind,
                accounts: accounts.clone(),
            }
            .notice(&repo, folder, resumes)
        };
        let delete = "delete the keychain entries `github.com/acme/web/linear-app-client-id` and \
                      `github.com/acme/web/linear-app-client-secret` (service `owlshift`)";
        let costs = "everything else is kept, but Linear then does not notify the key's holder \
                     of Owlshift's questions.";

        assert_eq!(
            notice(AppFaultKind::Half, &own, true),
            format!(
                "Only half of the Linear app Owlshift writes as is stored for acme/web, so none \
                 of its tickets move. Either store it whole: `cd '/work/web'` then `owlshift \
                 init`, and give the app's client ID and secret; your owlshift.toml, your other \
                 stored keys and your tickets are kept. Or write through the Linear API key: \
                 {delete}, whichever is there; {costs} Owlshift picks the project up again \
                 within a minute."
            )
        );
        assert_eq!(
            notice(AppFaultKind::Refused, &own, false),
            format!(
                "Linear refused the app Owlshift writes as for acme/web, or the API key it \
                 checks the app with, so none of its tickets move. Either store them again: `cd \
                 '/work/web'` then `owlshift init --replace-secrets`, and give the app's client \
                 ID and secret, and a new API key if Linear revoked it; it asks again for each \
                 stored key, where Enter keeps it, and signs in to GitHub again; your \
                 owlshift.toml and your tickets are kept. Or, if the API key still works, write \
                 through it: {delete}; {costs}"
            )
        );
        assert_eq!(
            notice(AppFaultKind::NoTeam("WEB".into()), &own, false),
            format!(
                "The Linear app Owlshift writes as for acme/web does not see team WEB, as an \
                 app reaches the workspace's public teams only, so none of its tickets move. \
                 Either make team WEB a public team in Linear; nothing needs storing again. Or \
                 write through the Linear API key: {delete}; {costs}"
            )
        );

        let own_set = "Give the project a Linear key and app of its own: `cd '/work/web'` then \
                       `owlshift init`, paste this project's Linear API key, then its app's \
                       client ID and secret, or leave those empty to write through the key; the \
                       shared entries stay for the other projects, and your owlshift.toml and \
                       your tickets are kept.";
        assert_eq!(
            notice(AppFaultKind::Refused, &shared, false),
            format!(
                "Linear refused the app Owlshift writes as for acme/web, or the API key it \
                 checks the app with, from the keychain entries every project of this machine \
                 shares, so none of its tickets move. {own_set}"
            )
        );
        for kind in [
            AppFaultKind::Half,
            AppFaultKind::Refused,
            AppFaultKind::NoTeam("WEB".into()),
        ] {
            let said = notice(kind, &shared, true);
            assert!(!said.contains("delete"), "{said}");
            assert!(!said.contains("--replace-secrets"), "{said}");
            assert!(said.contains("`owlshift init`"), "{said}");
            assert!(
                said.ends_with("Owlshift picks the project up again within a minute."),
                "{said}"
            );
        }
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
            // OWL-658: `owlshift init` asks for the project's own key and
            // app; `--replace-secrets` is not needed.
            assert!(
                text.contains("`owlshift init` in this repository"),
                "{text}"
            );
        }
    }
}
