//! The release (OWL-540): the guards of `release.yml`, which only a tag
//! runs; the tag's check; the Homebrew formula; the Linux install script;
//! and (OWL-549) the guards of `formula.yml`, which only a published release
//! runs, with the scripts that verify the release and commit its formula.
//! The scripts are run with `sh`, as the workflows and a user run them.

use std::fs;
use std::path::{Path, PathBuf};

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
    let job = |name: &str| &jobs.iter().find(|(job, _)| job == name).unwrap().1;
    assert!(
        job("build").contains("needs: [version, notices]"),
        "no archive is built before the notices"
    );
    let sign = job("sign");
    assert!(
        !sign.contains("uses:"),
        "the job holding the key runs no action"
    );
    // OWL-548: every archive carries its target's notices, written before
    // any build.
    for guard in [
        "sh scripts/release/notices.sh notices",
        "test -s \"notices/$TARGET/THIRD-PARTY-NOTICES.txt\"",
        "cp \"target/$TARGET/release/owlshift\" LICENSE README.md \"notices/$TARGET/THIRD-PARTY-NOTICES.txt\" \"dist/$name/\"",
    ] {
        assert!(lines.contains(&guard), "release.yml lost `{guard}`");
    }
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

/// `owlshift self-update` (OWL-541) asks for the archives the release
/// workflow builds, by the name `install.sh` asks for.
#[test]
fn self_update_names_the_archives_of_the_release() {
    let update = read("crates/owlshift-cli/src/self_update_cmd.rs");
    assert!(update.contains("format!(\"owlshift-{version}-{target}\")"));
    assert!(read("install.sh").contains("name=owlshift-$version-$target"));
    let workflow = read(".github/workflows/release.yml");
    let targets: Vec<&str> = workflow
        .lines()
        .filter_map(|line| line.trim().strip_prefix("- target: "))
        .collect();
    assert_eq!(targets.len(), 4, "{targets:?}");
    for target in targets {
        assert!(update.contains(&format!("Some(\"{target}\")")), "{target}");
    }
}

#[cfg(unix)]
fn sh(script: &Path, args: &[&str], envs: &[(&str, &str)]) -> std::process::Output {
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

/// The targets whose notices `notices.sh` writes by default are the targets
/// the release builds, so no archive lacks its notices.
#[test]
fn the_notices_cover_every_target_the_release_builds() {
    let workflow = read(".github/workflows/release.yml");
    let mut built: Vec<&str> = workflow
        .lines()
        .filter_map(|line| line.trim().strip_prefix("- target: "))
        .collect();
    let script = read("scripts/release/notices.sh");
    let listed = script
        .lines()
        .find_map(|line| line.strip_prefix("TARGETS=\""))
        .and_then(|rest| rest.strip_suffix('"'))
        .expect("notices.sh lists its targets");
    let mut listed: Vec<&str> = listed.split(' ').collect();
    built.sort_unstable();
    listed.sort_unstable();
    assert_eq!(built.len(), 4, "{built:?}");
    assert_eq!(built, listed);
    // CI writes them too, so a refused licence fails its pull request.
    assert!(
        read(".github/workflows/ci.yml")
            .lines()
            .any(|line| line.trim() == "sh scripts/release/notices.sh \"$RUNNER_TEMP/notices\""),
        "ci.yml no longer checks the licences"
    );
}

/// `notices.sh` with a stand-in `cargo`: it asks cargo-about for one
/// target's notices, offline and failing on a refused licence, appends the
/// fonts' licences, and writes no file when cargo-about fails.
#[cfg(unix)]
#[test]
fn the_notices_hold_the_crates_and_the_fonts_or_nothing() {
    use std::os::unix::fs::PermissionsExt;

    let script = root().join("scripts/release/notices.sh");
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    fs::create_dir(&bin).unwrap();
    // Records its arguments, then writes the file `-o` names, or fails when
    // FAKE_FAIL is set.
    let cargo = bin.join("cargo");
    fs::write(
        &cargo,
        "#!/bin/sh\n\
         printf '%s\\n' \"$@\" > \"$FAKE_ARGS\"\n\
         [ -z \"${FAKE_FAIL:-}\" ] || exit 1\n\
         while [ \"$#\" -gt 0 ]; do\n\
         \x20   if [ \"$1\" = -o ]; then echo 'crate notices' > \"$2\"; fi\n\
         \x20   shift\n\
         done\n",
    )
    .unwrap();
    fs::set_permissions(&cargo, fs::Permissions::from_mode(0o755)).unwrap();
    // OWL-551: answers `--print sysroot` with FAKE_SYSROOT, `--version`, and
    // fails when FAKE_RUSTC_FAIL is set.
    let rustc = bin.join("rustc");
    fs::write(
        &rustc,
        "#!/bin/sh\n\
         [ -z \"${FAKE_RUSTC_FAIL:-}\" ] || exit 1\n\
         case \"$1\" in\n\
         --version) echo 'rustc 9.9.9 (stand-in)' ;;\n\
         --print) echo \"$FAKE_SYSROOT\" ;;\n\
         esac\n",
    )
    .unwrap();
    fs::set_permissions(&rustc, fs::Permissions::from_mode(0o755)).unwrap();
    let sysroot = dir.path().join("sysroot");
    let doc = sysroot.join("share/doc/rust");
    fs::create_dir_all(&doc).unwrap();
    let html = doc.join("COPYRIGHT-library.html");
    let pre = "Copyright (c) A <a@example.org>\n    indented line\nlast line";
    fs::write(
        &html,
        "<html><head><title>T</title><style>p{}</style></head><body>\
         <h1>Std notices</h1><script>var secret = 1;</script>\
         <p>Fish &amp; chips</p>\
         <pre>Copyright (c) A &lt;a@example.org&gt;\n    indented line\nlast line</pre>\
         </body></html>",
    )
    .unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let args = dir.path().join("args");
    let out = dir.path().join("out");
    let targets = [
        "aarch64-apple-darwin",
        "x86_64-apple-darwin",
        "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-musl",
    ];
    let run_with = |fail: &str, rustc_fail: &str, targets: &[&str]| {
        let mut argv = vec![out.to_str().unwrap()];
        argv.extend_from_slice(targets);
        sh(
            &script,
            &argv,
            &[
                ("PATH", path.as_str()),
                ("FAKE_ARGS", args.to_str().unwrap()),
                ("FAKE_FAIL", fail),
                ("FAKE_RUSTC_FAIL", rustc_fail),
                ("FAKE_SYSROOT", sysroot.to_str().unwrap()),
            ],
        )
    };
    let run = |fail: &str| run_with(fail, "", &["x86_64-unknown-linux-musl"]);
    let file = out.join("x86_64-unknown-linux-musl/THIRD-PARTY-NOTICES.txt");

    // Every target's file holds the std section after the fonts, as text;
    // only the Linux files hold musl's.
    let all = run_with("", "", &targets);
    assert!(all.status.success(), "{}", text(&all.stderr));
    let musl = fs::read_to_string(root().join("scripts/release/musl-COPYRIGHT")).unwrap();
    let fonts_dir = root().join("crates/owlshift-runner/src/ui/assets/fonts");
    let last_font = fs::read_to_string(fonts_dir.join("OFL-JetBrainsMono.txt")).unwrap();
    let rule = "=".repeat(80);
    for target in targets {
        let notices = fs::read_to_string(out.join(target).join("THIRD-PARTY-NOTICES.txt")).unwrap();
        let header = format!(
            "{rule}\nThe Rust standard library (COPYRIGHT-library.html of rustc 9.9.9 (stand-in))\n\n"
        );
        let at = notices
            .find(&header)
            .unwrap_or_else(|| panic!("{target}: no std section"));
        assert!(
            at > notices.find(&last_font).unwrap(),
            "{target}: std follows the fonts"
        );
        let section = &notices[at + header.len()..];
        let std = section.split(&rule).next().unwrap();
        assert!(std.contains("Std notices"), "{target}: {std}");
        assert!(std.contains("Fish & chips"), "{target}: {std}");
        assert!(std.contains(pre), "{target}: <pre> is kept as it is: {std}");
        assert!(!std.contains("secret") && !std.contains("<h1>") && !std.contains("&amp;"));
        let linux = target.ends_with("-linux-musl");
        assert_eq!(
            notices.contains(&format!(
                "{rule}\nmusl libc (scripts/release/musl-COPYRIGHT)\n\n{musl}"
            )),
            linux,
            "{target}: musl only on Linux"
        );
    }

    let written = run("");
    assert!(written.status.success(), "{}", text(&written.stderr));
    let recorded = fs::read_to_string(&args).unwrap();
    let recorded: Vec<&str> = recorded.lines().collect();
    for wanted in ["about", "generate", "--frozen", "--fail"] {
        assert!(recorded.contains(&wanted), "{recorded:?}");
    }
    assert!(
        recorded
            .windows(2)
            .any(|pair| pair == ["--target", "x86_64-unknown-linux-musl"]),
        "{recorded:?}"
    );
    let notices = fs::read_to_string(&file).unwrap();
    assert!(notices.starts_with("crate notices\n"), "{notices}");
    let fonts = root().join("crates/owlshift-runner/src/ui/assets/fonts");
    for licence in ["OFL-BricolageGrotesque.txt", "OFL-JetBrainsMono.txt"] {
        assert!(
            notices.contains(&format!("SIL Open Font License 1.1 ({licence})")),
            "{licence}"
        );
        assert!(notices.contains(&fs::read_to_string(fonts.join(licence)).unwrap()));
    }

    let refused = run("1");
    assert!(!refused.status.success());
    assert!(
        text(&refused.stderr)
            .contains("the notices of x86_64-unknown-linux-musl could not be written"),
        "{}",
        text(&refused.stderr)
    );
    assert!(!file.exists(), "no notices are left behind");
    let left = || {
        fs::read_dir(out.join("x86_64-unknown-linux-musl"))
            .unwrap()
            .count()
    };
    assert_eq!(left(), 0);

    // A std notice that is missing, converts to nothing, or cannot be
    // located leaves no file either.
    let saved = fs::read_to_string(&html).unwrap();
    fs::write(&html, "<html><script>x</script> \n</html>").unwrap();
    let empty = run("");
    assert!(!empty.status.success());
    assert!(
        text(&empty.stderr).contains("standard library notice"),
        "{}",
        text(&empty.stderr)
    );
    assert!(!file.exists());
    assert_eq!(left(), 0);
    fs::remove_file(&html).unwrap();
    let missing = run("");
    assert!(!missing.status.success());
    assert!(!file.exists());
    assert_eq!(left(), 0);
    fs::write(&html, saved).unwrap();
    let no_rustc = run_with("", "1", &["x86_64-unknown-linux-musl"]);
    assert!(!no_rustc.status.success());
    assert!(!file.exists());
    assert_eq!(left(), 0);
}

/// The vendored musl notice is the licence text, with its version first.
#[test]
fn the_vendored_musl_notice_names_its_version_and_holds_the_licence() {
    let musl = read("scripts/release/musl-COPYRIGHT");
    assert!(musl.starts_with("musl "), "{musl}");
    assert!(musl.contains("standard MIT license"));
}

/// `formula.yml` (OWL-549), which only a published release runs: it commits
/// the formula on main with no review, so its guards are pinned here. The
/// release is verified with a read-only token; the job that writes is the
/// only one with `contents: write`, holds no secret and runs the commit
/// script, which never force-pushes.
#[test]
fn the_formula_workflow_keeps_its_guards() {
    // A Windows checkout may have CRLF line endings.
    let workflow = read(".github/workflows/formula.yml").replace("\r\n", "\n");
    let lines: Vec<&str> = workflow.lines().map(str::trim).collect();
    for guard in [
        "types: [published]",
        "if: ${{ !github.event.release.prerelease }}",
        "status=$(gh api \"repos/$GITHUB_REPOSITORY/compare/main...$GITHUB_SHA\" --jq .status)",
        "sh scripts/release/formula-verify.sh \"$VERSION\" \"$RUNNER_TEMP/release\"",
        "needs: formula",
        "printf '%s\\n' \"$FORMULA\" > \"$RUNNER_TEMP/owlshift.rb\"",
        "sh scripts/release/formula-commit.sh \"$VERSION\" \"$RUNNER_TEMP/owlshift.rb\" \"$GITHUB_REPOSITORY\"",
    ] {
        assert!(lines.contains(&guard), "formula.yml lost `{guard}`");
    }
    let top = workflow.split("\njobs:").next().unwrap();
    assert!(top.contains("\npermissions:\n  contents: read\n"), "{top}");
    let jobs = jobs(&workflow);
    let holding = |text: &str| -> Vec<&str> {
        jobs.iter()
            .filter(|(_, body)| body.contains(text))
            .map(|(name, _)| name.as_str())
            .collect()
    };
    assert_eq!(holding("contents: write"), ["commit"]);
    assert_eq!(holding("formula-verify.sh"), ["formula"]);
    assert_eq!(holding("formula-commit.sh"), ["commit"]);
    let code: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|line| !line.starts_with('#'))
        .collect();
    for banned in ["secrets.", "git push", "--force", "pull_request"] {
        assert!(
            !code.iter().any(|line| line.contains(banned)),
            "formula.yml uses `{banned}`"
        );
    }
    let commit = read("scripts/release/formula-commit.sh");
    assert!(
        !commit.contains("git "),
        "the commit goes through the API alone"
    );
}

/// Writes an executable stand-in for a command into `bin`.
#[cfg(unix)]
fn stand_in(bin: &Path, name: &str, body: &str) {
    use std::os::unix::fs::PermissionsExt;

    let file = bin.join(name);
    fs::write(&file, format!("#!/bin/sh\n{body}")).unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
}

/// `formula-verify.sh` on a copy of a release served from files, with a
/// stand-in `minisign`: it renders the formula only when the signature
/// verifies, its trusted comment names the version, and each macOS archive
/// has the checksum SHA256SUMS lists; otherwise it leaves no formula.
#[cfg(unix)]
#[test]
fn the_formula_is_rendered_only_from_a_release_that_verifies() {
    use sha2::{Digest, Sha256};

    let script = root().join("scripts/release/formula-verify.sh");
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    fs::create_dir(&bin).unwrap();
    // Records its arguments, prints the trusted comment FAKE_COMMENT, or
    // fails when FAKE_FAIL is set.
    stand_in(
        &bin,
        "minisign",
        "printf '%s\\n' \"$@\" > \"$FAKE_ARGS\"\n\
         [ -z \"${FAKE_FAIL:-}\" ] || exit 1\n\
         printf '%s\\n' \"$FAKE_COMMENT\"\n",
    );
    let release = dir.path().join("releases/v0.1.0");
    fs::create_dir_all(&release).unwrap();
    let mut sums = String::new();
    for target in [
        "aarch64-apple-darwin",
        "aarch64-unknown-linux-musl",
        "x86_64-apple-darwin",
        "x86_64-unknown-linux-musl",
    ] {
        let archive = format!("owlshift-0.1.0-{target}.tar.gz");
        let bytes = format!("archive of {target}");
        fs::write(release.join(&archive), &bytes).unwrap();
        let digest: String = Sha256::digest(bytes.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        sums.push_str(&format!("{digest}  {archive}\n"));
    }
    fs::write(release.join("SHA256SUMS"), &sums).unwrap();
    fs::write(release.join("SHA256SUMS.minisig"), "signature\n").unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let releases = format!("file://{}", dir.path().join("releases").display());
    let args = dir.path().join("args");
    let out = dir.path().join("out");
    let run = |version: &str, comment: &str, fail: &str| {
        sh(
            &script,
            &[version, out.to_str().unwrap()],
            &[
                ("PATH", path.as_str()),
                ("OWLSHIFT_RELEASES_URL", releases.as_str()),
                ("FAKE_ARGS", args.to_str().unwrap()),
                ("FAKE_COMMENT", comment),
                ("FAKE_FAIL", fail),
            ],
        )
    };
    let formula = out.join("owlshift.rb");

    let verified = run("0.1.0", "owlshift 0.1.0", "");
    assert!(verified.status.success(), "{}", text(&verified.stderr));
    let recorded = fs::read_to_string(&args).unwrap();
    let recorded: Vec<&str> = recorded.lines().collect();
    assert_eq!(recorded[..3], ["-V", "-Q", "-p"], "{recorded:?}");
    assert!(recorded[3].ends_with("/minisign.pub"), "{recorded:?}");
    assert_eq!(
        recorded[4..],
        ["-m", out.join("SHA256SUMS").to_str().unwrap()]
    );
    let rendered = fs::read_to_string(&formula).unwrap();
    assert!(rendered.contains("  version \"0.1.0\"\n"), "{rendered}");
    assert!(rendered.contains(&sums[..64]), "{rendered}");

    let mut refusals = vec![
        (run("0.1", "owlshift 0.1", ""), "'0.1' is not a version"),
        (
            run("0.1.0", "owlshift 0.1.0", "1"),
            "does not verify with minisign.pub",
        ),
        (
            run("0.1.0", "owlshift 0.0.9", ""),
            "signed as 'owlshift 0.0.9', not 'owlshift 0.1.0'",
        ),
    ];
    fs::write(
        release.join("owlshift-0.1.0-x86_64-apple-darwin.tar.gz"),
        "another archive",
    )
    .unwrap();
    refusals.push((
        run("0.1.0", "owlshift 0.1.0", ""),
        "owlshift-0.1.0-x86_64-apple-darwin.tar.gz has the checksum",
    ));
    for (refused, reason) in refusals {
        assert!(!refused.status.success());
        assert!(
            text(&refused.stderr).contains(reason),
            "{}",
            text(&refused.stderr)
        );
        assert!(!formula.exists(), "no formula is left behind: {reason}");
    }
}

/// `formula-commit.sh` with a stand-in `gh` holding main's formula: one
/// commit through the contents API on main, by the Actions bot and signed
/// off by it, guarded by the blob it read; nothing when main already has
/// this formula or a newer one; one more try when main's formula changed in
/// between, and none for any other refusal.
#[cfg(unix)]
#[test]
fn the_formula_is_committed_on_main_once_and_never_backwards() {
    let script = root().join("scripts/release/formula-commit.sh");
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    fs::create_dir(&bin).unwrap();
    // A read prints the blob and the content of $FAKE/main.rb, or a 404; the
    // Nth write records its arguments and decodes its content, failing with
    // $FAKE/refusal.N when there is one.
    stand_in(
        &bin,
        "gh",
        "case \"$*\" in\n\
         *'--method PUT'*)\n\
         \x20   n=$(($(cat \"$FAKE/writes\" 2>/dev/null || echo 0) + 1))\n\
         \x20   echo \"$n\" > \"$FAKE/writes\"\n\
         \x20   printf '%s\\n' \"$@\" > \"$FAKE/args.$n\"\n\
         \x20   if [ -f \"$FAKE/refusal.$n\" ]; then cat \"$FAKE/refusal.$n\" >&2; exit 1; fi\n\
         \x20   for arg in \"$@\"; do\n\
         \x20       case $arg in content=*) printf '%s' \"${arg#content=}\" | base64 -d > \"$FAKE/main.rb\" ;; esac\n\
         \x20   done\n\
         \x20   echo 0123abcd ;;\n\
         *)\n\
         \x20   if [ ! -f \"$FAKE/main.rb\" ]; then echo 'gh: Not Found (HTTP 404)' >&2; exit 1; fi\n\
         \x20   echo blob1\n\
         \x20   base64 < \"$FAKE/main.rb\" ;;\n\
         esac\n",
    );
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let formula_of =
        |version: &str| format!("class Owlshift < Formula\n  version \"{version}\"\nend\n");
    let formula = dir.path().join("owlshift.rb");
    fs::write(&formula, formula_of("0.2.0")).unwrap();
    let fake = dir.path().join("fake");
    let run = |main: Option<String>, refusals: &[&str]| {
        let _ = fs::remove_dir_all(&fake);
        fs::create_dir(&fake).unwrap();
        if let Some(main) = main {
            fs::write(fake.join("main.rb"), main).unwrap();
        }
        for (n, refusal) in refusals.iter().enumerate() {
            fs::write(fake.join(format!("refusal.{}", n + 1)), refusal).unwrap();
        }
        let output = sh(
            &script,
            &["0.2.0", formula.to_str().unwrap(), "pitchopp/owlshift"],
            &[("PATH", path.as_str()), ("FAKE", fake.to_str().unwrap())],
        );
        let count = fs::read_to_string(fake.join("writes"))
            .map_or(0, |n| n.trim().parse::<usize>().unwrap());
        let writes: Vec<String> = (1..=count)
            .map(|n| fs::read_to_string(fake.join(format!("args.{n}"))).unwrap())
            .collect();
        let main = fs::read_to_string(fake.join("main.rb")).ok();
        (output, writes, main)
    };

    // No formula on main yet: one write, with no blob to guard.
    let (output, writes, main) = run(None, &[]);
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert_eq!(writes.len(), 1);
    let args: Vec<&str> = writes[0].lines().collect();
    for wanted in [
        "repos/pitchopp/owlshift/contents/Formula/owlshift.rb",
        "branch=main",
        "author[name]=github-actions[bot]",
        "author[email]=41898282+github-actions[bot]@users.noreply.github.com",
    ] {
        assert!(args.contains(&wanted), "{args:?}");
    }
    assert!(!args.iter().any(|arg| arg.starts_with("sha=")), "{args:?}");
    let signed_off = "\nSigned-off-by: github-actions[bot] \
                      <41898282+github-actions[bot]@users.noreply.github.com>\n";
    assert!(
        writes[0].contains("message=Update the Homebrew formula to 0.2.0\n")
            && writes[0].contains(signed_off),
        "{}",
        writes[0]
    );
    assert_eq!(main.unwrap(), formula_of("0.2.0"));

    // The same formula, or a newer one, on main: nothing written.
    for on_main in [formula_of("0.2.0"), formula_of("0.10.0")] {
        let (output, writes, _) = run(Some(on_main.clone()), &[]);
        assert!(output.status.success(), "{}", text(&output.stderr));
        assert!(writes.is_empty(), "{on_main}");
        assert!(
            text(&output.stdout).starts_with("::notice::"),
            "{}",
            text(&output.stdout)
        );
    }

    // An older one: replaced, guarded by its blob, once more after a 409.
    let (output, writes, main) = run(Some(formula_of("0.1.9")), &["gh: Conflict (HTTP 409)"]);
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert_eq!(writes.len(), 2);
    assert!(
        writes[1].lines().any(|arg| arg == "sha=blob1"),
        "{}",
        writes[1]
    );
    assert_eq!(main.unwrap(), formula_of("0.2.0"));

    // Refused twice for a changed formula, or once for anything else: it
    // fails, saying why.
    for (refusals, tries, reason) in [
        (
            &["HTTP 409", "gh: Unprocessable (HTTP 422)"][..],
            2,
            "HTTP 422",
        ),
        (&["gh: Forbidden (HTTP 403)"][..], 1, "HTTP 403"),
    ] {
        let (output, writes, _) = run(Some(formula_of("0.1.9")), refusals);
        assert!(!output.status.success());
        assert_eq!(writes.len(), tries, "{reason}");
        assert!(
            text(&output.stderr).contains(reason),
            "{}",
            text(&output.stderr)
        );
    }
}
