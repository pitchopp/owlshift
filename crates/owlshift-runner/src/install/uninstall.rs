//! `owlshift uninstall [--purge]` (OWL-437): the daemon stopped checked,
//! then exactly what the install manifest lists removed, then, with
//! `--purge`, Owlshift's entries of the data directory and the personal file.

use std::fs::{self, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use owlshift_contracts::service::{DaemonRecord, KeychainEntry};
use owlshift_platform::keychain::{Keychain, SERVICE};
use owlshift_platform::service::Manager;

use super::manifest::Manifests;
use super::remove_service_files;
use crate::events;

/// How long `owlshift uninstall` waits for the daemon to end once it was sent
/// SIGTERM: past launchd's grace of 60 seconds and systemd's of 90, after
/// which the manager has killed it.
pub const STOP_WAIT: Duration = Duration::from_secs(100);

/// How often that wait looks.
pub const STOP_POLL: Duration = Duration::from_millis(250);

/// The entries of the data directory Owlshift makes, by their exact names:
/// each project's folder (its dedicated clone, worktrees, run logs, ticket
/// folders and obligations), the service's files, the usage and slot files,
/// the event log and its lock; and Finder's `.DS_Store`.
const OWN_ENTRIES: [&str; 7] = [
    "projects",
    "service",
    "usage",
    "slots",
    events::EVENTS_FILE,
    events::LOCK_FILE,
    ".DS_Store",
];

/// How a daemon that ran was stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stopped {
    /// None ran.
    NotRunning,
    /// The daemon of this pid was sent SIGTERM and ended.
    Ended(u32),
}

/// Stops the daemon that runs on `data`, checked: SIGTERM, which the
/// service manager's unload sends too, then a wait of up to `wait` for it to
/// end. `running` says whether one runs and its record; `terminate` sends
/// SIGTERM. The refusal says nothing was removed and how to go on.
pub fn stop_checked(
    data: &Path,
    running: &dyn Fn() -> io::Result<Option<Option<DaemonRecord>>>,
    terminate: &dyn Fn(u32) -> io::Result<()>,
    wait: Duration,
    poll: Duration,
) -> Result<Stopped, String> {
    let probe = || running().map_err(|e| format!("{}: {e}", data.display()));
    let record = match probe()? {
        None => return Ok(Stopped::NotRunning),
        Some(Some(record)) => record,
        Some(None) => {
            return Err(format!(
                "a daemon runs on {} whose record does not read, so it cannot be told to stop: \
                 nothing was removed. `owlshift stop` drains it; run `owlshift uninstall` again \
                 once it has ended",
                data.display()
            ));
        }
    };
    terminate(record.pid).map_err(|error| {
        format!(
            "the daemon (pid {}) could not be sent SIGTERM ({error}): nothing was removed. \
             `owlshift stop` drains it; run `owlshift uninstall` again once it has ended",
            record.pid
        )
    })?;
    let deadline = Instant::now() + wait;
    loop {
        if probe()?.is_none() {
            return Ok(Stopped::Ended(record.pid));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "the daemon (pid {}) still runs {} seconds after SIGTERM: nothing was removed. \
                 Run `owlshift stop` first, or wait for it to end and run `owlshift uninstall` \
                 again",
                record.pid,
                wait.as_secs()
            ));
        }
        thread::sleep(poll);
    }
}

/// What [`remove_listed`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Removed {
    /// Each thing removed, in words.
    pub removed: Vec<String>,
    /// Each thing that could not be removed and why: it stays listed.
    pub left: Vec<String>,
    /// What went wrong that did not keep a file from going, such as the
    /// manager's unload of a service not loaded.
    pub warnings: Vec<String>,
}

/// Removes exactly what the install manifest of `data` lists: the service,
/// unloaded by `manager`, then its files; then each keychain entry, from the
/// keychain `open` gives, opened only when one is listed. What could not be
/// removed stays listed; the manifest goes once it lists nothing.
pub fn remove_listed(
    data: &Path,
    manager: &dyn Manager,
    open: &dyn Fn() -> Result<Keychain, String>,
) -> Result<Removed, String> {
    Manifests::in_data_dir(data).update(|manifest, write| {
        let mut report = Removed::default();
        if let Some(service) = manifest.service.clone() {
            let left = remove_service_files(&service, manager, &mut report.warnings);
            if left.is_empty() {
                report
                    .removed
                    .push(format!("the service at login ({})", service.unit));
                manifest.service = None;
            } else {
                report.left.extend(left);
            }
        }
        if !manifest.keychain.is_empty() {
            let mut kept: Vec<KeychainEntry> = Vec::new();
            let keychain = open();
            for entry in std::mem::take(&mut manifest.keychain) {
                let named = format!(
                    "the keychain entry `{}` (service `{}`)",
                    entry.account, entry.service
                );
                let outcome = match &keychain {
                    _ if entry.service != SERVICE => Err("not Owlshift's service".to_owned()),
                    Err(error) => Err(error.clone()),
                    Ok(keychain) => keychain.delete(&entry.account).map_err(|e| e.to_string()),
                };
                match outcome {
                    Ok(_) => report.removed.push(named),
                    Err(why) => {
                        report.left.push(format!("{named}: {why}"));
                        kept.push(entry);
                    }
                }
            }
            manifest.keychain = kept;
        }
        write(manifest)?;
        Ok(report)
    })
}

/// Why `data` cannot be purged, before anything is removed: it is the
/// root, the home folder or a folder holding it; or nothing in it is
/// Owlshift's, so an `OWLSHIFT_DATA_DIR` set by mistake never empties a
/// person's folder.
pub fn purge_refusal(data: &Path, home: Option<&Path>) -> Option<String> {
    let nothing = "nothing was removed";
    if data.parent().is_none() || home.is_some_and(|home| home.starts_with(data)) {
        return Some(format!(
            "{} holds the home folder: Owlshift purges only a data directory of its own; \
             {nothing}",
            data.display()
        ));
    }
    let names = exact_names(data);
    let service = data.join("service");
    let owned = names.iter().any(|name| name == events::LOCK_FILE)
        || (names.iter().any(|name| name == "service")
            && exact_names(&service).iter().any(|name| {
                [
                    "daemon.lock",
                    "manifest.json",
                    "manifest.lock",
                    "projects.json",
                ]
                .contains(&name.as_str())
            }));
    (!owned).then(|| {
        format!(
            "{} does not look like Owlshift's data directory: it holds neither `{}` nor \
             Owlshift's `service` folder; {nothing}",
            data.display(),
            events::LOCK_FILE
        )
    })
}

/// The names of `dir`'s entries as the file system spells them: so a case
/// that does not tell names apart never takes `Projects` for `projects`.
fn exact_names(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter_map(|entry| entry.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// The commands that work on `data` now, by the locks they hold: each
/// project's own lock, its watch's and its served lock, and the daemon's.
/// Each lock file is opened without being made.
pub fn busy(data: &Path) -> Result<Vec<String>, String> {
    let mut held = Vec::new();
    let daemon = data.join("service").join("daemon.lock");
    if is_held(&daemon)? {
        held.push("the daemon".to_owned());
    }
    let hosts = data.join("projects");
    for host in exact_names(&hosts) {
        for owner in exact_names(&hosts.join(&host)) {
            for repo in exact_names(&hosts.join(&host).join(&owner)) {
                let root = hosts.join(&host).join(&owner).join(&repo);
                for lock in ["lock", "watch.lock", "served.lock"] {
                    if is_held(&root.join(lock))? {
                        held.push(format!("{owner}/{repo} ({lock})"));
                    }
                }
            }
        }
    }
    Ok(held)
}

/// Whether another open file holds the lock of `path`: one try, let go at
/// once; no file is no holder.
fn is_held(path: &Path) -> Result<bool, String> {
    let file = match OpenOptions::new().write(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    match file.try_lock() {
        Ok(()) => Ok(false),
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(TryLockError::Error(error)) => Err(format!("{}: {error}", path.display())),
    }
}

/// What [`purge`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Purged {
    pub removed: Vec<PathBuf>,
    /// What stays: entries of the data directory that are not Owlshift's,
    /// or the manifest still listing what could not be removed.
    pub left: Vec<PathBuf>,
    /// What could not be removed and why.
    pub failed: Vec<String>,
}

/// The personal file `--purge` removes.
pub struct Personal<'a> {
    pub file: &'a Path,
    /// Whether its folder is Owlshift's own, removed once empty.
    pub own_folder: bool,
}

/// Removes Owlshift's entries of `data` (`OWN_ENTRIES`, matched by their
/// exact names, links removed and never followed) and the personal file,
/// then `data` once it is empty. A manifest that still lists something
/// stays, with its folder, so that what it names is never forgotten.
pub fn purge(data: &Path, personal: Option<&Personal<'_>>) -> Purged {
    let mut purged = Purged::default();
    if let Some(personal) = personal {
        match fs::remove_file(personal.file) {
            Ok(()) => purged.removed.push(personal.file.to_owned()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => purged
                .failed
                .push(format!("{}: {error}", personal.file.display())),
        }
        if personal.own_folder
            && let Some(folder) = personal.file.parent()
            && folder != data
            && fs::remove_dir(folder).is_ok()
        {
            purged.removed.push(folder.to_owned());
        }
    }
    let manifest = Manifests::in_data_dir(data).file();
    let keep_manifest = fs::symlink_metadata(&manifest).is_ok();
    for name in exact_names(data) {
        if !OWN_ENTRIES.contains(&name.as_str()) {
            continue;
        }
        let path = data.join(&name);
        if name == "service" && keep_manifest {
            for inner in exact_names(&path) {
                if inner != "manifest.json" && inner != "manifest.lock" {
                    remove_entry(&path.join(inner), &mut purged);
                }
            }
            purged.left.push(manifest.clone());
            continue;
        }
        remove_entry(&path, &mut purged);
    }
    for name in exact_names(data) {
        if !OWN_ENTRIES.contains(&name.as_str()) {
            purged.left.push(data.join(name));
        }
    }
    if exact_names(data).is_empty() && fs::remove_dir(data).is_ok() {
        purged.removed.push(data.to_owned());
    }
    purged
}

/// Removes `path`: a folder with all it holds, anything else, a link
/// included, as one file.
fn remove_entry(path: &Path, purged: &mut Purged) {
    let removed = match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) => Err(error),
    };
    match removed {
        Ok(()) => purged.removed.push(path.to_owned()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => purged.failed.push(format!("{}: {error}", path.display())),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::cell::Cell;

    use owlshift_adapters::forge::Repo;
    use owlshift_contracts::format::Format;
    use owlshift_platform::keychain::Secret;

    use super::*;
    use crate::install::tests::FakeManager;
    use crate::project::ProjectDirs;
    use crate::served::Registry;

    fn record(pid: u32) -> DaemonRecord {
        DaemonRecord {
            format: Format,
            pid,
            version: "0.0.0".to_owned(),
            binary: "/bin/owlshift".to_owned(),
            since: jiff::Timestamp::now(),
        }
    }

    /// No daemon: nothing sent. One that ends: SIGTERM to its pid. One that
    /// outlives the wait, or whose record does not read: refused, naming
    /// `owlshift stop`.
    #[test]
    fn the_daemon_is_stopped_checked_or_nothing_is_removed() {
        let data = Path::new("/data");
        let sent = Cell::new(None);
        let terminate = |pid: u32| {
            sent.set(Some(pid));
            Ok(())
        };
        let none = || Ok(None);
        assert_eq!(
            stop_checked(data, &none, &terminate, Duration::ZERO, Duration::ZERO),
            Ok(Stopped::NotRunning)
        );
        assert_eq!(sent.get(), None);

        let looks = Cell::new(0);
        let ends = || {
            looks.set(looks.get() + 1);
            Ok((looks.get() < 3).then(|| Some(record(42))))
        };
        assert_eq!(
            stop_checked(
                data,
                &ends,
                &terminate,
                Duration::from_secs(5),
                Duration::ZERO
            ),
            Ok(Stopped::Ended(42))
        );
        assert_eq!(sent.get(), Some(42));

        let stays = || Ok(Some(Some(record(43))));
        let error =
            stop_checked(data, &stays, &terminate, Duration::ZERO, Duration::ZERO).unwrap_err();
        assert!(
            error.contains("pid 43")
                && error.contains("nothing was removed")
                && error.contains("Run `owlshift stop` first, or wait"),
            "{error}"
        );
        let unread = || Ok(Some(None));
        let error =
            stop_checked(data, &unread, &terminate, Duration::ZERO, Duration::ZERO).unwrap_err();
        assert!(error.contains("`owlshift stop` drains it"), "{error}");
    }

    /// Each listed entry goes, the manifest with them; one the keychain
    /// cannot remove, or filed under another service, stays listed.
    #[test]
    fn exactly_what_the_manifest_lists_is_removed() {
        let data = tempfile::tempdir().unwrap();
        let data = data.path();
        let entry = |service: &str, account: &str| KeychainEntry {
            service: service.to_owned(),
            account: account.to_owned(),
        };
        let manifests = Manifests::in_data_dir(data);
        manifests
            .record_keychain(&[entry("owlshift", "claude-agent"), entry("other", "x")])
            .unwrap();
        let keychain = || {
            let keychain = Keychain::in_memory();
            keychain
                .store("claude-agent", &Secret::new("token"))
                .unwrap();
            Ok(keychain)
        };
        let manager = FakeManager::default();
        let report = remove_listed(data, &manager, &keychain).unwrap();
        assert_eq!(
            report.removed,
            ["the keychain entry `claude-agent` (service `owlshift`)"]
        );
        assert_eq!(
            report.left,
            ["the keychain entry `x` (service `other`): not Owlshift's service"]
        );
        assert_eq!(manifests.read().unwrap().keychain, [entry("other", "x")]);
        assert!(manager.calls().is_empty(), "no service listed: no manager");

        let locked = || Err("keychain: locked".to_owned());
        manifests
            .record_keychain(&[entry("owlshift", "github.com/a/b/linear")])
            .unwrap();
        let report = remove_listed(data, &manager, &locked).unwrap();
        assert_eq!(report.left.len(), 2, "{report:?}");
        assert!(report.left[1].ends_with("keychain: locked"), "{report:?}");
        assert_eq!(manifests.read().unwrap().keychain.len(), 2);
    }

    /// Owlshift's entries go, a person's file stays and is named, and a
    /// folder that is not Owlshift's data directory is refused.
    #[test]
    fn a_purge_removes_owlshifts_entries_alone() {
        let home = tempfile::tempdir().unwrap();
        let data = home.path().join("data");
        let repo = Repo::parse("acme/web").unwrap();
        let checkout = home.path().join("web");
        Registry::in_data_dir(&data)
            .register(&repo, &checkout)
            .unwrap();
        let dirs = ProjectDirs::new(&data, &repo);
        fs::create_dir_all(dirs.worktrees().join("OWL-1")).unwrap();
        drop(dirs.lock().unwrap().unwrap());
        for file in [
            "slots/1.lock",
            "usage/claude.json",
            "events.jsonl",
            "events.lock",
            ".DS_Store",
        ] {
            let path = data.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "x").unwrap();
        }
        let personal = home.path().join("config/owlshift/config.toml");
        fs::create_dir_all(personal.parent().unwrap()).unwrap();
        fs::write(&personal, "").unwrap();

        assert_eq!(
            purge_refusal(home.path(), Some(home.path())).map(|r| r.contains("home folder")),
            Some(true)
        );
        fs::create_dir_all(&checkout).unwrap();
        assert!(
            purge_refusal(&checkout, None)
                .unwrap()
                .contains("does not look like Owlshift's data directory")
        );
        assert_eq!(purge_refusal(&data, Some(home.path())), None);
        assert!(busy(&data).unwrap().is_empty());
        let held = dirs.lock().unwrap().unwrap();
        assert_eq!(busy(&data).unwrap(), ["acme/web (lock)"]);
        drop(held);

        fs::write(data.join("notes.txt"), "mine").unwrap();
        let purged = purge(
            &data,
            Some(&Personal {
                file: &personal,
                own_folder: true,
            }),
        );
        assert!(purged.failed.is_empty(), "{purged:?}");
        assert_eq!(purged.left, [data.join("notes.txt")]);
        assert_eq!(
            exact_names(&data),
            ["notes.txt"],
            "every entry of Owlshift's went"
        );
        assert!(!personal.parent().unwrap().exists());

        fs::remove_file(data.join("notes.txt")).unwrap();
        fs::create_dir_all(data.join("service")).unwrap();
        Manifests::in_data_dir(&data)
            .record_keychain(&[KeychainEntry {
                service: "owlshift".to_owned(),
                account: "claude-agent".to_owned(),
            }])
            .unwrap();
        fs::write(data.join("service/daemon.log"), "").unwrap();
        let purged = purge(&data, None);
        assert_eq!(purged.left, [Manifests::in_data_dir(&data).file()]);
        assert!(!data.join("service/daemon.log").exists());
        assert!(
            Manifests::in_data_dir(&data).file().exists(),
            "a manifest still listing an entry stays"
        );
    }
}
