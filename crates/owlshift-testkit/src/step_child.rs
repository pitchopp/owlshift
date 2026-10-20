//! A project for the tests that run a step of `owlshift watch --drain` in a
//! real child process (OWL-399): the bench's drained ticket, its adapters
//! built again in each process from paths kept in a file, since the bench's
//! own stand-ins (its clock, its scripted replies, its GitHub) live in the
//! test's memory and cannot cross a process boundary.
//!
//! [`ChildProject::with_on_demand`] builds `OnDemand` as the bench does,
//! with the Markdown tracker, the fake harness playing one reply file for
//! every role, the agent environment unconfined, the system clock and a
//! forge that answers nothing: a drain's intake reads none.
//! `owlshift-step-child` is the child, calling the same entry as
//! `owlshift continue <TICKET> --step <ENVELOPE>`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use owlshift_adapters::forge::Repo;
use owlshift_adapters::forge::github::GitHubForge;
use owlshift_adapters::graphql::{Response, Transport};
use owlshift_adapters::tracker::markdown::MarkdownTracker;
use owlshift_contracts::config::ProjectConfig;
use owlshift_core::routing::RunRouting;
use owlshift_runner::agent_env::AgentEnv;
use owlshift_runner::events::{EventLog, EventSink};
use owlshift_runner::executor::{Git, Harness};
use owlshift_runner::on_demand::{self, OnDemand};
use owlshift_runner::project::ProjectDirs;
use owlshift_runner::routing::Router;
use owlshift_runner::slots::MachineSlots;
use owlshift_runner::usage::UsageStore;

use crate::gh;
use crate::git::{GitEnv, seed};
use crate::harness::FakeHarness;
use crate::reply::Reply;

/// The repository the project's dedicated folder is named after.
pub const REPO: &str = "demo/project";
/// The project's one ticket.
pub const TICKET: &str = "DEMO-1";

/// Where a step's child finds the project, kept in a TOML file.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChildProject {
    /// Owlshift's data directory: the event log and the project's folder.
    pub data: PathBuf,
    /// The person's checkout, which the Markdown tracker keeps tickets in.
    pub checkout: PathBuf,
    /// The bare remote the dedicated checkout clones.
    pub bare: PathBuf,
    /// The bench's home, with git's configuration.
    pub home: PathBuf,
    /// The harness program: the fake harness, or a stand-in of the test's.
    pub harness: PathBuf,
    /// The reply file every run plays.
    pub reply: PathBuf,
    /// The machine's run slots the child takes one of before its step, as
    /// `owlshift continue --step` does (OWL-409): the data directory they
    /// live in and their count. None: the child takes none.
    #[serde(default)]
    pub slots: Option<ChildSlots>,
}

/// The machine's run slots of [`ChildProject::slots`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChildSlots {
    /// The data directory whose `slots` folder holds them.
    pub data: PathBuf,
    /// `concurrent_runs`.
    pub count: u32,
}

impl ChildProject {
    /// Seeds under `root` a project whose ready column `Todo` holds DEMO-1,
    /// labelled `agent` by its assignee `maintainer`, the one admitter, its
    /// runs played by `harness` with the reply file `root/reply.toml`.
    pub fn seed(root: &Path, harness: PathBuf) -> Self {
        let project = root.join("project");
        let ticket = project.join("tickets").join(TICKET);
        fs::create_dir_all(&ticket).unwrap();
        fs::write(
            project.join("owlshift.toml"),
            "requires = \">=0.0\"\n[tracker]\nkind = \"markdown\"\n\
             admit = { label = \"agent\" }\nadmitters = [\"maintainer\"]\n\
             states = { ready = \"Todo\", working = \"In Progress\", needs_input = \"Needs Input\", \
             review = \"In Review\", parked = \"Parked\" }\n\
             [stack]\ngate = [\"git grep -q Hello -- GREETING.md\"]\n\
             [pipeline]\ndefault = \"trivial\"\nplan_approval = \"never\"\nfindings = \"arbiter\"\n\
             [models]\n[policy]\nalways_human = []\n",
        )
        .unwrap();
        fs::write(project.join("AGENTS.md"), "Sign off every commit.\n").unwrap();
        fs::write(
            ticket.join("ticket.md"),
            "+++\ntitle = \"Add a greeting\"\nauthor = \"maintainer\"\nstage = \"Todo\"\n\
             assignee = \"maintainer\"\nlabels = [\"agent\"]\n+++\n\nAdd GREETING.md saying Hello.\n",
        )
        .unwrap();
        let env = GitEnv::create(root.join("home")).unwrap();
        let remote = seed(&env, root, &project).unwrap();
        let reply = root.join("reply.toml");
        fs::write(&reply, Reply::default().render()).unwrap();
        Self {
            data: root.join("data"),
            checkout: remote.checkout,
            bare: remote.bare,
            home: env.home().to_owned(),
            harness,
            reply,
            slots: None,
        }
    }

    /// The machine's run slots this project's children take, if any.
    pub fn machine_slots(&self) -> Option<MachineSlots> {
        self.slots
            .as_ref()
            .map(|slots| MachineSlots::new(&slots.data, slots.count))
    }

    /// Writes this project's paths to `path`.
    pub fn save(&self, path: &Path) {
        fs::write(path, toml::to_string(self).unwrap()).unwrap();
    }

    /// The project whose paths `path` keeps.
    pub fn load(path: &Path) -> Self {
        toml::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    /// Writes `reply`, what every run plays from now on; a result it names
    /// is `result`, written beside it.
    pub fn play(&self, mut reply: Reply, result: Option<&str>) {
        if let Some(result) = result {
            let path = self.reply.with_file_name("result.json");
            fs::write(&path, result).unwrap();
            reply.result = Some(path);
        }
        fs::write(&self.reply, reply.render()).unwrap();
    }

    /// The project's folder in Owlshift's data directory.
    pub fn dirs(&self) -> ProjectDirs {
        ProjectDirs::new(&self.data, &Repo::parse(REPO).unwrap())
    }

    /// The bench's git environment.
    pub fn env(&self) -> GitEnv {
        GitEnv::existing(self.home.clone())
    }

    /// Runs `act` with the project's `OnDemand` and an event sink printing
    /// on `out`.
    pub fn with_on_demand<R>(
        &self,
        out: &mut dyn Write,
        act: impl FnOnce(&OnDemand<'_>, &mut EventSink<'_>) -> R,
    ) -> R {
        let env = self.env();
        // The executor's credential probes run the real gh.
        gh::warm_up(&env.agent_parent());
        let agent = AgentEnv::new(env.agent_parent())
            .unwrap()
            .without_confinement();
        let git = {
            let env = env.clone();
            Git::with_setup("git", move |command| env.apply(command))
        };
        let executor = on_demand::executor(git, agent);
        let harness = Routed(FakeHarness {
            program: self.harness.clone(),
            reply: self.reply.clone(),
            readable: env.agent_readable(),
            sessions: false,
        });
        let text = fs::read_to_string(self.checkout.join("owlshift.toml")).unwrap();
        let config = ProjectConfig::parse(&text).unwrap();
        let tracker = MarkdownTracker::new(&self.checkout);
        let forge = GitHubForge::with_transport(NoForge, Repo::parse(REPO).unwrap());
        let dirs = self.dirs();
        let remote_url = self.bare.to_string_lossy().into_owned();
        let usage = UsageStore::in_data_dir(&self.data);
        let on_demand = OnDemand {
            executor: &executor,
            tracker: &tracker,
            forge: &forge,
            build: &harness,
            intake: &harness.0,
            design: &harness,
            design_review: &harness,
            verify: &harness,
            codex: None,
            arbiter: &harness,
            answer_check: &harness.0,
            resolver: &harness,
            rebase: &harness,
            remote_url: &remote_url,
            config: &config,
            dirs: &dirs,
            head_wait: Duration::ZERO,
            clock: &on_demand::system_clock,
            notifier: None,
            usage: &usage,
        };
        let mut sink = EventSink::new(REPO, EventLog::in_dir(&self.data), out);
        act(&on_demand, &mut sink)
    }
}

/// A forge that answers nothing: no step of these tests reads it.
struct NoForge;

impl Transport for NoForge {
    fn send(&self, _request: &Value) -> Result<Response, String> {
        Err("this test's forge answers nothing".to_owned())
    }
}

/// The fake harness for every role and routing.
struct Routed(FakeHarness);

impl Router for Routed {
    fn harness(&self, _routing: RunRouting) -> Box<dyn Harness + '_> {
        Box::new(self.0.clone())
    }
}
