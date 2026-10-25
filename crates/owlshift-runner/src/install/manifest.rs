//! The install manifest, `<data>/service/manifest.json` (OWL-437): read
//! without a lock, changed under `manifest.lock`, written whole, and removed
//! once it lists nothing.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use owlshift_contracts::service::{InstallManifest, KeychainEntry};

use crate::{project, step};

/// The install manifest of one data directory.
#[derive(Clone, Debug)]
pub struct Manifests {
    dir: PathBuf,
}

impl Manifests {
    /// The manifest of the data directory `data`.
    pub fn in_data_dir(data: &Path) -> Self {
        Self {
            dir: crate::daemon::service_dir(data),
        }
    }

    /// The manifest's file.
    pub fn file(&self) -> PathBuf {
        self.dir.join("manifest.json")
    }

    fn lock_file(&self) -> PathBuf {
        self.dir.join("manifest.lock")
    }

    /// What the manifest lists, without its lock: nothing when there is
    /// none.
    pub fn read(&self) -> Result<InstallManifest, String> {
        let path = self.file();
        match fs::read_to_string(&path) {
            Ok(text) => {
                InstallManifest::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(InstallManifest::default()),
            Err(error) => Err(format!("{}: {error}", path.display())),
        }
    }

    /// Runs `change` under the manifest's lock, held for the whole of it, so
    /// that the files an install or a removal writes go with the manifest:
    /// `change` gets the manifest and a way to write it, whole, or to remove
    /// it once it lists nothing.
    pub fn update<T>(
        &self,
        change: impl FnOnce(
            &mut InstallManifest,
            &dyn Fn(&InstallManifest) -> Result<(), String>,
        ) -> Result<T, String>,
    ) -> Result<T, String> {
        project::under_lock(&self.lock_file(), || {
            let mut manifest = self.read()?;
            let path = self.file();
            let write = |manifest: &InstallManifest| -> Result<(), String> {
                if manifest.is_empty() {
                    match fs::remove_file(&path) {
                        Err(error) if error.kind() != io::ErrorKind::NotFound => {
                            Err(format!("{}: {error}", path.display()))
                        }
                        _ => Ok(()),
                    }
                } else {
                    step::write_atomically(&path, &manifest.render())
                        .map_err(|e| format!("{}: {e}", path.display()))
                }
            };
            change(&mut manifest, &write)
        })
    }

    /// Lists `entries` the manifest does not list yet: how many were added.
    pub fn record_keychain(&self, entries: &[KeychainEntry]) -> Result<usize, String> {
        self.update(|manifest, write| {
            let added = entries
                .iter()
                .filter(|entry| manifest.record((*entry).clone()))
                .count();
            if added > 0 {
                write(manifest)?;
            }
            Ok(added)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keychain_entries_are_listed_once_and_an_empty_manifest_goes() {
        let data = tempfile::tempdir().unwrap();
        let manifests = Manifests::in_data_dir(data.path());
        assert_eq!(manifests.read().unwrap(), InstallManifest::default());
        let entry = KeychainEntry {
            service: "owlshift".to_owned(),
            account: "claude-agent".to_owned(),
        };
        assert_eq!(
            manifests
                .record_keychain(std::slice::from_ref(&entry))
                .unwrap(),
            1
        );
        assert_eq!(
            manifests
                .record_keychain(std::slice::from_ref(&entry))
                .unwrap(),
            0
        );
        assert_eq!(manifests.read().unwrap().keychain, [entry]);
        manifests
            .update(|manifest, write| {
                manifest.keychain.clear();
                write(manifest)
            })
            .unwrap();
        assert!(
            !manifests.file().exists(),
            "a manifest listing nothing goes"
        );
    }
}
