//! The system keychain, where tracker and forge secrets live: the Keychain on
//! macOS, the Credential Manager on Windows, the Secret Service on Linux.
//! Owlshift never writes such a secret to a file.
//!
//! Every Owlshift entry is filed under the service [`SERVICE`] and named by
//! its account, such as `linear`. Access goes through `keyring-core` with one
//! store crate per platform, as the `keyring` project advises applications
//! to do (OWL-13, 2026-09-28). Tests use [`Keychain::in_memory`], so no test
//! needs a keychain entry, or a keychain at all.

use std::fmt;
use std::sync::Arc;

use keyring_core::{CredentialStore, Error as StoreError};

/// The keychain service under which Owlshift files its entries.
pub const SERVICE: &str = "owlshift";

/// A secret value. It never shows in `Debug` output, so it cannot leak into a
/// log or an error message by accident.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The value, for the one call that needs it.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether the value is one word: not empty, with no space and no
    /// control character. Every secret Owlshift keeps is a key or a token;
    /// one holding a line break or a space was pasted wrong.
    pub fn is_one_word(&self) -> bool {
        !self.0.is_empty() && !self.0.chars().any(|c| c.is_whitespace() || c.is_control())
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// A keychain that could not be opened, read or written. The message never
/// carries a stored value.
#[derive(Debug)]
pub struct KeychainError {
    message: String,
}

impl fmt::Display for KeychainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "keychain: {}", self.message)
    }
}

impl std::error::Error for KeychainError {}

impl From<StoreError> for KeychainError {
    fn from(error: StoreError) -> Self {
        // Two variants carry the stored bytes: name the problem only.
        let message = match error {
            StoreError::BadEncoding(_) => "the stored value is not UTF-8 text".to_owned(),
            StoreError::BadDataFormat(..) => "the stored value is malformed".to_owned(),
            StoreError::Ambiguous(entries) => {
                format!("{} entries match; remove all but one", entries.len())
            }
            other => other.to_string(),
        };
        Self { message }
    }
}

/// The entries Owlshift keeps in a credential store.
pub struct Keychain {
    store: Arc<CredentialStore>,
}

impl fmt::Debug for Keychain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Keychain")
            .field("store", &self.store.vendor())
            .finish()
    }
}

impl Keychain {
    /// The system keychain. On Linux this fails when no Secret Service is
    /// running, as on a headless machine without a desktop session.
    pub fn system() -> Result<Self, KeychainError> {
        #[cfg(target_os = "macos")]
        let store: Arc<CredentialStore> = apple_native_keyring_store::keychain::Store::new()?;
        #[cfg(windows)]
        let store: Arc<CredentialStore> = windows_native_keyring_store::Store::new()?;
        #[cfg(all(unix, not(target_os = "macos")))]
        let store: Arc<CredentialStore> = zbus_secret_service_keyring_store::Store::new()?;
        Ok(Self { store })
    }

    /// A keychain held in memory and dropped with it, for tests.
    pub fn in_memory() -> Self {
        let store: Arc<CredentialStore> =
            keyring_core::mock::Store::new().expect("the in-memory store always opens");
        Self { store }
    }

    /// The secret stored for `account`, or `None` when there is none.
    pub fn read(&self, account: &str) -> Result<Option<Secret>, KeychainError> {
        match self.entry(account)?.get_password() {
            Ok(value) => Ok(Some(Secret(value))),
            Err(StoreError::NoEntry) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Whether a secret is stored for `account`, for a check that needs its
    /// presence alone, such as `owlshift doctor`'s: the value is never
    /// returned. The store may still read the entry to answer, as the macOS
    /// Keychain does, so the system may ask to allow the access, as it does
    /// for [`Keychain::read`].
    pub fn contains(&self, account: &str) -> Result<bool, KeychainError> {
        match self.entry(account)?.get_credential() {
            Ok(_) => Ok(true),
            Err(StoreError::NoEntry) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    /// Stores a secret for `account`, replacing any previous one.
    pub fn store(&self, account: &str, secret: &Secret) -> Result<(), KeychainError> {
        Ok(self.entry(account)?.set_password(secret.expose())?)
    }

    /// Removes the secret stored for `account`; `false` when there was none.
    pub fn delete(&self, account: &str) -> Result<bool, KeychainError> {
        match self.entry(account)?.delete_credential() {
            Ok(()) => Ok(true),
            Err(StoreError::NoEntry) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    fn entry(&self, account: &str) -> Result<keyring_core::Entry, KeychainError> {
        Ok(self.store.build(SERVICE, account, None)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entry_is_stored_read_replaced_and_deleted() {
        let keychain = Keychain::in_memory();
        assert_eq!(keychain.read("linear").unwrap(), None);
        assert!(!keychain.contains("linear").unwrap());
        assert!(!keychain.delete("linear").unwrap());

        keychain.store("linear", &Secret::new("first")).unwrap();
        assert!(keychain.contains("linear").unwrap());
        // An account is matched whole, not as part of another's name.
        assert!(!keychain.contains("line").unwrap());
        keychain.store("linear", &Secret::new("second")).unwrap();
        assert_eq!(
            keychain.read("linear").unwrap(),
            Some(Secret::new("second"))
        );
        assert_eq!(keychain.read("github").unwrap(), None);

        assert!(keychain.delete("linear").unwrap());
        assert_eq!(keychain.read("linear").unwrap(), None);
        assert!(!keychain.contains("linear").unwrap());
    }

    #[test]
    fn a_secret_is_one_word() {
        assert!(Secret::new("sk-ant-oat01-a_b").is_one_word());
        for pasted_wrong in ["", "a b", "a\nb", "a\tb", "a\u{0}b", "a\u{a0}b"] {
            assert!(!Secret::new(pasted_wrong).is_one_word(), "{pasted_wrong:?}");
        }
    }

    #[test]
    fn neither_a_secret_nor_a_store_error_shows_the_value() {
        let secret = Secret::new("lin_api_do_not_print");
        assert_eq!(format!("{secret:?}"), "Secret(<redacted>)");
        for error in [
            StoreError::BadEncoding(b"lin_api_do_not_print".to_vec()),
            StoreError::BadDataFormat(b"lin_api_do_not_print".to_vec(), "bad".into()),
        ] {
            let error = KeychainError::from(error);
            assert!(!format!("{error} {error:?}").contains("do_not_print"));
        }
    }

    /// A round trip through the real system keychain. Ignored by default:
    /// it needs a keychain, which a CI machine may not have. Run it with
    /// `cargo test -p owlshift-platform -- --ignored system_keychain`.
    #[test]
    #[ignore = "touches the system keychain"]
    fn system_keychain_round_trip() {
        let keychain = Keychain::system().unwrap();
        let account = format!("live-check-{}", std::process::id());
        keychain.delete(&account).unwrap();
        assert!(!keychain.contains(&account).unwrap());
        keychain.store(&account, &Secret::new("dummy")).unwrap();
        assert!(keychain.contains(&account).unwrap());
        assert_eq!(keychain.read(&account).unwrap(), Some(Secret::new("dummy")));
        assert!(keychain.delete(&account).unwrap());
        assert_eq!(keychain.read(&account).unwrap(), None);
    }
}
