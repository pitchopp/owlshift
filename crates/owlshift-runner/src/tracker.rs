//! Opening the project's tracker adapter with its credentials.
//!
//! The runner is the only holder of tracker credentials: it reads them from
//! the system keychain and hands them to the adapter, never to an agent
//! (architecture section 8).

use owlshift_adapters::tracker::linear::{ApiKey, LinearTracker};
use owlshift_platform::keychain::{Keychain, SERVICE};

/// The keychain account, under [`SERVICE`], that holds the Linear API key:
/// one key per machine for now.
pub const LINEAR_ACCOUNT: &str = "linear";

/// The Linear adapter, with its API key read from `keychain`: `owlshift do`
/// reads the ticket with it and hands it to the [writer](crate::writer);
/// `owlshift init` stores the key.
pub fn linear(keychain: &Keychain) -> Result<LinearTracker, String> {
    match keychain.read(LINEAR_ACCOUNT) {
        Ok(Some(key)) => Ok(LinearTracker::new(ApiKey::new(key.expose()))),
        Ok(None) => Err(format!(
            "no Linear API key in the system keychain: run `owlshift init` in a terminal, or \
             store one under service `{SERVICE}`, account `{LINEAR_ACCOUNT}`"
        )),
        Err(error) => Err(error.to_string()),
    }
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

        keychain
            .store(LINEAR_ACCOUNT, &Secret::new("lin_api_test"))
            .unwrap();
        assert!(linear(&keychain).is_ok());
    }
}
