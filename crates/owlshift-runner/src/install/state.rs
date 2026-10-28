//! What `owlshift doctor` reports of the service (OWL-437): what the
//! install manifest lists and whether its files are there, what lies at
//! Owlshift's paths unlisted, and the daemon that runs.

use std::fs;
use std::path::{Path, PathBuf};

use owlshift_contracts::service::{DaemonRecord, InstalledService, Request};
use owlshift_platform::service::Layout;

use super::manifest::Manifests;
use crate::daemon;

/// The service as the data directory and this machine's folders show it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceState {
    /// Where Owlshift's unit and launcher go on this machine.
    pub layout: Layout,
    /// The install manifest's file.
    pub manifest: PathBuf,
    /// The service it lists, or why it does not read.
    pub listed: Result<Option<InstalledService>, String>,
    /// The listed service's files that are gone.
    pub gone: Vec<String>,
    /// Whether the listed service's binary is there.
    pub binary_there: bool,
    /// The files at Owlshift's paths the manifest does not list.
    pub unlisted: Vec<PathBuf>,
    /// The daemon that runs on the data directory, with its record when it
    /// reads; or why that could not be told.
    pub daemon: Result<Option<Option<DaemonRecord>>, String>,
    /// The upgrade's drain asked of the daemon, when one stands (OWL-436).
    pub drain: Option<Request>,
}

/// The service of the data directory `data`, Owlshift's paths being
/// `layout`'s.
pub fn state(data: &Path, layout: Layout) -> ServiceState {
    let manifests = Manifests::in_data_dir(data);
    let listed = manifests.read().map(|manifest| manifest.service);
    let there = |path: &Path| fs::symlink_metadata(path).is_ok();
    let (gone, binary_there, named): (Vec<String>, bool, Vec<String>) = match &listed {
        Ok(Some(service)) => (
            service
                .files()
                .into_iter()
                .filter(|file| !there(Path::new(file)))
                .map(str::to_owned)
                .collect(),
            Path::new(&service.binary).exists(),
            service.files().into_iter().map(str::to_owned).collect(),
        ),
        _ => (Vec::new(), true, Vec::new()),
    };
    let unlisted = [layout.unit(), layout.launcher()]
        .into_iter()
        .filter(|path| there(path) && !named.iter().any(|file| Path::new(file) == path))
        .collect();
    ServiceState {
        layout,
        manifest: manifests.file(),
        listed,
        gone,
        binary_there,
        unlisted,
        daemon: daemon::running(data).map_err(|e| e.to_string()),
        drain: daemon::requests::drain_asked(data),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use owlshift_platform::service::Kind;

    use super::*;
    use crate::install::tests::spec;
    use crate::install::{Install, install};

    #[test]
    fn the_state_tells_listed_gone_and_unlisted_files_apart() {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        let layout = Layout {
            kind: Kind::Launchd,
            dir: root.path().join("agents"),
        };
        let nothing = state(&data, layout.clone());
        assert_eq!(nothing.listed, Ok(None));
        assert!(nothing.unlisted.is_empty() && nothing.gone.is_empty());
        assert_eq!(nothing.daemon, Ok(None));

        let binary = root.path().join("owlshift");
        fs::write(&binary, "").unwrap();
        install(&Install {
            ..spec(&data, &layout, &binary)
        })
        .unwrap();
        let installed = state(&data, layout.clone());
        assert!(matches!(installed.listed, Ok(Some(_))));
        assert!(installed.binary_there && installed.gone.is_empty());
        assert!(installed.unlisted.is_empty());

        fs::remove_file(&binary).unwrap();
        fs::remove_file(layout.launcher()).unwrap();
        let broken = state(&data, layout.clone());
        assert!(!broken.binary_there);
        assert_eq!(broken.gone, [layout.launcher().display().to_string()]);

        let other = root.path().join("other");
        let foreign = state(&other, layout.clone());
        assert_eq!(foreign.unlisted, [layout.unit()]);
    }
}
