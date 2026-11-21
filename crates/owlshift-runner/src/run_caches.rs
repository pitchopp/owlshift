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

/// One project's run caches: the cache directory their tree lies in, and
/// each variable with its folder there.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunCaches {
    root: PathBuf,
    folders: Vec<(String, PathBuf)>,
}

impl RunCaches {
    /// `folders`, each a variable and its folder under the cache directory
    /// `root`. Nothing is made or checked: [`RunCaches::make`] does.
    pub fn new(root: PathBuf, folders: Vec<(String, PathBuf)>) -> Self {
        Self { root, folders }
    }

    /// Each variable and its folder.
    pub fn folders(&self) -> &[(String, PathBuf)] {
        &self.folders
    }

    /// Makes every folder as [`make`] does; the first refusal stops.
    pub fn make(&self) -> Result<(), String> {
        self.folders
            .iter()
            .try_for_each(|(_, folder)| make(&self.root, folder))
    }
}

/// Each variable of `entries` (`stack.run_caches`, as declared) with its
/// folder in `repo`'s tree under `cache_dir`, every folder made. None
/// declared: nothing made, nothing asked of `cache_dir`. Refused, naming the
/// path: no cache directory; a tree whose path holds a space, which some
/// build tools refuse, `OWLSHIFT_CACHE_DIR` moving it; what [`make`]
/// refuses.
pub fn prepare(
    cache_dir: Option<&Path>,
    repo: &Repo,
    entries: &[(&str, &str)],
) -> Result<RunCaches, String> {
    if entries.is_empty() {
        return Ok(RunCaches::default());
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
    let folders = entries
        .iter()
        .map(|(name, folder)| ((*name).to_owned(), tree.join(folder)))
        .collect();
    let caches = RunCaches::new(cache_dir.to_owned(), folders);
    caches.make()?;
    Ok(caches)
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

/// Makes the run cache `folder`, which lies under the cache directory
/// `root`, and the folders between them; one that exists already is kept
/// as it is. The sandbox opens the folder by its real path, read and
/// written, so it must stay in Owlshift's tree: every part from `root` down
/// to the folder, the folder included, is checked before and after it is
/// made, and one that is a symbolic link or not a folder refuses, naming
/// it; so does a folder whose real path leaves `root`'s. `root` itself is
/// the operator's (`OWLSHIFT_CACHE_DIR`) and may be a link.
pub fn make(root: &Path, folder: &Path) -> Result<(), String> {
    let Ok(below) = folder.strip_prefix(root) else {
        return Err(format!(
            "the run cache {} lies outside {}",
            folder.display(),
            root.display()
        ));
    };
    check_parts(root, below)?;
    fs::create_dir_all(folder)
        .map_err(|error| format!("the run cache {} cannot be made: {error}", folder.display()))?;
    check_parts(root, below)?;
    let real = |path: &Path| {
        fs::canonicalize(path).map_err(|error| format!("{}: {error}", path.display()))
    };
    if !real(folder)?.starts_with(real(root)?) {
        return Err(format!(
            "the run cache {} leads out of {}: remove it",
            folder.display(),
            root.display()
        ));
    }
    Ok(())
}

/// Refuses a part of `below`, from `root` down, that is a symbolic link or
/// not a folder; the parts not made yet end the walk.
fn check_parts(root: &Path, below: &Path) -> Result<(), String> {
    let mut path = root.to_owned();
    for part in below.components() {
        path.push(part);
        let Ok(meta) = fs::symlink_metadata(&path) else {
            break;
        };
        if meta.file_type().is_symlink() {
            return Err(format!(
                "the run cache path {} is a symbolic link, and the sandbox would open where it \
                 leads, read and written: remove it",
                path.display()
            ));
        }
        if !meta.is_dir() {
            return Err(format!(
                "the run cache path {} exists and is not a folder: remove it",
                path.display()
            ));
        }
    }
    Ok(())
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
            made.folders(),
            [
                ("CARGO_HOME".to_owned(), tree.join("cargo")),
                ("UV_CACHE_DIR".to_owned(), tree.join("uv")),
            ]
        );
        fs::write(tree.join("cargo").join("kept"), "x").unwrap();
        assert_eq!(prepare(Some(dir.path()), &repo(), &entries).unwrap(), made);
        assert!(tree.join("cargo").join("kept").is_file());
        // Nothing declared asks nothing, a missing cache directory included.
        assert_eq!(prepare(None, &repo(), &[]).unwrap(), RunCaches::default());
    }

    /// OWL-617: a run cache that is, or lies under, a symbolic link in the
    /// tree is refused, before and after the folders are made, so the
    /// sandbox never opens where it leads; the link's target is left as it
    /// was. A link inside a cache folder is the run's own and changes
    /// nothing.
    #[cfg(unix)]
    #[test]
    fn a_planted_link_in_the_tree_is_refused() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = dir.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        let caches = dir.path().join("caches");
        let tree = tree(&caches, &repo());
        let entries = [("CARGO_HOME", "cargo")];
        for planted in [tree.join("cargo"), tree.clone(), caches.join("github.com")] {
            let _ = fs::remove_dir_all(&caches);
            fs::create_dir_all(planted.parent().unwrap()).unwrap();
            symlink(&elsewhere, &planted).unwrap();
            let error = prepare(Some(&caches), &repo(), &entries).unwrap_err();
            assert!(
                error.contains(&planted.display().to_string()) && error.contains("symbolic link"),
                "{error}"
            );
            assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
            // The spawn's own check refuses it too.
            let made = RunCaches::new(
                caches.clone(),
                vec![("CARGO_HOME".into(), tree.join("cargo"))],
            );
            assert!(made.make().unwrap_err().contains("symbolic link"));
        }
        fs::remove_dir_all(&caches).unwrap();
        let made = prepare(Some(&caches), &repo(), &entries).unwrap();
        symlink(&elsewhere, tree.join("cargo").join("inside")).unwrap();
        assert_eq!(made.make(), Ok(()));
        // A cache directory that is itself a link is the operator's choice.
        let linked = dir.path().join("linked");
        symlink(&caches, &linked).unwrap();
        assert!(prepare(Some(&linked), &repo(), &entries).is_ok());
        let outside = RunCaches::new(caches.clone(), vec![("A".into(), elsewhere.clone())]);
        assert!(outside.make().unwrap_err().contains("lies outside"));
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
