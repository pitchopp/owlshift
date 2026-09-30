//! Where Owlshift's own files live on each platform.

use std::ffi::OsString;
use std::path::PathBuf;

/// The personal configuration file: `owlshift/config.toml` in the user's
/// configuration directory. That is `~/Library/Application Support` on macOS,
/// `$XDG_CONFIG_HOME` or `~/.config` on Linux, and `%APPDATA%` on Windows.
///
/// `OWLSHIFT_CONFIG_DIR`, when set to a non-empty absolute path, overrides
/// the directory that holds `config.toml` directly (it replaces
/// `<config_dir>/owlshift`, not the platform's parent configuration
/// directory), on every platform. This exists so tests and tooling can
/// redirect the personal file deterministically: on Windows, `dirs::config_dir()`
/// reads the OS known-folder API, which no other environment variable
/// redirects. A relative or empty value is treated as unset, since the
/// override's meaning must not depend on the current directory Owlshift
/// happens to run from.
///
/// `None` when there is no override and the platform reports no
/// configuration directory, for instance without a home directory.
pub fn personal_config_file() -> Option<PathBuf> {
    resolve(std::env::var_os("OWLSHIFT_CONFIG_DIR"), dirs::config_dir())
}

/// Owlshift's data directory: the dedicated clones, worktrees and run logs of
/// `owlshift do`, and the event log. That is `owlshift` in the user's local
/// data directory: `~/Library/Application Support` on macOS,
/// `$XDG_DATA_HOME` or `~/.local/share` on Linux, `%LOCALAPPDATA%` on
/// Windows.
///
/// `OWLSHIFT_DATA_DIR`, when set to a non-empty absolute path, is used as is
/// instead, on every platform, by the same rule as `OWLSHIFT_CONFIG_DIR`.
///
/// `None` when there is no override and the platform reports no data
/// directory.
pub fn data_dir() -> Option<PathBuf> {
    resolve_data(
        std::env::var_os("OWLSHIFT_DATA_DIR"),
        dirs::data_local_dir(),
    )
}

/// The user's home directory, `None` when the platform reports none.
pub fn home_dir() -> Option<PathBuf> {
    dirs::home_dir()
}

/// `override_dir`: `OWLSHIFT_CONFIG_DIR` as read from the environment.
/// Unset, empty or relative means "no override". `config_dir`: the
/// platform's parent configuration directory, as `dirs::config_dir()`
/// reports it.
fn resolve(override_dir: Option<OsString>, config_dir: Option<PathBuf>) -> Option<PathBuf> {
    match absolute(override_dir) {
        Some(dir) => Some(dir.join("config.toml")),
        None => config_dir.map(|dir| dir.join("owlshift").join("config.toml")),
    }
}

/// `override_dir`: `OWLSHIFT_DATA_DIR`; `data_dir`: the platform's local data
/// directory, as `dirs::data_local_dir()` reports it.
fn resolve_data(override_dir: Option<OsString>, data_dir: Option<PathBuf>) -> Option<PathBuf> {
    absolute(override_dir).or_else(|| data_dir.map(|dir| dir.join("owlshift")))
}

/// An override that counts: set, non-empty and absolute.
fn absolute(value: Option<OsString>) -> Option<PathBuf> {
    value
        .map(PathBuf::from)
        .filter(|dir| !dir.as_os_str().is_empty() && dir.is_absolute())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_absolute_replaces_the_owlshift_subfolder() {
        let override_dir = PathBuf::from(if cfg!(windows) {
            r"C:\Users\test\owlshift-config"
        } else {
            "/tmp/owlshift-config"
        });
        let config_dir = PathBuf::from(if cfg!(windows) {
            r"C:\Users\test\AppData\Roaming"
        } else {
            "/home/test/.config"
        });
        assert_eq!(
            resolve(
                Some(override_dir.clone().into_os_string()),
                Some(config_dir)
            ),
            Some(override_dir.join("config.toml"))
        );
    }

    #[test]
    fn override_empty_falls_back_to_config_dir() {
        let config_dir = PathBuf::from(if cfg!(windows) {
            r"C:\Users\test\AppData\Roaming"
        } else {
            "/home/test/.config"
        });
        assert_eq!(
            resolve(Some(OsString::new()), Some(config_dir.clone())),
            Some(config_dir.join("owlshift").join("config.toml"))
        );
    }

    #[test]
    fn override_relative_falls_back_to_config_dir() {
        let config_dir = PathBuf::from(if cfg!(windows) {
            r"C:\Users\test\AppData\Roaming"
        } else {
            "/home/test/.config"
        });
        assert_eq!(
            resolve(
                Some(OsString::from("relative/owlshift-config")),
                Some(config_dir.clone())
            ),
            Some(config_dir.join("owlshift").join("config.toml"))
        );
    }

    #[test]
    fn override_absent_falls_back_to_config_dir() {
        let config_dir = PathBuf::from(if cfg!(windows) {
            r"C:\Users\test\AppData\Roaming"
        } else {
            "/home/test/.config"
        });
        assert_eq!(
            resolve(None, Some(config_dir.clone())),
            Some(config_dir.join("owlshift").join("config.toml"))
        );
    }

    #[test]
    fn no_override_and_no_config_dir_is_none() {
        assert_eq!(resolve(None, None), None);
    }

    #[test]
    fn override_relative_with_no_config_dir_is_none() {
        assert_eq!(resolve(Some(OsString::from("relative")), None), None);
    }

    #[test]
    fn the_data_directory_follows_the_same_override_rule() {
        let (over, local) = if cfg!(windows) {
            (r"C:\owlshift-data", r"C:\Users\test\AppData\Local")
        } else {
            ("/tmp/owlshift-data", "/home/test/.local/share")
        };
        let local = PathBuf::from(local);
        assert_eq!(
            resolve_data(Some(over.into()), Some(local.clone())),
            Some(PathBuf::from(over))
        );
        for ignored in [None, Some(OsString::new()), Some("relative".into())] {
            assert_eq!(
                resolve_data(ignored, Some(local.clone())),
                Some(local.join("owlshift"))
            );
        }
        assert_eq!(resolve_data(None, None), None);
    }
}
