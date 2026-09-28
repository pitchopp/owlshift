//! `read_confined` refuses every symbolic link from the root down and reads
//! a plain file byte for byte.

use std::fs;
use std::path::{Path, PathBuf};

use owlshift_platform::confined::{ConfinedError, Refusal, read_confined};
use tempfile::TempDir;

/// Contents with bytes that are not UTF-8, to prove the read is byte for byte.
const INSIDE: &[u8] = b"inside \xff\x00 plan\n";
const SECRET: &[u8] = b"outside secret\n";

/// A temporary `base` holding the worktree `base/wt` and an outside file
/// `base/outside/secret.txt`.
struct Fixture {
    base: TempDir,
    wt: PathBuf,
    outside: PathBuf,
}

fn fixture() -> Fixture {
    let base = tempfile::tempdir().unwrap();
    let wt = base.path().join("wt");
    let outside = base.path().join("outside");
    fs::create_dir(&wt).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("secret.txt"), SECRET).unwrap();
    Fixture { base, wt, outside }
}

/// Creates a symbolic link to a file. On Windows this needs Developer Mode or
/// the symbolic-link privilege: without it the test is skipped locally, and
/// fails under CI, which must prove the case.
fn symlink_file(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    let created = std::os::unix::fs::symlink(target, link);
    #[cfg(windows)]
    let created = std::os::windows::fs::symlink_file(target, link);
    privileged(created)
}

/// Creates a symbolic link to a directory, with the same Windows rule as
/// [`symlink_file`].
fn symlink_dir(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    let created = std::os::unix::fs::symlink(target, link);
    #[cfg(windows)]
    let created = std::os::windows::fs::symlink_dir(target, link);
    privileged(created)
}

fn privileged(created: std::io::Result<()>) -> bool {
    match created {
        Ok(()) => true,
        // ERROR_PRIVILEGE_NOT_HELD
        Err(error) if cfg!(windows) && error.raw_os_error() == Some(1314) => {
            assert!(
                std::env::var_os("CI").is_none(),
                "creating a symbolic link needs the symbolic-link privilege on CI: {error}"
            );
            eprintln!("skipped: creating a symbolic link needs Developer Mode on Windows");
            false
        }
        Err(error) => panic!("cannot create a symbolic link: {error}"),
    }
}

/// Makes `link` an alias of the directory `target` without any privilege: a
/// symbolic link on Unix, a junction on Windows.
fn dir_alias(target: &Path, link: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
    #[cfg(windows)]
    junction(target, link);
}

#[cfg(windows)]
fn junction(target: &Path, link: &Path) {
    let status = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "mklink /J failed");
}

fn refused_at(result: Result<Vec<u8>, ConfinedError>) -> (String, Refusal) {
    match result {
        Err(ConfinedError::Refused { at, reason, .. }) => (at, reason),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn plain_files_read_byte_for_byte() {
    let f = fixture();
    fs::write(f.wt.join("plan.md"), INSIDE).unwrap();
    fs::create_dir_all(f.wt.join("docs/deep")).unwrap();
    fs::write(f.wt.join("docs/deep/plan.md"), INSIDE).unwrap();

    assert_eq!(read_confined(&f.wt, "plan.md").unwrap(), INSIDE);
    assert_eq!(read_confined(&f.wt, "docs/deep/plan.md").unwrap(), INSIDE);
    assert_eq!(
        read_confined(&f.wt, "./docs/./deep/plan.md").unwrap(),
        INSIDE
    );
}

#[test]
fn root_reached_through_an_aliased_ancestor_reads() {
    let f = fixture();
    let real = f.base.path().join("real");
    fs::create_dir_all(real.join("wt/docs")).unwrap();
    fs::write(real.join("wt/docs/plan.md"), INSIDE).unwrap();
    let alias = f.base.path().join("alias");
    dir_alias(&real, &alias);

    assert_eq!(
        read_confined(&alias.join("wt"), "docs/plan.md").unwrap(),
        INSIDE
    );
}

#[test]
fn symlink_to_an_outside_file_is_refused() {
    let f = fixture();
    if !symlink_file(&f.outside.join("secret.txt"), &f.wt.join("plan.md")) {
        return;
    }
    let error = read_confined(&f.wt, "plan.md").unwrap_err();
    assert_eq!(error.to_string(), "`plan.md` is a symbolic link");
    assert_eq!(
        refused_at(Err(error)),
        ("plan.md".to_owned(), Refusal::SymbolicLink)
    );
}

#[test]
fn symlinked_parent_directory_is_refused() {
    let f = fixture();
    if !symlink_dir(&f.outside, &f.wt.join("docs")) {
        return;
    }
    let error = read_confined(&f.wt, "docs/secret.txt").unwrap_err();
    assert_eq!(
        error.to_string(),
        "`docs/secret.txt`: `docs` is a symbolic link"
    );
    assert_eq!(
        refused_at(Err(error)),
        ("docs".to_owned(), Refusal::SymbolicLink)
    );
}

#[test]
fn symlink_pointing_inside_is_refused_too() {
    let f = fixture();
    fs::write(f.wt.join("real.md"), INSIDE).unwrap();
    if !symlink_file(Path::new("real.md"), &f.wt.join("plan.md")) {
        return;
    }
    assert_eq!(
        refused_at(read_confined(&f.wt, "plan.md")),
        ("plan.md".to_owned(), Refusal::SymbolicLink)
    );
}

#[cfg(windows)]
#[test]
fn junction_parent_is_refused() {
    let f = fixture();
    junction(&f.outside, &f.wt.join("docs"));
    assert_eq!(
        refused_at(read_confined(&f.wt, "docs/secret.txt")),
        ("docs".to_owned(), Refusal::SymbolicLink)
    );
}

#[test]
fn missing_entries_are_refused() {
    let f = fixture();
    let error = read_confined(&f.wt, "plan.md").unwrap_err();
    assert_eq!(error.to_string(), "`plan.md` does not exist");
    assert_eq!(
        refused_at(Err(error)),
        ("plan.md".to_owned(), Refusal::NotFound)
    );
    assert_eq!(
        refused_at(read_confined(&f.wt, "docs/plan.md")),
        ("docs".to_owned(), Refusal::NotFound)
    );
}

#[test]
fn a_directory_is_not_a_file() {
    let f = fixture();
    fs::create_dir(f.wt.join("docs")).unwrap();
    fs::write(f.wt.join("a.txt"), INSIDE).unwrap();
    assert_eq!(
        refused_at(read_confined(&f.wt, "docs")),
        ("docs".to_owned(), Refusal::NotARegularFile("directory"))
    );
    assert_eq!(
        refused_at(read_confined(&f.wt, "a.txt/plan.md")),
        ("a.txt".to_owned(), Refusal::NotADirectory)
    );
}

#[cfg(unix)]
#[test]
fn a_fifo_is_refused_without_blocking() {
    let f = fixture();
    let status = std::process::Command::new("mkfifo")
        .arg(f.wt.join("plan.md"))
        .status()
        .unwrap();
    assert!(status.success(), "mkfifo failed");

    // With no writer, a blocking open of a FIFO would never return: read on a
    // thread so a regression fails here instead of hanging the suite.
    let (sender, receiver) = std::sync::mpsc::channel();
    let wt = f.wt.clone();
    std::thread::spawn(move || sender.send(read_confined(&wt, "plan.md")));
    let result = receiver
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("opening a FIFO blocked");
    assert_eq!(
        refused_at(result),
        ("plan.md".to_owned(), Refusal::NotARegularFile("FIFO"))
    );
}

#[cfg(unix)]
#[test]
fn a_device_is_refused() {
    assert_eq!(
        refused_at(read_confined(Path::new("/dev"), "null")),
        (
            "null".to_owned(),
            Refusal::NotARegularFile("character device")
        )
    );
}

#[test]
fn a_root_that_is_an_alias_is_refused_however_it_is_spelled() {
    let f = fixture();
    fs::write(f.wt.join("plan.md"), INSIDE).unwrap();
    let link = f.base.path().join("link");
    dir_alias(&f.wt, &link);

    for root in [link.clone(), link.join(""), link.join(".")] {
        match read_confined(&root, "plan.md") {
            Err(ConfinedError::Root {
                reason: Refusal::SymbolicLink,
                ..
            }) => {}
            other => panic!("{} was not refused: {other:?}", root.display()),
        }
    }
    assert!(matches!(
        read_confined(&f.wt.join(".."), "wt/plan.md"),
        Err(ConfinedError::InvalidRoot { .. })
    ));
}

#[test]
fn paths_that_are_not_plain_relative_paths_are_refused() {
    let f = fixture();
    fs::write(f.wt.join("plan.md"), INSIDE).unwrap();
    for relative in [
        "",
        "/plan.md",
        "docs//plan.md",
        "../wt/plan.md",
        "docs/..",
        "docs\\plan.md",
        "c:plan.md",
        "docs/.",
        ".",
        "plan\0.md",
    ] {
        assert!(
            matches!(
                read_confined(&f.wt, relative),
                Err(ConfinedError::InvalidPath { .. })
            ),
            "{relative:?} was not refused"
        );
    }
}
