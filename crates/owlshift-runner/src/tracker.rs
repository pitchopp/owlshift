//! Opening the project's tracker adapter with its credentials.
//!
//! The runner is the only holder of tracker credentials: it reads them from
//! the system keychain and hands them to the adapter, never to an agent
//! (architecture section 8). The token of the Linear app lives in the
//! runner's memory alone, for one command (OWL-157). Which accounts hold a
//! project's credentials is [`crate::secrets`]'s to say (OWL-308).

use std::fmt;

use owlshift_adapters::tracker;
use owlshift_adapters::tracker::linear::app::{AppTransport, ClientCredentials, HttpOAuth};
use owlshift_adapters::tracker::linear::{AdmissionFacts, ApiKey, AppUser, LinearTracker};
use owlshift_platform::keychain::{Keychain, SERVICE, Secret};

use crate::secrets::{Source, TrackerAccounts, TrackerSecrets};

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
pub fn linear(secrets: &TrackerSecrets<Secret>, team: &str) -> Result<LinearTracker, String> {
    let Some(key) = &secrets.key else {
        return Err(format!(
            "no Linear API key in the system keychain: run `owlshift init` in a terminal, or \
             store one under service `{SERVICE}`, account `{}`",
            secrets.accounts.key
        ));
    };
    let tracker = LinearTracker::new(ApiKey::new(key.expose()));
    match app_credentials(secrets).map_err(|e| e.to_string())? {
        None => Ok(tracker),
        Some(credentials) => open_app(tracker, credentials, &secrets.accounts, team)
            .map(|(tracker, _)| tracker)
            .map_err(|e| e.to_string()),
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
                        accounts.app_id, accounts.app_secret
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
    match (&secrets.app_id, &secrets.app_secret) {
        (None, None) => Ok(None),
        (Some(id), Some(secret)) => Ok(Some(ClientCredentials::new(id.expose(), secret.expose()))),
        (Some(_), None) => Err(incomplete(&accounts.app_secret)),
        (None, Some(_)) => Err(incomplete(&accounts.app_id)),
    }
}

/// The tracker set stored under `accounts`, each entry read once.
pub fn read(
    keychain: &Keychain,
    accounts: &TrackerAccounts,
) -> Result<TrackerSecrets<Secret>, String> {
    let read = |account: &str| keychain.read(account).map_err(|error| error.to_string());
    Ok(TrackerSecrets {
        key: read(&accounts.key)?,
        app_id: read(&accounts.app_id)?,
        app_secret: read(&accounts.app_secret)?,
        accounts: accounts.clone(),
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
    let key = secrets.key.ok_or_else(|| AppError::NoKey {
        account: accounts.key.clone(),
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

fn read_key(keychain: &Keychain, accounts: &TrackerAccounts) -> Result<Option<ApiKey>, String> {
    keychain
        .read(&accounts.key)
        .map(|key| key.map(|key| ApiKey::new(key.expose())))
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use owlshift_adapters::forge::Repo;
    use owlshift_adapters::tracker::ErrorKind;

    use super::*;

    fn own() -> TrackerAccounts {
        TrackerAccounts::project(&Repo::parse("pitchopp/owlshift").unwrap())
    }

    #[test]
    fn the_linear_key_comes_from_the_accounts_given() {
        let keychain = Keychain::in_memory();
        let missing = linear(&read(&keychain, &own()).unwrap(), "OWL").unwrap_err();
        assert!(
            missing.contains("service `owlshift`, account `github.com/pitchopp/owlshift/linear`"),
            "{missing}"
        );
        assert_eq!(
            linear_team_states(&keychain, &own(), "OWL"),
            Err(StatesError::NoKey)
        );

        keychain
            .store(&own().key, &Secret::new("lin_api_test"))
            .unwrap();
        assert!(linear(&read(&keychain, &own()).unwrap(), "OWL").is_ok());
        // The shared account is another entry.
        assert_eq!(
            read(&keychain, &TrackerAccounts::shared()).unwrap().key,
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
            .store(&accounts.app_id, &Secret::new(SENTINEL))
            .unwrap();
        assert_eq!(
            app_credentials(&set()).unwrap_err(),
            AppError::Incomplete {
                missing: accounts.app_secret.clone(),
                source: Source::Project,
            }
        );
        assert_eq!(
            linear_app_user(&keychain, &accounts, "OWL"),
            Err(AppError::Incomplete {
                missing: accounts.app_secret.clone(),
                source: Source::Project,
            })
        );
        keychain
            .store(&accounts.key, &Secret::new("lin_api_test"))
            .unwrap();
        let refused = linear(&set(), "OWL").unwrap_err();
        assert!(
            refused.contains("`github.com/pitchopp/owlshift/linear-app-client-secret`"),
            "{refused}"
        );
        assert!(!refused.contains("SENTINEL"), "{refused}");

        keychain.delete(&accounts.app_id).unwrap();
        keychain
            .store(&accounts.app_secret, &Secret::new(SENTINEL))
            .unwrap();
        assert_eq!(
            app_credentials(&set()).unwrap_err(),
            AppError::Incomplete {
                missing: accounts.app_id.clone(),
                source: Source::Project,
            }
        );
        keychain
            .store(&accounts.app_id, &Secret::new(SENTINEL))
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
                accounts: TrackerAccounts::shared(),
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
