//! The release (OWL-540): the guards of `release.yml`, which only a tag
//! runs; the tag's check; the Homebrew formula; the Linux install script.
//! The scripts are run with `sh`, as the workflow and a user run them.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(path: &str) -> String {
    fs::read_to_string(root().join(path)).unwrap()
}

/// The jobs of a workflow, by name: the lines of each, from its `  name:`
/// line under `jobs:` to the next one.
fn jobs(workflow: &str) -> Vec<(String, String)> {
    let mut jobs: Vec<(String, String)> = Vec::new();
    let mut in_jobs = false;
    for line in workflow.lines() {
        if line == "jobs:" {
            in_jobs = true;
        } else if in_jobs
            && line.starts_with("  ")
            && !line.starts_with("   ")
            && line.ends_with(':')
        {
            jobs.push((line.trim().trim_end_matches(':').to_owned(), String::new()));
        } else if let Some((_, body)) = jobs.last_mut() {
            body.push_str(line);
            body.push('\n');
        }
    }
    jobs
}

/// A tag is the only thing that runs `release.yml`, so its guards are pinned
/// here: a release is a draft of a checked tag, signed then checked against
/// the repository's public key, and built without a cache. The signing key
/// is in one job, of the environment `release`, which runs no action; the
/// token that writes is in another job, which has no secret.
#[test]
fn the_release_workflow_keeps_its_guards() {
    let workflow = read(".github/workflows/release.yml");
    let lines: Vec<&str> = workflow.lines().map(str::trim).collect();
    for guard in [
        "tags: ['v*']",
        "version=$(sh scripts/release/version.sh \"$TAG\")",
        "if ! git merge-base --is-ancestor \"$GITHUB_SHA\" origin/main; then",
        "sh scripts/release/homebrew-formula.sh \"$VERSION\" dist/SHA256SUMS > formula/owlshift.rb",
        "run: minisign -V -p ../minisign.pub -m SHA256SUMS",
        "gh release create \"$TAG\" -R \"$GITHUB_REPOSITORY\" --draft --verify-tag \\",
    ] {
        assert!(lines.contains(&guard), "release.yml lost `{guard}`");
    }
    assert!(
        !workflow.contains("actions/cache") && !workflow.contains("rust-cache"),
        "a release is built without a cache"
    );
    let jobs = jobs(&workflow);
    let holding = |text: &str| -> Vec<&str> {
        jobs.iter()
            .filter(|(_, body)| body.contains(text))
            .map(|(name, _)| name.as_str())
            .collect()
    };
    assert_eq!(holding("environment: release"), ["sign"]);
    assert_eq!(holding("secrets."), ["sign"]);
    assert_eq!(holding("contents: write"), ["publish"]);
    let sign = &jobs.iter().find(|(name, _)| name == "sign").unwrap().1;
    assert!(
        !sign.contains("uses:"),
        "the job holding the key runs no action"
    );
    // The key the release is checked against: minisign's two lines.
    let key = read("minisign.pub");
    let key: Vec<&str> = key.lines().collect();
    assert!(
        key.len() == 2
            && key[0].starts_with("untrusted comment: minisign public key ")
            && key[1].starts_with("RW"),
        "{key:?}"
    );
}

#[cfg(unix)]
fn sh(script: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    std::process::Command::new("sh")
        .arg(script)
        .args(args)
        .envs(envs.iter().copied())
        .output()
        .unwrap()
}

#[cfg(unix)]
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The release workflow's first job: a tag `v<version>` passes only when
/// `<version>` is the workspace's and CHANGELOG.md has its section. Run on
/// the repository itself, so a version bump without its changelog section
/// fails here rather than at the tag.
#[cfg(unix)]
#[test]
fn a_tag_must_name_the_workspace_version_and_its_changelog_section() {
    let script = root().join("scripts/release/version.sh");
    let repository = root();
    let repository = repository.to_str().unwrap();
    let version = env!("CARGO_PKG_VERSION");

    let tagged = sh(&script, &[&format!("v{version}"), repository], &[]);
    assert!(tagged.status.success(), "{}", text(&tagged.stderr));
    assert_eq!(text(&tagged.stdout), format!("{version}\n"));

    let other = sh(&script, &["v99.0.0", repository], &[]);
    assert!(!other.status.success());
    assert!(
        text(&other.stderr).contains(&format!("is not v{version}")),
        "{}",
        text(&other.stderr)
    );

    // A heading that only starts like the version is not its section.
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("Cargo.toml"),
        "[workspace]\nmembers = []\n\n[workspace.package]\nversion = \"1.2.3\"\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("CHANGELOG.md"),
        "# Changelog\n\n## 1.2.30\n",
    )
    .unwrap();
    let unlisted = sh(&script, &["v1.2.3", dir.path().to_str().unwrap()], &[]);
    assert!(!unlisted.status.success());
    assert!(
        text(&unlisted.stderr).contains("no '## 1.2.3' section"),
        "{}",
        text(&unlisted.stderr)
    );
}

/// The formula names each macOS archive with the checksum SHA256SUMS lists,
/// and is not rendered when one is missing.
#[cfg(unix)]
#[test]
fn the_formula_names_each_macos_archive_and_its_checksum() {
    let script = root().join("scripts/release/homebrew-formula.sh");
    let dir = tempfile::tempdir().unwrap();
    let sums = dir.path().join("SHA256SUMS");
    let line = |digit: char, target: &str| {
        format!(
            "{}  owlshift-0.1.0-{target}.tar.gz\n",
            digit.to_string().repeat(64)
        )
    };
    let all = [
        line('a', "aarch64-apple-darwin"),
        line('b', "aarch64-unknown-linux-musl"),
        line('c', "x86_64-apple-darwin"),
        line('d', "x86_64-unknown-linux-musl"),
    ];
    fs::write(&sums, all.concat()).unwrap();
    let rendered = sh(&script, &["0.1.0", sums.to_str().unwrap()], &[]);
    assert!(rendered.status.success(), "{}", text(&rendered.stderr));
    let formula = text(&rendered.stdout);
    let base = "https://github.com/pitchopp/owlshift/releases/download/v0.1.0";
    for (target, digit) in [("aarch64-apple-darwin", 'a'), ("x86_64-apple-darwin", 'c')] {
        let pair = format!(
            "url \"{base}/owlshift-0.1.0-{target}.tar.gz\"\n      sha256 \"{}\"",
            digit.to_string().repeat(64)
        );
        assert!(formula.contains(&pair), "{formula}");
    }
    assert!(formula.contains("  version \"0.1.0\"\n"), "{formula}");

    fs::write(
        &sums,
        [&all[0], &all[1], &all[3]].map(String::as_str).concat(),
    )
    .unwrap();
    let missing = sh(&script, &["0.1.0", sums.to_str().unwrap()], &[]);
    assert!(!missing.status.success());
    assert!(missing.stdout.is_empty());
    assert!(
        text(&missing.stderr).contains("does not list owlshift-0.1.0-x86_64-apple-darwin.tar.gz"),
        "{}",
        text(&missing.stderr)
    );
}

/// The install script, on a copy of a release served from files: it
/// installs the archive whose checksum SHA256SUMS lists, leaving nothing
/// else in the folder; it refuses a version that is not one, an archive
/// SHA256SUMS does not list and an archive whose checksum differs, and then
/// installs nothing.
#[cfg(target_os = "linux")]
#[test]
fn the_install_script_installs_only_an_archive_whose_checksum_matches() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    let target = match std::env::consts::ARCH {
        "x86_64" => "x86_64-unknown-linux-musl",
        "aarch64" => "aarch64-unknown-linux-musl",
        _ => return,
    };
    let script = root().join("install.sh");
    let dir = tempfile::tempdir().unwrap();
    let name = format!("owlshift-0.1.0-{target}");
    let archive = format!("{name}.tar.gz");
    let binary = "#!/bin/sh\necho owlshift 0.1.0\n";
    fs::create_dir_all(dir.path().join("stage").join(&name)).unwrap();
    fs::write(
        dir.path().join("stage").join(&name).join("owlshift"),
        binary,
    )
    .unwrap();
    // A release, and a copy whose SHA256SUMS lists another archive.
    let release = |folder: &str| {
        let at = dir.path().join(folder).join("v0.1.0");
        fs::create_dir_all(&at).unwrap();
        let packed = Command::new("tar")
            .arg("-czf")
            .arg(at.join(&archive))
            .arg("-C")
            .arg(dir.path().join("stage"))
            .arg(&name)
            .status()
            .unwrap();
        assert!(packed.success());
        let sums = Command::new("sha256sum")
            .arg(&archive)
            .current_dir(&at)
            .output()
            .unwrap();
        assert!(sums.status.success());
        fs::write(at.join("SHA256SUMS"), &sums.stdout).unwrap();
        at
    };
    release("good");
    let unlisted = release("unlisted");
    let sums = fs::read_to_string(unlisted.join("SHA256SUMS")).unwrap();
    fs::write(
        unlisted.join("SHA256SUMS"),
        sums.replace(&archive, &format!("other-{archive}")),
    )
    .unwrap();
    let tampered = release("tampered");
    let mut bytes = fs::read(tampered.join(&archive)).unwrap();
    bytes.push(0);
    fs::write(tampered.join(&archive), bytes).unwrap();

    let install = |folder: &str, version: &str, into: &Path| {
        let releases = format!("file://{}", dir.path().join(folder).display());
        sh(
            &script,
            &[],
            &[
                ("OWLSHIFT_RELEASES_URL", releases.as_str()),
                ("OWLSHIFT_VERSION", version),
                ("OWLSHIFT_INSTALL_DIR", into.to_str().unwrap()),
            ],
        )
    };

    let bin = dir.path().join("bin");
    let installed = install("good", "v0.1.0", &bin);
    assert!(installed.status.success(), "{}", text(&installed.stderr));
    assert_eq!(fs::read_to_string(bin.join("owlshift")).unwrap(), binary);
    let mode = fs::metadata(bin.join("owlshift"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o755);
    let left: Vec<_> = fs::read_dir(&bin)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(left, ["owlshift"]);

    let unlisted_refusal = format!("SHA256SUMS does not list {archive}");
    let tampered_refusal = format!("the checksum of {archive} is");
    for (folder, version, refusal) in [
        ("good", "0.1.0/../../x", "is not a release version"),
        ("unlisted", "0.1.0", unlisted_refusal.as_str()),
        ("tampered", "0.1.0", tampered_refusal.as_str()),
    ] {
        let into = dir.path().join(format!("bin-{folder}"));
        let refused = install(folder, version, &into);
        assert!(!refused.status.success(), "{folder}");
        assert!(
            text(&refused.stderr).contains(refusal),
            "{folder}: {}",
            text(&refused.stderr)
        );
        assert!(!into.exists(), "{folder}: nothing is installed");
    }
}
