//! `owlshift self-update` end to end (OWL-541): the bench's command line,
//! `owlshift-cli-bench`, which trusts the bench's release key, put in a
//! folder of its own as an install made by hand, against a local server
//! standing for GitHub's releases. Each case is a path prefix of that
//! server: `<case>/releases/latest` redirects to `<case>/releases/tag/v<v>`,
//! and `<case>/releases/download/v<v>/<file>` serves the release's files.
//!
//! What the unit tests of `self_update_cmd.rs` pin (the signature of
//! minisign itself, the parsing of `SHA256SUMS`, the paths of Homebrew and
//! cargo) is not repeated here; these cases check that each refusal leaves
//! the binary and its folder as they were.

#![cfg(unix)]

use std::collections::HashMap;
use std::fs;
use std::io::Cursor;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};

const BIN: &str = env!("CARGO_BIN_EXE_owlshift-cli-bench");

/// The secret half of `owlshift_testkit::release::BENCH_RELEASE_KEY`, made
/// with the `minisign` crate on 2026-11-12: test only, it signs nothing
/// else.
const BENCH_RELEASE_SECRET: &str = "untrusted comment: Owlshift test bench release key: test only
RWQAAEIyAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAy+7luUqrKpMXsAB9Dn1TVQi+6AUWCszGha8/+O7PsJ3rvo6ptpI37f0s2Ls93JBgJzbVOsC7f7On3P4LVEjQZJVCgoyLc/Jktiub3hDJyZ479JH3HIwL5OQjm7HV1knzh9GtE9ckzqk=
";

/// The bench's own version, the workspace's.
const OWN: &str = env!("CARGO_PKG_VERSION");

fn target() -> &'static str {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("x86_64", "linux") => "x86_64-unknown-linux-musl",
        ("aarch64", "linux") => "aarch64-unknown-linux-musl",
        ("x86_64", "macos") => "x86_64-apple-darwin",
        ("aarch64", "macos") => "aarch64-apple-darwin",
        other => panic!("no release target for {other:?}"),
    }
}

enum Route {
    Redirect(String),
    File(Vec<u8>),
}

/// The local server: its routes, and every path asked of it, in order.
struct Releases {
    port: u16,
    routes: Arc<Mutex<HashMap<String, Route>>>,
    asked: Arc<Mutex<Vec<String>>>,
}

impl Releases {
    fn start() -> Self {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let routes: Arc<Mutex<HashMap<String, Route>>> = Arc::default();
        let asked: Arc<Mutex<Vec<String>>> = Arc::default();
        let (served, log) = (Arc::clone(&routes), Arc::clone(&asked));
        std::thread::spawn(move || {
            for request in server.incoming_requests() {
                let path = request.url().to_owned();
                log.lock().unwrap().push(path.clone());
                let response = match served.lock().unwrap().get(&path) {
                    Some(Route::Redirect(to)) => tiny_http::Response::from_data(Vec::new())
                        .with_status_code(302)
                        .with_header(
                            tiny_http::Header::from_bytes("Location", to.as_str()).unwrap(),
                        ),
                    Some(Route::File(bytes)) => tiny_http::Response::from_data(bytes.clone()),
                    None => tiny_http::Response::from_data(Vec::new()).with_status_code(404),
                };
                let _ = request.respond(response);
            }
        });
        Self {
            port,
            routes,
            asked,
        }
    }

    fn url(&self, case: &str) -> String {
        format!("http://127.0.0.1:{}/{case}", self.port)
    }

    /// `case`'s latest release is `version`, whose files are `files`.
    fn publish(&self, case: &str, version: &str, files: Vec<(String, Vec<u8>)>) {
        let mut routes = self.routes.lock().unwrap();
        routes.insert(
            format!("/{case}/releases/latest"),
            Route::Redirect(format!("/{case}/releases/tag/v{version}")),
        );
        for (name, bytes) in files {
            routes.insert(
                format!("/{case}/releases/download/v{version}/{name}"),
                Route::File(bytes),
            );
        }
    }

    /// The paths asked under `case`.
    fn asked(&self, case: &str) -> Vec<String> {
        let prefix = format!("/{case}/");
        self.asked
            .lock()
            .unwrap()
            .iter()
            .filter(|path| path.starts_with(&prefix))
            .map(|path| path[prefix.len()..].to_owned())
            .collect()
    }
}

/// A release of `version` as the workflow makes it, signed by `key` with
/// the trusted comment `owlshift <version>`: its archive (a folder holding
/// a stand-in `owlshift` that answers `--version`, and `LICENSE`, which the
/// update leaves out), `SHA256SUMS` and `SHA256SUMS.minisig`.
struct Release {
    archive_name: String,
    archive: Vec<u8>,
    sums: Vec<u8>,
    signature: Vec<u8>,
}

impl Release {
    fn make(version: &str, key: &minisign::SecretKey, scratch: &Path) -> Self {
        let folder = format!("owlshift-{version}-{}", target());
        let stage = scratch.join(format!("stage-{version}"));
        fs::create_dir_all(stage.join(&folder)).unwrap();
        fs::write(
            stage.join(&folder).join("owlshift"),
            format!("#!/bin/sh\necho 'owlshift {version}'\n"),
        )
        .unwrap();
        fs::write(stage.join(&folder).join("LICENSE"), "Apache-2.0\n").unwrap();
        let archive_name = format!("{folder}.tar.gz");
        let packed = Command::new("tar")
            .arg("-czf")
            .arg(stage.join(&archive_name))
            .arg("-C")
            .arg(&stage)
            .arg(&folder)
            .status()
            .unwrap();
        assert!(packed.success());
        let archive = fs::read(stage.join(&archive_name)).unwrap();
        let sum: String = Sha256::digest(&archive)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let sums = format!("{sum}  {archive_name}\n").into_bytes();
        let release = Self {
            archive_name,
            archive,
            signature: Vec::new(),
            sums,
        };
        release.signed(key, &format!("owlshift {version}"))
    }

    fn signed(mut self, key: &minisign::SecretKey, comment: &str) -> Self {
        self.signature = minisign::sign(None, key, Cursor::new(&self.sums), Some(comment), None)
            .unwrap()
            .into_string()
            .into_bytes();
        self
    }

    /// Its files, the archive served as `archive_name`.
    fn files_as(self, archive_name: &str) -> Vec<(String, Vec<u8>)> {
        vec![
            (archive_name.to_owned(), self.archive),
            ("SHA256SUMS".to_owned(), self.sums),
            ("SHA256SUMS.minisig".to_owned(), self.signature),
        ]
    }

    fn files(self) -> Vec<(String, Vec<u8>)> {
        let name = self.archive_name.clone();
        self.files_as(&name)
    }
}

fn bench_key() -> minisign::SecretKey {
    minisign::SecretKeyBox::from_string(BENCH_RELEASE_SECRET)
        .unwrap()
        .into_unencrypted_secret_key()
        .unwrap()
}

/// The bench installed by hand as `<folder>/owlshift`: a hard link, which
/// the update's rename replaces without touching the bench, or a copy where
/// the two folders are on different file systems.
fn install(folder: &Path) -> PathBuf {
    fs::create_dir_all(folder).unwrap();
    let at = folder.join("owlshift");
    if fs::hard_link(BIN, &at).is_err() {
        fs::copy(BIN, &at).unwrap();
    }
    at
}

/// What tells a file apart from a replacement or a change in place.
fn identity(path: &Path) -> (u64, u64, u64, i64, i64) {
    let meta = fs::metadata(path).unwrap();
    (
        meta.dev(),
        meta.ino(),
        meta.len(),
        meta.mtime(),
        meta.mtime_nsec(),
    )
}

fn entries(folder: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(folder)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn self_update(binary: &Path, base: &str, args: &[&str], home: &Path) -> Output {
    Command::new(binary)
        .arg("self-update")
        .args(args)
        .current_dir(home)
        .env("OWLSHIFT_UPDATE_URL", base)
        .env("OWLSHIFT_DATA_DIR", home.join("data"))
        .env("OWLSHIFT_CONFIG_DIR", home.join("config"))
        .env("HOME", home)
        .env_remove("CARGO_HOME")
        .output()
        .unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// A newer release, signed by the bench's key, replaces the binary at its
/// own path: the new one answers `--version`, nothing else is left in the
/// folder, and the command ends by saying to run `owlshift start`.
#[test]
fn self_update_installs_a_newer_signed_release() {
    let dir = tempfile::tempdir().unwrap();
    let releases = Releases::start();
    releases.publish(
        "good",
        "9.9.9",
        Release::make("9.9.9", &bench_key(), dir.path()).files(),
    );
    let bin = dir.path().join("bin");
    let binary = install(&bin);

    let updated = self_update(&binary, &releases.url("good"), &[], dir.path());
    assert!(updated.status.success(), "{}", text(&updated.stderr));
    let said = text(&updated.stdout);
    assert!(
        said.trim_end()
            .lines()
            .last()
            .unwrap()
            .contains("Run `owlshift start`"),
        "{said}"
    );
    assert!(
        said.contains(&format!("Updated Owlshift from {OWN} to 9.9.9")),
        "{said}"
    );
    let answer = Command::new(&binary).arg("--version").output().unwrap();
    assert_eq!(text(&answer.stdout), "owlshift 9.9.9\n");
    assert_eq!(entries(&bin), ["owlshift"]);
    assert_eq!(fs::metadata(&binary).unwrap().mode() & 0o777, 0o755);
}

/// An altered archive, an altered `SHA256SUMS`, a signature of another key,
/// and a redirect to a newer version serving an older release, signed as
/// that older version: each is refused, the binary and its folder left as
/// they were. An older latest release is refused before any download, an
/// equal one is up to date, and `--check` reads only the redirect.
#[test]
fn self_update_refuses_and_leaves_the_binary_as_it_was() {
    let dir = tempfile::tempdir().unwrap();
    let scratch = dir.path().join("scratch");
    let releases = Releases::start();
    let key = bench_key();

    let mut archive = Release::make("9.9.9", &key, &scratch);
    archive.archive.push(0);
    releases.publish("archive", "9.9.9", archive.files());

    let mut sums = Release::make("9.9.9", &key, &scratch);
    let name = sums.archive_name.clone();
    // One digit of the archive's checksum, changed after signing.
    sums.sums[0] = if sums.sums[0] == b'0' { b'1' } else { b'0' };
    releases.publish("sums", "9.9.9", sums.files_as(&name));

    let other = minisign::KeyPair::generate_unencrypted_keypair()
        .unwrap()
        .sk;
    releases.publish(
        "key",
        "9.9.9",
        Release::make("9.9.9", &other, &scratch).files(),
    );

    let wanted = format!("owlshift-9.9.9-{}.tar.gz", target());
    let older = Release::make("0.0.1", &key, &scratch);
    releases.publish("older-signed", "9.9.9", older.files_as(&wanted));

    releases.publish(
        "older",
        "0.0.1",
        Release::make("0.0.1", &key, &scratch).files(),
    );
    releases.publish("same", OWN, Vec::new());

    let bin = dir.path().join("bin");
    let binary = install(&bin);
    let before = identity(&binary);
    let unchanged = |case: &str| {
        assert_eq!(identity(&binary), before, "{case}: the binary changed");
        assert_eq!(entries(&bin), ["owlshift"], "{case}: something was left");
    };

    for (case, refusal) in [
        ("archive", format!("the checksum of {wanted} is")),
        (
            "sums",
            "the signature of SHA256SUMS is not Owlshift's".to_owned(),
        ),
        (
            "key",
            "the signature of SHA256SUMS is not Owlshift's".to_owned(),
        ),
        (
            "older-signed",
            "the release named 9.9.9 is signed as 0.0.1".to_owned(),
        ),
        (
            "older",
            format!("the latest release, 0.0.1, is older than this binary, {OWN}"),
        ),
    ] {
        let refused = self_update(&binary, &releases.url(case), &[], dir.path());
        assert!(
            !refused.status.success(),
            "{case}: {}",
            text(&refused.stdout)
        );
        let error = text(&refused.stderr);
        assert!(
            error.contains(&refusal) && error.contains("nothing installed"),
            "{case}: {error}"
        );
        unchanged(case);
    }
    assert_eq!(releases.asked("older"), ["releases/latest"]);

    let same = self_update(&binary, &releases.url("same"), &[], dir.path());
    assert!(same.status.success(), "{}", text(&same.stderr));
    assert_eq!(
        text(&same.stdout),
        format!("This is the latest Owlshift, {OWN}.\n")
    );
    unchanged("same");

    let check = self_update(&binary, &releases.url("key"), &["--check"], dir.path());
    assert!(check.status.success(), "{}", text(&check.stderr));
    assert_eq!(
        text(&check.stdout),
        format!(
            "Owlshift 9.9.9 is available; this is {OWN}: `owlshift self-update` installs it.\n"
        )
    );
    unchanged("--check");
    // The refused update above read `key`'s files; `--check` adds only the
    // redirect.
    let download = |file: &str| format!("releases/download/v9.9.9/{file}");
    assert_eq!(
        releases.asked("key"),
        [
            "releases/latest".to_owned(),
            download("SHA256SUMS"),
            download("SHA256SUMS.minisig"),
            download(&wanted),
            "releases/latest".to_owned(),
        ]
    );
}

/// A binary under Homebrew's Cellar is refused, naming the command that
/// updates it, before anything is asked of the releases.
#[test]
fn self_update_refuses_a_package_managed_binary() {
    let dir = tempfile::tempdir().unwrap();
    let releases = Releases::start();
    let bin = dir.path().join("Cellar/owlshift/0.1.0/bin");
    let binary = install(&bin);
    let before = identity(&binary);

    let refused = self_update(&binary, &releases.url("brew"), &[], dir.path());
    assert!(!refused.status.success());
    assert!(
        text(&refused.stderr)
            .contains("was installed by Homebrew: `brew upgrade owlshift` updates it"),
        "{}",
        text(&refused.stderr)
    );
    assert_eq!(identity(&binary), before);
    assert!(releases.asked("brew").is_empty());
}
