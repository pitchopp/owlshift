//! Where Owlshift's own files live on each platform.

use std::path::PathBuf;

/// The personal configuration file: `owlshift/config.toml` in the user's
/// configuration directory. That is `~/Library/Application Support` on macOS,
/// `$XDG_CONFIG_HOME` or `~/.config` on Linux, and `%APPDATA%` on Windows.
///
/// `None` when the platform reports no configuration directory, for instance
/// without a home directory.
pub fn personal_config_file() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("owlshift").join("config.toml"))
}
