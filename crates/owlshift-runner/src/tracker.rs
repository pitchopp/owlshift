//! Opening the project's tracker adapter with its credentials.
//!
//! The runner is the only holder of tracker credentials: it reads them from
//! the system keychain and hands them to the adapter, never to an agent
//! (architecture section 8).

use owlshift_adapters::tracker;
use owlshift_adapters::tracker::linear::{ApiKey, LinearTracker};
use owlshift_platform::keychain::{Keychain, SERVICE};

/// The keychain account, under [`SERVICE`], that holds the Linear API key:
/// one key per machine for now.
pub const LINEAR_ACCOUNT: &str = "linear";

/// The Linear adapter, with its API key read from `keychain`: `owlshift do`
/// reads the ticket with it and hands it to the [writer](crate::writer);
/// `owlshift init` stores the key.
pub fn linear(keychain: &Keychain) -> Result<LinearTracker, String> {
    match read_key(keychain)? {
        Some(key) => Ok(LinearTracker::new(key)),
        None => Err(format!(
            "no Linear API key in the system keychain: run `owlshift init` in a terminal, or \
             store one under service `{SERVICE}`, account `{LINEAR_ACCOUNT}`"
        )),
    }
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
        let missing = linear(&keychain).unwrap_err();
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
        assert!(linear(&keychain).is_ok());
    }
}
