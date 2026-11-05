//! The local web UI (P8, OWL-467, architecture section 5, "The local web
//! UI"): `owlshift ui --daemon`, a child of the daemon that serves the page
//! frame and its header on a `127.0.0.1` socket the daemon binds and keeps.
//!
//! - *The token* ([`token`]): 32 random bytes in `<data>/service/ui/token`,
//!   the folder 0700 and the file 0600, on every confined run's hidden list.
//! - *The proof, the code and the session* ([`auth`]): HMAC-SHA256 under the
//!   token. `owlshift ui` proves the listener before it mints a one-time
//!   code; the page trades the code for a session secret bound to the
//!   daemon's run, through the run secret the daemon hands each UI it
//!   starts.
//! - *The header* ([`header`]), read from the files `owlshift status` reads
//!   (`status::gather_files`), and the UI's record ([`record`]),
//!   `<data>/service/ui.json`.
//! - *The server* ([`server`]): tiny_http and threads on the socket the
//!   daemon hands over, its routes and checks, and the page carried in the
//!   binary.

use owlshift_contracts::config::{DEFAULT_UI_PORT, PersonalConfig};

use crate::config::FileState;

pub mod auth;
pub mod header;
pub mod record;
pub mod server;
pub mod token;

/// The port of `127.0.0.1` the web UI listens on: `[ui] port` of the
/// personal file, [`DEFAULT_UI_PORT`] when the file is absent or does not
/// load.
pub fn port_of(personal: &FileState<PersonalConfig>) -> u16 {
    match personal {
        FileState::Loaded { config, .. } => config.ui.port(),
        _ => DEFAULT_UI_PORT,
    }
}

/// The personal file as it is now, read again: the web UI reads it at each
/// refresh of its header, so a change to `concurrent_runs` or a usage cap
/// shows without a restart.
pub fn personal_now() -> FileState<PersonalConfig> {
    match owlshift_platform::paths::personal_config_file() {
        Some(path) => personal_at(&path),
        None => FileState::NotApplicable("this system has no configuration directory".into()),
    }
}

/// The personal file at `path`, read now.
pub fn personal_at(path: &std::path::Path) -> FileState<PersonalConfig> {
    FileState::load(path.to_owned(), PersonalConfig::parse)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_port_is_the_personal_files_or_the_default() {
        let absent = FileState::Absent("/nowhere/owlshift.toml".into());
        assert_eq!(port_of(&absent), 18466);
        let set = FileState::from_text(
            "/home/ada/.config/owlshift/owlshift.toml".into(),
            "[ui]\nport = 18500\n",
            PersonalConfig::parse,
        );
        assert_eq!(port_of(&set), 18500);
        let invalid = FileState::from_text(
            "/home/ada/.config/owlshift/owlshift.toml".into(),
            "[ui]\nport = 0\n",
            PersonalConfig::parse,
        );
        assert_eq!(port_of(&invalid), 18466);
    }
}
