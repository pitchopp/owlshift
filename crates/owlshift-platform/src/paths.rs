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

/// The folder that holds the login Claude Code uses for agent runs: a second
/// login of the operator's own account, made once with `claude auth login`
/// inside the sandbox, which Owlshift names through `CLAUDE_CONFIG_DIR` and
/// never reads (OWL-41). It sits beside the personal configuration file, as
/// `agent-login/claude`, under the same `OWLSHIFT_CONFIG_DIR` override.
///
/// `None` when the configuration directory is unknown.
pub fn claude_agent_login_dir() -> Option<PathBuf> {
    login_dir(personal_config_file()?)
}

/// The Claude Code login folder beside a personal configuration file.
fn login_dir(config_file: PathBuf) -> Option<PathBuf> {
    Some(config_file.parent()?.join("agent-login").join("claude"))
}

/// `override_dir`: `OWLSHIFT_CONFIG_DIR` as read from the environment.
/// Unset, empty or relative means "no override". `config_dir`: the
/// platform's parent configuration directory, as `dirs::config_dir()`
/// reports it.
fn resolve(override_dir: Option<OsString>, config_dir: Option<PathBuf>) -> Option<PathBuf> {
    let overridden = override_dir
        .map(PathBuf::from)
        .filter(|dir| !dir.as_os_str().is_empty() && dir.is_absolute());
    match overridden {
        Some(dir) => Some(dir.join("config.toml")),
        None => config_dir.map(|dir| dir.join("owlshift").join("config.toml")),
    }
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
    fn the_agent_login_sits_beside_the_personal_file() {
        let dir = PathBuf::from(if cfg!(windows) {
            r"C:\Users\test\owlshift-config"
        } else {
            "/tmp/owlshift-config"
        });
        assert_eq!(
            login_dir(dir.join("config.toml")),
            Some(dir.join("agent-login").join("claude"))
        );
    }
}
