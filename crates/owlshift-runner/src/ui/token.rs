//! The web UI's token (OWL-467, architecture section 5, "Who may drive
//! it", "The token"): 32 random bytes in `<data>/service/ui/token`, written
//! as 64 lowercase hexadecimal digits and nothing else, the folder 0700 and
//! the file 0600 on Unix. The daemon makes it before it first binds the
//! UI's port and keeps it across restarts; `owlshift ui --new-token`
//! replaces it, which ends every session, since the UI reads the file again
//! at every request that checks a code or a session.
//!
//! The folder is on the hidden list of every confined run's policy
//! (`agent_env`), so no agent reads it.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use super::auth::{self, hex};
use crate::daemon::service_dir;

/// The bytes of a token.
pub const TOKEN_LEN: usize = 32;

/// The most bytes a token file is read up to: the token, a line end, and
/// room for whitespace an editor added.
const MAX_TOKEN_FILE: u64 = 1024;

/// The token's folder in the data directory `data`: `<data>/service/ui`.
pub fn token_dir(data: &Path) -> PathBuf {
    service_dir(data).join("ui")
}

/// The token's file: `<data>/service/ui/token`.
pub fn token_file(data: &Path) -> PathBuf {
    token_dir(data).join("token")
}

/// The web UI's token. Its `Debug` prints nothing of it.
#[derive(Clone, PartialEq, Eq)]
pub struct Token([u8; TOKEN_LEN]);

impl Token {
    /// A token of these bytes.
    pub fn from_bytes(bytes: [u8; TOKEN_LEN]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; TOKEN_LEN] {
        &self.0
    }

    /// The token as its file holds it: 64 lowercase hexadecimal digits.
    pub fn to_hex(&self) -> String {
        hex(&self.0)
    }

    fn parse(text: &str) -> Option<Self> {
        auth::from_hex_array(text.trim_ascii()).map(Self)
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(..)")
    }
}

/// Makes the token's folder `dir` when it is missing, its parents
/// included, and sets it to 0700 on Unix, also when it already exists. A
/// `dir` that is not a folder, a symbolic link included, is refused.
pub fn ensure_dir(dir: &Path) -> io::Result<()> {
    if let Some(parent) = dir.parent() {
        fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        builder
    };
    #[cfg(not(unix))]
    let builder = fs::DirBuilder::new();
    match builder.create(dir) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let metadata = fs::symlink_metadata(dir)?;
    if !metadata.is_dir() {
        return Err(io::Error::other(format!(
            "{} is not a folder: the web UI keeps its token there",
            dir.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o777 != 0o700 {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}

/// The token of `data`, made when there is none. When two processes make
/// it at once, both get the one that landed first.
pub fn ensure_token(data: &Path) -> io::Result<Token> {
    ensure_dir(&token_dir(data))?;
    if let Some(token) = read_token_if_any(data)? {
        return Ok(token);
    }
    let token = Token(auth::random()?);
    let temporary = write_temporary(data, &token)?;
    let linked = fs::hard_link(&temporary, token_file(data));
    let _ = fs::remove_file(&temporary);
    match linked {
        Ok(()) => Ok(token),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => read_token(data),
        Err(error) => Err(error),
    }
}

/// Replaces the token of `data` with a new one, which ends every session
/// of the web UI.
pub fn new_token(data: &Path) -> io::Result<Token> {
    ensure_dir(&token_dir(data))?;
    let token = Token(auth::random()?);
    let temporary = write_temporary(data, &token)?;
    if let Err(error) = fs::rename(&temporary, token_file(data)) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(token)
}

/// The token of `data`; a file that holds anything but 64 lowercase
/// hexadecimal digits, around ASCII whitespace, is refused, the error
/// never quoting it.
pub fn read_token(data: &Path) -> io::Result<Token> {
    let path = token_file(data);
    let mut text = String::new();
    fs::File::open(&path)?
        .take(MAX_TOKEN_FILE + 1)
        .read_to_string(&mut text)
        .map_err(|_| unreadable(&path))?;
    if text.len() as u64 > MAX_TOKEN_FILE {
        return Err(unreadable(&path));
    }
    Token::parse(&text).ok_or_else(|| unreadable(&path))
}

/// The token of `data`, `None` when its file does not exist.
pub fn read_token_if_any(data: &Path) -> io::Result<Option<Token>> {
    match read_token(data) {
        Ok(token) => Ok(Some(token)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn unreadable(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "{} does not hold a token, 64 lowercase hexadecimal digits: `owlshift ui \
             --new-token` writes a new one",
            path.display()
        ),
    )
}

/// Writes `token` to a new file of the token's folder, created 0600 on
/// Unix, and returns its path.
fn write_temporary(data: &Path, token: &Token) -> io::Result<PathBuf> {
    let name = format!(
        ".token.{}.{}",
        std::process::id(),
        hex(&auth::random::<8>()?)
    );
    let path = token_dir(data).join(name);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let written = options.open(&path).and_then(|mut file| {
        file.write_all(format!("{}\n", token.to_hex()).as_bytes())?;
        file.sync_all()
    });
    if let Err(error) = written {
        let _ = fs::remove_file(&path);
        return Err(error);
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn the_token_is_made_once_private_and_replaced_on_request() {
        let data = tempfile::tempdir().unwrap();
        let data = data.path();
        let token = ensure_token(data).unwrap();
        let text = fs::read_to_string(token_file(data)).unwrap();
        assert_eq!(text.trim_end().len(), 64);
        assert!(text.trim_end().bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(text.trim_end(), token.to_hex());
        #[cfg(unix)]
        {
            assert_eq!(mode(&token_dir(data)), 0o700);
            assert_eq!(mode(&token_file(data)), 0o600);
        }
        assert_eq!(ensure_token(data).unwrap(), token);
        assert_eq!(read_token(data).unwrap(), token);

        let new = new_token(data).unwrap();
        assert_ne!(new, token);
        assert_eq!(read_token(data).unwrap(), new);
        #[cfg(unix)]
        assert_eq!(mode(&token_file(data)), 0o600);
        let left: Vec<_> = fs::read_dir(token_dir(data))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(left, ["token"], "no temporary file is left");
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_found_open_is_set_back_to_0700() {
        use std::os::unix::fs::PermissionsExt;
        let data = tempfile::tempdir().unwrap();
        let dir = token_dir(data.path());
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        ensure_dir(&dir).unwrap();
        assert_eq!(mode(&dir), 0o700);
        ensure_token(data.path()).unwrap();
        assert_eq!(mode(&dir), 0o700);
    }

    #[cfg(unix)]
    #[test]
    fn a_symbolic_link_in_place_of_the_folder_is_refused() {
        let data = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        fs::create_dir_all(service_dir(data.path())).unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), token_dir(data.path())).unwrap();
        assert!(ensure_dir(&token_dir(data.path())).is_err());
    }

    #[test]
    fn a_file_that_holds_no_token_is_refused_without_quoting_it() {
        let data = tempfile::tempdir().unwrap();
        let data = data.path();
        assert!(read_token_if_any(data).unwrap().is_none());
        ensure_dir(&token_dir(data)).unwrap();
        fs::write(token_file(data), "xyz\n").unwrap();
        let error = read_token(data).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(!error.to_string().contains("xyz"), "{error}");
        assert!(read_token_if_any(data).is_err());
        let upper = "AB".repeat(32);
        fs::write(token_file(data), &upper).unwrap();
        assert!(read_token(data).is_err(), "only lowercase digits");
        let spaced = format!("  {}\r\n", "ab".repeat(32));
        fs::write(token_file(data), spaced).unwrap();
        assert_eq!(read_token(data).unwrap().to_hex(), "ab".repeat(32));
    }

    #[test]
    fn a_token_prints_nothing_of_itself() {
        let token = Token::from_bytes([0xab; TOKEN_LEN]);
        assert_eq!(format!("{token:?}"), "Token(..)");
    }
}
