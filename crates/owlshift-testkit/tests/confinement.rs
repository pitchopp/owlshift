//! OWL-41's acceptance through the executor: a harness and the processes it
//! starts, spawned by `Executor::run`, run inside the OS sandbox. A harness
//! that is a shell script stands in, so its commands are the harness's own
//! descendants. The gate's commands are checked the same way, and in more
//! detail, by the unit tests of `owlshift-runner`'s `executor::gate`.
//!
//! Where agents cannot be confined (a Linux machine without bwrap or user
//! namespaces), the test is skipped with a message, unless
//! `OWLSHIFT_REQUIRE_CONFINEMENT` is set; native Windows must refuse.

use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::Duration;

use tempfile::TempDir;

use owlshift_adapters::harness::claude;
use owlshift_contracts::Role;
use owlshift_contracts::brief::{
    Author, Brief, PermissionLevel, Permissions, Relation, TicketBrief,
};
use owlshift_contracts::format::Format;
use owlshift_contracts::ids::{RelativePath, TicketId};
use owlshift_platform::keychain::Secret;
use owlshift_runner::agent_env::AgentEnv;
use owlshift_runner::executor::harness::drive_plain;
use owlshift_runner::executor::{
    Executor, ExecutorError, Git, Harness, HarnessEnd, HarnessError, HarnessLogin, HarnessRun,
    HarnessStatus, RunLog, RunReport, RunSpec, SandboxNeeds,
};
use owlshift_testkit::git::{GitEnv, seed};

const BRANCH: &str = "owlshift/T-1";

/// A harness that is one shell script, logged in with a token when one is
/// given, as Claude Code is, and naming a temporary folder of its own when
/// asked, as Claude Code does (OWL-100).
struct Script {
    line: String,
    login: Option<Secret>,
    temp_variable: Option<&'static str>,
}

impl Harness for Script {
    fn command(&self, _run: &HarnessRun<'_>) -> Result<Command, HarnessError> {
        let mut command = Command::new("sh");
        command.arg("-c").arg(&self.line);
        Ok(command)
    }

    fn sandbox_needs(&self, _agent: &AgentEnv) -> Result<SandboxNeeds, HarnessError> {
        Ok(SandboxNeeds {
            login: self.login.clone().map(|token| HarnessLogin {
                descriptor_variable: claude::LOGIN_TOKEN_FD_ENV,
                token,
                config_variable: claude::CONFIG_DIR_ENV,
            }),
            temp_variable: self.temp_variable,
            ..SandboxNeeds::default()
        })
    }

    fn drive(
        &self,
        _run: &HarnessRun<'_>,
        child: &mut Child,
        log: &mut RunLog,
    ) -> io::Result<HarnessEnd> {
        let run = drive_plain(child, log)?;
        Ok(HarnessEnd::new(run.exit_code, HarnessStatus::Completed))
    }
}

struct Bench {
    tmp: TempDir,
    env: GitEnv,
    main: PathBuf,
    worktree: PathBuf,
}

impl Bench {
    /// A project, and fake secrets planted in the bench's home: a gh token
    /// file and another project's `.env`.
    fn new() -> Self {
        let tmp = tempfile::Builder::new()
            .prefix("owlshift confinement ")
            .tempdir()
            .unwrap();
        let project = tmp.path().join("project");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("README.md"), "hello\n").unwrap();
        let home = tmp.path().join("home");
        let env = GitEnv::create(home.clone()).unwrap();
        let remote = seed(&env, tmp.path(), &project).unwrap();
        fs::create_dir_all(home.join(".config/gh")).unwrap();
        fs::create_dir_all(home.join("other-project")).unwrap();
        fs::write(
            home.join(".config/gh/hosts.yml"),
            "example.invalid:\n    oauth_token: gho_FAKE_owl41\n",
        )
        .unwrap();
        fs::write(home.join("other-project/.env"), "API_KEY=FAKE_owl41\n").unwrap();
        Self {
            worktree: tmp.path().join("worktree"),
            main: remote.checkout,
            tmp,
            env,
        }
    }

    fn agent(&self) -> AgentEnv {
        AgentEnv::new(self.env.agent_parent()).unwrap()
    }

    fn run(&self, agent: AgentEnv, script: &str) -> Result<RunReport, ExecutorError> {
        self.run_logged_in(agent, script, None)
    }

    fn run_logged_in(
        &self,
        agent: AgentEnv,
        script: &str,
        login: Option<Secret>,
    ) -> Result<RunReport, ExecutorError> {
        let harness = Script {
            line: script.to_owned(),
            login,
            temp_variable: None,
        };
        self.run_harness(agent, &harness)
    }

    fn run_harness(&self, agent: AgentEnv, harness: &Script) -> Result<RunReport, ExecutorError> {
        let runner = self.env.clone();
        let executor = Executor {
            git: Git::with_setup("git", move |command| runner.apply(command)),
            agent,
            forge_hosts: Vec::new(),
            timeout: Duration::from_secs(60),
            gate_timeout: Duration::from_secs(60),
        };
        let spec = RunSpec {
            main: &self.main,
            worktree: &self.worktree,
            branch: BRANCH,
            base: "origin/main",
            run_dir: &self.run_dir(),
            brief: &brief(),
        };
        executor.run(&spec, harness)
    }

    fn run_dir(&self) -> PathBuf {
        self.tmp.path().join("run")
    }

    /// The probe's lines, `name=status`, written in the worktree.
    #[cfg(unix)]
    fn probe(&self) -> Vec<String> {
        fs::read_to_string(self.worktree.join("probe.txt"))
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

fn brief() -> Brief {
    Brief {
        format: Format,
        role: Role::Build,
        project: "bench".into(),
        ticket: TicketBrief {
            id: TicketId::new("T-1").unwrap(),
            title: "A ticket".into(),
            url: None,
            labels: Vec::new(),
            author: Author {
                name: "maintainer".into(),
                relation: Relation::Decider,
            },
            description: "Do it.".into(),
        },
        decider: "maintainer".into(),
        thread: Vec::new(),
        resolve: Vec::new(),
        checkpoint: None,
        zones: Vec::new(),
        resources: Vec::new(),
        rules: Vec::new(),
        permissions: Permissions {
            level: PermissionLevel::WriteWorktree,
            network: false,
            browser: false,
        },
        gate: Vec::new(),
        always_human: Vec::new(),
        gate_failure: None,
        result_refusal: None,
        result_path: RelativePath::new("result.json").unwrap(),
    }
}

/// The harness reads the planted files itself, then through a child of its
/// own, and leaves a grandchild that would write into the worktree two
/// seconds later, its output let go so the run does not wait for it.
#[cfg(unix)]
const PROBE: &str = r#"
cat "$HOME/.config/gh/hosts.yml" > /dev/null 2>&1; echo "token=$?" > probe.txt
cat "$HOME/other-project/.env" > /dev/null 2>&1; echo "env=$?" >> probe.txt
sh -c 'cat "$HOME/.config/gh/hosts.yml" > /dev/null 2>&1; echo "child=$?" >> probe.txt'
(sleep 2; echo late > late.txt) > /dev/null 2>&1 &
"#;

#[cfg(unix)]
fn sandbox_or_skip(agent: &AgentEnv) -> bool {
    match agent.sandbox_ready() {
        Ok(()) => true,
        Err(error) => {
            assert!(
                std::env::var_os("OWLSHIFT_REQUIRE_CONFINEMENT").is_none(),
                "confinement is required here, but: {error}"
            );
            eprintln!("skipped: agent runs cannot be confined here: {error}");
            false
        }
    }
}

/// The run reaches no planted secret, from the harness or its child, while
/// the bare control reads both; and the grandchild the run left behind is
/// stopped with it, so its late write never lands.
#[cfg(unix)]
#[test]
fn a_confined_run_and_its_descendants_reach_no_planted_secret() {
    let bench = Bench::new();
    let agent = bench.agent();
    if !sandbox_or_skip(&agent) {
        return;
    }

    bench
        .run(agent.clone().without_confinement(), PROBE)
        .unwrap();
    assert_eq!(bench.probe(), ["token=0", "env=0", "child=0"]);
    std::thread::sleep(Duration::from_secs(3));
    assert!(
        !bench.worktree.join("late.txt").exists(),
        "the bare grandchild outlived the run"
    );

    let report = bench.run(agent, PROBE).unwrap();
    let probe = bench.probe();
    for line in &probe {
        assert!(
            !line.ends_with("=0"),
            "a confined read succeeded: {probe:?}"
        );
    }
    assert_eq!(probe.len(), 3, "{probe:?}");
    std::thread::sleep(Duration::from_secs(3));
    assert!(
        !bench.worktree.join("late.txt").exists(),
        "a confined grandchild outlived the run"
    );
    // The run wrote only in its worktree, and left the rest as it was.
    assert!(
        !matches!(
            report.outcome,
            owlshift_runner::executor::Outcome::Quarantined(_)
        ),
        "{:?}",
        report.outcome
    );
}

/// The token of OWL-94's bench test, made up.
#[cfg(unix)]
const TOKEN: &str = "sk-ant-oat01-owl94-FAKE-token";

/// A logged-in harness that reads its token from the descriptor it is named,
/// as Claude Code does (OWL-96), and sees a configuration folder of the
/// run's own, empty. Its probe says, never showing the token: whether it
/// read it; whether `CLAUDE_CODE_OAUTH_TOKEN` is set and whether its raw
/// environment holds the token; whether a process it starts afterwards reads
/// anything from the descriptor; and, where there is a `/proc`, how many
/// processes inside the sandbox have the token in their exec block, with
/// one started with it on purpose as the control, so the count is 1. Then
/// it prints its whole environment and the token, and writes a result
/// holding the token when `LEAK` is set.
#[cfg(unix)]
const LOGGED_IN: &str = r#"
fd="${CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR:-none}"
tok=
[ "$fd" != none ] && read -r tok < "/dev/fd/$fd"
case "$tok" in sk-ant-oat01-owl94-*) echo token=seen;; *) echo token=missing;; esac > probe.txt
echo "var=${CLAUDE_CODE_OAUTH_TOKEN+set}" >> probe.txt
case "$(env)" in *"$tok"*) echo env=holds;; *) echo env=absent;; esac >> probe.txt
rest=$(sh -c 'cat "/dev/fd/$0"' "$fd" 2>/dev/null)
[ -z "$rest" ] && echo after=empty >> probe.txt || echo after=more >> probe.txt
if [ -d /proc/self ]; then
  T="$tok" sleep 30 &
  control=$!
  tries=0
  while :; do
    n=0
    for f in /proc/[0-9]*/environ; do
      c=$(tr '\0' '\n' < "$f" 2>/dev/null) || continue
      case "$c" in *"$tok"*) n=$((n+1));; esac
    done
    tries=$((tries+1))
    { [ "$n" -ge 1 ] || [ "$tries" -ge 50 ]; } && break
    sleep 0.1
  done
  kill "$control"
  echo "environ=$n" >> probe.txt
else
  echo environ=skipped >> probe.txt
fi
case "${CLAUDE_CONFIG_DIR:-}" in "$TMPDIR"/*) echo config=run;; *) echo config=other;; esac >> probe.txt
echo "config_files=$(ls -A "$CLAUDE_CONFIG_DIR" | wc -l | tr -d ' ')" >> probe.txt
echo "config_path=$CLAUDE_CONFIG_DIR" >> probe.txt
echo written > "$CLAUDE_CONFIG_DIR/session"
env
printf 'token: %s\n' "$tok" >&2
mkdir -p .owlshift/run
printf '{"format":6,"status":"blocked","summary":"%s"}' "${LEAK:+$tok}" > .owlshift/run/result.json
"#;

/// OWL-94's acceptance through the executor: a confined harness logs in
/// with its token, in an empty configuration folder of the run's own that
/// is gone with the run. The token it prints reaches no log file, no file
/// of the run directory and not the report; a result holding it fails the
/// run with a finding that does not show it. OWL-96's: the token reaches the
/// harness through the sandbox (Seatbelt, or bwrap on the Linux CI job) on a
/// descriptor, in no environment and no exec block, and what the harness
/// starts after reading it gets nothing from the descriptor.
#[cfg(unix)]
#[test]
fn a_confined_harness_logs_in_with_its_token_and_no_output_keeps_it() {
    let bench = Bench::new();
    let agent = bench.agent();
    if !sandbox_or_skip(&agent) {
        return;
    }
    let token = Some(Secret::new(TOKEN));
    let report = bench
        .run_logged_in(agent.clone(), LOGGED_IN, token.clone())
        .unwrap();
    let probe = bench.probe();
    let environ = if cfg!(target_os = "linux") {
        "environ=1"
    } else {
        "environ=skipped"
    };
    assert_eq!(
        probe[..7],
        [
            "token=seen",
            "var=",
            "env=absent",
            "after=empty",
            environ,
            "config=run",
            "config_files=0"
        ],
        "{probe:?}"
    );
    let config = probe[7].strip_prefix("config_path=").unwrap();
    assert!(!PathBuf::from(config).exists(), "{config} outlived the run");
    assert!(
        matches!(
            report.outcome,
            owlshift_runner::executor::Outcome::Finished { .. }
        ),
        "{:?}",
        report.outcome
    );
    let stdout = fs::read_to_string(&report.stdout_log).unwrap();
    assert!(
        stdout.contains("CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR="),
        "{stdout}"
    );
    assert!(
        !stdout
            .lines()
            .any(|line| line.starts_with("CLAUDE_CODE_OAUTH_TOKEN=")),
        "{stdout}"
    );
    assert_eq!(
        fs::read_to_string(&report.stderr_log).unwrap(),
        "token: <redacted>\n"
    );
    let mut files = vec![bench.run_dir()];
    while let Some(path) = files.pop() {
        if path.is_dir() {
            files.extend(fs::read_dir(&path).unwrap().map(|e| e.unwrap().path()));
        } else {
            let bytes = fs::read(&path).unwrap();
            assert!(
                !String::from_utf8_lossy(&bytes).contains(TOKEN),
                "{} holds the token",
                path.display()
            );
        }
    }
    assert!(!format!("{report:?}").contains(TOKEN));

    let leaked = bench
        .run_logged_in(agent, &format!("LEAK=1\n{LOGGED_IN}"), token)
        .unwrap();
    let owlshift_runner::executor::Outcome::Failed(failure) = &leaked.outcome else {
        panic!("{:?}", leaked.outcome);
    };
    let text = failure.to_string();
    assert!(
        text.starts_with(".owlshift/run/result.json holds the harness's login token"),
        "{text}"
    );
    assert!(!format!("{leaked:?}").contains(TOKEN));
}

/// A harness that keeps its temporary files as Claude Code does: in a
/// `claude-<uid>` folder under `CLAUDE_CODE_TMPDIR`, which must be set (the
/// `:?` fails the script otherwise). It says whether it wrote there, whether
/// that folder lies in the run's `TMPDIR`, and what it gets from the user's
/// own `/tmp/claude-<uid>`.
#[cfg(unix)]
const TEMP_FILES: &str = r#"
uid=$(id -u) || exit 3
d="${CLAUDE_CODE_TMPDIR:?}/claude-$uid"
mkdir -p "$d" && echo x > "$d/probe" && echo own=ok > probe.txt || echo own=failed > probe.txt
case "$CLAUDE_CODE_TMPDIR" in "$TMPDIR"/*) echo tmpdir=run;; *) echo tmpdir=other;; esac >> probe.txt
out=$(ls "/tmp/claude-$uid" 2>&1)
case "$out" in
  *"not permitted"*) echo operator=denied;;
  *"No such file"*) echo operator=absent;;
  *) echo operator=other;;
esac >> probe.txt
echo "path=$d" >> probe.txt
mkdir -p .owlshift/run
printf '{"format":6,"status":"blocked","summary":"probed"}' > .owlshift/run/result.json
"#;

/// The user's own `/tmp/claude-<uid>`, made for the test when it is absent
/// and removed after it only then, and only if still empty.
#[cfg(target_os = "macos")]
struct OperatorFolder(Option<PathBuf>);

#[cfg(target_os = "macos")]
impl OperatorFolder {
    fn ensure() -> Self {
        let uid = Command::new("id").arg("-u").output().unwrap().stdout;
        let path = PathBuf::from(format!(
            "/tmp/claude-{}",
            String::from_utf8(uid).unwrap().trim()
        ));
        match fs::create_dir(&path) {
            Ok(()) => Self(Some(path)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Self(None),
            Err(e) => panic!("{}: {e}", path.display()),
        }
    }
}

#[cfg(target_os = "macos")]
impl Drop for OperatorFolder {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = fs::remove_dir(path);
        }
    }
}

/// OWL-100's acceptance through the executor: a confined harness that names
/// its temporary folder runs while the user's own `/tmp/claude-<uid>` exists,
/// which made a confined `claude -p` exit at start-up on macOS. It writes in
/// a folder of the run's own temporary folder instead, and cannot reach the
/// user's: Seatbelt denies it on macOS, where the test makes sure it exists;
/// bwrap's private `/tmp` does not hold it on Linux. On macOS the harness's
/// folder is also shown gone after the run; under bwrap it never reaches the
/// host.
#[cfg(unix)]
#[test]
fn a_confined_harness_keeps_its_temporary_files_clear_of_the_operators_folder() {
    let bench = Bench::new();
    let agent = bench.agent();
    if !sandbox_or_skip(&agent) {
        return;
    }
    #[cfg(target_os = "macos")]
    let _operator = OperatorFolder::ensure();
    let harness = Script {
        line: TEMP_FILES.to_owned(),
        login: None,
        temp_variable: Some(claude::TMPDIR_ENV),
    };
    let report = bench.run_harness(agent, &harness).unwrap();
    assert!(
        matches!(
            report.outcome,
            owlshift_runner::executor::Outcome::Finished { .. }
        ),
        "{:?}",
        report.outcome
    );
    let probe = bench.probe();
    let operator = if cfg!(target_os = "macos") {
        "operator=denied"
    } else {
        "operator=absent"
    };
    assert_eq!(probe[..3], ["own=ok", "tmpdir=run", operator], "{probe:?}");
    let path = probe[3].strip_prefix("path=").unwrap();
    assert!(!PathBuf::from(path).exists(), "{path} outlived the run");
}

/// Native Windows has no sandbox: the run is refused before anything is
/// written, and the error says to use WSL2.
#[cfg(windows)]
#[test]
fn a_run_on_native_windows_is_refused() {
    let bench = Bench::new();
    let error = bench.run(bench.agent(), "echo hi").unwrap_err();
    assert!(
        matches!(&error, ExecutorError::Spawn(e) if e.to_string().contains("WSL2")),
        "{error:?}"
    );
    assert!(!bench.worktree.exists(), "the worktree was created");
}
