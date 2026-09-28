//! `owlshift doctor`: whether this machine is ready, and how to fix what is
//! not.
//!
//! Nothing a probed program prints reaches the report except a version
//! rebuilt from its digits and a login label from a fixed list (live check
//! C8 in `docs/design/build-plan.md`).

use std::fmt;

use owlshift_adapters::harness::{self, Login};
use owlshift_contracts::Harness;
use owlshift_contracts::config::TrackerKind;

use crate::config::{Effective, FileState, exit_text};
use crate::system::{RunError, System, version_of};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Ok,
    Info,
    Warn,
    Fail,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub subject: String,
    pub status: Status,
    pub detail: String,
    /// What to do about a warning or a failure.
    pub fix: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    /// Ready when no check failed; warnings do not count.
    pub fn ready(&self) -> bool {
        self.failures() == 0
    }

    fn failures(&self) -> usize {
        self.checks
            .iter()
            .filter(|check| check.status == Status::Fail)
            .count()
    }
}

/// Runs every check against the host and the loaded configuration.
pub fn run(system: &dyn System, config: &Effective) -> Report {
    let mut checks = vec![git(system)];
    checks.extend(
        required_harnesses(config)
            .into_iter()
            .map(|harness| harness_check(system, harness)),
    );
    checks.push(file_check("project config", &config.project));
    checks.push(file_check("personal config", &config.personal));
    checks.push(tracker_check(config));
    Report { checks }
}

fn check(subject: &str, status: Status, detail: impl Into<String>, fix: Option<&str>) -> Check {
    Check {
        subject: subject.to_owned(),
        status,
        detail: detail.into(),
        fix: fix.map(str::to_owned),
    }
}

fn git(system: &dyn System) -> Check {
    const SUBJECT: &str = "git";
    const INSTALL: &str = "install git: https://git-scm.com/downloads";
    let Some(path) = system.locate("git") else {
        return check(SUBJECT, Status::Fail, "not found on the PATH", Some(INSTALL));
    };
    match system.run(&path, &["--version"], None) {
        Ok(out) if out.code == Some(0) => match version_of(&out.stdout) {
            Some(version) => check(
                SUBJECT,
                Status::Ok,
                format!("{version} ({})", path.display()),
                None,
            ),
            None => check(
                SUBJECT,
                Status::Warn,
                format!("unfamiliar `git --version` answer ({})", path.display()),
                None,
            ),
        },
        Ok(out) => check(
            SUBJECT,
            Status::Fail,
            format!("`git --version` failed ({})", exit_text(out.code)),
            Some(INSTALL),
        ),
        Err(error) => check(
            SUBJECT,
            Status::Fail,
            format!("`git --version`: {error}"),
            Some(INSTALL),
        ),
    }
}

/// The harnesses declared in the personal file, or both when it declares
/// none (scenario S15: each configured harness must be ready).
fn required_harnesses(config: &Effective) -> Vec<Harness> {
    if let FileState::Loaded { config, .. } = &config.personal {
        let declared: Vec<Harness> = config.harnesses.iter().map(|(h, _)| h).collect();
        if !declared.is_empty() {
            return declared;
        }
    }
    vec![Harness::Claude, Harness::Codex]
}

fn harness_check(system: &dyn System, harness: Harness) -> Check {
    let program = harness::program(harness);
    let Some(path) = system.locate(program) else {
        return check(
            program,
            Status::Fail,
            "not found on the PATH",
            Some(harness::install_hint(harness)),
        );
    };
    let version = match system.run(&path, &["--version"], None) {
        Ok(out) if out.code == Some(0) => version_of(&out.stdout),
        _ => None,
    }
    .unwrap_or_else(|| "version unknown".to_owned());
    let found = format!("{version} ({})", path.display());

    let status_args = harness::login_status_args(harness);
    let status_command = format!("{program} {}", status_args.join(" "));
    let (login, why) = match system.run(&path, status_args, None) {
        Ok(out) => (
            harness::parse_login(harness, out.code, &out.stdout, &out.stderr),
            "gave an answer Owlshift does not know",
        ),
        Err(RunError::TimedOut) => (Login::Unknown, "did not answer in time"),
        Err(RunError::Io(_)) => (Login::Unknown, "could not be run"),
    };
    match login {
        Login::LoggedIn { method, plan } => {
            let plan = plan.map(|p| format!(", {p} plan")).unwrap_or_default();
            check(
                program,
                Status::Ok,
                format!("{found}, logged in ({method}{plan})"),
                None,
            )
        }
        Login::LoggedOut => check(
            program,
            Status::Fail,
            format!("{found}, not logged in"),
            Some(harness::login_hint(harness)),
        ),
        Login::Unknown => Check {
            subject: program.to_owned(),
            status: Status::Fail,
            detail: format!("{found}, login state unknown: `{status_command}` {why}"),
            fix: Some(format!("run `{status_command}` yourself to see why")),
        },
    }
}

fn file_check<T>(subject: &str, state: &FileState<T>) -> Check {
    match state {
        FileState::Loaded { path, .. } => {
            check(subject, Status::Ok, path.display().to_string(), None)
        }
        FileState::Absent(path) => check(
            subject,
            Status::Info,
            format!("not found at {}", path.display()),
            None,
        ),
        FileState::NotApplicable(reason) => check(subject, Status::Info, format!("none ({reason})"), None),
        FileState::Unavailable(reason) => check(
            subject,
            Status::Fail,
            reason.clone(),
            Some("make sure git works in this directory"),
        ),
        FileState::Invalid { path, error } => check(
            subject,
            Status::Fail,
            format!("{}: {error}", path.display()),
            None,
        ),
    }
}

/// No tracker adapter exists yet (the first arrives in P1), and no
/// capability contract is written before three adapters exist (principle 8),
/// so the configured tracker can only be named.
fn tracker_check(config: &Effective) -> Check {
    const SUBJECT: &str = "tracker";
    let FileState::Loaded { config, .. } = &config.project else {
        return check(
            SUBJECT,
            Status::Info,
            "no project configuration, no adapter to check",
            None,
        );
    };
    let kind = match config.tracker.kind {
        TrackerKind::Linear => "linear",
        TrackerKind::Markdown => "markdown",
    };
    check(
        SUBJECT,
        Status::Warn,
        format!("`{kind}` is configured, and this build has no tracker adapter yet"),
        None,
    )
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let width = self
            .checks
            .iter()
            .map(|check| check.subject.len())
            .max()
            .unwrap_or(0);
        for check in &self.checks {
            let label = match check.status {
                Status::Ok => "ok",
                Status::Info => "info",
                Status::Warn => "warn",
                Status::Fail => "FAIL",
            };
            writeln!(f, "{label:<5} {:<width$}  {}", check.subject, check.detail)?;
            if let Some(fix) = &check.fix {
                writeln!(f, "{:<5} {:<width$}  fix: {fix}", "", "")?;
            }
        }
        match self.failures() {
            0 => writeln!(f, "\nReady."),
            1 => writeln!(f, "\nNot ready: 1 problem to fix."),
            n => writeln!(f, "\nNot ready: {n} problems to fix."),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::system::fake::{Answer, FakeSystem};

    const CLAUDE_STATUS: &str = "claude auth status --json";
    const CODEX_STATUS: &str = "codex login status";

    /// Status answers carrying an e-mail address, an organisation and part
    /// of an API key, as C8 recorded them.
    const CLAUDE_LOGGED_IN: &str = r#"{"loggedIn": true, "authMethod": "claude.ai", "email": "ada@example.com", "orgId": "0f0e0d0c-org-id", "orgName": "Ada's Organization", "subscriptionType": "max"}"#;
    const CODEX_API_KEY: &str = "Logged in using an API key - sk-dummy***0fake\n";

    fn no_config() -> Effective {
        Effective {
            project: FileState::NotApplicable("not in a git repository".into()),
            personal: FileState::Absent(PathBuf::from("/home/ada/.config/owlshift/config.toml")),
        }
    }

    fn with_git(system: FakeSystem) -> FakeSystem {
        system
            .install("git")
            .answer("git --version", Answer::Exit(0, "git version 2.54.0 (Apple Git-157)\n", ""))
    }

    fn with_harnesses(system: FakeSystem) -> FakeSystem {
        system
            .install("claude")
            .install("codex")
            .answer("claude --version", Answer::Exit(0, "2.1.283 (Claude Code)\n", ""))
            .answer("codex --version", Answer::Exit(0, "codex-cli 0.154.0\n", ""))
    }

    fn fixes(report: &Report, subject: &str) -> Vec<String> {
        report
            .checks
            .iter()
            .filter(|check| check.subject == subject && check.status == Status::Fail)
            .filter_map(|check| check.fix.clone())
            .collect()
    }

    #[test]
    fn a_ready_machine_and_nothing_private_in_the_report() {
        let system = with_harnesses(with_git(FakeSystem::default()))
            .answer(CLAUDE_STATUS, Answer::Exit(0, CLAUDE_LOGGED_IN, ""))
            .answer(CODEX_STATUS, Answer::Exit(0, "", CODEX_API_KEY));
        let report = run(&system, &no_config());
        let shown = report.to_string();

        assert!(report.ready(), "{shown}");
        assert!(shown.contains("2.54.0 (/fake/bin/git)"), "{shown}");
        assert!(shown.contains("2.1.283 (/fake/bin/claude), logged in (claude.ai, max plan)"), "{shown}");
        assert!(shown.contains("0.154.0 (/fake/bin/codex), logged in (API key)"), "{shown}");
        // Defense in depth: `Login` holds only fixed labels, by construction.
        for private in ["ada@", "example.com", "0f0e0d0c", "Organization", "sk-dummy", "0fake"] {
            assert!(!shown.contains(private), "{private} leaked:\n{shown}");
        }
    }

    #[test]
    fn missing_and_logged_out_harnesses_say_how_to_fix_them() {
        let system = with_git(FakeSystem::default())
            .install("codex")
            .answer("codex --version", Answer::Exit(0, "codex-cli 0.154.0\n", ""))
            .answer(CODEX_STATUS, Answer::Exit(1, "", "Not logged in\n"));
        let report = run(&system, &no_config());

        assert!(!report.ready());
        assert_eq!(fixes(&report, "claude"), [harness::install_hint(Harness::Claude)]);
        assert_eq!(fixes(&report, "codex"), ["run `codex login`"]);
        assert!(report.to_string().contains("Not ready: 2 problems to fix."));
    }

    #[test]
    fn an_undeterminable_login_fails_without_echoing_the_answer() {
        let system = with_harnesses(with_git(FakeSystem::default()))
            .answer(CLAUDE_STATUS, Answer::TimedOut)
            .answer(CODEX_STATUS, Answer::Exit(0, "", "Signed in as ada@example.com\n"));
        let report = run(&system, &no_config());
        let shown = report.to_string();

        assert!(!report.ready());
        assert!(shown.contains("`claude auth status --json` did not answer in time"), "{shown}");
        assert_eq!(fixes(&report, "claude"), ["run `claude auth status --json` yourself to see why"]);
        assert!(!shown.contains("ada@example.com"), "{shown}");
    }

    #[test]
    fn a_missing_git_fails() {
        let system = with_harnesses(FakeSystem::default())
            .answer(CLAUDE_STATUS, Answer::Exit(0, CLAUDE_LOGGED_IN, ""))
            .answer(CODEX_STATUS, Answer::Exit(0, "", "Logged in using ChatGPT\n"));
        let report = run(&system, &no_config());
        assert_eq!(fixes(&report, "git"), ["install git: https://git-scm.com/downloads"]);
    }

    #[test]
    fn only_declared_harnesses_are_required() {
        let dir = tempfile::tempdir().unwrap();
        let personal = dir.path().join("config.toml");
        std::fs::write(&personal, "[harnesses.claude]\n").unwrap();
        let config = Effective {
            project: FileState::NotApplicable("not in a git repository".into()),
            personal: FileState::load(personal, owlshift_contracts::config::PersonalConfig::parse),
        };
        let system = with_git(FakeSystem::default())
            .install("claude")
            .answer("claude --version", Answer::Exit(0, "2.1.283 (Claude Code)\n", ""))
            .answer(CLAUDE_STATUS, Answer::Exit(0, CLAUDE_LOGGED_IN, ""));
        let report = run(&system, &config);

        assert!(report.ready(), "{report}");
        assert!(report.checks.iter().all(|check| check.subject != "codex"));
    }
}
