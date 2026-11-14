//! Which keychain accounts hold a project's tracker and forge secrets
//! (OWL-308, build plan "P5 · Quality pipeline", part K).
//!
//! A project is named by its GitHub repository: each secret lives under
//! service [`SERVICE`], account `github.com/<owner>/<repo>/<name>` in lower
//! case, where `<name>` is the account every project of a machine shared
//! before: an entry of its tracker's [`SecretSet`], such as
//! [`crate::tracker::LINEAR_ACCOUNT`], or [`GITHUB_ACCOUNT`]. Those shared
//! accounts are still read for a project that has none of its own, so an
//! existing install keeps working; Owlshift never deletes them.
//!
//! The tracker set is the one the project's tracker kind declares
//! ([`crate::tracker::Kind::secret_set`]): Linear's holds its key and its
//! app's pair. It is read whole from one place, since an app belongs to one
//! workspace: the project's accounts when any of its entries is stored
//! there, otherwise the shared ones when any of those is, otherwise the
//! project's, which then hold nothing. The GitHub credential is resolved
//! on its own, in the order of "Which credential a command uses" (build
//! plan, "`init`", OWL-336): the project's GitHub App token
//! ([`GITHUB_APP_ACCOUNT`]), else the project's personal token, else the
//! shared personal token. `init` writes project accounts only, so no shared
//! app account is read. Each account is looked up once, by a function the
//! caller gives: a read for a command, so the macOS Keychain asks once per
//! entry, or a presence check for `owlshift doctor`.

use owlshift_adapters::forge::Repo;
use owlshift_platform::keychain::SERVICE;

use crate::forge::{GITHUB_ACCOUNT, GITHUB_APP_ACCOUNT};

/// Where a set of secrets is read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The project's own accounts, named by its repository.
    Project,
    /// The accounts every project of this machine shares.
    Shared,
}

/// The project's account for the shared account `name`.
pub fn project_account(repo: &Repo, name: &str) -> String {
    format!("github.com/{}/{}/{name}", repo.owner(), repo.name()).to_lowercase()
}

/// The secrets a tracker kind keeps in the keychain, read whole from one
/// place ([`tracker`]).
#[derive(Debug, PartialEq, Eq)]
pub struct SecretSet {
    /// What a notice calls the set, such as "the Linear key and app".
    pub name: &'static str,
    /// The shared account of each entry, in the order they are looked up;
    /// a project's own account ends with it ([`project_account`]).
    pub entries: &'static [&'static str],
}

impl SecretSet {
    /// The place of `entry` in the set; it panics, naming the entry, when
    /// the set has none such: a programming error.
    fn index(&self, entry: &str) -> usize {
        self.entries
            .iter()
            .position(|e| *e == entry)
            .unwrap_or_else(|| panic!("{} has no entry `{entry}`", self.name))
    }
}

/// The keychain accounts of a tracker set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackerAccounts {
    pub set: &'static SecretSet,
    /// The account of each entry of the set, in its order.
    pub accounts: Vec<String>,
    pub source: Source,
}

impl TrackerAccounts {
    /// The project's own accounts.
    pub fn project(repo: &Repo, set: &'static SecretSet) -> Self {
        Self {
            set,
            accounts: set
                .entries
                .iter()
                .map(|entry| project_account(repo, entry))
                .collect(),
            source: Source::Project,
        }
    }

    /// The accounts every project of this machine shares.
    pub fn shared(set: &'static SecretSet) -> Self {
        Self {
            set,
            accounts: set.entries.iter().map(|&entry| entry.to_owned()).collect(),
            source: Source::Shared,
        }
    }

    /// The account of the set's `entry`; it panics when the set has no such
    /// entry.
    pub fn of(&self, entry: &str) -> &str {
        &self.accounts[self.set.index(entry)]
    }
}

/// Which kind of GitHub credential an account holds (OWL-336).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForgeKind {
    /// The user token of Owlshift's GitHub App, with its refresh token and
    /// expiries, as one entry
    /// (`owlshift_adapters::forge::github_app::UserToken::to_entry`).
    App,
    /// A personal token, the expert option.
    Personal,
}

/// The keychain account of a GitHub credential.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForgeAccount {
    /// The account's name under [`SERVICE`].
    pub token: String,
    pub kind: ForgeKind,
    pub source: Source,
}

impl ForgeAccount {
    /// The project's own GitHub App account.
    pub fn project_app(repo: &Repo) -> Self {
        Self {
            token: project_account(repo, GITHUB_APP_ACCOUNT),
            kind: ForgeKind::App,
            source: Source::Project,
        }
    }

    /// The project's own personal-token account.
    pub fn project(repo: &Repo) -> Self {
        Self {
            token: project_account(repo, GITHUB_ACCOUNT),
            kind: ForgeKind::Personal,
            source: Source::Project,
        }
    }

    /// The personal-token account every project of this machine shares.
    pub fn shared() -> Self {
        Self {
            token: GITHUB_ACCOUNT.to_owned(),
            kind: ForgeKind::Personal,
            source: Source::Shared,
        }
    }

    /// Every account the GitHub credential of the project of `repo` is read
    /// from, in order.
    pub fn in_order(repo: &Repo) -> [Self; 3] {
        [Self::project_app(repo), Self::project(repo), Self::shared()]
    }
}

/// A tracker set, as found: `T` is the secret for a command, `()` for a
/// presence check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackerSecrets<T> {
    pub accounts: TrackerAccounts,
    /// What is stored under each account, in the set's order.
    pub values: Vec<Option<T>>,
}

impl<T> TrackerSecrets<T> {
    /// What is stored for the set's `entry`; it panics when the set has no
    /// such entry.
    pub fn get(&self, entry: &str) -> Option<&T> {
        self.values[self.accounts.set.index(entry)].as_ref()
    }

    /// Whether any entry is stored.
    pub fn any(&self) -> bool {
        self.values.iter().any(Option::is_some)
    }
}

/// The GitHub credential, as found. With nothing stored, `account` is the
/// project's personal-token account, where a token can be stored by hand.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForgeSecret<T> {
    pub account: ForgeAccount,
    pub token: Option<T>,
}

impl<T> ForgeSecret<T> {
    /// Which kind of credential was found; `None` when none was.
    pub fn kind(&self) -> Option<ForgeKind> {
        self.token.as_ref().map(|_| self.account.kind)
    }
}

/// Looks one account up: `None` when nothing is stored there. The error
/// says why the keychain could not be read, never a value.
pub type Lookup<'a, T> = dyn FnMut(&str) -> Result<Option<T>, String> + 'a;

/// The tracker set `set` of the project of `repo`, by the rule of this
/// module: each account looked up once, in the set's order.
pub fn tracker<T>(
    repo: &Repo,
    set: &'static SecretSet,
    look: &mut Lookup<'_, T>,
) -> Result<TrackerSecrets<T>, String> {
    for accounts in [
        TrackerAccounts::project(repo, set),
        TrackerAccounts::shared(set),
    ] {
        let values = accounts
            .accounts
            .iter()
            .map(|account| look(account))
            .collect::<Result<_, _>>()?;
        let found = TrackerSecrets { accounts, values };
        if found.any() {
            return Ok(found);
        }
    }
    Ok(TrackerSecrets {
        accounts: TrackerAccounts::project(repo, set),
        values: set.entries.iter().map(|_| None).collect(),
    })
}

/// The GitHub credential of the project of `repo`, by the rule of this
/// module.
pub fn forge<T>(repo: &Repo, look: &mut Lookup<'_, T>) -> Result<ForgeSecret<T>, String> {
    for account in ForgeAccount::in_order(repo) {
        if let Some(token) = look(&account.token)? {
            return Ok(ForgeSecret {
                account,
                token: Some(token),
            });
        }
    }
    Ok(ForgeSecret {
        account: ForgeAccount::project(repo),
        token: None,
    })
}

/// What a command says, once, when a set comes from a shared account:
/// the accounts and the repository, never a value. `None` when every set
/// given is the project's own.
pub fn shared_notice(
    repo: &Repo,
    tracker: Option<&TrackerAccounts>,
    forge: &ForgeAccount,
) -> Option<String> {
    let mut shared = Vec::new();
    if let Some(accounts) = tracker.filter(|a| a.source == Source::Shared) {
        let named: Vec<String> = accounts.accounts.iter().map(|a| format!("`{a}`")).collect();
        let name = accounts.set.name;
        shared.push(match named.split_last() {
            Some((last, [])) => format!("{name} (account {last})"),
            Some((last, rest)) => format!("{name} (accounts {} and {last})", rest.join(", ")),
            None => name.to_owned(),
        });
    }
    if forge.source == Source::Shared {
        shared.push(format!("the GitHub token (account `{}`)", forge.token));
    }
    if shared.is_empty() {
        return None;
    }
    Some(format!(
        "github.com/{repo} has no secrets of its own: using {} that every project of this \
         machine shares, under service `{SERVICE}`. Run `owlshift init --replace-secrets` in \
         this repository to store its own.",
        shared.join(" and ")
    ))
}

/// How a report names `account`: "`x`" for the project's own, "the shared
/// `x`" for one every project of this machine shares.
pub fn named(account: &str, source: Source) -> String {
    match source {
        Source::Project => format!("`{account}`"),
        Source::Shared => format!("the shared `{account}`"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracker::{
        LINEAR_ACCOUNT, LINEAR_APP_ID_ACCOUNT, LINEAR_APP_SECRET_ACCOUNT, LINEAR_SECRETS,
    };

    fn repo() -> Repo {
        Repo::parse("Pitchopp/Owlshift").unwrap()
    }

    /// Resolves both sets in a keychain holding the accounts `stored`, with
    /// every account looked up, in order.
    fn resolve(stored: &[&str]) -> (TrackerSecrets<()>, ForgeSecret<()>, Vec<String>) {
        let mut asked = Vec::new();
        let mut look = |account: &str| {
            asked.push(account.to_owned());
            Ok(stored.contains(&account).then_some(()))
        };
        let tracker = tracker(&repo(), &LINEAR_SECRETS, &mut look).unwrap();
        let forge = forge(&repo(), &mut look).unwrap();
        (tracker, forge, asked)
    }

    #[test]
    fn a_projects_accounts_are_named_by_its_repository_in_lower_case() {
        assert_eq!(
            TrackerAccounts::project(&repo(), &LINEAR_SECRETS),
            TrackerAccounts {
                set: &LINEAR_SECRETS,
                accounts: vec![
                    "github.com/pitchopp/owlshift/linear".to_owned(),
                    "github.com/pitchopp/owlshift/linear-app-client-id".to_owned(),
                    "github.com/pitchopp/owlshift/linear-app-client-secret".to_owned(),
                ],
                source: Source::Project,
            }
        );
        assert_eq!(
            ForgeAccount::project(&repo()).token,
            "github.com/pitchopp/owlshift/github"
        );
    }

    #[test]
    fn the_projects_own_accounts_win_and_each_is_looked_up_once() {
        let own = TrackerAccounts::project(&repo(), &LINEAR_SECRETS);
        let token = ForgeAccount::project(&repo()).token;
        let (tracker, forge, asked) = resolve(&[
            own.of(LINEAR_ACCOUNT),
            &token,
            LINEAR_ACCOUNT,
            LINEAR_APP_ID_ACCOUNT,
            LINEAR_APP_SECRET_ACCOUNT,
            GITHUB_ACCOUNT,
        ]);
        assert_eq!(tracker.accounts, own);
        assert_eq!(
            (
                tracker.get(LINEAR_ACCOUNT),
                tracker.get(LINEAR_APP_ID_ACCOUNT)
            ),
            (Some(&()), None)
        );
        assert_eq!(forge.account.source, Source::Project);
        assert_eq!(forge.kind(), Some(ForgeKind::Personal));
        // No shared account is even looked up.
        let app = ForgeAccount::project_app(&repo()).token;
        assert_eq!(
            asked,
            [
                own.of(LINEAR_ACCOUNT),
                own.of(LINEAR_APP_ID_ACCOUNT),
                own.of(LINEAR_APP_SECRET_ACCOUNT),
                &app,
                &token
            ]
        );
    }

    /// OWL-336: the project's app token, else its personal token, else the
    /// shared one, each account looked up once and none past the one found.
    #[test]
    fn the_github_credential_is_the_apps_then_the_projects_then_the_shared_one() {
        let app = ForgeAccount::project_app(&repo());
        let own = ForgeAccount::project(&repo());
        assert_eq!(app.token, "github.com/pitchopp/owlshift/github-app");
        let found = |stored: &[&str]| {
            let mut asked = Vec::new();
            let mut look = |account: &str| {
                asked.push(account.to_owned());
                Ok(stored.contains(&account).then_some(()))
            };
            let found = forge(&repo(), &mut look).unwrap();
            let kind = found.kind();
            (found.account, kind, asked)
        };

        let every = [app.token.as_str(), own.token.as_str(), GITHUB_ACCOUNT];
        assert_eq!(
            found(&every),
            (app.clone(), Some(ForgeKind::App), vec![app.token.clone()])
        );
        assert_eq!(
            found(&[&own.token, GITHUB_ACCOUNT]),
            (
                own.clone(),
                Some(ForgeKind::Personal),
                vec![app.token.clone(), own.token.clone()]
            )
        );
        assert_eq!(
            found(&[GITHUB_ACCOUNT]),
            (
                ForgeAccount::shared(),
                Some(ForgeKind::Personal),
                vec![
                    app.token.clone(),
                    own.token.clone(),
                    GITHUB_ACCOUNT.to_owned()
                ]
            )
        );
        // Nothing stored: the project's personal account is named, no kind.
        assert_eq!(found(&[]).0, own);
        assert_eq!(found(&[]).1, None);
    }

    #[test]
    fn a_project_with_none_of_its_own_reads_the_shared_accounts() {
        let (tracker, forge, _) = resolve(&[LINEAR_ACCOUNT, LINEAR_APP_ID_ACCOUNT, GITHUB_ACCOUNT]);
        assert_eq!(tracker.accounts, TrackerAccounts::shared(&LINEAR_SECRETS));
        assert_eq!(tracker.get(LINEAR_ACCOUNT), Some(&()));
        assert_eq!(forge.account, ForgeAccount::shared());
        assert_eq!(forge.token, Some(()));
    }

    /// The tracker set is never mixed: a project's own app without its own
    /// key keeps the project's set, key missing, and a project's own key
    /// never goes with the shared app. The forge token is resolved apart.
    #[test]
    fn the_tracker_set_is_never_mixed() {
        let own = TrackerAccounts::project(&repo(), &LINEAR_SECRETS);
        let (tracker, forge, _) = resolve(&[
            own.of(LINEAR_APP_ID_ACCOUNT),
            LINEAR_ACCOUNT,
            LINEAR_APP_ID_ACCOUNT,
            LINEAR_APP_SECRET_ACCOUNT,
            GITHUB_ACCOUNT,
        ]);
        assert_eq!(tracker.accounts, own);
        assert_eq!(tracker.get(LINEAR_ACCOUNT), None);
        assert_eq!(forge.account.source, Source::Shared);

        let (tracker, _, _) = resolve(&[
            own.of(LINEAR_ACCOUNT),
            LINEAR_APP_ID_ACCOUNT,
            LINEAR_APP_SECRET_ACCOUNT,
        ]);
        assert_eq!(tracker.accounts, own);
        assert_eq!(
            (
                tracker.get(LINEAR_APP_ID_ACCOUNT),
                tracker.get(LINEAR_APP_SECRET_ACCOUNT)
            ),
            (None, None)
        );
    }

    #[test]
    fn with_nothing_stored_the_projects_accounts_are_named() {
        let (tracker, forge, _) = resolve(&[]);
        assert_eq!(
            tracker.accounts,
            TrackerAccounts::project(&repo(), &LINEAR_SECRETS)
        );
        assert_eq!(tracker.get(LINEAR_ACCOUNT), None);
        assert_eq!(forge.account, ForgeAccount::project(&repo()));
        assert_eq!(forge.token, None);
    }

    #[test]
    fn a_keychain_that_cannot_be_read_stops_the_resolution() {
        let mut look = |_: &str| -> Result<Option<()>, String> { Err("locked".to_owned()) };
        assert_eq!(
            tracker(&repo(), &LINEAR_SECRETS, &mut look).unwrap_err(),
            "locked"
        );
        assert_eq!(forge(&repo(), &mut look).unwrap_err(), "locked");
    }

    #[test]
    fn the_notice_names_the_shared_accounts_only() {
        let own = TrackerAccounts::project(&repo(), &LINEAR_SECRETS);
        assert_eq!(
            shared_notice(&repo(), Some(&own), &ForgeAccount::project(&repo())),
            None
        );
        let notice =
            shared_notice(&repo(), Some(&own), &ForgeAccount::shared()).expect("a shared token");
        assert!(
            notice.starts_with("github.com/Pitchopp/Owlshift has no"),
            "{notice}"
        );
        assert!(
            notice.contains("the GitHub token (account `github`)"),
            "{notice}"
        );
        assert!(!notice.contains("Linear"), "{notice}");
        let notice = shared_notice(
            &repo(),
            Some(&TrackerAccounts::shared(&LINEAR_SECRETS)),
            &ForgeAccount::shared(),
        )
        .unwrap();
        assert!(
            notice.contains("accounts `linear`, `linear-app-client-id` and `linear-app-client-secret`) and the GitHub token"),
            "{notice}"
        );
        assert!(
            notice.contains("using the Linear key and app (accounts `linear`, "),
            "{notice}"
        );
        assert!(
            notice.contains("`owlshift init --replace-secrets`"),
            "{notice}"
        );
    }

    /// OWL-544: a set that is not Linear's is resolved and noticed by the
    /// same rule, through the kind-neutral API alone.
    #[test]
    fn a_set_of_one_entry_is_resolved_and_noticed() {
        static ACME: SecretSet = SecretSet {
            name: "the Acme token",
            entries: &["acme"],
        };
        let own = TrackerAccounts::project(&repo(), &ACME);
        assert_eq!(own.of("acme"), "github.com/pitchopp/owlshift/acme");
        let found = |stored: &[&str]| {
            let mut asked = Vec::new();
            let mut look = |account: &str| {
                asked.push(account.to_owned());
                Ok(stored.contains(&account).then_some(()))
            };
            (tracker(&repo(), &ACME, &mut look).unwrap(), asked)
        };

        let (mine, asked) = found(&["github.com/pitchopp/owlshift/acme", "acme"]);
        assert_eq!(mine.accounts, own);
        assert_eq!(mine.get("acme"), Some(&()));
        assert_eq!(asked, ["github.com/pitchopp/owlshift/acme"]);

        let (shared, asked) = found(&["acme"]);
        assert_eq!(shared.accounts, TrackerAccounts::shared(&ACME));
        assert!(shared.any());
        assert_eq!(asked, ["github.com/pitchopp/owlshift/acme", "acme"]);
        let notice = shared_notice(
            &repo(),
            Some(&shared.accounts),
            &ForgeAccount::project(&repo()),
        )
        .unwrap();
        assert!(
            notice.contains(
                "using the Acme token (account `acme`) that every project of this machine shares"
            ),
            "{notice}"
        );

        let (none, _) = found(&[]);
        assert_eq!(none.accounts, own);
        assert!(!none.any());
    }
}
