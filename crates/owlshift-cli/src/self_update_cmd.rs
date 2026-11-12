//! `owlshift self-update [--check]` (OWL-541; build plan, "P9 · Open-source
//! readiness", part 11): updates an install made by hand, by `install.sh` or
//! from an archive, to the latest release.
//!
//! The one thing trusted is the minisign key built into the binary
//! ([`crate::RELEASE_KEY`]): the address the releases are read from, the
//! redirect that names the latest one and the files are not. In order:
//!
//! 1. An install a package manager or `cargo` made is refused, naming the
//!    command that updates it.
//! 2. The latest version is read from where `releases/latest` redirects; an
//!    older or equal one stops here, nothing downloaded.
//! 3. The archive of this binary's target, `SHA256SUMS` and
//!    `SHA256SUMS.minisig` are downloaded.
//! 4. The signature of `SHA256SUMS` is verified with the built-in key,
//!    prehashed only, as minisign 0.11 signs; its trusted comment, which the
//!    signature covers, must be `owlshift <version>` for the version the
//!    redirect named, and that signed version must be newer than this binary.
//! 5. Only then is `SHA256SUMS` read: it must list the archive on exactly one
//!    line, and the archive's SHA-256 must be that line's.
//! 6. `<name>/owlshift` alone is extracted, by the system's `tar`, as
//!    `install.sh` does; it must be a regular file.
//! 7. It is copied beside the binary as `.owlshift.<pid>`, made executable
//!    and synced; it must answer `--version` with `owlshift <version>`.
//! 8. It replaces the binary by a rename, atomic in that folder: a running
//!    daemon keeps its old file (build plan, R3).
//!
//! Any refusal leaves the binary as it was and removes the staged file.
//! `OWLSHIFT_UPDATE_URL` replaces GitHub's page of the repository, for the
//! tests' local server or a mirror; it changes nothing of what is trusted.
//!
//! Not guarded: SHA256SUMS carries no "latest" mark, so whoever controls the
//! redirect can hold an install at any signed release newer than its own,
//! never move it back.

use std::cmp::Ordering;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use owlshift_adapters::release::{self, ARCHIVE_LIMIT, Latest, Releases, SMALL_FILE_LIMIT};
use owlshift_runner::config::OWLSHIFT_VERSION;
use sha2::{Digest, Sha256};

use crate::fail;

/// The variable that replaces GitHub's page of the repository.
pub const UPDATE_URL: &str = "OWLSHIFT_UPDATE_URL";

/// The repository, for the command that builds from source.
const REPOSITORY: &str = "https://github.com/pitchopp/owlshift";

/// `owlshift self-update`, `--check` only reading the latest version.
pub fn run(check: bool, release_key: &str) -> ExitCode {
    if cfg!(windows) {
        return fail(
            "`owlshift self-update` updates an install on Linux or macOS; on Windows, Owlshift \
             runs under WSL2",
        );
    }
    let base = std::env::var(UPDATE_URL)
        .ok()
        .filter(|base| !base.is_empty())
        .unwrap_or_else(|| release::DEFAULT_BASE.to_owned());
    let releases = match Releases::new(&base) {
        Ok(releases) => releases,
        Err(error) => return fail(&format!("{UPDATE_URL}: {error}")),
    };
    let exe = match std::env::current_exe().and_then(|exe| exe.canonicalize()) {
        Ok(exe) => exe,
        Err(error) => return fail(&format!("this program's own path: {error}")),
    };
    let managed = package_managed(
        &exe,
        std::env::var_os("CARGO_HOME").map(PathBuf::from).as_deref(),
        owlshift_platform::paths::home_dir().as_deref(),
    );
    if check {
        return match releases.latest() {
            Ok(latest) => {
                println!("{}", checked(&latest, OWLSHIFT_VERSION, managed.as_ref()));
                ExitCode::SUCCESS
            }
            Err(error) => fail(&error),
        };
    }
    if let Some(managed) = managed {
        return fail(&format!(
            "{} was installed by {}: `{}` updates it",
            exe.display(),
            managed.by,
            managed.command
        ));
    }
    let update = Update {
        releases: &releases,
        key: release_key,
        own: OWLSHIFT_VERSION,
        exe: &exe,
    };
    match update.install() {
        Ok(Installed::NoRelease) => {
            println!("Owlshift has no published release yet: nothing to install.");
            ExitCode::SUCCESS
        }
        Ok(Installed::UpToDate) => {
            println!("This is the latest Owlshift, {OWLSHIFT_VERSION}.");
            ExitCode::SUCCESS
        }
        Ok(Installed::Updated { to }) => {
            println!(
                "Updated Owlshift from {OWLSHIFT_VERSION} to {to}: {}.",
                exe.display()
            );
            println!(
                "Run `owlshift start` in a served project so that the background service, if \
                 it runs, drains onto the new version."
            );
            ExitCode::SUCCESS
        }
        Err(error) => fail(&error),
    }
}

/// What `--check` says of `latest`, this binary being `own`.
fn checked(latest: &Latest, own: &str, managed: Option<&Managed>) -> String {
    let version = match latest {
        Latest::NoneYet => return "Owlshift has no published release yet.".to_owned(),
        Latest::Version(version) => version,
    };
    match newer(version, own) {
        Ok(Ordering::Greater) => {
            let command = managed.map_or("owlshift self-update", |managed| managed.command);
            format!("Owlshift {version} is available; this is {own}: `{command}` installs it.")
        }
        Ok(_) => format!("This is the latest Owlshift, {own}."),
        Err(error) => format!("The latest release is {version}, this is {own}: {error}."),
    }
}

/// The package manager, or `cargo`, that installed a binary, and the
/// command that updates it.
#[derive(Debug, PartialEq, Eq)]
struct Managed {
    by: &'static str,
    command: &'static str,
}

/// Who installed the binary at `exe`, its canonical path, when not a person:
/// Homebrew, whose files are under a `Cellar` folder; `cargo install`, whose
/// binaries are in `<cargo home>/bin` (`CARGO_HOME`, or `~/.cargo`); `cargo
/// build`, under a `target` folder beside a `Cargo.toml`. Other package
/// managers are not told: their folders are not writable by the user, and
/// the update then fails before the binary is touched.
fn package_managed(exe: &Path, cargo_home: Option<&Path>, home: Option<&Path>) -> Option<Managed> {
    if exe.components().any(|part| part.as_os_str() == "Cellar") {
        return Some(Managed {
            by: "Homebrew",
            command: "brew upgrade owlshift",
        });
    }
    let cargo_home = cargo_home
        .filter(|path| path.is_absolute())
        .map(Path::to_owned)
        .or_else(|| home.map(|home| home.join(".cargo")));
    if let Some(cargo_home) = cargo_home {
        let bin = cargo_home.join("bin");
        // A symlink on the way, such as macOS's /var to /private/var, would
        // hide it from the canonical path.
        let bin = bin.canonicalize().unwrap_or(bin);
        if exe.parent() == Some(bin.as_path()) {
            return Some(Managed {
                by: "cargo install",
                command: "cargo install --git https://github.com/pitchopp/owlshift --locked \
                          owlshift-cli",
            });
        }
    }
    let built = exe.ancestors().skip(1).any(|folder| {
        folder.file_name().is_some_and(|name| name == "target")
            && folder
                .parent()
                .is_some_and(|tree| tree.join("Cargo.toml").is_file())
    });
    built.then_some(Managed {
        by: "cargo build",
        command: "git pull && cargo build --release",
    })
}

/// The release target of this binary, as the release names its archives.
fn target() -> Option<&'static str> {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("x86_64", "linux") => Some("x86_64-unknown-linux-musl"),
        ("aarch64", "linux") => Some("aarch64-unknown-linux-musl"),
        ("x86_64", "macos") => Some("x86_64-apple-darwin"),
        ("aarch64", "macos") => Some("aarch64-apple-darwin"),
        _ => None,
    }
}

/// The folder an archive holds, `owlshift-<version>-<target>`, which names
/// the archive too, with `.tar.gz`.
fn folder_name(version: &str, target: &str) -> String {
    format!("owlshift-{version}-{target}")
}

/// How `candidate` compares to `own` by semver precedence, build metadata
/// ignored.
fn newer(candidate: &str, own: &str) -> Result<Ordering, String> {
    let parse = |version: &str| {
        semver::Version::parse(version)
            .map_err(|error| format!("\"{version}\" is not a version: {error}"))
    };
    Ok(parse(candidate)?.cmp_precedence(&parse(own)?))
}

/// The version `signature` signs `sums` for, with the public key `key`
/// (minisign's two-line file): the signature must be prehashed, by that
/// key, and its trusted comment, which the signature covers, `owlshift
/// <version>`.
fn signed_version(key: &str, sums: &[u8], signature: &[u8]) -> Result<String, String> {
    let key = minisign_verify::PublicKey::decode(key)
        .map_err(|error| format!("the key built into this binary does not read: {error}"))?;
    let refused = |why: String| Err(format!("{why}: nothing installed"));
    let Ok(signature) = std::str::from_utf8(signature) else {
        return refused("SHA256SUMS.minisig is not text".to_owned());
    };
    let signature = match minisign_verify::Signature::decode(signature) {
        Ok(signature) => signature,
        Err(error) => return refused(format!("SHA256SUMS.minisig does not read: {error}")),
    };
    if let Err(error) = key.verify(sums, &signature, false) {
        return refused(format!(
            "the signature of SHA256SUMS is not Owlshift's ({error})"
        ));
    }
    match signature.trusted_comment().strip_prefix("owlshift ") {
        Some(version) if release::is_version(version) => Ok(version.to_owned()),
        _ => refused(format!(
            "the signature of SHA256SUMS names \"{}\", not a version of Owlshift",
            signature.trusted_comment()
        )),
    }
}

/// The SHA-256 `sums`, a `sha256sum` output, lists for `archive`: one line,
/// `<64 lowercase hex digits>  <archive>`, and only one.
fn expected_checksum(sums: &[u8], archive: &str) -> Result<String, String> {
    let Ok(sums) = std::str::from_utf8(sums) else {
        return Err("SHA256SUMS is not text: nothing installed".to_owned());
    };
    let listed: Vec<&str> = sums
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let sum = fields.next()?;
            (fields.next() == Some(archive) && fields.next().is_none()).then_some(sum)
        })
        .collect();
    match listed.as_slice() {
        [sum] if sum.len() == 64 && sum.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) => {
            Ok((*sum).to_owned())
        }
        [] => Err(format!(
            "SHA256SUMS does not list {archive}: nothing installed"
        )),
        _ => Err(format!(
            "SHA256SUMS does not list {archive} once with a SHA-256 checksum: nothing installed"
        )),
    }
}

/// What an update did.
#[derive(Debug, PartialEq, Eq)]
enum Installed {
    NoRelease,
    UpToDate,
    Updated { to: String },
}

/// An update of the binary at `exe`, of version `own`, from `releases`,
/// trusting `key`.
struct Update<'a> {
    releases: &'a Releases,
    key: &'a str,
    own: &'a str,
    exe: &'a Path,
}

impl Update<'_> {
    fn install(&self) -> Result<Installed, String> {
        let target = target().ok_or_else(|| {
            format!(
                "no release is built for this machine ({} {}): build from source, `cargo \
                 install --git {REPOSITORY} --locked owlshift-cli`",
                std::env::consts::OS,
                std::env::consts::ARCH
            )
        })?;
        let version = match self.releases.latest()? {
            Latest::NoneYet => return Ok(Installed::NoRelease),
            Latest::Version(version) => version,
        };
        match newer(&version, self.own)? {
            Ordering::Equal => return Ok(Installed::UpToDate),
            Ordering::Less => {
                return Err(format!(
                    "the latest release, {version}, is older than this binary, {}: nothing \
                     installed",
                    self.own
                ));
            }
            Ordering::Greater => {}
        }
        let folder = folder_name(&version, target);
        let archive_name = format!("{folder}.tar.gz");
        println!(
            "Downloading Owlshift {version} for {target} from {}…",
            self.releases.base()
        );
        let sums = self
            .releases
            .file(&version, "SHA256SUMS", SMALL_FILE_LIMIT)?;
        let signature = self
            .releases
            .file(&version, "SHA256SUMS.minisig", SMALL_FILE_LIMIT)?;
        let archive = self.releases.file(&version, &archive_name, ARCHIVE_LIMIT)?;

        // The signature first: nothing of SHA256SUMS is read before it.
        let signed = signed_version(self.key, &sums, &signature)?;
        if signed != version {
            return Err(format!(
                "the release named {version} is signed as {signed}: nothing installed"
            ));
        }
        if newer(&signed, self.own)? != Ordering::Greater {
            return Err(format!(
                "the release signed as {signed} is not newer than this binary, {}: nothing \
                 installed",
                self.own
            ));
        }
        let expected = expected_checksum(&sums, &archive_name)?;
        let actual = hex(&Sha256::digest(&archive));
        if actual != expected {
            return Err(format!(
                "the checksum of {archive_name} is {actual}, not {expected} as SHA256SUMS says: \
                 the download is damaged or is not the release's, nothing installed"
            ));
        }
        println!("Verified the signature of SHA256SUMS and the checksum of {archive_name}.");

        let work = tempfile::tempdir()
            .map_err(|error| format!("a temporary folder to open the archive in: {error}"))?;
        let extracted = extract(work.path(), &archive_name, &archive, &folder)?;
        let staged = Staged::beside(self.exe, &extracted)?;
        staged.answers(&version)?;
        staged.replace(self.exe)?;
        Ok(Installed::Updated { to: version })
    }
}

/// Extracts `<folder>/owlshift` alone from `archive` into `work`, with the
/// system's `tar`: a regular file, or a refusal.
fn extract(
    work: &Path,
    archive_name: &str,
    archive: &[u8],
    folder: &str,
) -> Result<PathBuf, String> {
    let at = work.join(archive_name);
    fs::write(&at, archive).map_err(|error| format!("{}: {error}", at.display()))?;
    let member = format!("{folder}/owlshift");
    let tar = owlshift_platform::process::find_executable("tar")
        .ok_or("`owlshift self-update` needs tar, which is not on the PATH")?;
    let opened = owlshift_platform::process::run(
        &tar,
        &[
            "-xzf",
            &at.to_string_lossy(),
            "--no-same-owner",
            "-C",
            &work.to_string_lossy(),
            &member,
        ],
        None,
        Duration::from_secs(60),
    )
    .map_err(|error| format!("tar could not open {archive_name}: {error}"))?;
    if !opened.success() {
        return Err(format!(
            "{archive_name} does not hold {member}: nothing installed ({})",
            String::from_utf8_lossy(&opened.stderr).trim()
        ));
    }
    let binary = work.join(folder).join("owlshift");
    match fs::symlink_metadata(&binary) {
        Ok(meta) if meta.file_type().is_file() => Ok(binary),
        _ => Err(format!(
            "{member} in {archive_name} is not a regular file: nothing installed"
        )),
    }
}

/// The new binary, staged beside the one it replaces, removed unless it
/// replaced it.
struct Staged {
    path: PathBuf,
    placed: bool,
}

impl Staged {
    /// A copy of `binary` as `.owlshift.<pid>` in the folder of `exe`,
    /// executable and synced to disk.
    fn beside(exe: &Path, binary: &Path) -> Result<Self, String> {
        let folder = exe
            .parent()
            .ok_or_else(|| format!("{} has no folder", exe.display()))?;
        let path = folder.join(format!(".owlshift.{}", std::process::id()));
        let cannot = |error: io::Error| {
            format!(
                "could not write in {}: {error}; nothing installed",
                folder.display()
            )
        };
        let mut file = File::create_new(&path).map_err(cannot)?;
        let staged = Self {
            path: path.clone(),
            placed: false,
        };
        let mut source = File::open(binary).map_err(cannot)?;
        io::copy(&mut source, &mut file).map_err(cannot)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o755))
                .map_err(cannot)?;
        }
        file.sync_all().map_err(cannot)?;
        drop(file);
        Ok(staged)
    }

    /// Whether it runs on this machine and says it is `owlshift <version>`.
    fn answers(&self, version: &str) -> Result<(), String> {
        let ran = owlshift_platform::process::run(
            &self.path,
            &["--version"],
            None,
            Duration::from_secs(30),
        )
        .map_err(|error| format!("the new binary did not run: {error}; nothing installed"))?;
        let said = String::from_utf8_lossy(&ran.stdout);
        let first = said.lines().next().unwrap_or_default();
        if !ran.success() || first != format!("owlshift {version}") {
            return Err(format!(
                "the new binary answers \"{first}\" to --version, not \"owlshift {version}\": \
                 nothing installed"
            ));
        }
        Ok(())
    }

    /// Renames it over `exe`, then syncs the folder so the rename lasts.
    fn replace(mut self, exe: &Path) -> Result<(), String> {
        fs::rename(&self.path, exe)
            .map_err(|error| format!("could not replace {}: {error}", exe.display()))?;
        self.placed = true;
        if let Some(folder) = exe.parent()
            && let Ok(folder) = File::open(folder)
        {
            // Best effort: the rename is already done.
            let _ = folder.sync_all();
        }
        Ok(())
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if !self.placed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_KEY: &str = include_str!("../tests/fixtures/self-update/minisign.pub");
    const FIXTURE_SUMS: &[u8] = include_bytes!("../tests/fixtures/self-update/SHA256SUMS");
    const FIXTURE_SIGNATURE: &[u8] =
        include_bytes!("../tests/fixtures/self-update/SHA256SUMS.minisig");

    /// The fixture was signed on 2026-11-12 by minisign 0.11, the version
    /// the release workflow signs with, `-t "owlshift 9.9.9"`: prehashed,
    /// its signature line starting `RU`. A signature of the release's own
    /// tool is read as the release's will be.
    #[test]
    fn a_signature_of_minisign_itself_gives_the_version_it_signs() {
        assert!(
            std::str::from_utf8(FIXTURE_SIGNATURE)
                .unwrap()
                .lines()
                .nth(1)
                .unwrap()
                .starts_with("RU")
        );
        assert_eq!(
            signed_version(FIXTURE_KEY, FIXTURE_SUMS, FIXTURE_SIGNATURE),
            Ok("9.9.9".to_owned())
        );
    }

    /// Each refusal says nothing is installed: SHA256SUMS changed after
    /// signing, a signature by another key, a trusted comment changed, a
    /// signature that does not read.
    #[test]
    fn a_signature_of_anything_else_is_refused() {
        let mut altered = FIXTURE_SUMS.to_vec();
        altered[0] = b'c';
        let signature = std::str::from_utf8(FIXTURE_SIGNATURE).unwrap();
        let relabelled = signature.replace("owlshift 9.9.9", "owlshift 9.9.8");
        for (key, sums, signature, why) in [
            (
                FIXTURE_KEY,
                altered.as_slice(),
                signature,
                "is not Owlshift's",
            ),
            (
                crate::RELEASE_KEY,
                FIXTURE_SUMS,
                signature,
                "is not Owlshift's",
            ),
            (
                FIXTURE_KEY,
                FIXTURE_SUMS,
                relabelled.as_str(),
                "is not Owlshift's",
            ),
            (
                FIXTURE_KEY,
                FIXTURE_SUMS,
                "untrusted comment: x\nRU\n",
                "does not read",
            ),
        ] {
            let refused = signed_version(key, sums, signature.as_bytes()).unwrap_err();
            assert!(
                refused.contains(why) && refused.ends_with("nothing installed"),
                "{refused}"
            );
        }
    }

    /// The archive's line, exactly once and well formed, as `install.sh`
    /// reads it.
    #[test]
    fn sha256sums_lists_the_archive_once() {
        let name = "owlshift-9.9.9-x86_64-unknown-linux-musl.tar.gz";
        let a = "a".repeat(64);
        assert_eq!(
            expected_checksum(FIXTURE_SUMS, name),
            Ok(a.clone()),
            "the fixture's"
        );
        for (sums, why) in [
            (format!("{a}  other-{name}\n"), "does not list"),
            (format!("{a}  {name}\n{a}  {name}\n"), "once"),
            (format!("{}  {name}\n", "A".repeat(64)), "once"),
            (format!("{}  {name}\n", "a".repeat(63)), "once"),
            (format!("{a}  {name} extra\n"), "does not list"),
        ] {
            let refused = expected_checksum(sums.as_bytes(), name).unwrap_err();
            assert!(refused.contains(why), "{sums}: {refused}");
        }
    }

    #[test]
    fn versions_compare_by_precedence() {
        assert_eq!(newer("0.2.0", "0.1.0"), Ok(Ordering::Greater));
        assert_eq!(newer("0.1.0+other", "0.1.0"), Ok(Ordering::Equal));
        assert_eq!(newer("0.1.0-rc.1", "0.1.0"), Ok(Ordering::Less));
        assert_eq!(newer("0.0.9", "0.1.0"), Ok(Ordering::Less));
        assert!(newer("x", "0.1.0").is_err());
    }

    /// Homebrew's Cellar, `cargo install`'s bin folder, symlinked or not,
    /// and a `cargo build` tree are told; a folder of the person's is not.
    #[cfg(unix)]
    #[test]
    fn an_install_by_a_package_manager_or_cargo_is_told() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let cellar = root.join("Cellar/owlshift/0.1.0/bin/owlshift");
        assert_eq!(
            package_managed(&cellar, None, None).map(|m| m.command),
            Some("brew upgrade owlshift")
        );

        let cargo = root.join("cargo");
        fs::create_dir_all(cargo.join("bin")).unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&cargo, &link).unwrap();
        let installed = cargo.join("bin/owlshift");
        for (cargo_home, home, expected) in [
            (Some(cargo.as_path()), None, Some("cargo install")),
            (Some(link.as_path()), None, Some("cargo install")),
            // `<home>/.cargo/bin` is another folder.
            (None, Some(root.as_path()), None),
            (Some(Path::new("relative")), Some(root.as_path()), None),
        ] {
            assert_eq!(
                package_managed(&installed, cargo_home, home).map(|m| m.by),
                expected,
                "{cargo_home:?} {home:?}"
            );
        }
        // `~/.cargo/bin`, when CARGO_HOME is unset or not absolute.
        let default = root.join(".cargo/bin");
        fs::create_dir_all(&default).unwrap();
        for cargo_home in [None, Some(Path::new("relative"))] {
            assert_eq!(
                package_managed(&default.join("owlshift"), cargo_home, Some(&root)).map(|m| m.by),
                Some("cargo install")
            );
        }

        let tree = root.join("checkout");
        fs::create_dir_all(tree.join("target/release")).unwrap();
        let built = tree.join("target/release/owlshift");
        assert_eq!(package_managed(&built, None, None), None, "no Cargo.toml");
        fs::write(tree.join("Cargo.toml"), "").unwrap();
        assert_eq!(
            package_managed(&built, None, None).map(|m| m.by),
            Some("cargo build")
        );

        let by_hand = root.join(".local/bin/owlshift");
        assert_eq!(package_managed(&by_hand, Some(&cargo), Some(&root)), None);
    }

    #[test]
    fn check_says_what_to_run() {
        let latest = Latest::Version("0.2.0".to_owned());
        assert_eq!(
            checked(&latest, "0.1.0", None),
            "Owlshift 0.2.0 is available; this is 0.1.0: `owlshift self-update` installs it."
        );
        let brew = Managed {
            by: "Homebrew",
            command: "brew upgrade owlshift",
        };
        assert!(checked(&latest, "0.1.0", Some(&brew)).contains("`brew upgrade owlshift`"));
        assert_eq!(
            checked(&latest, "0.2.0", None),
            "This is the latest Owlshift, 0.2.0."
        );
        assert_eq!(
            checked(&Latest::NoneYet, "0.2.0", None),
            "Owlshift has no published release yet."
        );
    }
}
