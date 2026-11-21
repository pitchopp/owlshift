//! The caches of Owlshift's own that a project's runs may write
//! (`stack.run_caches`, OWL-617; architecture section 8, "Caches a run may
//! write"): one tree per project under the machine's cache directory
//! (`owlshift_platform::paths::cache_dir`), `<cache>/github.com/<owner>/<repo>`,
//! a folder per declared variable in it.
//!
//! A command that builds an agent environment makes the folders first
//! ([`prepare`]), refusing what no run can use: a project declaring caches on
//! a system with no cache directory, a tree whose path holds a space, a place
//! taken by something that is not a folder. Every confined spawn makes them
//! again ([`make`], `AgentEnv`), so a tree the operator deleted to empty it
//! comes back at the next run, empty. The sandbox opens them read and written
//! and the spawned command gets each variable set to its folder
//! (`crate::agent_env`); `owlshift doctor` gives the tree's path and size
//! ([`size`]).

use std::fs;
use std::path::{Path, PathBuf};

use owlshift_adapters::forge::Repo;

use crate::project::repo_folder;

/// The tree of `repo`'s run caches under the cache directory `cache_dir`.
pub fn tree(cache_dir: &Path, repo: &Repo) -> PathBuf {
    repo_folder(cache_dir, repo)
}

/// Each variable of `entries` (`stack.run_caches`, as declared) with its
/// folder in `repo`'s tree under `cache_dir`, every folder made. None
/// declared: nothing made, nothing asked of `cache_dir`. Refused, naming the
/// path: no cache directory; a tree whose path holds a space, which some
/// build tools refuse, `OWLSHIFT_CACHE_DIR` moving it; a folder that cannot
/// be made.
pub fn prepare(
    cache_dir: Option<&Path>,
    repo: &Repo,
    entries: &[(&str, &str)],
) -> Result<Vec<(String, PathBuf)>, String> {
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let Some(cache_dir) = cache_dir else {
        return Err(
            "the project declares `stack.run_caches`, and this system has no cache directory to \
             hold them: set OWLSHIFT_CACHE_DIR to an absolute path without a space"
                .to_owned(),
        );
    };
    let tree = tree(cache_dir, repo);
    check_tree(&tree)?;
    entries
        .iter()
        .map(|(name, folder)| {
            let path = tree.join(folder);
            make(&path)?;
            Ok(((*name).to_owned(), path))
        })
        .collect()
}

/// Refuses a tree whose path holds a space, which some build tools refuse
/// (architecture section 8: the cache directory is chosen for a path
/// without one), naming `OWLSHIFT_CACHE_DIR`, which moves it.
pub fn check_tree(tree: &Path) -> Result<(), String> {
    if tree.as_os_str().to_string_lossy().contains(' ') {
        return Err(format!(
            "the caches of `stack.run_caches` would lie in {}, whose path holds a space, which \
             some build tools refuse: set OWLSHIFT_CACHE_DIR to an absolute path without one",
            tree.display()
        ));
    }
    Ok(())
}

/// `bytes` for a person: `0 B`, `12 KB`, `327 MB`, `1.2 GB`, in powers of
/// a thousand.
pub fn shown_size(bytes: u64) -> String {
    match bytes {
        0..1_000 => format!("{bytes} B"),
        1_000..1_000_000 => format!("{} KB", bytes / 1_000),
        1_000_000..1_000_000_000 => format!("{} MB", bytes / 1_000_000),
        _ => format!("{:.1} GB", bytes as f64 / 1e9),
    }
}

/// Makes the run cache folder `folder` and the folders above it; one that
/// exists already is kept as it is.
pub fn make(folder: &Path) -> Result<(), String> {
    match fs::create_dir_all(folder) {
        Ok(()) => Ok(()),
        Err(_) if fs::symlink_metadata(folder).is_ok() && !folder.is_dir() => Err(format!(
            "the run cache {} exists and is not a folder: remove it",
            folder.display()
        )),
        Err(error) => Err(format!(
            "the run cache {} cannot be made: {error}",
            folder.display()
        )),
    }
}

/// The most entries [`size`] reads before it stops: about a second's walk.
pub const SIZE_WALK_LIMIT: usize = 200_000;

/// What a folder holds, as [`size`] found it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Size {
    /// The lengths of its files, summed.
    pub bytes: u64,
    /// Whether the walk read every entry, under [`SIZE_WALK_LIMIT`];
    /// otherwise `bytes` is a floor.
    pub complete: bool,
}

/// The lengths of the files under `folder` summed, links not followed and
/// entries that cannot be read left out, the walk stopping after `limit`
/// entries. A folder that is not there holds nothing.
pub fn size(folder: &Path, limit: usize) -> Size {
    let mut bytes = 0u64;
    let mut seen = 0usize;
    let mut pending = vec![folder.to_owned()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            seen += 1;
            if seen > limit {
                return Size {
                    bytes,
                    complete: false,
                };
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() {
                bytes += entry.metadata().map(|meta| meta.len()).unwrap_or(0);
            }
        }
    }
    Size {
        bytes,
        complete: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> Repo {
        Repo::parse("Acme/Web").unwrap()
    }

    #[test]
    fn the_folders_are_made_in_the_projects_tree_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let entries = [("CARGO_HOME", "cargo"), ("UV_CACHE_DIR", "uv")];
        let made = prepare(Some(dir.path()), &repo(), &entries).unwrap();
        let tree = dir.path().join("github.com").join("acme").join("web");
        assert_eq!(tree, super::tree(dir.path(), &repo()));
        assert_eq!(
            made,
            [
                ("CARGO_HOME".to_owned(), tree.join("cargo")),
                ("UV_CACHE_DIR".to_owned(), tree.join("uv")),
            ]
        );
        fs::write(tree.join("cargo").join("kept"), "x").unwrap();
        assert_eq!(prepare(Some(dir.path()), &repo(), &entries).unwrap(), made);
        assert!(tree.join("cargo").join("kept").is_file());
        // Nothing declared asks nothing, a missing cache directory included.
        assert_eq!(prepare(None, &repo(), &[]).unwrap(), []);
    }

    /// OWL-617: a tree on a path holding a space, or no cache directory,
    /// refuses the command, naming `OWLSHIFT_CACHE_DIR`; a file in a
    /// folder's place is named.
    #[test]
    fn what_no_run_can_use_is_refused_naming_the_way_out() {
        let dir = tempfile::tempdir().unwrap();
        let spaced = dir.path().join("Application Support");
        let error = prepare(Some(&spaced), &repo(), &[("CARGO_HOME", "cargo")]).unwrap_err();
        assert!(
            error.contains("Application Support") && error.contains("OWLSHIFT_CACHE_DIR"),
            "{error}"
        );
        assert!(!spaced.exists());
        let error = prepare(None, &repo(), &[("CARGO_HOME", "cargo")]).unwrap_err();
        assert!(error.contains("OWLSHIFT_CACHE_DIR"), "{error}");
        let tree = tree(dir.path(), &repo());
        fs::create_dir_all(&tree).unwrap();
        fs::write(tree.join("cargo"), "").unwrap();
        let error = prepare(Some(dir.path()), &repo(), &[("CARGO_HOME", "cargo")]).unwrap_err();
        assert!(error.contains("is not a folder"), "{error}");
    }

    #[test]
    fn the_size_sums_the_files_and_stops_at_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            size(&dir.path().join("missing"), 10),
            Size {
                bytes: 0,
                complete: true
            }
        );
        fs::create_dir_all(dir.path().join("a/b")).unwrap();
        fs::write(dir.path().join("a/one"), "12345").unwrap();
        fs::write(dir.path().join("a/b/two"), "123").unwrap();
        assert_eq!(
            size(dir.path(), 10),
            Size {
                bytes: 8,
                complete: true
            }
        );
        assert!(!size(dir.path(), 2).complete);
        let shown: Vec<String> = [0, 999, 12_345, 327_000_000, 1_234_000_000]
            .into_iter()
            .map(shown_size)
            .collect();
        assert_eq!(shown, ["0 B", "999 B", "12 KB", "327 MB", "1.2 GB"]);
    }
}
