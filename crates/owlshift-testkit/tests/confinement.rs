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
    Executor, ExecutorError, Failure, Git, Harness, HarnessEnd, HarnessError, HarnessLogin,
    HarnessRun, HarnessStatus, RunLog, RunReport, RunSpec, SandboxNeeds,
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
    /// Asks for the read-only profile (OWL-298).
    read_only: bool,
}

impl Harness for Script {
    /// The script gets the result file as `$1` and the brief's file as `$2`.
    fn command(&self, run: &HarnessRun<'_>) -> Result<Command, HarnessError> {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(&self.line)
            .arg("sh")
            .arg(run.result_file)
            .arg(run.brief_file);
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
            read_only: self.read_only,
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
            read_only: false,
        };
        self.run_harness(agent, &harness)
    }

    fn run_harness(&self, agent: AgentEnv, harness: &Script) -> Result<RunReport, ExecutorError> {
        self.run_brief(agent, harness, &brief())
    }

    fn run_brief(
        &self,
        agent: AgentEnv,
        harness: &dyn Harness,
        brief: &Brief,
    ) -> Result<RunReport, ExecutorError> {
        self.run_set_up(agent, harness, brief, &[])
    }

    /// A run with the project's `setup` (OWL-330).
    fn run_set_up(
        &self,
        agent: AgentEnv,
        harness: &dyn Harness,
        brief: &Brief,
        setup: &[String],
    ) -> Result<RunReport, ExecutorError> {
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
            brief,
            setup,
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
        delegation: None,
        asked_by: None,
        gate_failure: None,
        result_refusal: None,
        decisions_refused: false,
        refused_choices: Vec::new(),
        check_failure: None,
        correction: None,
        rebase: None,
        result_path: RelativePath::new("result.json").unwrap(),
        plan: None,
        approval: false,
        findings: None,
        review: None,
        pull_request_reviews: Vec::new(),
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

/// What the cache probe of OWL-330 reads and writes, its lines in the run's
/// own files, out of git: the harness's view of a declared cache and of a
/// home folder beside it.
#[cfg(unix)]
const CACHE_PROBE: &str = r#"
probe=.owlshift/run/probe.txt
cat "$HOME/Library/pnpm/store/pkg" > .owlshift/run/cache.txt 2>/dev/null; echo "read=$?" > $probe
touch "$HOME/Library/pnpm/store/planted" 2>/dev/null; echo "write=$?" >> $probe
cat "$HOME/Library/other/file" > /dev/null 2>&1; echo "sibling=$?" >> $probe
"#;

/// The gate's view of the same folders: it passes only when the cache reads
/// and neither the write nor the sibling's read succeeds.
#[cfg(unix)]
const CACHE_GATE: [&str; 3] = [
    "grep -q CACHED_owl330 \"$HOME/Library/pnpm/store/pkg\"",
    "! touch \"$HOME/Library/pnpm/store/planted-by-gate\" 2>/dev/null",
    "! cat \"$HOME/Library/other/file\" > /dev/null 2>&1",
];

/// OWL-330: a cache the project declares and the operator allows is read by
/// the harness and by the gate, never written; a folder of the home beside
/// it stays closed. Without the cache the harness reads nothing there.
#[cfg(unix)]
#[test]
fn a_declared_cache_is_read_and_never_written() {
    let bench = Bench::new();
    let home = bench.tmp.path().join("home");
    let store = home.join("Library/pnpm/store");
    fs::create_dir_all(&store).unwrap();
    fs::write(store.join("pkg"), "CACHED_owl330\n").unwrap();
    fs::create_dir_all(home.join("Library/other")).unwrap();
    fs::write(home.join("Library/other/file"), "FAKE_owl330\n").unwrap();
    let agent = bench.agent().with_caches(&["Library/pnpm"]);
    if !sandbox_or_skip(&agent) {
        return;
    }
    let lines = || {
        fs::read_to_string(bench.worktree.join(".owlshift/run/probe.txt"))
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };

    bench.run(bench.agent(), CACHE_PROBE).unwrap();
    assert_eq!(lines()[0], "read=1", "without the cache: {:?}", lines());

    let done = r#"{"format":12,"status":"done","summary":"s","pr":{"branch":"owlshift/T-1","title":"t","body":"b"}}"#;
    let script = format!("{CACHE_PROBE}\nprintf '%s' '{done}' > \"$1\"\n");
    let mut brief = brief();
    brief.gate = CACHE_GATE.iter().map(|line| (*line).to_owned()).collect();
    let harness = Script {
        line: script,
        login: None,
        temp_variable: None,
        read_only: false,
    };
    let report = bench.run_brief(agent, &harness, &brief).unwrap();
    let probe = lines();
    assert_eq!(probe[0], "read=0", "{probe:?}");
    assert!(
        !probe[1].ends_with("=0") && !probe[2].ends_with("=0"),
        "{probe:?}"
    );
    assert_eq!(
        fs::read_to_string(bench.worktree.join(".owlshift/run/cache.txt")).unwrap(),
        "CACHED_owl330\n"
    );
    let gate = report.gate.as_ref().expect("the gate ran after the done");
    assert!(gate.passed(), "{gate:?}");
    assert!(!store.join("planted").exists() && !store.join("planted-by-gate").exists());
}

/// OWL-330: a setup command that writes into a declared cache is refused by
/// the sandbox, so the setup of a Build on a pristine worktree fails the
/// run before its harness, naming that command.
#[cfg(unix)]
#[test]
fn a_setup_command_cannot_write_into_a_declared_cache() {
    let bench = Bench::new();
    let store = bench.tmp.path().join("home/Library/pnpm/store");
    fs::create_dir_all(&store).unwrap();
    let agent = bench.agent().with_caches(&["Library/pnpm"]);
    if !sandbox_or_skip(&agent) {
        return;
    }
    let write = "touch \"$HOME/Library/pnpm/store/x\"".to_owned();
    let harness = Script {
        line: "echo ran > .owlshift/run/harness-ran".to_owned(),
        login: None,
        temp_variable: None,
        read_only: false,
    };
    let report = bench
        .run_set_up(
            agent,
            &harness,
            &brief(),
            &["true".to_owned(), write.clone()],
        )
        .unwrap();
    let owlshift_runner::executor::Outcome::Failed(Failure::Setup(failure)) = &report.outcome
    else {
        panic!("{:?}", report.outcome);
    };
    assert_eq!(failure.command.as_deref(), Some(write.as_str()));
    assert!(
        failure.reason.starts_with("setup: exit status"),
        "{failure:?}"
    );
    assert!(!store.join("x").exists());
    assert!(!bench.worktree.join(".owlshift/run/harness-ran").exists());
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
printf '{"format":13,"status":"blocked","summary":"%s"}' "${LEAK:+$tok}" > .owlshift/run/result.json
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
printf '{"format":13,"status":"blocked","summary":"probed"}' > .owlshift/run/result.json
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
        read_only: false,
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

/// A review brief, read-only, as a Codex critic gets (OWL-298).
#[cfg(unix)]
fn review_brief() -> Brief {
    let mut brief = brief();
    brief.role = Role::DesignReview;
    brief.permissions.level = PermissionLevel::ReadOnly;
    brief
}

/// A read-only harness for `line`.
fn read_only(line: &str) -> Script {
    Script {
        line: line.to_owned(),
        login: None,
        temp_variable: None,
        read_only: true,
    }
}

/// A review run's result, valid against [`review_brief`].
#[cfg(unix)]
const REVIEW_RESULT: &str = r#"{"format":13,"status":"done","summary":"no finding"}"#;

/// OWL-298: the read-only profile is refused for a brief that lets its role
/// write the worktree, before anything is written, on every platform.
#[test]
fn the_read_only_profile_is_refused_to_a_brief_that_writes() {
    let bench = Bench::new();
    let agent = bench.agent().without_confinement();
    let error = bench.run_harness(agent, &read_only("exit 0")).unwrap_err();
    assert!(
        matches!(&error, ExecutorError::Spec(reason) if reason.contains("read-only")),
        "{error:?}"
    );
    assert!(!bench.worktree.exists(), "the worktree was created");
}

/// OWL-298: a read-only run's result is read from its own files folder
/// alone, never from a result an earlier run left in the worktree, and
/// nothing of the run is written in the worktree. Unconfined: what is
/// checked here is where the executor puts and reads the run's files.
#[cfg(unix)]
#[test]
fn a_read_only_run_reads_its_result_from_its_own_files_alone() {
    use owlshift_runner::executor::{Failure, Outcome};
    let bench = Bench::new();
    let agent = bench.agent().without_confinement();
    // An earlier run, with the write profile, leaves a valid result.
    let earlier = format!(
        "mkdir -p .owlshift/run && printf '%s' '{REVIEW_RESULT}' > .owlshift/run/result.json"
    );
    let harness = Script {
        read_only: false,
        ..read_only(&earlier)
    };
    let report = bench
        .run_brief(agent.clone(), &harness, &review_brief())
        .unwrap();
    assert!(
        matches!(report.outcome, Outcome::Finished { .. }),
        "{:?}",
        report.outcome
    );

    let report = bench
        .run_brief(agent.clone(), &read_only("exit 0"), &review_brief())
        .unwrap();
    assert!(
        matches!(report.outcome, Outcome::Failed(Failure::NoResult)),
        "{:?}",
        report.outcome
    );

    fs::remove_dir_all(bench.worktree.join(".owlshift")).unwrap();
    let writes = format!(
        "test -r \"$2\" || exit 3; case \"$1\" in \"$PWD\"/*) exit 4;; esac; \
         printf '%s' '{REVIEW_RESULT}' > \"$1\""
    );
    let report = bench
        .run_brief(agent, &read_only(&writes), &review_brief())
        .unwrap();
    assert!(
        matches!(report.outcome, Outcome::Finished { .. }),
        "{:?}",
        report.outcome
    );
    assert!(
        !bench.worktree.join(".owlshift").exists(),
        "a read-only run wrote its files in the worktree"
    );
}

/// What a read-only run tries, one `name=status` line each on its standard
/// output: read the worktree, change a file there, create one, write in the
/// repository's git folder, ask `git status`, read its brief, write in its
/// own files folder; then what the resolver's Bash does to check a fact
/// (OWL-344): `git log`, `git blame` and `grep`, and a write outside the
/// worktree and the git folder, beside the worktree; then it writes its
/// result.
#[cfg(unix)]
const READ_ONLY_PROBE: &str = r#"
cat README.md > /dev/null 2>&1; echo "read=$?"
(echo changed >> README.md) 2> /dev/null; echo "edit=$?"
(echo new > new.txt) 2> /dev/null; echo "create=$?"
common=$(git rev-parse --path-format=absolute --git-common-dir)
(echo planted > "$common/objects/owl298") 2> /dev/null; echo "git=$?"
git --no-optional-locks status --porcelain > /dev/null 2>&1; echo "status=$?"
test -r "$2"; echo "brief=$?"
(echo own > "$(dirname "$1")/own") 2> /dev/null; echo "own=$?"
git log --oneline > /dev/null 2>&1; echo "log=$?"
git blame README.md > /dev/null 2>&1; echo "blame=$?"
grep -rn hello README.md > /dev/null 2>&1; echo "grep=$?"
(echo outside > "$(dirname "$PWD")/outside.txt") 2> /dev/null; echo "outside=$?"
"#;

/// OWL-298's acceptance through the executor, with the real sandbox
/// (Seatbelt, or bwrap on the Linux CI job): under the read-only profile the
/// run reads the worktree and git answers, but every write to the worktree
/// and to the repository's git folder is refused; it reads its brief and
/// writes its own files folder, where its result is read from.
///
/// It also stands for the confinement of the resolver's run with Bash
/// (OWL-344), whose harness asks for the same profile: `git log`, `git
/// blame` and `grep` succeed, and a write beside the worktree leaves
/// nothing there. It checks the profile, not the resolver: a shell script
/// stands in for Claude Code, whose run under the profile is the live check
/// recorded in `docs/design/build-plan.md`, and the test is skipped where
/// there is no sandbox. On macOS the outside write is refused; under bwrap,
/// where the bench's folder can lie in the sandbox's private `/tmp`, it may
/// land there and vanish with the run.
#[cfg(unix)]
#[test]
fn a_read_only_run_reads_the_worktree_and_writes_only_its_own_files() {
    use owlshift_runner::executor::Outcome;
    let bench = Bench::new();
    let agent = bench.agent();
    if !sandbox_or_skip(&agent) {
        return;
    }
    let line = format!("{READ_ONLY_PROBE}printf '%s' '{REVIEW_RESULT}' > \"$1\"\n");
    let report = bench
        .run_brief(agent, &read_only(&line), &review_brief())
        .unwrap();
    assert!(
        matches!(report.outcome, Outcome::Finished { .. }),
        "{:?}",
        report.outcome
    );
    let stdout = fs::read_to_string(&report.stdout_log).unwrap();
    let probe: Vec<&str> = stdout.lines().collect();
    let refused = |name: &str| {
        probe.iter().any(|line| {
            line.strip_prefix(name)
                .is_some_and(|rest| rest.starts_with('=') && rest != "=0")
        })
    };
    for name in ["read", "status", "brief", "own", "log", "blame", "grep"] {
        assert!(
            probe.contains(&format!("{name}=0").as_str()),
            "{name}: {probe:?}"
        );
    }
    for name in ["edit", "create", "git"] {
        assert!(refused(name), "{name} was not refused: {probe:?}");
    }
    if cfg!(target_os = "macos") {
        assert!(refused("outside"), "outside was not refused: {probe:?}");
    }
    assert!(
        !bench.tmp.path().join("outside.txt").exists(),
        "a write beside the worktree reached it"
    );
    assert_eq!(
        fs::read_to_string(bench.worktree.join("README.md")).unwrap(),
        "hello\n"
    );
    assert!(!bench.worktree.join("new.txt").exists());
    let common = fs::read_to_string(bench.worktree.join(".git")).unwrap();
    let common = PathBuf::from(common.trim().strip_prefix("gitdir: ").unwrap());
    let objects = common.parent().unwrap().parent().unwrap().join("objects");
    assert!(
        !objects.join("owl298").exists(),
        "the git folder was written"
    );
}

/// What a run tries on another ticket's worktree entry in the common git
/// folder, one `name=status` line each (OWL-339): append to its `index`,
/// overwrite its `HEAD`, add a file, remove one, and make an entry of its own.
#[cfg(unix)]
const WRITES_TO_OTHER_ENTRY: &str = r##"
common=$(git rev-parse --path-format=absolute --git-common-dir)
other="$common/worktrees/other"
(echo x >> "$other/index") 2> /dev/null; echo "append=$?"
(echo x > "$other/HEAD") 2> /dev/null; echo "head=$?"
(echo x > "$other/planted") 2> /dev/null; echo "new=$?"
(rm "$other/gitdir") 2> /dev/null; echo "rm=$?"
(mkdir "$common/worktrees/evil") 2> /dev/null; echo "mkdir=$?"
"##;

/// The run's own work in its own worktree and entry: two commits and an
/// interactive rebase that drops the second, which writes and removes
/// `rebase-merge/` in the entry; then `git worktree prune` and `git gc`,
/// with their statuses and output, and the worktree's state.
#[cfg(unix)]
const OWN_WORK: &str = r##"
start=$(git rev-parse HEAD)
who="-c user.name=run -c user.email=run@example.invalid"
echo a > a.txt; git add a.txt; echo "add=$?"
git $who commit -q -m first; echo "commit1=$?"
echo b > b.txt; git add b.txt; git $who commit -q -m second; echo "commit2=$?"
GIT_SEQUENCE_EDITOR="sed -i.bak 's/^pick \([0-9a-f]*\) \(# \)\{0,1\}second\$/drop \1 second/'" \
  git $who rebase -q -i "$start" > /dev/null 2>&1; echo "rebase=$?"
echo "log=$(git log --format=%s "$start"..HEAD | tr '\n' ',')"
test -e a.txt; echo "a=$?"
test -e b.txt; echo "b=$?"
test -e "$(git rev-parse --git-dir)/rebase-merge"; echo "rebase_dir=$?"
out=$(git worktree prune --verbose 2>&1); echo "prune=$?"
echo "prune.out=$(echo "$out" | tr '\n' '|')"
out=$(git gc --quiet 2>&1); echo "gc=$?"
echo "gc.err=$(echo "$out" | tr '\n' '|')"
git status --porcelain > /dev/null 2>&1; echo "status=$?"
echo "dirty=$(git status --porcelain | wc -l | tr -d ' ')"
"##;

/// The gate's version of the same: the writes, then `add`, `commit` and a
/// `reset` back to where the gate started, so the worktree ends clean on the
/// same commit. It exits 0 whatever the writes answered.
#[cfg(unix)]
const GATE_COMMAND: &str = r##"
common=$(git rev-parse --path-format=absolute --git-common-dir)
other="$common/worktrees/other"
(echo x >> "$other/index") 2> /dev/null; echo "append=$?"
(echo x > "$other/HEAD") 2> /dev/null; echo "head=$?"
(echo x > "$other/planted") 2> /dev/null; echo "new=$?"
(rm "$other/gitdir") 2> /dev/null; echo "rm=$?"
(mkdir "$common/worktrees/evil") 2> /dev/null; echo "mkdir=$?"
echo a > a.txt; git add a.txt; echo "add=$?"
git -c user.name=gate -c user.email=gate@example.invalid commit -q -m gate; echo "commit=$?"
git reset -q --hard HEAD~1; echo "reset=$?"
"##;

/// A second worktree, `other`, and a stale one, `stale`, in the common git
/// folder: the stale one's directory is gone and its `gitdir` is dated 2020,
/// so that `git worktree prune` and `git gc` find it expired.
#[cfg(unix)]
fn plant_entries(bench: &Bench) {
    for name in ["other", "stale"] {
        bench
            .env
            .run(
                &bench.main,
                &[
                    "worktree",
                    "add",
                    "--quiet",
                    "-b",
                    name,
                    &format!("../{name}"),
                    "origin/main",
                ],
            )
            .unwrap();
    }
    fs::remove_dir_all(bench.tmp.path().join("stale")).unwrap();
    let gitdir = bench.main.join(".git/worktrees/stale/gitdir");
    let status = Command::new("touch")
        .args(["-t", "202001010000"])
        .arg(&gitdir)
        .status()
        .unwrap();
    assert!(status.success());
}

/// Every file under `worktrees/other` and `worktrees/stale`, with its bytes.
#[cfg(unix)]
fn entry_files(bench: &Bench) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
    fn walk(dir: &std::path::Path, out: &mut std::collections::BTreeMap<PathBuf, Vec<u8>>) {
        let Ok(children) = fs::read_dir(dir) else {
            return;
        };
        for child in children {
            let path = child.unwrap().path();
            if path.is_dir() {
                walk(&path, out);
            } else {
                out.insert(path.clone(), fs::read(&path).unwrap());
            }
        }
    }
    let mut files = std::collections::BTreeMap::new();
    for name in ["other", "stale"] {
        walk(&bench.main.join(".git/worktrees").join(name), &mut files);
    }
    files
}

/// `name=value` lines as a map.
#[cfg(unix)]
fn named(output: &str) -> std::collections::BTreeMap<String, String> {
    output
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect()
}

#[cfg(unix)]
fn show(what: &str, lines: &std::collections::BTreeMap<String, String>) {
    // Printed so that CI's log of a passing run carries them (OWL-339).
    eprintln!(
        "{what}: prune={} gc={}\n{what}: prune.out={}\n{what}: gc.err={}",
        lines.get("prune").map_or("-", String::as_str),
        lines.get("gc").map_or("-", String::as_str),
        lines.get("prune.out").map_or("-", String::as_str),
        lines.get("gc.err").map_or("-", String::as_str),
    );
}

/// A confined run's writes to another worktree's entry are refused, while
/// its own commits, `add` and rebase in its own entry work. `git worktree
/// prune` and `git gc` leave another ticket's expired entry intact. The bare
/// control makes the same writes, and the isolation check marks the breach.
#[cfg(unix)]
#[test]
fn a_confined_run_cannot_write_another_worktrees_entry_but_its_own() {
    use owlshift_runner::executor::{Outcome, Violation};
    let script = format!("{WRITES_TO_OTHER_ENTRY}{OWN_WORK}");

    // The bare control, which runs wherever the unit tests do.
    let bench = Bench::new();
    plant_entries(&bench);
    let bare = bench
        .run(bench.agent().without_confinement(), &script)
        .unwrap();
    let lines = named(&fs::read_to_string(&bare.stdout_log).unwrap());
    show("bare run", &lines);
    for name in [
        "append", "head", "new", "rm", "mkdir", "add", "commit1", "commit2", "rebase", "prune",
        "gc", "a", "status",
    ] {
        assert_eq!(
            lines.get(name).map(String::as_str),
            Some("0"),
            "{name}: {lines:?}"
        );
    }
    assert_eq!(lines["log"], "first,");
    assert_eq!(lines["b"], "1");
    assert_eq!(lines["rebase_dir"], "1");
    assert_eq!(lines["dirty"], "0");
    assert!(lines["prune.out"].contains("worktrees/stale"), "{lines:?}");
    assert!(
        !bench.main.join(".git/worktrees/stale").exists(),
        "the expired entry was not pruned: the fixture is not eligible"
    );
    let Outcome::Quarantined(violations) = &bare.outcome else {
        panic!("{:?}", bare.outcome);
    };
    assert!(
        violations.iter().any(|violation| matches!(
            violation,
            Violation::WorktreeEntries(paths)
                if paths.contains(&"worktrees/other/index".to_owned())
        )),
        "{violations:?}"
    );

    let bench = Bench::new();
    let agent = bench.agent();
    if !sandbox_or_skip(&agent) {
        return;
    }
    plant_entries(&bench);
    let before = entry_files(&bench);
    assert!(!before.is_empty());
    let report = bench.run(agent, &script).unwrap();
    let lines = named(&fs::read_to_string(&report.stdout_log).unwrap());
    show("confined run", &lines);
    for name in ["append", "head", "new", "rm", "mkdir"] {
        assert_ne!(
            lines.get(name).map(String::as_str),
            Some("0"),
            "{name}: {lines:?}"
        );
        assert!(lines.contains_key(name), "{name}: {lines:?}");
    }
    assert_eq!(entry_files(&bench), before, "an entry was changed");
    assert!(!bench.main.join(".git/worktrees/evil").exists());
    for name in ["add", "commit1", "commit2", "rebase", "a", "status"] {
        assert_eq!(
            lines.get(name).map(String::as_str),
            Some("0"),
            "{name}: {lines:?}"
        );
    }
    assert_eq!(lines["log"], "first,");
    assert_eq!(lines["b"], "1");
    assert_eq!(lines["rebase_dir"], "1");
    assert_eq!(lines["dirty"], "0");
    // Statuses step 1(d) of the plan recorded under a read-only `worktrees`
    // (build plan, "OWL-339 live checks"): both commands end 0 and say what
    // they could not do. The command found the entry it could not remove.
    assert_eq!(
        lines.get("prune").map(String::as_str),
        Some("0"),
        "{lines:?}"
    );
    assert_eq!(lines.get("gc").map(String::as_str), Some("0"), "{lines:?}");
    assert!(lines["prune.out"].contains("worktrees/stale"), "{lines:?}");
    assert!(
        !matches!(report.outcome, Outcome::Quarantined(_)),
        "{:?}",
        report.outcome
    );
}

impl Bench {
    /// Runs the project's gate alone with one command, in a fresh worktree
    /// of its own, as a Build's gate runs: the same `project_gate` and
    /// `gate_violations`.
    #[cfg(unix)]
    fn run_gate(&self, agent: AgentEnv, command: &str) -> owlshift_runner::executor::GateRun {
        let runner = self.env.clone();
        let executor = Executor {
            git: Git::with_setup("git", move |command| runner.apply(command)),
            agent,
            forge_hosts: Vec::new(),
            timeout: Duration::from_secs(60),
            gate_timeout: Duration::from_secs(60),
        };
        let worktree = self.tmp.path().join("gate-worktree");
        let run_dir = self.tmp.path().join("gate-run");
        let commands = [command.to_owned()];
        executor
            .run_gate(&owlshift_runner::executor::GateSpec {
                main: &self.main,
                worktree: &worktree,
                branch: "owlshift/gate",
                base: "origin/main",
                run_dir: &run_dir,
                commands: &commands,
            })
            .unwrap()
    }
}

/// The gate runs confined as the run was: its writes to another entry are
/// refused, its `add`, `commit` and `reset` in its own worktree work, and
/// isolation holds after it. A bare gate that makes the same writes passes
/// its own commands, and the check after it marks the breach.
#[cfg(unix)]
#[test]
fn a_confined_gate_cannot_write_another_worktrees_entry_but_its_own() {
    use owlshift_runner::executor::Violation;

    let bench = Bench::new();
    plant_entries(&bench);
    let bare = bench.run_gate(bench.agent().without_confinement(), GATE_COMMAND);
    let lines = named(&bare.report.steps[0].output);
    for name in [
        "append", "head", "new", "rm", "mkdir", "add", "commit", "reset",
    ] {
        assert_eq!(
            lines.get(name).map(String::as_str),
            Some("0"),
            "{name}: {lines:?}"
        );
    }
    assert!(bare.report.passed(), "{:?}", bare.report);
    assert!(
        bare.violations.iter().any(|violation| matches!(
            violation,
            Violation::WorktreeEntries(paths)
                if paths.contains(&"worktrees/other/index".to_owned())
        )),
        "{:?}",
        bare.violations
    );
    assert!(!bare.passed());

    let bench = Bench::new();
    let agent = bench.agent();
    if !sandbox_or_skip(&agent) {
        return;
    }
    plant_entries(&bench);
    let before = entry_files(&bench);
    let confined = bench.run_gate(agent, GATE_COMMAND);
    let lines = named(&confined.report.steps[0].output);
    for name in ["append", "head", "new", "rm", "mkdir"] {
        assert!(lines.contains_key(name), "{name}: {lines:?}");
        assert_ne!(lines[name], "0", "{name}: {lines:?}");
    }
    for name in ["add", "commit", "reset"] {
        assert_eq!(
            lines.get(name).map(String::as_str),
            Some("0"),
            "{name}: {lines:?}"
        );
    }
    assert!(confined.report.passed(), "{:?}", confined.report);
    assert_eq!(confined.violations, []);
    assert_eq!(entry_files(&bench), before, "an entry was changed");
    assert!(!bench.main.join(".git/worktrees/evil").exists());
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

/// A Codex login of `account`, shaped as codex-cli 0.154.0's `auth.json`
/// (key names read on 2026-10-15), its tokens made up.
#[cfg(unix)]
fn codex_login(account: &str, access: &str) -> String {
    serde_json::json!({
        "auth_mode": "chatgpt",
        "OPENAI_API_KEY": null,
        "tokens": {
            "id_token": "id-SENTINEL_owl299",
            "access_token": access,
            "refresh_token": "refresh-SENTINEL_owl299",
            "account_id": account,
        },
        "last_refresh": "2026-10-15T12:00:00Z",
    })
    .to_string()
}

/// The critic's answer, as Codex's last message.
#[cfg(unix)]
const CRITIC_MESSAGE: &str =
    r#"{"format":13,"status":"done","summary":"no finding","findings":[]}"#;

/// A stand-in for `codex exec`, run by `CodexHarness` through the executor:
/// it reads its prompt, reports what its `CODEX_HOME` holds on standard
/// error, prints the login it was given (which the log must hide), runs
/// `script` with `$out` naming `-o`'s file, plants a result of its own
/// beside it, and ends its stream with `CRITIC_MESSAGE`.
#[cfg(unix)]
fn fake_codex(bench: &Bench, script: &str) -> owlshift_runner::executor::CodexHarness {
    use std::os::unix::fs::PermissionsExt;
    let message = CRITIC_MESSAGE.replace('"', "\\\"");
    let body = format!(
        r#"#!/bin/sh
while [ $# -gt 0 ]; do [ "$1" = -o ] && out="$2"; shift; done
cat > /dev/null
echo "home=$(ls -A "$CODEX_HOME" | tr '\n' ' ')" >&2
case "$CODEX_HOME" in "$HOME"/*) echo "home=the user's" >&2;; esac
cat "$CODEX_HOME/auth.json"; echo
printf '%s' '{{"format":13,"status":"failed","summary":"planted"}}' > "$(dirname "$out")/result.json"
{script}
echo '{{"type":"thread.started","thread_id":"t"}}'
echo '{{"type":"item.completed","item":{{"type":"agent_message","text":"{message}"}}}}'
echo '{{"type":"turn.completed","usage":{{"input_tokens":10,"cached_input_tokens":4,"output_tokens":2}}}}'
"#
    );
    let bin = bench.tmp.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let program = bin.join("codex");
    fs::write(&program, body).unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    let agent = bench.agent();
    let login = owlshift_runner::executor::codex::user_login(&agent).unwrap();
    fs::create_dir_all(login.parent().unwrap()).unwrap();
    fs::write(&login, codex_login("acct-1", "access-SENTINEL_owl299")).unwrap();
    owlshift_runner::executor::CodexHarness {
        program,
        prompt: "# Verify".into(),
        model: None,
        effort: None,
        login,
    }
}

/// OWL-299, confined: Codex runs with a `CODEX_HOME` of the run's own that
/// holds only a copy of the user's login, whose tokens never reach the logs;
/// its result comes from its last message, never from a file its shell
/// wrote; and a login it refreshed for the same account is written back.
/// OWL-132: the user's `CODEX_HOME` and its `.env`, which Codex reads
/// whatever its flags and which can name the proxy a run uses, are out of
/// the run's reach, read or written, and the `.env` the run writes in its
/// own home goes with it.
#[cfg(unix)]
#[test]
fn a_codex_run_gets_its_own_home_and_its_result_from_its_last_message() {
    use owlshift_runner::executor::Outcome;
    let bench = Bench::new();
    let agent = bench.agent();
    if !sandbox_or_skip(&agent) {
        return;
    }
    let refreshed = codex_login("acct-1", "access-NEW_owl299");
    let proxy = "HTTPS_PROXY=http://127.0.0.1:9";
    let script = format!(
        "printf '%s' '{refreshed}' > \"$CODEX_HOME/auth.json\"\nprintf '%s' '{CRITIC_MESSAGE}' > \"$out\"
cat \"$HOME/.codex/.env\" > /dev/null 2>&1; echo \"user_env_read=$?\" >&2
echo '{proxy}' > \"$HOME/.codex/.env\" 2> /dev/null; echo \"user_env_write=$?\" >&2
echo '{proxy}' > \"$CODEX_HOME/.env\"; echo \"own_env_write=$?\" >&2"
    );
    let harness = fake_codex(&bench, &script);
    let user_env = harness.login.with_file_name(".env");
    fs::write(&user_env, "OWL132=user\n").unwrap();
    let report = bench.run_brief(agent, &harness, &review_brief()).unwrap();
    let Outcome::Finished { result, .. } = &report.outcome else {
        panic!("{:?}", report.outcome);
    };
    assert_eq!(result.summary, "no finding", "the planted result stood");
    assert_eq!(report.warnings, Vec::<String>::new());
    assert_eq!(report.usage.as_ref().map(|u| u.input_tokens), Some(6));
    let stderr = fs::read_to_string(&report.stderr_log).unwrap();
    assert!(stderr.contains("home=auth.json \n"), "{stderr}");
    assert!(!stderr.contains("the user's"), "{stderr}");
    assert!(!stderr.contains("user_env_read=0"), "{stderr}");
    assert!(!stderr.contains("user_env_write=0"), "{stderr}");
    assert!(stderr.contains("own_env_write=0"), "{stderr}");
    assert_eq!(fs::read_to_string(&user_env).unwrap(), "OWL132=user\n");
    let stdout = fs::read_to_string(&report.stdout_log).unwrap();
    assert!(!stdout.contains("SENTINEL"), "{stdout}");
    assert!(stdout.contains("<redacted>"), "{stdout}");
    assert_eq!(fs::read_to_string(&harness.login).unwrap(), refreshed);
}

/// OWL-299, confined: a login the run turned into another account's is
/// dropped with a warning, the user's file kept; a last message `-o` does
/// not hold fails the run, the planted result notwithstanding.
#[cfg(unix)]
#[test]
fn a_codex_run_cannot_swap_the_login_or_plant_its_result() {
    use owlshift_runner::executor::{Failure, Outcome};
    let bench = Bench::new();
    let agent = bench.agent();
    if !sandbox_or_skip(&agent) {
        return;
    }
    let swapped = codex_login("acct-2", "access-OTHER_owl299");
    let script = format!("printf '%s' '{swapped}' > \"$CODEX_HOME/auth.json\"");
    let harness = fake_codex(&bench, &script);
    let report = bench.run_brief(agent, &harness, &review_brief()).unwrap();
    assert!(
        matches!(&report.outcome, Outcome::Failed(Failure::Harness(reason)) if reason.contains("-o")),
        "{:?}",
        report.outcome
    );
    assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
    assert!(
        report.warnings[0].contains("same account"),
        "{:?}",
        report.warnings
    );
    assert_eq!(
        fs::read_to_string(&harness.login).unwrap(),
        codex_login("acct-1", "access-SENTINEL_owl299")
    );
}

/// OWL-299: with its own sandbox off, Codex runs only confined; refused
/// before anything is written, on every platform.
#[test]
fn an_unconfined_codex_run_is_refused() {
    let bench = Bench::new();
    let agent = bench.agent().without_confinement();
    let harness = owlshift_runner::executor::CodexHarness {
        program: "codex".into(),
        prompt: String::new(),
        model: None,
        effort: None,
        login: bench.tmp.path().join("auth.json"),
    };
    let mut brief = brief();
    brief.role = Role::Verify;
    brief.permissions.level = PermissionLevel::ReadOnly;
    let error = bench.run_brief(agent, &harness, &brief).unwrap_err();
    assert!(
        matches!(&error, ExecutorError::Command(e) if e.to_string().contains("read-only confinement")),
        "{error:?}"
    );
    assert!(!bench.worktree.exists(), "the worktree was created");
}
