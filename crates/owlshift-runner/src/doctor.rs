//! `owlshift doctor`: whether this machine is ready, and how to fix what is
//! not.
//!
//! Nothing a probed program prints reaches the report except a version
//! rebuilt from its digits and a login label from a fixed list (live check
//! C8 in `docs/design/build-plan.md`).

use std::fmt;
use std::path::Path;

use owlshift_adapters::harness::{self, Login, tested};
use owlshift_adapters::tracker::Capability;
use owlshift_adapters::tracker::linear::LinearTracker;
use owlshift_adapters::tracker::markdown::MarkdownTracker;
use owlshift_contracts::Harness;
use owlshift_contracts::config::TrackerKind;
use owlshift_platform::keychain::SERVICE;

use crate::config::{Effective, FileState, exit_text};
use crate::executor::harness::{AGENT_LOGIN_FIX, CLAUDE_AGENT_ACCOUNT};
use crate::system::{RunError, System, exact_version_of, version_of};
#[cfg(unix)]
use crate::system::{SentinelProbe, SentinelStatus};

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
    let harnesses = required_harnesses(config);
    checks.extend(
        harnesses
            .iter()
            .map(|harness| harness_check(system, *harness)),
    );
    checks.push(sandbox_check(system));
    #[cfg(unix)]
    checks.push(sentinel_check(&system.sentinel()));
    #[cfg(unix)]
    checks.push(sentinel_probe_check(&system.sentinel_probe()));
    // A missing `claude` fails its own line: its agent login is not asked.
    if harnesses.contains(&Harness::Claude) && system.locate("claude").is_some() {
        checks.push(agent_login_check(system));
    }
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
        return check(
            SUBJECT,
            Status::Fail,
            "not found on the PATH",
            Some(INSTALL),
        );
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

/// Whether agent runs can be confined (OWL-41): without it, `owlshift do`
/// refuses to start one. The fix is the sandbox's own: install bwrap, allow
/// it user namespaces, or use WSL2.
fn sandbox_check(system: &dyn System) -> Check {
    const SUBJECT: &str = "sandbox";
    match system.sandbox() {
        Ok(()) => check(
            SUBJECT,
            Status::Ok,
            if cfg!(target_os = "macos") {
                "agent runs are confined with sandbox-exec"
            } else {
                "agent runs are confined with bwrap"
            },
            None,
        ),
        Err(error) => Check {
            subject: SUBJECT.to_owned(),
            status: Status::Fail,
            detail: "agent runs cannot be confined here, so none is started".to_owned(),
            fix: Some(error.to_string()),
        },
    }
}

/// Whether the sentinel of this very command runs, the process that stops
/// the probes and agent runs Owlshift started when Owlshift is killed
/// outright (OWL-86). A warning, never a failure: it is a best effort, not a
/// guardrail, and it is never restarted (OWL-88). A stopped one protects
/// nothing either (OWL-91).
#[cfg(unix)]
fn sentinel_check(status: &SentinelStatus) -> Check {
    const SUBJECT: &str = "sentinel";
    const UNPROTECTED: &str =
        "a hard kill of Owlshift would leave the processes it started running";
    const FIND: &str = "look for what ends the `/bin/sh` process whose command line ends in \
                        `owlshift-sentinel`";
    const FIND_STOP: &str = "look for what stops the `/bin/sh` process whose command line ends \
                             in `owlshift-sentinel`";
    match status {
        SentinelStatus::Running { pid } => check(
            SUBJECT,
            Status::Ok,
            format!(
                "running (pid {pid}): a hard kill of Owlshift stops, best effort, the processes it started"
            ),
            None,
        ),
        SentinelStatus::Stopped { pid } => check(
            SUBJECT,
            Status::Warn,
            format!("stopped (pid {pid}), it reads nothing until continued: {UNPROTECTED}"),
            Some(FIND_STOP),
        ),
        SentinelStatus::Ended(how) => check(
            SUBJECT,
            Status::Warn,
            format!("ended ({how}): {UNPROTECTED}"),
            Some(FIND),
        ),
        SentinelStatus::NotRunning => check(
            SUBJECT,
            Status::Warn,
            format!("not running, it could not start: {UNPROTECTED}"),
            Some("check that `/bin/sh` runs; the error is printed above"),
        ),
    }
}

/// Whether a sentinel works on this host, not only runs: a test sentinel,
/// told of a test process group, stops it when its input ends (OWL-90).
/// Independent of the `sentinel` line, and, like it, a warning at worst.
#[cfg(unix)]
fn sentinel_probe_check(probe: &SentinelProbe) -> Check {
    const SUBJECT: &str = "sentinel test";
    const UNPROTECTED: &str = "a hard kill of Owlshift may leave the processes it started running";
    match probe {
        SentinelProbe::Works { elapsed } => check(
            SUBJECT,
            Status::Ok,
            format!(
                "a test sentinel stopped a test process group {} ms after its input ended",
                elapsed.as_millis()
            ),
            None,
        ),
        SentinelProbe::CannotStart(why) => check(
            SUBJECT,
            Status::Warn,
            format!("could not start {why}: {UNPROTECTED}"),
            Some("check that `/bin/sh` runs"),
        ),
        SentinelProbe::Fails(why) => check(
            SUBJECT,
            Status::Warn,
            format!("{why}: {UNPROTECTED}"),
            Some(
                "check that `/bin/sh` is a POSIX shell whose `kill -s KILL -- -<group>` stops \
                 a process group",
            ),
        ),
    }
}

/// Whether agent runs have their Claude Code login: a token made by
/// `claude setup-token`, in the system keychain (OWL-94). Confined, agent
/// runs cannot reach the Keychain, where the operator's own login lives.
/// Only the token's presence is asked, never its value; whether it still
/// works shows at the first run.
fn agent_login_check(system: &dyn System) -> Check {
    const SUBJECT: &str = "claude agent login";
    let place =
        format!("in the system keychain (service `{SERVICE}`, account `{CLAUDE_AGENT_ACCOUNT}`)");
    match system.secret_stored(CLAUDE_AGENT_ACCOUNT) {
        Ok(true) => check(
            SUBJECT,
            Status::Ok,
            format!("a token for agent runs is stored {place}"),
            None,
        ),
        Ok(false) => check(
            SUBJECT,
            Status::Fail,
            format!(
                "no token for agent runs {place}: agent runs cannot reach the Keychain, so \
                 they log in with a long-lived token of your subscription"
            ),
            Some(AGENT_LOGIN_FIX),
        ),
        Err(error) => check(
            SUBJECT,
            Status::Fail,
            format!("could not tell whether a token for agent runs is stored {place}: {error}"),
            Some(
                "make the system keychain available (on Linux, a running Secret Service), \
                 then run `owlshift doctor` again",
            ),
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
    let (version, exact) = match system.run(&path, &["--version"], None) {
        Ok(out) if out.code == Some(0) => (version_of(&out.stdout), exact_version_of(&out.stdout)),
        _ => (None, None),
    };
    let found = format!(
        "{} ({})",
        version.as_deref().unwrap_or("version unknown"),
        path.display()
    );
    let mut result = login_check(system, harness, &path, &found);
    flag_untested(&mut result, harness, exact.as_deref());
    result
}

fn login_check(system: &dyn System, harness: Harness, path: &Path, found: &str) -> Check {
    let program = harness::program(harness);

    let status_args = harness::login_status_args(harness);
    let status_command = format!("{program} {}", status_args.join(" "));
    let (login, why) = match system.run(path, status_args, None) {
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

/// Warns on a harness version Owlshift was not tested with, or could not
/// read: the CLI may have changed a flag or a message under the user
/// (`docs/design/runtime-and-operations.md`, "Updates & versions"). Never a
/// failure, since an untested version may well work; a failed check keeps
/// its status and its fix.
fn flag_untested(check: &mut Check, harness: Harness, version: Option<&str>) {
    if version.is_some_and(|version| tested::is_tested(harness, version)) {
        return;
    }
    let tested = tested::tested_versions(harness);
    if tested.is_empty() {
        check
            .detail
            .push_str("; no version tested with Owlshift yet");
    } else {
        check.detail.push_str("; version not tested with Owlshift");
    }
    if check.status == Status::Ok {
        check.status = Status::Warn;
        if !tested.is_empty() {
            check.fix = Some(format!(
                "Owlshift is tested with {} {}; if a run misbehaves, install a tested version",
                harness::program(harness),
                tested.join(", ")
            ));
        }
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
        FileState::NotApplicable(reason) => {
            check(subject, Status::Info, format!("none ({reason})"), None)
        }
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

/// The configured tracker's adapter and the capabilities it declares
/// (architecture section 6). A required capability this build does not
/// implement yet is a warning, not a failure: `owlshift do` needs only to
/// read tickets and comments, and refusing a project is `init`'s job. The
/// check reads the adapter's constants: it opens neither the tracker nor the
/// keychain.
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
    let (kind, declared) = match config.tracker.kind {
        TrackerKind::Linear => ("linear", LinearTracker::CAPABILITIES),
        TrackerKind::Markdown => ("markdown", MarkdownTracker::CAPABILITIES),
    };
    let list = |capabilities: &[Capability]| {
        capabilities
            .iter()
            .map(|c| c.describe())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let missing: Vec<Capability> = Capability::ALL
        .into_iter()
        .filter(|c| c.required() && !declared.contains(c))
        .collect();
    let detail = format!("`{kind}`: {}", list(declared));
    if missing.is_empty() {
        check(SUBJECT, Status::Ok, detail, None)
    } else {
        check(
            SUBJECT,
            Status::Warn,
            format!("{detail}; not built yet: {}", list(&missing)),
            None,
        )
    }
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
    use owlshift_platform::sandbox::SandboxError;

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
        system.install("git").answer(
            "git --version",
            Answer::Exit(0, "git version 2.54.0 (Apple Git-157)\n", ""),
        )
    }

    fn with_harnesses(system: FakeSystem) -> FakeSystem {
        system
            .install("claude")
            .install("codex")
            .answer(
                "claude --version",
                Answer::Exit(0, "2.1.283 (Claude Code)\n", ""),
            )
            .answer(
                "codex --version",
                Answer::Exit(0, "codex-cli 0.154.0\n", ""),
            )
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
        assert!(
            shown.contains("2.1.283 (/fake/bin/claude), logged in (claude.ai, max plan)"),
            "{shown}"
        );
        assert!(
            shown.contains("0.154.0 (/fake/bin/codex), logged in (API key)"),
            "{shown}"
        );
        // Defense in depth: `Login` holds only fixed labels, by construction.
        for private in [
            "ada@",
            "example.com",
            "0f0e0d0c",
            "Organization",
            "sk-dummy",
            "0fake",
        ] {
            assert!(!shown.contains(private), "{private} leaked:\n{shown}");
        }
    }

    /// A machine where agent runs cannot be confined is not ready, and the
    /// fix names what to do: install bwrap, or let it create user
    /// namespaces with the AppArmor profile.
    #[test]
    fn a_missing_or_blocked_sandbox_is_not_ready() {
        let ready = || {
            with_harnesses(with_git(FakeSystem::default()))
                .answer(CLAUDE_STATUS, Answer::Exit(0, CLAUDE_LOGGED_IN, ""))
                .answer(CODEX_STATUS, Answer::Exit(0, "", CODEX_API_KEY))
        };
        let missing = run(
            &ready().no_sandbox(SandboxError::Missing { program: "bwrap" }),
            &no_config(),
        );
        assert!(!missing.ready());
        assert!(fixes(&missing, "sandbox")[0].contains("apt install bubblewrap"));

        let blocked = run(
            &ready().no_sandbox(SandboxError::Blocked {
                program: "bwrap",
                reason: "setting up uid map: Permission denied".into(),
            }),
            &no_config(),
        );
        assert!(!blocked.ready());
        let fix = &fixes(&blocked, "sandbox")[0];
        assert!(
            fix.contains("apparmor_parser -r /etc/apparmor.d/bwrap"),
            "{fix}"
        );
        assert!(fix.contains("setting up uid map"), "{fix}");
    }

    /// OWL-88: a sentinel that ended or never started is a warning that says
    /// what is lost, never a failure; a running one is quiet. OWL-91: a
    /// stopped one warns too.
    #[cfg(unix)]
    #[test]
    fn a_missing_or_ended_sentinel_warns_without_failing() {
        let ready = || logged_in(with_harnesses(with_git(FakeSystem::default())));
        let running = run(&ready(), &no_config());
        let sentinel = line(&running, "sentinel");
        assert_eq!(sentinel.status, Status::Ok, "{running}");
        assert!(
            sentinel.detail.starts_with("running (pid 4242)"),
            "{running}"
        );

        for (status, detail, fix) in [
            (
                SentinelStatus::Ended("signal: 9 (SIGKILL)".into()),
                "ended (signal: 9 (SIGKILL)): a hard kill of Owlshift would leave",
                "`owlshift-sentinel`",
            ),
            (
                SentinelStatus::Stopped { pid: 4242 },
                "stopped (pid 4242), it reads nothing until continued: a hard kill of Owlshift \
                 would leave",
                "look for what stops the `/bin/sh` process",
            ),
            (
                SentinelStatus::NotRunning,
                "not running, it could not start: a hard kill of Owlshift would leave",
                "check that `/bin/sh` runs",
            ),
        ] {
            let report = run(&ready().sentinel_is(status), &no_config());
            let sentinel = line(&report, "sentinel");
            assert!(report.ready(), "{report}");
            assert_eq!(sentinel.status, Status::Warn, "{report}");
            assert!(sentinel.detail.starts_with(detail), "{report}");
            assert!(sentinel.fix.as_deref().unwrap().contains(fix), "{report}");
        }
    }

    /// OWL-90: a test sentinel that does not stop its test group, or cannot
    /// be started, is a warning that says why, never a failure.
    #[cfg(unix)]
    #[test]
    fn a_sentinel_that_fails_its_test_warns_without_failing() {
        let ready = || logged_in(with_harnesses(with_git(FakeSystem::default())));
        let works = run(&ready(), &no_config());
        let probe = line(&works, "sentinel test");
        assert_eq!(probe.status, Status::Ok, "{works}");
        assert!(
            probe
                .detail
                .starts_with("a test sentinel stopped a test process group 3 ms"),
            "{works}"
        );

        for (probe, detail, fix) in [
            (
                SentinelProbe::Fails("the test group still ran 5000 ms".into()),
                "the test group still ran 5000 ms: a hard kill of Owlshift may leave",
                "`kill -s KILL -- -<group>`",
            ),
            (
                SentinelProbe::CannotStart("a test sentinel: no such file".into()),
                "could not start a test sentinel: no such file: a hard kill",
                "check that `/bin/sh` runs",
            ),
        ] {
            let report = run(&ready().sentinel_probe_is(probe), &no_config());
            let tested = line(&report, "sentinel test");
            assert!(report.ready(), "{report}");
            assert_eq!(tested.status, Status::Warn, "{report}");
            assert!(tested.detail.starts_with(detail), "{report}");
            assert!(tested.fix.as_deref().unwrap().contains(fix), "{report}");
        }
    }

    /// OWL-94: agent runs need their token in the keychain. Stored, the
    /// line is ok; absent, the fix names `claude setup-token` and `owlshift
    /// init`; an unreadable keychain says so. Without `claude`, it is not
    /// asked.
    #[test]
    fn the_agent_login_is_a_token_in_the_keychain() {
        let ready = || logged_in(with_harnesses(with_git(FakeSystem::default())));
        let stored = run(&ready(), &no_config());
        let login = line(&stored, "claude agent login");
        assert_eq!(login.status, Status::Ok, "{stored}");
        assert!(
            login
                .detail
                .contains("(service `owlshift`, account `claude-agent`)"),
            "{stored}"
        );

        let missing = run(&ready().unstored("claude-agent"), &no_config());
        assert!(!missing.ready());
        let fix = &fixes(&missing, "claude agent login")[0];
        assert!(fix.contains("`claude setup-token`"), "{fix}");
        assert!(fix.contains("`owlshift init`"), "{fix}");
        assert!(!missing.to_string().contains("auth login"), "{missing}");

        let locked = run(&ready().keychain_fails("keychain: locked"), &no_config());
        let login = line(&locked, "claude agent login");
        assert_eq!(login.status, Status::Fail, "{locked}");
        assert!(login.detail.ends_with("keychain: locked"), "{locked}");

        let no_claude = with_git(FakeSystem::default()).install("codex").answer(
            "codex --version",
            Answer::Exit(0, "codex-cli 0.154.0\n", ""),
        );
        let report = run(&logged_in(no_claude), &no_config());
        assert!(
            report
                .checks
                .iter()
                .all(|check| check.subject != "claude agent login"),
            "{report}"
        );
    }

    #[test]
    fn missing_and_logged_out_harnesses_say_how_to_fix_them() {
        let system = with_git(FakeSystem::default())
            .install("codex")
            .answer(
                "codex --version",
                Answer::Exit(0, "codex-cli 0.154.0\n", ""),
            )
            .answer(CODEX_STATUS, Answer::Exit(1, "", "Not logged in\n"));
        let report = run(&system, &no_config());

        assert!(!report.ready());
        assert_eq!(
            fixes(&report, "claude"),
            [harness::install_hint(Harness::Claude)]
        );
        assert_eq!(fixes(&report, "codex"), ["run `codex login`"]);
        assert!(report.to_string().contains("Not ready: 2 problems to fix."));
    }

    #[test]
    fn an_undeterminable_login_fails_without_echoing_the_answer() {
        let system = with_harnesses(with_git(FakeSystem::default()))
            .answer(CLAUDE_STATUS, Answer::TimedOut)
            .answer(
                CODEX_STATUS,
                Answer::Exit(0, "", "Signed in as ada@example.com\n"),
            );
        let report = run(&system, &no_config());
        let shown = report.to_string();

        assert!(!report.ready());
        assert!(
            shown.contains("`claude auth status --json` did not answer in time"),
            "{shown}"
        );
        assert_eq!(
            fixes(&report, "claude"),
            ["run `claude auth status --json` yourself to see why"]
        );
        assert!(!shown.contains("ada@example.com"), "{shown}");
    }

    fn line<'a>(report: &'a Report, subject: &str) -> &'a Check {
        report
            .checks
            .iter()
            .find(|check| check.subject == subject)
            .unwrap()
    }

    fn logged_in(system: FakeSystem) -> FakeSystem {
        system
            .answer(CLAUDE_STATUS, Answer::Exit(0, CLAUDE_LOGGED_IN, ""))
            .answer(CODEX_STATUS, Answer::Exit(0, "", CODEX_API_KEY))
    }

    #[test]
    fn an_untested_or_unreadable_version_warns_without_failing() {
        let fix = "Owlshift is tested with claude 2.1.283, 2.1.284; \
                   if a run misbehaves, install a tested version";
        for answer in [
            Answer::Exit(0, "9.9.9 (Claude Code)\n", ""),
            Answer::Exit(0, "2.1.283-beta.1 (Claude Code)\n", ""),
            Answer::TimedOut,
        ] {
            let system = logged_in(with_harnesses(with_git(FakeSystem::default())))
                .answer("claude --version", answer);
            let report = run(&system, &no_config());
            let claude = line(&report, "claude");
            assert!(report.ready(), "{report}");
            assert_eq!(claude.status, Status::Warn, "{report}");
            assert!(
                claude
                    .detail
                    .ends_with("logged in (claude.ai, max plan); version not tested with Owlshift"),
                "{report}"
            );
            assert_eq!(claude.fix.as_deref(), Some(fix));
        }
    }

    #[test]
    fn a_tested_version_is_quiet_and_codex_has_none_yet() {
        let system = logged_in(with_harnesses(with_git(FakeSystem::default())));
        let report = run(&system, &no_config());
        let claude = line(&report, "claude");
        assert_eq!(claude.status, Status::Ok, "{report}");
        assert!(!claude.detail.contains("tested"), "{report}");
        assert_eq!(claude.fix, None);

        // Codex has no contract tests, so no version of it is tested.
        let codex = line(&report, "codex");
        assert!(report.ready(), "{report}");
        assert_eq!(codex.status, Status::Warn);
        assert_eq!(
            codex.detail,
            "0.154.0 (/fake/bin/codex), logged in (API key); no version tested with Owlshift yet"
        );
        assert_eq!(codex.fix, None);
    }

    #[test]
    fn a_failure_at_an_untested_version_keeps_its_fix() {
        let system = with_harnesses(with_git(FakeSystem::default()))
            .answer(
                "claude --version",
                Answer::Exit(0, "9.9.9 (Claude Code)\n", ""),
            )
            .answer(
                CLAUDE_STATUS,
                Answer::Exit(1, r#"{"loggedIn": false, "authMethod": "none"}"#, ""),
            )
            .answer(CODEX_STATUS, Answer::Exit(0, "", CODEX_API_KEY));
        let report = run(&system, &no_config());
        let claude = line(&report, "claude");
        assert_eq!(claude.status, Status::Fail);
        assert_eq!(
            claude.detail,
            "9.9.9 (/fake/bin/claude), not logged in; version not tested with Owlshift"
        );
        assert_eq!(fixes(&report, "claude"), ["run `claude auth login`"]);
    }

    #[test]
    fn a_missing_git_fails() {
        let system = with_harnesses(FakeSystem::default())
            .answer(CLAUDE_STATUS, Answer::Exit(0, CLAUDE_LOGGED_IN, ""))
            .answer(
                CODEX_STATUS,
                Answer::Exit(0, "", "Logged in using ChatGPT\n"),
            );
        let report = run(&system, &no_config());
        assert_eq!(
            fixes(&report, "git"),
            ["install git: https://git-scm.com/downloads"]
        );
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
            .answer(
                "claude --version",
                Answer::Exit(0, "2.1.283 (Claude Code)\n", ""),
            )
            .answer(CLAUDE_STATUS, Answer::Exit(0, CLAUDE_LOGGED_IN, ""));
        let report = run(&system, &config);

        assert!(report.ready(), "{report}");
        assert!(report.checks.iter().all(|check| check.subject != "codex"));
    }

    #[test]
    fn the_tracker_line_names_what_the_adapter_does_and_does_not_do_yet() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("owlshift.toml");
        std::fs::write(
            &project,
            r#"requires = ">=0.0"
[tracker]
kind = "linear"
team = "OWL"
admit = "delegation"
states = { ready = "Todo", working = "In Progress", needs_input = "Needs Input", review = "In Review" }
[stack]
gate = ["cargo test"]
[pipeline]
default = "trivial"
plan_approval = "never"
[models]
[policy]
always_human = []
"#,
        )
        .unwrap();
        let config = Effective {
            project: FileState::load(project, owlshift_contracts::config::ProjectConfig::parse),
            personal: FileState::NotApplicable("no configuration directory".into()),
        };
        let tracker = tracker_check(&config);
        assert_eq!(tracker.status, Status::Warn);
        assert_eq!(
            tracker.detail,
            "`linear`: read a ticket, read and post comments; \
             not built yet: list admitted tickets, visible stage"
        );
    }
}
