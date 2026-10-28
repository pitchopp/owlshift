//! The build identity (OWL-462, architecture section 5, "An upgrade",
//! "Newer"): the commit this binary is built from, whether the tree held
//! uncommitted changes to tracked files, when it was built and the tree it
//! was built from, handed to the code as the `OWLSHIFT_BUILD_*` variables
//! that `owlshift_contracts::service::BuildIdentity::from_build` reads.
//!
//! Any failure (no git, no git tree, a tree that is not Owlshift's own
//! workspace) leaves the commit unknown and never fails the build. The
//! script runs again when the commit can change (the worktree's `HEAD`, its
//! branch's ref, `packed-refs`) or the code can (`crates`, `roles`, the
//! workspace's manifest and lockfile): only paths that exist are named,
//! since cargo runs a script again at every build while a path it names is
//! missing (checked on 2026-10-28 with cargo 1.98.1). The build time is the
//! time this script last ran.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let manifest = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"),
    );
    let built_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let tree = Tree::of(&manifest);
    let (commit, dirty, source) = match &tree {
        Some(tree) => (
            tree.commit.as_str(),
            tree.dirty,
            tree.top.to_str().unwrap_or_default(),
        ),
        None => ("", false, ""),
    };
    println!("cargo:rustc-env=OWLSHIFT_BUILD_COMMIT={commit}");
    println!(
        "cargo:rustc-env=OWLSHIFT_BUILD_DIRTY={}",
        if dirty { "1" } else { "0" }
    );
    println!("cargo:rustc-env=OWLSHIFT_BUILD_AT={built_at}");
    let source = if source.contains(['\n', '\r']) {
        ""
    } else {
        source
    };
    println!("cargo:rustc-env=OWLSHIFT_BUILD_SOURCE={source}");
    // Without a tree, nothing is named, so cargo's default applies: the
    // script runs again when any file of this package changes.
    if let Some(tree) = &tree {
        for path in tree.watched() {
            if path.exists() {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
    }
}

/// The git tree this package is built in, when it is Owlshift's workspace.
struct Tree {
    top: PathBuf,
    /// This worktree's own git folder, which holds its `HEAD`.
    git_dir: PathBuf,
    /// The folder every worktree shares, which holds the refs.
    common_dir: PathBuf,
    /// The ref `HEAD` names, such as `refs/heads/main`; none when detached.
    head_ref: Option<String>,
    commit: String,
    dirty: bool,
}

impl Tree {
    fn of(manifest: &Path) -> Option<Self> {
        let paths = git(
            manifest,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--show-toplevel",
                "--git-dir",
                "--git-common-dir",
                "HEAD",
            ],
        )?;
        let mut lines = paths.lines();
        let top = PathBuf::from(lines.next()?);
        let git_dir = PathBuf::from(lines.next()?);
        let common_dir = PathBuf::from(lines.next()?);
        let commit = lines.next()?.trim().to_owned();
        // Only Owlshift's own workspace: a copy of this package inside
        // another repository never takes that repository's commit.
        let same = |a: &Path, b: &Path| match (fs::canonicalize(a), fs::canonicalize(b)) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        };
        if !same(&top.join("crates").join("owlshift-cli"), manifest) {
            return None;
        }
        let dirty = !git(&top, &["status", "--porcelain", "--untracked-files=no"])?
            .trim()
            .is_empty();
        let head_ref = git(manifest, &["symbolic-ref", "-q", "HEAD"])
            .map(|name| name.trim().to_owned())
            .filter(|name| name.starts_with("refs/"));
        Some(Self {
            top,
            git_dir,
            common_dir,
            head_ref,
            commit,
            dirty,
        })
    }

    /// What the script runs again on, whether or not it exists.
    fn watched(&self) -> Vec<PathBuf> {
        let mut paths = vec![
            self.git_dir.join("HEAD"),
            self.common_dir.join("packed-refs"),
        ];
        if let Some(name) = &self.head_ref {
            paths.push(self.common_dir.join(name));
        }
        paths.extend(
            ["crates", "roles", "Cargo.toml", "Cargo.lock"].map(|path| self.top.join(path)),
        );
        paths
    }
}

/// What `git -C <dir> <args>` printed, when it succeeded. It writes nothing
/// in the repository (`--no-optional-locks`), and a `GIT_DIR` or
/// `GIT_WORK_TREE` of the environment, such as a hook's, does not point it
/// elsewhere.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}
