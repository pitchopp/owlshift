//! Opening the project's tracker adapter with its credentials.
//!
//! The runner is the only holder of tracker credentials: it reads them from
//! the system keychain and hands them to the adapter, never to an agent
//! (architecture section 8). The token of the Linear app lives in the
//! runner's memory alone, for one command (OWL-157).

use std::fmt;

use owlshift_adapters::tracker;
use owlshift_adapters::tracker::linear::app::{AppTransport, ClientCredentials, HttpOAuth};
use owlshift_adapters::tracker::linear::{ApiKey, AppUser, LinearTracker};
use owlshift_platform::keychain::{Keychain, SERVICE};

/// The keychain account, under [`SERVICE`], that holds the Linear API key:
/// one key per machine for now.
pub const LINEAR_ACCOUNT: &str = "linear";

/// The keychain account of the client ID of the Linear OAuth app Owlshift
/// writes as (decision D8, OWL-157). Optional, and stored with
/// [`LINEAR_APP_SECRET_ACCOUNT`] or not at all.
pub const LINEAR_APP_ID_ACCOUNT: &str = "linear-app-client-id";

/// The keychain account of that app's client secret.
pub const LINEAR_APP_SECRET_ACCOUNT: &str = "linear-app-client-secret";

/// The Linear adapter, with its API key read from `keychain`: `owlshift do`
/// reads the ticket with it and hands it to the [writer](crate::writer);
/// `owlshift init` stores the key.
///
/// When the Linear app's client ID and secret are stored too, the adapter
/// requests the app's token now, before any work, and writes through it,
/// checked to act as an app user of the key's workspace that sees team
/// `team`. Anything that keeps the app from being used refuses: Owlshift
/// never falls back to the key, which would keep Linear from notifying a
/// decider who holds it. The token is revoked when the adapter is dropped.
pub fn linear(keychain: &Keychain, team: &str) -> Result<LinearTracker, String> {
    let Some(key) = read_key(keychain)? else {
        return Err(format!(
            "no Linear API key in the system keychain: run `owlshift init` in a terminal, or \
             store one under service `{SERVICE}`, account `{LINEAR_ACCOUNT}`"
        ));
    };
    let tracker = LinearTracker::new(key);
    match app_credentials(keychain).map_err(|e| e.to_string())? {
        None => Ok(tracker),
        Some(credentials) => open_app(tracker, credentials, team)
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
    /// missing.
    Incomplete { missing: &'static str },
    /// No Linear API key is stored, which reads and the app's check need.
    NoKey,
    /// Linear refused the app, or did not answer.
    Tracker(tracker::Error),
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Keychain(reason) => write!(
                f,
                "could not read the Linear app's credentials in the system keychain: {reason}"
            ),
            Self::Incomplete { missing } => write!(
                f,
                "only half of the Linear app's credentials is stored: nothing under service \
                 `{SERVICE}`, account `{missing}`. Run `owlshift init` to store its client ID \
                 and secret together"
            ),
            Self::NoKey => write!(
                f,
                "no Linear API key in the system keychain (service `{SERVICE}`, account \
                 `{LINEAR_ACCOUNT}`): run `owlshift init`"
            ),
            Self::Tracker(error) => write!(
                f,
                "the Linear app Owlshift writes as cannot be used: {error}. Owlshift does not \
                 write through the API key instead, so that its questions keep reaching the \
                 decider: fix the app, run `owlshift init --replace-secrets` to store other \
                 credentials, or delete both of the app's keychain entries (service \
                 `{SERVICE}`, accounts `{LINEAR_APP_ID_ACCOUNT}` and \
                 `{LINEAR_APP_SECRET_ACCOUNT}`) to write through the API key"
            ),
        }
    }
}

/// The Linear app's client ID and secret: `None` when neither is stored,
/// [`AppError::Incomplete`] when only one is. Each is read once.
pub fn app_credentials(keychain: &Keychain) -> Result<Option<ClientCredentials>, AppError> {
    let read = |account| {
        keychain
            .read(account)
            .map_err(|error| AppError::Keychain(error.to_string()))
    };
    match (
        read(LINEAR_APP_ID_ACCOUNT)?,
        read(LINEAR_APP_SECRET_ACCOUNT)?,
    ) {
        (None, None) => Ok(None),
        (Some(id), Some(secret)) => Ok(Some(ClientCredentials::new(id.expose(), secret.expose()))),
        (Some(_), None) => Err(AppError::Incomplete {
            missing: LINEAR_APP_SECRET_ACCOUNT,
        }),
        (None, Some(_)) => Err(AppError::Incomplete {
            missing: LINEAR_APP_ID_ACCOUNT,
        }),
    }
}

/// The app user the Linear adapter writes as, checked exactly as
/// [`linear`] checks it: `None` when no app is stored. Its token is revoked
/// before this returns. What `owlshift doctor` reports (OWL-157).
pub fn linear_app_user(keychain: &Keychain, team: &str) -> Result<Option<AppUser>, AppError> {
    let Some(credentials) = app_credentials(keychain)? else {
        return Ok(None);
    };
    let key = read_key(keychain)
        .map_err(AppError::Keychain)?
        .ok_or(AppError::NoKey)?;
    open_app(LinearTracker::new(key), credentials, team).map(|(_, user)| Some(user))
}

/// Requests the app's token, then checks it against the key's workspace and
/// the team.
fn open_app(
    tracker: LinearTracker,
    credentials: ClientCredentials,
    team: &str,
) -> Result<(LinearTracker, AppUser), AppError> {
    let app = AppTransport::connect(HttpOAuth::new(credentials)).map_err(AppError::Tracker)?;
    tracker.writing_as_app(app, team).map_err(AppError::Tracker)
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
/// the key in `keychain` (OWL-147). The key is read once: on macOS each read
/// may ask to allow the access.
pub fn linear_team_states(keychain: &Keychain, team: &str) -> Result<Vec<String>, StatesError> {
    match read_key(keychain) {
        Ok(Some(key)) => LinearTracker::new(key)
            .team_states(team)
            .map_err(StatesError::Tracker),
        Ok(None) => Err(StatesError::NoKey),
        Err(reason) => Err(StatesError::Keychain(reason)),
    }
}

fn read_key(keychain: &Keychain) -> Result<Option<ApiKey>, String> {
    keychain
        .read(LINEAR_ACCOUNT)
        .map(|key| key.map(|key| ApiKey::new(key.expose())))
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use owlshift_platform::keychain::Secret;

    use super::*;

    #[test]
    fn the_linear_key_comes_from_the_keychain() {
        let keychain = Keychain::in_memory();
        let missing = linear(&keychain, "OWL").unwrap_err();
        assert!(
            missing.contains("service `owlshift`, account `linear`"),
            "{missing}"
        );
        assert_eq!(
            linear_team_states(&keychain, "OWL"),
            Err(StatesError::NoKey)
        );

        keychain
            .store(LINEAR_ACCOUNT, &Secret::new("lin_api_test"))
            .unwrap();
        assert!(linear(&keychain, "OWL").is_ok());
    }

    /// The app is the pair or nothing: half of it is refused, naming the
    /// entry missing, and no stored value shows in a message.
    #[test]
    fn the_apps_credentials_are_both_stored_or_neither() {
        const SENTINEL: &str = "SENTINEL_never_shown";
        let keychain = Keychain::in_memory();
        assert!(app_credentials(&keychain).unwrap().is_none());
        assert_eq!(linear_app_user(&keychain, "OWL"), Ok(None));

        keychain
            .store(LINEAR_APP_ID_ACCOUNT, &Secret::new(SENTINEL))
            .unwrap();
        let half = app_credentials(&keychain).unwrap_err();
        assert_eq!(
            half,
            AppError::Incomplete {
                missing: LINEAR_APP_SECRET_ACCOUNT
            }
        );
        keychain
            .store(LINEAR_ACCOUNT, &Secret::new("lin_api_test"))
            .unwrap();
        let refused = linear(&keychain, "OWL").unwrap_err();
        assert!(refused.contains("`linear-app-client-secret`"), "{refused}");
        assert!(!refused.contains("SENTINEL"), "{refused}");

        keychain.delete(LINEAR_APP_ID_ACCOUNT).unwrap();
        keychain
            .store(LINEAR_APP_SECRET_ACCOUNT, &Secret::new(SENTINEL))
            .unwrap();
        assert_eq!(
            app_credentials(&keychain).unwrap_err(),
            AppError::Incomplete {
                missing: LINEAR_APP_ID_ACCOUNT
            }
        );
        keychain
            .store(LINEAR_APP_ID_ACCOUNT, &Secret::new(SENTINEL))
            .unwrap();
        let both = format!("{:?}", app_credentials(&keychain).unwrap().unwrap());
        assert!(!both.contains("SENTINEL"), "{both}");
    }
}
