//! The service installed at login, and everything Owlshift installed removed
//! (OWL-437; runtime and operations, "Install, lifecycle & uninstall").
//!
//! - `owlshift start --at-login` writes the unit, a user LaunchAgent
//!   `dev.owlshift.daemon` or a systemd user unit `owlshift.service`, and the
//!   `/bin/sh` launcher beside it ([`install`]), listing both in the install
//!   manifest first ([`Manifests`]). The unit only runs the launcher, which
//!   names the binary, the data and configuration directories and the `PATH`,
//!   so an install again rewrites the launcher alone.
//! - The launcher starts `owlshift daemon` while its binary and the manifest
//!   exist; once either is gone it removes the unit, its link and itself,
//!   drops the manifest's service line and exits 0, which neither manager
//!   restarts (architecture section 5, "Restarts").
//! - `owlshift stop --at-login` unloads and removes the service
//!   ([`remove_service`]); `owlshift uninstall` stops the daemon checked,
//!   then removes exactly what the manifest lists, and `--purge` Owlshift's
//!   entries of the data directory ([`uninstall`]).
//! - `owlshift doctor` reads [`state`].
//!
//! The service manager is behind [`Manager`], so no test runs `launchctl` or
//! `systemctl`.

mod manifest;
mod state;
pub mod uninstall;
mod units;

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use owlshift_contracts::service::{InstalledService, ServiceManager};
use owlshift_platform::service::{Kind, Layout, Manager};

pub use manifest::Manifests;
pub use state::{ServiceState, state};
pub use units::{LAUNCHD_GRACE, RESTART_DELAY, SYSTEMD_GRACE};

/// What `owlshift start --at-login` installs.
pub struct Install<'a> {
    /// The data directory, whose manifest lists the service.
    pub data: &'a Path,
    /// Where the unit and its launcher go.
    pub layout: &'a Layout,
    /// The binary the launcher starts.
    pub binary: &'a Path,
    /// The personal file's folder, named to the daemon, when there is one.
    pub config_dir: Option<&'a Path>,
    /// The `PATH` of the command that installs, whose absolute entries the
    /// daemon is given.
    pub path: Option<&'a OsStr>,
}

/// What [`install`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Installed {
    /// The service was not installed: it is now.
    New,
    /// It was: its files are rewritten.
    Updated,
    /// It was, with these very files.
    Unchanged,
}

/// Installs the service at login: the manifest lists it first, then the
/// launcher and the unit are written, so a crash leaves a file listed and
/// missing, never one unlisted. Refused when a unit or launcher is at those
/// paths that this manifest does not list, another data directory's or a
/// leftover, with the commands that remove it.
pub fn install(spec: &Install<'_>) -> Result<Installed, String> {
    let layout = spec.layout;
    let path = units::service_path(spec.path);
    let launcher = units::launcher_text(&units::Launcher {
        binary: spec.binary,
        data: spec.data,
        config_dir: spec.config_dir,
        path: path.as_deref(),
        layout,
    })?;
    let unit = units::unit_text(layout)?;
    let text = |path: &Path| units::checked(path, "the service's file").map(str::to_owned);
    let service = InstalledService {
        manager: manager_of(layout.kind),
        unit: text(&layout.unit())?,
        launcher: text(&layout.launcher())?,
        link: layout.link().as_deref().map(text).transpose()?,
        binary: text(spec.binary)?,
    };
    Manifests::in_data_dir(spec.data).update(|manifest, write| {
        let listed = manifest.service.clone();
        match &listed {
            Some(listed) if listed.unit == service.unit && listed.launcher == service.launcher => {}
            Some(listed) => {
                return Err(format!(
                    "the install manifest lists a service at {}: `owlshift stop --at-login` \
                     removes it, then `owlshift start --at-login` installs it here",
                    listed.unit
                ));
            }
            None => {
                let present: Vec<String> = [layout.unit(), layout.launcher()]
                    .iter()
                    .filter(|path| fs::symlink_metadata(path).is_ok())
                    .map(|path| path.display().to_string())
                    .collect();
                if !present.is_empty() {
                    return Err(format!(
                        "{} {} Owlshift's but not listed by this data directory's install \
                         manifest, {}: another data directory installed it, or it was left \
                         behind. Remove it with `{}`, then run `owlshift start --at-login` again",
                        present.join(" and "),
                        if present.len() == 1 { "is" } else { "are" },
                        Manifests::in_data_dir(spec.data).file().display(),
                        layout.removal_commands().join("; ")
                    ));
                }
            }
        }
        let same = |path: &Path, text: &str| fs::read_to_string(path).is_ok_and(|on| on == text);
        if listed.as_ref() == Some(&service)
            && same(&layout.launcher(), &launcher)
            && same(&layout.unit(), &unit)
        {
            return Ok(Installed::Unchanged);
        }
        manifest.service = Some(service.clone());
        write(manifest)?;
        write_service_file(&layout.launcher(), &launcher)?;
        write_service_file(&layout.unit(), &unit)?;
        Ok(if listed.is_some() {
            Installed::Updated
        } else {
            Installed::New
        })
    })
}

/// The service the manifest of `data` lists, when its unit is there: what
/// `owlshift start` starts through its manager rather than as a second
/// daemon.
pub fn installed(data: &Path) -> Result<Option<InstalledService>, String> {
    Ok(Manifests::in_data_dir(data)
        .read()?
        .service
        .filter(|service| fs::symlink_metadata(&service.unit).is_ok()))
}

/// The layout of a service the manifest lists: its manager, its unit's
/// folder.
pub fn layout_of(service: &InstalledService) -> Layout {
    Layout {
        kind: match service.manager {
            ServiceManager::Launchd => Kind::Launchd,
            ServiceManager::Systemd => Kind::Systemd,
        },
        dir: Path::new(&service.unit)
            .parent()
            .map_or_else(PathBuf::new, Path::to_owned),
    }
}

fn manager_of(kind: Kind) -> ServiceManager {
    match kind {
        Kind::Launchd => ServiceManager::Launchd,
        Kind::Systemd => ServiceManager::Systemd,
    }
}

/// What [`remove_service`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServiceRemoval {
    /// The service removed; `None` when the manifest listed none.
    pub removed: Option<InstalledService>,
    /// What the manager said that did not keep the files from going.
    pub warnings: Vec<String>,
}

/// Unloads and removes the service the manifest of `data` lists, the
/// manager unloading it first (`launchctl bootout`, `systemctl --user
/// disable --now`), then its files, then its manifest entry. A file that
/// could not be removed keeps the entry, and the error names it.
pub fn remove_service(data: &Path, manager: &dyn Manager) -> Result<ServiceRemoval, String> {
    Manifests::in_data_dir(data).update(|manifest, write| {
        let Some(service) = manifest.service.clone() else {
            return Ok(ServiceRemoval::default());
        };
        let mut warnings = Vec::new();
        let left = remove_service_files(&service, manager, &mut warnings);
        if !left.is_empty() {
            return Err(format!(
                "could not remove {}: the install manifest still lists the service",
                left.join("; ")
            ));
        }
        manifest.service = None;
        write(manifest)?;
        Ok(ServiceRemoval {
            removed: Some(service),
            warnings,
        })
    })
}

/// Unloads `service`, removes its files, then has the manager forget them:
/// each file that could not be removed, and why. What the manager says goes
/// to `warnings`: a unit it could not unload is no daemon running, since the
/// caller checked none runs, and its files still go.
fn remove_service_files(
    service: &InstalledService,
    manager: &dyn Manager,
    warnings: &mut Vec<String>,
) -> Vec<String> {
    let layout = layout_of(service);
    warnings.extend(manager.unload(&layout).err());
    let mut left = Vec::new();
    for file in service.files() {
        match fs::remove_file(file) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                left.push(format!("{file}: {error}"));
            }
            _ => {}
        }
    }
    warnings.extend(manager.reload(&layout).err());
    left
}

/// Writes one of the service's files whole, readable by all and writable by
/// the user alone, as launchd requires of a property list.
fn write_service_file(path: &Path, text: &str) -> Result<(), String> {
    let failed = |e: io::Error| format!("{}: {e}", path.display());
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(failed)?;
    }
    let mut partial = path.as_os_str().to_owned();
    partial.push(".partial");
    let partial = PathBuf::from(partial);
    fs::write(&partial, text).map_err(failed)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&partial, fs::Permissions::from_mode(0o644)).map_err(failed)?;
    }
    fs::rename(&partial, path).map_err(failed)
}

#[cfg(all(test, unix))]
pub(crate) mod tests {
    use std::cell::RefCell;

    use owlshift_contracts::service::{InstallManifest, KeychainEntry};

    use super::*;

    /// A service manager that records what it is asked, failing the
    /// commands named.
    #[derive(Default)]
    pub(crate) struct FakeManager {
        calls: RefCell<Vec<String>>,
        pub(crate) fails: Vec<&'static str>,
    }

    impl FakeManager {
        pub(crate) fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }

        fn call(&self, what: &'static str, layout: &Layout) -> Result<(), String> {
            self.calls
                .borrow_mut()
                .push(format!("{what} {:?}", layout.kind));
            if self.fails.contains(&what) {
                Err(format!("{what} failed"))
            } else {
                Ok(())
            }
        }
    }

    impl Manager for FakeManager {
        fn enable(&self, layout: &Layout) -> Result<(), String> {
            self.call("enable", layout)
        }
        fn start(&self, layout: &Layout) -> Result<(), String> {
            self.call("start", layout)
        }
        fn unload(&self, layout: &Layout) -> Result<(), String> {
            self.call("unload", layout)
        }
        fn reload(&self, layout: &Layout) -> Result<(), String> {
            self.call("reload", layout)
        }
    }

    pub(crate) fn spec<'a>(data: &'a Path, layout: &'a Layout, binary: &'a Path) -> Install<'a> {
        Install {
            data,
            layout,
            binary,
            config_dir: Some(data),
            path: Some(OsStr::new("/opt/homebrew/bin:relative:/usr/bin")),
        }
    }

    /// Both managers' files are written and listed; an install again with
    /// the same files changes nothing, with another binary rewrites them; a
    /// unit this manifest does not list is refused; the removal unloads,
    /// removes and forgets.
    #[test]
    fn the_service_is_installed_listed_and_removed_on_both_systems() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        for kind in [Kind::Launchd, Kind::Systemd] {
            let layout = Layout {
                kind,
                dir: root.path().join(format!("{kind:?}")),
            };
            let binary = root.path().join("bin/owlshift");
            assert_eq!(install(&spec(&data, &layout, &binary)), Ok(Installed::New));
            let listed = installed(&data).unwrap().unwrap();
            assert_eq!(listed.unit, layout.unit().to_str().unwrap());
            assert_eq!(listed.link, layout.link().map(|l| l.display().to_string()));
            assert_eq!(listed.binary, binary.to_str().unwrap());
            assert_eq!(layout_of(&listed), layout);
            let launcher = fs::read_to_string(layout.launcher()).unwrap();
            assert!(
                launcher.contains("PATH='/opt/homebrew/bin:/usr/bin'\n"),
                "{launcher}"
            );
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = fs::metadata(layout.unit()).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o644);
            }
            assert_eq!(
                install(&spec(&data, &layout, &binary)),
                Ok(Installed::Unchanged)
            );
            let other = root.path().join("bin/owlshift-0.5");
            assert_eq!(
                install(&spec(&data, &layout, &other)),
                Ok(Installed::Updated)
            );
            assert!(
                fs::read_to_string(layout.launcher())
                    .unwrap()
                    .contains("owlshift-0.5")
            );

            let elsewhere = Layout {
                kind,
                dir: root.path().join("elsewhere"),
            };
            let error = install(&spec(&data, &elsewhere, &binary)).unwrap_err();
            assert!(
                error.contains("`owlshift stop --at-login` removes it"),
                "{error}"
            );

            let manager = FakeManager {
                fails: vec!["unload"],
                ..FakeManager::default()
            };
            let removal = remove_service(&data, &manager).unwrap();
            assert_eq!(removal.removed.map(|s| s.unit), Some(listed.unit));
            assert_eq!(removal.warnings, ["unload failed"]);
            assert_eq!(
                manager.calls(),
                [format!("unload {kind:?}"), format!("reload {kind:?}")]
            );
            assert!(!layout.unit().exists() && !layout.launcher().exists());
            assert!(!Manifests::in_data_dir(&data).file().exists());
            assert_eq!(
                remove_service(&data, &manager).unwrap(),
                ServiceRemoval::default()
            );

            // A unit at Owlshift's paths that no manifest lists is refused.
            fs::write(layout.unit(), "left behind").unwrap();
            let error = install(&spec(&data, &layout, &binary)).unwrap_err();
            assert!(
                error.contains("not listed by this data directory's install manifest")
                    && error.contains(&layout.removal_commands()[0]),
                "{error}"
            );
            fs::remove_file(layout.unit()).unwrap();
        }
    }

    /// The launcher, run by the real `/bin/sh` on paths that need quoting:
    /// with its binary, it starts `<binary> daemon` with the directories and
    /// the `PATH` named, its output in `daemon.log`; once the binary is gone,
    /// or the manifest, it removes the unit, the link and itself, drops the
    /// manifest's service line, keeps the keychain entries, and exits 0.
    #[test]
    fn the_launcher_starts_the_daemon_and_removes_the_service_once_its_binary_is_gone() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;

        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("da ta'$x \"q\" %h");
        let layout = Layout {
            kind: Kind::Systemd,
            dir: root.path().join("units & more"),
        };
        let binary = root.path().join("bin/owl shift");
        fs::create_dir_all(binary.parent().unwrap()).unwrap();
        let stub = "#!/bin/sh\nprintf '%s\\n' \"$@\" \"$OWLSHIFT_DATA_DIR\" \"$OWLSHIFT_CONFIG_DIR\" \
                    \"$PATH\" \"$(pwd)\" >ran\necho $$ >pid\necho said\n";
        let put_binary = || {
            fs::write(&binary, stub).unwrap();
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        };
        put_binary();
        let entry = KeychainEntry {
            service: "owlshift".to_owned(),
            account: "claude-agent".to_owned(),
        };
        let manifests = Manifests::in_data_dir(&data);
        let installed_with_link = || {
            install(&spec(&data, &layout, &binary)).unwrap();
            let link = layout.link().unwrap();
            fs::create_dir_all(link.parent().unwrap()).unwrap();
            fs::write(&link, "").unwrap();
        };
        installed_with_link();
        manifests
            .record_keychain(std::slice::from_ref(&entry))
            .unwrap();
        let run = || {
            let status = Command::new("/bin/sh")
                .arg(layout.launcher())
                .env_clear()
                .status()
                .unwrap();
            assert!(status.success(), "{status}");
        };

        run();
        let service = data.join("service");
        let ran = fs::read_to_string(service.join("ran")).unwrap();
        let data_text = data.to_str().unwrap();
        assert_eq!(
            ran,
            format!(
                "daemon\n{data_text}\n{data_text}\n/opt/homebrew/bin:/usr/bin\n{}\n",
                service.display()
            )
        );
        assert_eq!(
            fs::read_to_string(service.join("daemon.log")).unwrap(),
            "said\n"
        );
        // OWL-436: the marker names the daemon's own pid, `exec` keeping it.
        assert_eq!(
            fs::read_to_string(crate::daemon::upgrade::launched_file(&data)).unwrap(),
            fs::read_to_string(service.join("pid")).unwrap()
        );

        fs::remove_file(&binary).unwrap();
        run();
        for gone in [layout.unit(), layout.link().unwrap(), layout.launcher()] {
            assert!(!gone.exists(), "{}", gone.display());
        }
        let left = manifests.read().unwrap();
        assert_eq!(
            left,
            InstallManifest {
                keychain: vec![entry],
                ..InstallManifest::default()
            }
        );
        assert_eq!(
            fs::read_dir(&service)
                .unwrap()
                .filter(|e| e
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains(".json."))
                .count(),
            0,
            "no temporary manifest left"
        );

        // A data directory removed by hand: the manifest is gone.
        put_binary();
        installed_with_link();
        fs::remove_file(manifests.file()).unwrap();
        run();
        assert!(!layout.unit().exists() && !layout.launcher().exists());
    }
}
