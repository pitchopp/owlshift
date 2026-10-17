//! The system keychain, where tracker and forge secrets live: the Keychain on
//! macOS, the Credential Manager on Windows, the Secret Service on Linux.
//! Owlshift never writes such a secret to a file.
//!
//! Every Owlshift entry is filed under the service [`SERVICE`] and named by
//! its account, such as `github.com/pitchopp/owlshift/linear` for a
//! project's Linear key (OWL-308) or `claude-agent` for one kept per
//! machine. On Windows and Linux access goes through `keyring-core` with one
//! store crate per platform, as the `keyring` project advises applications
//! to do (OWL-13, 2026-09-28). On macOS it goes through `/usr/bin/security`
//! (OWL-335): the Keychain trusts an entry's reader by its code, and a
//! binary `cargo` rebuilt is new code, so every build of Owlshift asked again
//! for each entry; `security` is signed by Apple and stays the same, so one
//! "Always Allow" holds. Tests use [`Keychain::in_memory`], so no test needs
//! a keychain entry, or a keychain at all.

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
    backend: Backend,
}

enum Backend {
    Store(Arc<CredentialStore>),
    #[cfg(target_os = "macos")]
    SecurityTool,
}

impl fmt::Debug for Keychain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let store = match &self.backend {
            Backend::Store(store) => store.vendor(),
            #[cfg(target_os = "macos")]
            Backend::SecurityTool => security_tool::PROGRAM.to_owned(),
        };
        f.debug_struct("Keychain").field("store", &store).finish()
    }
}

impl Keychain {
    /// The system keychain. On Linux this fails when no Secret Service is
    /// running, as on a headless machine without a desktop session.
    pub fn system() -> Result<Self, KeychainError> {
        #[cfg(target_os = "macos")]
        let backend = Backend::SecurityTool;
        #[cfg(windows)]
        let backend = Backend::Store(windows_native_keyring_store::Store::new()?);
        #[cfg(all(unix, not(target_os = "macos")))]
        let backend = Backend::Store(zbus_secret_service_keyring_store::Store::new()?);
        Ok(Self { backend })
    }

    /// A keychain held in memory and dropped with it, for tests.
    pub fn in_memory() -> Self {
        let store: Arc<CredentialStore> =
            keyring_core::mock::Store::new().expect("the in-memory store always opens");
        Self {
            backend: Backend::Store(store),
        }
    }

    /// The secret stored for `account`, or `None` when there is none.
    pub fn read(&self, account: &str) -> Result<Option<Secret>, KeychainError> {
        #[cfg(target_os = "macos")]
        if let Backend::SecurityTool = self.backend {
            return security_tool::read(account);
        }
        match self.entry(account)?.get_password() {
            Ok(value) => Ok(Some(Secret(value))),
            Err(StoreError::NoEntry) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Whether a secret is stored for `account`, for a check that needs its
    /// presence alone, such as `owlshift doctor`'s: the value is never
    /// returned. On macOS only the entry's attributes are read, which the
    /// Keychain never asks about; another store may read the entry to answer,
    /// so the system may ask to allow the access, as it does for
    /// [`Keychain::read`].
    pub fn contains(&self, account: &str) -> Result<bool, KeychainError> {
        #[cfg(target_os = "macos")]
        if let Backend::SecurityTool = self.backend {
            return security_tool::contains(account);
        }
        match self.entry(account)?.get_credential() {
            Ok(_) => Ok(true),
            Err(StoreError::NoEntry) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    /// Stores a secret for `account`, replacing any previous one.
    pub fn store(&self, account: &str, secret: &Secret) -> Result<(), KeychainError> {
        #[cfg(target_os = "macos")]
        if let Backend::SecurityTool = self.backend {
            return security_tool::store(account, secret);
        }
        Ok(self.entry(account)?.set_password(secret.expose())?)
    }

    /// Removes the secret stored for `account`; `false` when there was none.
    pub fn delete(&self, account: &str) -> Result<bool, KeychainError> {
        #[cfg(target_os = "macos")]
        if let Backend::SecurityTool = self.backend {
            return security_tool::delete(account);
        }
        match self.entry(account)?.delete_credential() {
            Ok(()) => Ok(true),
            Err(StoreError::NoEntry) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    fn entry(&self, account: &str) -> Result<keyring_core::Entry, KeychainError> {
        match &self.backend {
            Backend::Store(store) => Ok(store.build(SERVICE, account, None)?),
            #[cfg(target_os = "macos")]
            Backend::SecurityTool => unreachable!("the macOS Keychain has no keyring-core store"),
        }
    }
}

/// The macOS Keychain through Apple's `security` program (OWL-335), as `gh`
/// reaches it. Checked live on 2026-10-17 (build plan, C4): two different
/// ad-hoc signed binaries stored, read and replaced one entry through it with
/// no prompt. The trade-off: any process of the user's that is not confined
/// can run `security` and read Owlshift's entries without a prompt, as it can
/// `gh`'s; confined agent runs cannot, the sandbox closes the Keychain.
#[cfg(target_os = "macos")]
mod security_tool {
    use std::io::Write;
    use std::process::{Command, Output, Stdio};

    use super::{KeychainError, SERVICE, Secret};

    /// An absolute path, so no program earlier on the `PATH` stands in.
    pub const PROGRAM: &str = "/usr/bin/security";

    /// `security`'s exit status when no entry matches (`errSecItemNotFound`,
    /// truncated to a byte).
    const NOT_FOUND: i32 = 44;

    pub fn read(account: &str) -> Result<Option<Secret>, KeychainError> {
        let output = run(&["find-generic-password", "-s", SERVICE, "-a", account, "-w"])?;
        match output.status.code() {
            Some(0) => {
                let mut value = String::from_utf8(output.stdout)
                    .map_err(|_| error("the stored value is not UTF-8 text"))?;
                if value.ends_with('\n') {
                    value.pop();
                }
                Ok(Some(Secret::new(value)))
            }
            Some(NOT_FOUND) => Ok(None),
            _ => Err(failed("find-generic-password", &output)),
        }
    }

    pub fn contains(account: &str) -> Result<bool, KeychainError> {
        // No `-w`: the attributes alone, which need no access to the secret.
        let output = run(&["find-generic-password", "-s", SERVICE, "-a", account])?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(NOT_FOUND) => Ok(false),
            _ => Err(failed("find-generic-password", &output)),
        }
    }

    pub fn delete(account: &str) -> Result<bool, KeychainError> {
        let output = run(&["delete-generic-password", "-s", SERVICE, "-a", account])?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(NOT_FOUND) => Ok(false),
            _ => Err(failed("delete-generic-password", &output)),
        }
    }

    /// Writes the secret through `security -i` on standard input: an
    /// argument would show in `ps` while the program runs.
    pub fn store(account: &str, secret: &Secret) -> Result<(), KeychainError> {
        let command = add_command(account, secret)?;
        let mut child = Command::new(PROGRAM)
            .arg("-i")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| error(&format!("cannot run {PROGRAM}: {e}")))?;
        let written = child
            .stdin
            .take()
            .expect("standard input is piped")
            .write_all(command.as_bytes());
        let output = child
            .wait_with_output()
            .map_err(|e| error(&format!("cannot run {PROGRAM}: {e}")))?;
        written.map_err(|e| error(&format!("cannot write to {PROGRAM}: {e}")))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(failed("add-generic-password", &output))
        }
    }

    /// The one `security -i` line that stores `secret` for `account`,
    /// replacing any previous value. A line break would end the line and
    /// start another command, so a value holding a control character is
    /// refused; no key or token holds one.
    pub(super) fn add_command(account: &str, secret: &Secret) -> Result<String, KeychainError> {
        if account.chars().any(char::is_control) {
            return Err(error("an account name holds a control character"));
        }
        if secret.expose().chars().any(char::is_control) {
            return Err(error("the secret holds a control character"));
        }
        Ok(format!(
            "add-generic-password -U -s {} -a {} -w {}\n",
            quoted(SERVICE),
            quoted(account),
            quoted(secret.expose())
        ))
    }

    /// `value` in double quotes, as `security -i` reads it: a backslash or a
    /// double quote inside is escaped with a backslash.
    pub(super) fn quoted(value: &str) -> String {
        let mut out = String::with_capacity(value.len() + 2);
        out.push('"');
        for c in value.chars() {
            if c == '\\' || c == '"' {
                out.push('\\');
            }
            out.push(c);
        }
        out.push('"');
        out
    }

    fn run(args: &[&str]) -> Result<Output, KeychainError> {
        Command::new(PROGRAM)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| error(&format!("cannot run {PROGRAM}: {e}")))
    }

    /// A failed command, named with its status and `security`'s own
    /// message, which never holds a stored value; standard output, which
    /// may, is left out.
    fn failed(command: &str, output: &Output) -> KeychainError {
        let stderr = String::from_utf8_lossy(&output.stderr);
        error(&format!(
            "`security {command}` failed ({}): {}",
            output.status,
            stderr.trim()
        ))
    }

    fn error(message: &str) -> KeychainError {
        KeychainError {
            message: message.to_owned(),
        }
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

    #[cfg(target_os = "macos")]
    #[test]
    fn a_security_tool_line_quotes_and_refuses() {
        use security_tool::{add_command, quoted};
        assert_eq!(quoted(r#"a"b\c d"#), r#""a\"b\\c d""#);
        assert_eq!(
            add_command("github.com/o/r/linear", &Secret::new("lin_api_1")).unwrap(),
            "add-generic-password -U -s \"owlshift\" -a \"github.com/o/r/linear\" -w \"lin_api_1\"\n"
        );
        // A line break would start a second command.
        for (account, value) in [("linear", "a\nb"), ("linear", "a\u{0}b"), ("li\nnear", "a")] {
            let error = add_command(account, &Secret::new(value)).unwrap_err();
            assert!(error.to_string().contains("control character"), "{error}");
        }
        // Refused before `security` runs: nothing reaches the Keychain.
        let keychain = Keychain::system().unwrap();
        assert!(keychain.store("linear", &Secret::new("a\nb")).is_err());
    }

    /// A round trip through the real system keychain. Ignored by default:
    /// it needs a keychain, which a CI machine may not have. Run it with
    /// `cargo test -p owlshift-platform -- --ignored system_keychain`.
    #[test]
    #[ignore = "touches the system keychain"]
    fn system_keychain_round_trip() {
        let keychain = Keychain::system().unwrap();
        // Shaped as a project's account (OWL-308): `/` and `.` in the name.
        let account = format!(
            "github.com/owlshift-live-check/repo-{}/linear",
            std::process::id()
        );
        keychain.delete(&account).unwrap();
        assert!(!keychain.contains(&account).unwrap());
        keychain.store(&account, &Secret::new("dummy")).unwrap();
        assert!(keychain.contains(&account).unwrap());
        assert_eq!(keychain.read(&account).unwrap(), Some(Secret::new("dummy")));
        assert!(keychain.delete(&account).unwrap());
        assert_eq!(keychain.read(&account).unwrap(), None);
    }
}
