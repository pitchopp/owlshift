//! Which keychain accounts hold a project's tracker and forge secrets
//! (OWL-308, build plan "P5 · Quality pipeline", part K).
//!
//! A project is named by its GitHub repository: each secret lives under
//! service [`SERVICE`], account `github.com/<owner>/<repo>/<name>` in lower
//! case, where `<name>` is the account every project of a machine shared
//! before: [`LINEAR_ACCOUNT`], [`LINEAR_APP_ID_ACCOUNT`],
//! [`LINEAR_APP_SECRET_ACCOUNT`] and [`GITHUB_ACCOUNT`]. Those shared
//! accounts are still read for a project that has none of its own, so an
//! existing install keeps working; Owlshift never deletes them.
//!
//! The tracker set, the Linear key and the Linear app's pair, is read whole
//! from one place, since an app belongs to one workspace: the project's
//! accounts when any of the three is stored there, otherwise the shared ones
//! when any of those is, otherwise the project's, which then hold nothing.
//! The GitHub token is resolved the same way, on its own. Each account is
//! looked up once, by a function the caller gives: a read for a command, so
//! the macOS Keychain asks once per entry, or a presence check for
//! `owlshift doctor`.

use owlshift_adapters::forge::Repo;
use owlshift_platform::keychain::SERVICE;

use crate::forge::GITHUB_ACCOUNT;
use crate::tracker::{LINEAR_ACCOUNT, LINEAR_APP_ID_ACCOUNT, LINEAR_APP_SECRET_ACCOUNT};

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

/// The keychain accounts of the tracker set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackerAccounts {
    /// The Linear API key.
    pub key: String,
    /// The Linear app's client ID.
    pub app_id: String,
    /// The Linear app's client secret.
    pub app_secret: String,
    pub source: Source,
}

impl TrackerAccounts {
    /// The project's own accounts.
    pub fn project(repo: &Repo) -> Self {
        Self {
            key: project_account(repo, LINEAR_ACCOUNT),
            app_id: project_account(repo, LINEAR_APP_ID_ACCOUNT),
            app_secret: project_account(repo, LINEAR_APP_SECRET_ACCOUNT),
            source: Source::Project,
        }
    }

    /// The accounts every project of this machine shares.
    pub fn shared() -> Self {
        Self {
            key: LINEAR_ACCOUNT.to_owned(),
            app_id: LINEAR_APP_ID_ACCOUNT.to_owned(),
            app_secret: LINEAR_APP_SECRET_ACCOUNT.to_owned(),
            source: Source::Shared,
        }
    }
}

/// The keychain account of the GitHub token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForgeAccount {
    pub token: String,
    pub source: Source,
}

impl ForgeAccount {
    /// The project's own account.
    pub fn project(repo: &Repo) -> Self {
        Self {
            token: project_account(repo, GITHUB_ACCOUNT),
            source: Source::Project,
        }
    }

    /// The account every project of this machine shares.
    pub fn shared() -> Self {
        Self {
            token: GITHUB_ACCOUNT.to_owned(),
            source: Source::Shared,
        }
    }
}

/// The tracker set, as found: `T` is the secret for a command, `()` for a
/// presence check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackerSecrets<T> {
    pub accounts: TrackerAccounts,
    pub key: Option<T>,
    pub app_id: Option<T>,
    pub app_secret: Option<T>,
}

/// The GitHub token, as found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForgeSecret<T> {
    pub account: ForgeAccount,
    pub token: Option<T>,
}

/// Looks one account up: `None` when nothing is stored there. The error
/// says why the keychain could not be read, never a value.
pub type Lookup<'a, T> = dyn FnMut(&str) -> Result<Option<T>, String> + 'a;

/// The tracker set of the project of `repo`, by the rule of this module.
pub fn tracker<T>(repo: &Repo, look: &mut Lookup<'_, T>) -> Result<TrackerSecrets<T>, String> {
    for accounts in [TrackerAccounts::project(repo), TrackerAccounts::shared()] {
        let found = TrackerSecrets {
            key: look(&accounts.key)?,
            app_id: look(&accounts.app_id)?,
            app_secret: look(&accounts.app_secret)?,
            accounts,
        };
        if found.key.is_some() || found.app_id.is_some() || found.app_secret.is_some() {
            return Ok(found);
        }
    }
    Ok(TrackerSecrets {
        accounts: TrackerAccounts::project(repo),
        key: None,
        app_id: None,
        app_secret: None,
    })
}

/// The GitHub token of the project of `repo`, by the rule of this module.
pub fn forge<T>(repo: &Repo, look: &mut Lookup<'_, T>) -> Result<ForgeSecret<T>, String> {
    for account in [ForgeAccount::project(repo), ForgeAccount::shared()] {
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
        shared.push(format!(
            "the Linear key and app (accounts `{}`, `{}` and `{}`)",
            accounts.key, accounts.app_id, accounts.app_secret
        ));
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let tracker = tracker(&repo(), &mut look).unwrap();
        let forge = forge(&repo(), &mut look).unwrap();
        (tracker, forge, asked)
    }

    #[test]
    fn a_projects_accounts_are_named_by_its_repository_in_lower_case() {
        assert_eq!(
            TrackerAccounts::project(&repo()),
            TrackerAccounts {
                key: "github.com/pitchopp/owlshift/linear".to_owned(),
                app_id: "github.com/pitchopp/owlshift/linear-app-client-id".to_owned(),
                app_secret: "github.com/pitchopp/owlshift/linear-app-client-secret".to_owned(),
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
        let own = TrackerAccounts::project(&repo());
        let token = ForgeAccount::project(&repo()).token;
        let (tracker, forge, asked) = resolve(&[
            &own.key,
            &token,
            LINEAR_ACCOUNT,
            LINEAR_APP_ID_ACCOUNT,
            LINEAR_APP_SECRET_ACCOUNT,
            GITHUB_ACCOUNT,
        ]);
        assert_eq!(tracker.accounts, own);
        assert_eq!((tracker.key, tracker.app_id), (Some(()), None));
        assert_eq!(forge.account.source, Source::Project);
        // No shared account is even looked up.
        assert_eq!(asked, [own.key, own.app_id, own.app_secret, token]);
    }

    #[test]
    fn a_project_with_none_of_its_own_reads_the_shared_accounts() {
        let (tracker, forge, _) = resolve(&[LINEAR_ACCOUNT, LINEAR_APP_ID_ACCOUNT, GITHUB_ACCOUNT]);
        assert_eq!(tracker.accounts, TrackerAccounts::shared());
        assert_eq!(tracker.key, Some(()));
        assert_eq!(forge.account, ForgeAccount::shared());
        assert_eq!(forge.token, Some(()));
    }

    /// The tracker set is never mixed: a project's own app without its own
    /// key keeps the project's set, key missing, and a project's own key
    /// never goes with the shared app. The forge token is resolved apart.
    #[test]
    fn the_tracker_set_is_never_mixed() {
        let own = TrackerAccounts::project(&repo());
        let (tracker, forge, _) = resolve(&[
            &own.app_id,
            LINEAR_ACCOUNT,
            LINEAR_APP_ID_ACCOUNT,
            LINEAR_APP_SECRET_ACCOUNT,
            GITHUB_ACCOUNT,
        ]);
        assert_eq!(tracker.accounts, own);
        assert_eq!(tracker.key, None);
        assert_eq!(forge.account.source, Source::Shared);

        let (tracker, _, _) =
            resolve(&[&own.key, LINEAR_APP_ID_ACCOUNT, LINEAR_APP_SECRET_ACCOUNT]);
        assert_eq!(tracker.accounts, own);
        assert_eq!((tracker.app_id, tracker.app_secret), (None, None));
    }

    #[test]
    fn with_nothing_stored_the_projects_accounts_are_named() {
        let (tracker, forge, _) = resolve(&[]);
        assert_eq!(tracker.accounts, TrackerAccounts::project(&repo()));
        assert_eq!(tracker.key, None);
        assert_eq!(forge.account, ForgeAccount::project(&repo()));
        assert_eq!(forge.token, None);
    }

    #[test]
    fn a_keychain_that_cannot_be_read_stops_the_resolution() {
        let mut look = |_: &str| -> Result<Option<()>, String> { Err("locked".to_owned()) };
        assert_eq!(tracker(&repo(), &mut look).unwrap_err(), "locked");
        assert_eq!(forge(&repo(), &mut look).unwrap_err(), "locked");
    }

    #[test]
    fn the_notice_names_the_shared_accounts_only() {
        let own = TrackerAccounts::project(&repo());
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
            Some(&TrackerAccounts::shared()),
            &ForgeAccount::shared(),
        )
        .unwrap();
        assert!(
            notice.contains("accounts `linear`, `linear-app-client-id` and `linear-app-client-secret`) and the GitHub token"),
            "{notice}"
        );
        assert!(
            notice.contains("`owlshift init --replace-secrets`"),
            "{notice}"
        );
    }
}
