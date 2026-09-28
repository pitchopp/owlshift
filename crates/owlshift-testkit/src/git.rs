//! Git with none of the host's configuration, and a project seeded into a
//! local bare remote.
//!
//! Every git command the bench runs goes through [`GitEnv::apply`]. The
//! fake harness is launched by the executor with the agent environment,
//! which drops every `GIT_*` variable: built from [`GitEnv::agent_parent`],
//! it finds the same configuration as git's global one in the bench's home
//! (`$XDG_CONFIG_HOME/git/config`). Its git also reads the host's system
//! configuration, whose keys that matter here the bench's file overrides.

use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use jiff::Timestamp;

/// The file name of the bench's git configuration, in its home.
const FIXTURE_CONFIG: &str = "owlshift-test.gitconfig";

/// A folder that stands in for the host's home, with git's configuration for
/// the bench.
#[derive(Clone, Debug)]
pub struct GitEnv {
    home: PathBuf,
    /// Author and committer date, in git's internal format.
    date: Option<String>,
}

impl GitEnv {
    /// Writes the configuration into `home`, created if missing: an identity,
    /// no signing, no line-ending conversion, and ignore and attributes files
    /// and a hooks folder of its own, all empty. It is written twice: as the
    /// file [`GitEnv::apply`] names, and where git looks for its global
    /// configuration under `XDG_CONFIG_HOME`, for the agent's git.
    pub fn create(home: PathBuf) -> Result<Self, GitError> {
        let io = |error: std::io::Error| GitError(format!("{}: {error}", home.display()));
        fs::create_dir_all(home.join("hooks")).map_err(io)?;
        fs::create_dir_all(home.join("git")).map_err(io)?;
        fs::write(home.join("ignore"), "").map_err(io)?;
        fs::write(home.join("attributes"), "").map_err(io)?;
        let config = format!(
            "[user]\n\tname = Owlshift Test\n\temail = test@owlshift.invalid\n\
             [commit]\n\tgpgsign = false\n\
             [tag]\n\tgpgsign = false\n\
             [init]\n\tdefaultBranch = main\n\
             [core]\n\tautocrlf = false\n\texcludesFile = {}\n\tattributesFile = {}\n\thooksPath = {}\n",
            config_path(&home.join("ignore")),
            config_path(&home.join("attributes")),
            config_path(&home.join("hooks")),
        );
        fs::write(home.join(FIXTURE_CONFIG), &config).map_err(io)?;
        fs::write(home.join("git").join("config"), &config).map_err(io)?;
        Ok(Self { home, date: None })
    }

    /// The runner's environment as the bench stands it in, for building the
    /// agent environment: this process's variables with every `GIT_*` one
    /// removed, and the home and configuration folder in the bench's home.
    pub fn agent_parent(&self) -> Vec<(OsString, OsString)> {
        let replaced = ["HOME", "XDG_CONFIG_HOME", "LC_ALL", "LANGUAGE"];
        let mut vars: Vec<(OsString, OsString)> = std::env::vars_os()
            .filter(|(name, _)| {
                let name = name.to_string_lossy().to_ascii_uppercase();
                !name.starts_with("GIT_") && !replaced.contains(&name.as_str())
            })
            .collect();
        vars.extend([
            ("HOME".into(), self.home.clone().into_os_string()),
            ("XDG_CONFIG_HOME".into(), self.home.clone().into_os_string()),
            ("LC_ALL".into(), "C".into()),
            ("LANGUAGE".into(), OsString::new()),
        ]);
        vars
    }

    /// The same environment, committing at `at`.
    pub fn at(&self, at: Timestamp) -> Self {
        Self {
            home: self.home.clone(),
            date: Some(format!("{} +0000", at.as_second())),
        }
    }

    /// Makes `command` hermetic: every `GIT_*` variable, inherited or already
    /// set on it, is removed; git then reads no system configuration, takes
    /// its global one from the bench's file, and finds its home in the
    /// bench's folder.
    pub fn apply(&self, command: &mut Command) {
        let git_variables: Vec<OsString> = std::env::vars_os()
            .map(|(name, _)| name)
            .chain(command.get_envs().map(|(name, _)| name.to_owned()))
            .filter(|name| {
                name.to_string_lossy()
                    .to_ascii_uppercase()
                    .starts_with("GIT_")
            })
            .collect();
        for name in git_variables {
            command.env_remove(name);
        }
        command
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.home.join(FIXTURE_CONFIG))
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", &self.home)
            .env("LC_ALL", "C")
            .env("LANGUAGE", "");
        if let Some(date) = &self.date {
            command
                .env("GIT_AUTHOR_DATE", date)
                .env("GIT_COMMITTER_DATE", date);
        }
    }

    /// Runs git in `dir` and returns its standard output; a non-zero exit is
    /// an error carrying git's standard error.
    pub fn run(&self, dir: &Path, args: &[&str]) -> Result<Vec<u8>, GitError> {
        let mut command = Command::new("git");
        command.args(args).current_dir(dir).stdin(Stdio::null());
        self.apply(&mut command);
        let failed = |detail: String| {
            GitError(format!(
                "git {} (in {}) {detail}",
                args.join(" "),
                dir.display()
            ))
        };
        let output = command
            .output()
            .map_err(|error| failed(format!("could not run: {error}")))?;
        if output.status.success() {
            Ok(output.stdout)
        } else {
            Err(failed(format!(
                "failed with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }
    }
}

/// A path as a quoted git configuration value: backslashes, which Windows
/// paths carry, are escapes there.
fn config_path(path: &Path) -> String {
    let path = path.to_string_lossy();
    format!("\"{}\"", path.replace('\\', "\\\\").replace('"', "\\\""))
}

/// A bare remote seeded with a project, and a clone of it.
#[derive(Clone, Debug)]
pub struct Remote {
    pub bare: PathBuf,
    pub checkout: PathBuf,
}

/// Seeds `root/remote.git` with the files of `fixture` as `main`, and clones
/// it into `root/checkout`. Paths given to git are relative, so no platform
/// path syntax reaches a git argument.
pub fn seed(env: &GitEnv, root: &Path, fixture: &Path) -> Result<Remote, GitError> {
    let seed = root.join("seed");
    copy_tree(fixture, &seed)?;
    env.run(root, &["init", "--quiet", "--bare", "remote.git"])?;
    let bare = root.join("remote.git");
    env.run(&bare, &["symbolic-ref", "HEAD", "refs/heads/main"])?;
    env.run(&seed, &["init", "--quiet"])?;
    env.run(&seed, &["add", "--all"])?;
    env.run(&seed, &["commit", "--quiet", "-m", "Seed the project"])?;
    env.run(
        &seed,
        &["push", "--quiet", "../remote.git", "HEAD:refs/heads/main"],
    )?;
    env.run(root, &["clone", "--quiet", "remote.git", "checkout"])?;
    Ok(Remote {
        bare,
        checkout: root.join("checkout"),
    })
}

/// Copies a folder's files, byte for byte.
fn copy_tree(from: &Path, to: &Path) -> Result<(), GitError> {
    let io = |path: &Path, error: std::io::Error| GitError(format!("{}: {error}", path.display()));
    fs::create_dir_all(to).map_err(|e| io(to, e))?;
    for entry in fs::read_dir(from).map_err(|e| io(from, e))? {
        let entry = entry.map_err(|e| io(from, e))?;
        let source = entry.path();
        let target = to.join(entry.file_name());
        if entry.file_type().map_err(|e| io(&source, e))?.is_dir() {
            copy_tree(&source, &target)?;
        } else {
            fs::copy(&source, &target).map_err(|e| io(&source, e))?;
        }
    }
    Ok(())
}

/// A git command that failed, or a fixture file that could not be written.
#[derive(Debug)]
pub struct GitError(String);

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for GitError {}

#[cfg(test)]
mod tests {
    use std::process::Output;

    use super::*;

    /// The live check behind the hermetic setup (build plan, "The test
    /// bench"): git is handed a hostile environment and home, and must read
    /// only the fixture's configuration.
    #[test]
    fn git_reads_only_the_fixture_configuration() {
        let tmp = tempfile::tempdir().unwrap();
        let env = GitEnv::create(tmp.path().join("home")).unwrap();
        let host = tmp.path().join("host");
        fs::create_dir_all(host.join("git")).unwrap();
        let host_config = "[user]\n\tname = Host\n[core]\n\tautocrlf = true\n";
        fs::write(host.join(".gitconfig"), host_config).unwrap();
        fs::write(host.join("git").join("config"), host_config).unwrap();
        fs::write(host.join("git").join("ignore"), "*\n").unwrap();
        fs::write(host.join("git").join("attributes"), "* text eol=crlf\n").unwrap();
        env.run(tmp.path(), &["init", "--quiet", "repo"]).unwrap();
        let repo = tmp.path().join("repo");
        fs::write(repo.join("a.txt"), "a\n").unwrap();

        let git = |args: &[&str]| -> Output {
            let mut command = Command::new("git");
            command
                .args(args)
                .current_dir(&repo)
                .env("HOME", &host)
                .env("XDG_CONFIG_HOME", &host)
                .env("USERPROFILE", &host)
                .env("GIT_DIR", tmp.path().join("elsewhere"))
                .env("GIT_CONFIG_PARAMETERS", "'user.name'='Host'")
                .env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "core.autocrlf")
                .env("GIT_CONFIG_VALUE_0", "true");
            env.apply(&mut command);
            command.output().unwrap()
        };
        let stdout = |output: Output| {
            assert!(output.status.success(), "{output:?}");
            String::from_utf8(output.stdout).unwrap()
        };

        // The inherited GIT_DIR is gone: git finds the repository itself.
        assert_eq!(stdout(git(&["rev-parse", "--git-dir"])), ".git\n");
        // Every setting comes from the fixture's file or the repository's.
        for line in stdout(git(&["config", "--list", "--show-origin"])).lines() {
            // Git quotes a path holding backslashes, as on Windows:
            // `file:"C:\\Users\\…\\owlshift-test.gitconfig"`.
            let origin = line
                .split('\t')
                .next()
                .unwrap_or_default()
                .trim_end_matches('"');
            assert!(
                origin == "file:.git/config" || origin.ends_with(FIXTURE_CONFIG),
                "{line}"
            );
        }
        assert_eq!(
            stdout(git(&["config", "--get-all", "user.name"])),
            "Owlshift Test\n"
        );
        assert_eq!(
            stdout(git(&["config", "--get-all", "core.autocrlf"])),
            "false\n"
        );
        // No ignore rule and no attribute of the host applies.
        assert_eq!(
            git(&["check-ignore", "--quiet", "a.txt"]).status.code(),
            Some(1)
        );
        assert_eq!(
            stdout(git(&["check-attr", "eol", "--", "a.txt"])),
            "a.txt: eol: unspecified\n"
        );
    }

    #[test]
    fn a_seeded_remote_is_cloned_and_takes_a_pushed_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = tmp.path().join("fixture");
        fs::create_dir_all(fixture.join("docs")).unwrap();
        fs::write(fixture.join("README.md"), "hello\n").unwrap();
        fs::write(fixture.join("docs").join("notes.md"), "notes\r\n").unwrap();
        let start: Timestamp = "2026-09-28T09:00:00Z".parse().unwrap();
        let env = GitEnv::create(tmp.path().join("home")).unwrap().at(start);

        let remote = seed(&env, &tmp.path().join("one"), &fixture).unwrap();
        // Bytes are kept: no line-ending conversion on the way.
        assert_eq!(
            fs::read(remote.checkout.join("docs").join("notes.md")).unwrap(),
            b"notes\r\n"
        );

        env.run(
            &remote.checkout,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "owlshift/T-1",
                "../worktree",
                "origin/main",
            ],
        )
        .unwrap();
        let worktree = tmp.path().join("one").join("worktree");
        fs::write(worktree.join("new.txt"), "new\n").unwrap();
        env.run(&worktree, &["add", "--", "new.txt"]).unwrap();
        env.run(&worktree, &["commit", "--quiet", "-m", "Add a file"])
            .unwrap();
        env.run(&worktree, &["push", "--quiet", "origin", "owlshift/T-1"])
            .unwrap();
        assert_eq!(
            env.run(&remote.bare, &["show", "owlshift/T-1:new.txt"])
                .unwrap(),
            b"new\n"
        );

        // The same project seeded at the same time is the same commit.
        let again = seed(&env, &tmp.path().join("two"), &fixture).unwrap();
        let main = |remote: &Remote| env.run(&remote.bare, &["rev-parse", "main"]).unwrap();
        assert_eq!(main(&remote), main(&again));
    }
}
