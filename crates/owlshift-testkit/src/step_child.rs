//! A project for the tests that run a step of `owlshift watch --drain` in a
//! real child process (OWL-399): the bench's drained ticket, its adapters
//! built again in each process from paths kept in a file, since the bench's
//! own stand-ins (its clock, its scripted replies, its GitHub) live in the
//! test's memory and cannot cross a process boundary.
//!
//! [`ChildProject::with_on_demand`] builds `OnDemand` as the bench does,
//! with the Markdown tracker, the fake harness playing one reply file for
//! every role, the agent environment unconfined, the system clock and a
//! forge with no pull request: a drain's intake reads none, and a
//! dispatch reads that none is open (OWL-401).
//! `owlshift-step-child` is the child, calling the same entry as
//! `owlshift continue <TICKET> --step <ENVELOPE>`; [`step_children`] starts
//! it for a watch, and [`ChildProject::watch_until_stopped`] is the watch
//! of the tests that stop one (OWL-161) and of the daemon's bench (OWL-441).
//!
//! The rest is what those tests play with: the results of [`intake_done`],
//! [`intake_declaring`] and [`build_asks`], the replies of [`reply_for`],
//! the harness of `holding_harness` (Unix), a ticket's record ([`record_of`],
//! [`asks`]), the event log ([`events`], [`ended_as`]) and a notifier writing to a file
//! ([`FileNotifier`]).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use owlshift_adapters::forge::Repo;
use owlshift_adapters::forge::github::GitHubForge;
use owlshift_adapters::graphql::{Response, Transport};
use owlshift_adapters::notifier::{self, Notifier};
use owlshift_adapters::tracker::markdown::MarkdownTracker;
use owlshift_contracts::config::ProjectConfig;
use owlshift_contracts::event::{Event, EventKind};
use owlshift_contracts::format::RESULT_FORMAT;
use owlshift_contracts::ids::TicketId;
use owlshift_core::routing::RunRouting;
use owlshift_core::state::{Status, TicketState};
use owlshift_platform::process;
use owlshift_runner::agent_env::AgentEnv;
use owlshift_runner::events::{EventLog, EventSink};
use owlshift_runner::executor::{Git, Harness};
use owlshift_runner::on_demand::{self, OnDemand};
use owlshift_runner::project::ProjectDirs;
use owlshift_runner::routing::Router;
use owlshift_runner::slots::MachineSlots;
use owlshift_runner::step::Children;
use owlshift_runner::ticket_ref::{self, TicketRecord};
use owlshift_runner::usage::UsageStore;
use owlshift_runner::watch::Watch;

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
    /// The file each notification is appended to, one line each
    /// ([`FileNotifier`]). None: this machine shows no notification.
    #[serde(default)]
    pub notes: Option<PathBuf>,
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
            notes: None,
        }
    }

    /// [`Self::seed`], its runs ending as [`intake_done`] says, then the
    /// tickets DEMO-1 to DEMO-`count` added, each admitted and held by
    /// DEMO-9, which no gesture admits, until a test moves it to Done; its
    /// children taking the machine's `slots` in `machine`, its capacity
    /// `max_in_flight` (OWL-401).
    pub fn seed_parallel(
        root: &Path,
        harness: PathBuf,
        count: usize,
        max_in_flight: u32,
        (machine, slots): (&Path, u32),
    ) -> Self {
        let mut project = Self::seed(root, harness);
        project.play(Reply::default(), Some(&intake_done()));
        project.slots = Some(ChildSlots {
            data: machine.to_owned(),
            count: slots,
        });
        project.set_policy(&format!("max_in_flight = {max_in_flight}"));
        project.add_ticket("DEMO-9", &[], "");
        for n in 1..=count {
            project.add_ticket(
                &format!("DEMO-{n}"),
                &["agent"],
                "blocked_by = [\"DEMO-9\"]\n",
            );
        }
        project
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

    /// Writes the ticket `id` in the ready column, assigned to its decider,
    /// labelled `labels`, with `front` added to its front matter (OWL-401).
    pub fn add_ticket(&self, id: &str, labels: &[&str], front: &str) {
        let dir = self.checkout.join("tickets").join(id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("ticket.md"),
            format!(
                "+++\ntitle = \"Ticket {id}\"\nauthor = \"maintainer\"\nstage = \"Todo\"\n\
                 assignee = \"maintainer\"\nlabels = {labels:?}\n{front}+++\n\nGreet {id}.\n"
            ),
        )
        .unwrap();
    }

    /// Moves the ticket `id` to `stage`, its front matter otherwise kept.
    pub fn move_ticket(&self, id: &str, stage: &str) {
        let file = self.checkout.join("tickets").join(id).join("ticket.md");
        let text = fs::read_to_string(&file).unwrap();
        let moved: Vec<String> = text
            .lines()
            .map(|line| {
                if line.starts_with("stage = ") {
                    format!("stage = \"{stage}\"")
                } else {
                    line.to_owned()
                }
            })
            .collect();
        // Replaced whole, as an editor saves, so a command reading the
        // tickets meanwhile never finds it empty (OWL-425); the tracker
        // skips a file in `tickets/`.
        let partial = self.checkout.join("tickets").join(format!(".{id}.partial"));
        fs::write(&partial, format!("{}\n", moved.join("\n"))).unwrap();
        fs::rename(&partial, &file).unwrap();
    }

    /// Adds `line` to the `[policy]` of the project file the commands load,
    /// such as `max_in_flight = 3` (OWL-401).
    pub fn set_policy(&self, line: &str) {
        let file = self.checkout.join("owlshift.toml");
        let mut text = fs::read_to_string(&file).unwrap();
        assert!(text.ends_with("[policy]\nalways_human = []\n"), "{text}");
        text.push_str(line);
        text.push('\n');
        fs::write(&file, text).unwrap();
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
        self.with_on_demand_at(&on_demand::system_clock, out, act)
    }

    /// [`Self::with_on_demand`], the `OnDemand` reading `clock`, such as one
    /// a test moves past watch's retry delay; the children read the system
    /// clock still.
    pub fn with_on_demand_at<R>(
        &self,
        clock: &dyn Fn() -> Timestamp,
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
        let notes = self.notes.clone().map(FileNotifier);
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
            review_fallback: None,
            arbiter: &harness,
            answer_check: &harness.0,
            resolver: &harness,
            rebase: &harness,
            remote_url: &remote_url,
            config: &config,
            dirs: &dirs,
            head_wait: Duration::ZERO,
            clock,
            notifier: notes.as_ref().map(|notes| notes as &dyn Notifier),
            usage: &usage,
            web_tools: true,
        };
        let mut sink = EventSink::new(REPO, EventLog::in_dir(&self.data), out);
        act(&on_demand, &mut sink)
    }

    /// Passes of `owlshift watch --drain` over this project, a pass every
    /// 200 ms, until a stop is asked of it or `between`, called after each
    /// pass, says to end: its steps in `children`, each taking one of this
    /// project's machine slots, or with none in its own process, as plain
    /// `owlshift watch` runs them. The caller installs the signal policy, the
    /// sentinel and the stop by degrees, as `owlshift` does.
    pub fn watch_until_stopped(
        &self,
        children: Option<Children<'_>>,
        between: &mut dyn FnMut() -> bool,
    ) {
        let children = children.map(|children| match self.machine_slots() {
            Some(slots) => children.with_slots(slots),
            None => children,
        });
        let mut sleep = |interval| {
            process::wait_asked(interval);
            between()
        };
        self.with_on_demand(&mut std::io::stdout(), |on_demand, sink| {
            let watch = Watch {
                on_demand,
                interval: Duration::from_millis(200),
            };
            let mut out = std::io::stdout();
            match &children {
                Some(children) => watch.run_with_children(
                    true,
                    children,
                    sink,
                    &mut out,
                    &mut |_, _, _| {},
                    &mut sleep,
                ),
                None => watch.run_drain(sink, &mut out, &mut |_, _, _| {}, &mut sleep),
            }
        });
    }
}

/// The children of a watch's steps over the project whose paths `file`
/// keeps: `program`, the step's child, given the file, the ticket and the
/// envelope.
pub fn step_children(file: &Path, program: &Path) -> Children<'static> {
    let (file, program) = (file.to_owned(), program.to_owned());
    Children::new(move |ticket, envelope| {
        let mut command = Command::new(&program);
        command.arg(&file).arg(ticket.as_str()).arg(envelope);
        command
    })
}

/// Appends each notification to the file it names, one line each.
pub struct FileNotifier(pub PathBuf);

impl Notifier for FileNotifier {
    fn notify(&self, line: &str) -> Result<(), notifier::Error> {
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.0)
            .and_then(|mut file| writeln!(file, "{line}"))
            .map_err(|error| notifier::Error::new(format!("{}: {error}", self.0.display())))
    }
}

/// An intake that ends done, the ticket back in the ready column.
pub fn intake_done() -> String {
    json!({
        "format": RESULT_FORMAT,
        "status": "done",
        "summary": "A small greeting.",
        "intake": {
            "variant": "trivial",
            "routing": { "harness": "claude", "tier": "fast", "effort": "low" },
            "dependencies": [],
        },
    })
    .to_string()
}

/// An intake that ends done, the ticket `trivial`, declaring `zones`.
pub fn intake_declaring(zones: &[&str]) -> String {
    let mut result: Value = serde_json::from_str(&intake_done()).unwrap();
    result["intake"]["zones"] = json!(zones);
    result.to_string()
}

/// A Build that asks its decider one question: the step ends with the
/// ticket waiting for the answer, holding what it holds.
pub fn build_asks() -> String {
    json!({
        "format": RESULT_FORMAT,
        "status": "questions",
        "summary": "The greeting's language is not given.",
        "questions": [{
            "id": "Q1", "category": "scope", "context": "The ticket names no language.",
            "text": "Which language should the greeting use?",
            "recommendation": { "why": "The repository is in English." },
        }],
    })
    .to_string()
}

/// Writes in `dir` the reply of `role`'s runs on `ticket` (any ticket when
/// `None`), its result `result`, for the tests' harness scripts such as
/// `holding_harness` (Unix).
pub fn reply_for(dir: &Path, role: &str, ticket: Option<&str>, result: &str) {
    let name = ticket.map_or_else(|| role.to_owned(), |ticket| format!("{role}-{ticket}"));
    let result_file = dir.join(format!("{name}.json"));
    fs::write(&result_file, result).unwrap();
    let reply = Reply {
        result: Some(result_file),
        ..Reply::default()
    };
    fs::write(dir.join(format!("reply-{name}.toml")), reply.render()).unwrap();
}

/// A harness for the stop tests (OWL-161): a script that, for a run of
/// `role` while `dir/hold` stands, runs `before` (shell text, in the run's
/// worktree), writes its pid to `dir/<role>-<ticket>.pid`, then runs `hold`
/// (shell text), such as `exec sleep 30`, a run under way until a stop cuts
/// it; past it, and for any other run, it plays `fake`, the fake harness,
/// with the reply [`reply_for`] wrote in `dir`, or the bench's.
#[cfg(unix)]
pub fn holding_harness(dir: &Path, fake: &Path, role: &str, before: &str, hold: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let script = dir.join(format!("harness-{role}.sh"));
    fs::write(
        &script,
        format!(
            "#!/bin/sh\ndir='{dir}'\nbrief=\"\"\nreply=\"\"\n\
             while [ $# -gt 0 ]; do\n  case \"$1\" in\n    --brief) brief=\"$2\"; shift 2;;\n    \
             --reply) reply=\"$2\"; shift 2;;\n    *) shift;;\n  esac\ndone\n\
             role=$(sed -n 's/^  \"role\": \"\\([a-z_]*\\)\",$/\\1/p' \"$brief\" | head -n 1)\n\
             ticket=$(sed -n 's/^    \"id\": \"\\([A-Z0-9-]*\\)\",$/\\1/p' \"$brief\" | head -n 1)\n\
             if [ \"$role\" = '{role}' ] && [ -e \"$dir/hold\" ]; then\n  {before}\n  \
             echo $$ > \"$dir/$role-$ticket.pid.partial\"\n  \
             mv \"$dir/$role-$ticket.pid.partial\" \"$dir/$role-$ticket.pid\"\n  {hold}\nfi\n\
             if [ -f \"$dir/reply-$role-$ticket.toml\" ]; then reply=\"$dir/reply-$role-$ticket.toml\"\n\
             elif [ -f \"$dir/reply-$role.toml\" ]; then reply=\"$dir/reply-$role.toml\"; fi\n\
             exec '{fake}' --brief \"$brief\" --reply \"$reply\"\n",
            dir = dir.display(),
            fake = fake.display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    script
}

/// The ticket ref of `id` in `project`'s dedicated checkout, if it reads.
pub fn record_of(project: &ChildProject, id: &str) -> Option<TicketRecord> {
    let env = project.env();
    let git = Git::with_setup("git", move |command| env.apply(command));
    let checkout = project.dirs().checkout();
    ticket_ref::read(&git, &checkout, &TicketId::new(id).unwrap())
        .ok()
        .flatten()
        .map(|stored| stored.record)
}

/// Whether the ticket `id` of `project` waits for its decider's answer.
pub fn asks(project: &ChildProject, id: &str) -> bool {
    record_of(project, id).is_some_and(|record| {
        TicketState::try_from(&record.state)
            .is_ok_and(|state| matches!(state.status(), Status::NeedsInput { .. }))
    })
}

/// Every event of the log in the data directory `data`, oldest first.
pub fn events(data: &Path) -> Vec<Event> {
    fs::read_to_string(EventLog::in_dir(data).path())
        .unwrap_or_default()
        .lines()
        .map(|line| Event::parse(line).unwrap())
        .collect()
}

/// The tickets whose run ended `outcome`, as their `run_ended` events in
/// `project`'s log say, sorted.
pub fn ended_as(project: &ChildProject, outcome: &str) -> Vec<String> {
    let mut tickets: Vec<String> = events(&project.data)
        .into_iter()
        .filter(|event| event.kind == EventKind::RunEnded && event.data["outcome"] == outcome)
        .filter_map(|event| event.ticket.map(|ticket| ticket.as_str().to_owned()))
        .collect();
    tickets.sort();
    tickets
}

/// A forge with no pull request (OWL-401): the open pull requests of the
/// repository and the one of a ticket's branch, which a dispatch and its
/// admission read, are none; any other request is refused, so a delivery
/// stops at its pull request.
struct NoForge;

impl Transport for NoForge {
    fn send(&self, request: &Value) -> Result<Response, String> {
        let query = request["query"].as_str().unwrap_or_default();
        let data = if query.starts_with("query OpenPullRequests") {
            json!({ "repository": { "pullRequests": { "totalCount": 0, "nodes": [] } } })
        } else if query.starts_with("query RepositoryPulls") {
            json!({ "repository": { "pullRequests": {
                "totalCount": 0,
                "pageInfo": { "hasNextPage": false, "endCursor": null },
                "nodes": [],
            } } })
        } else {
            return Err("this test's forge answers nothing else".to_owned());
        };
        Ok(Response::new(200, json!({ "data": data }).to_string()))
    }
}

/// The fake harness for every role and routing.
struct Routed(FakeHarness);

impl Router for Routed {
    fn harness(&self, _routing: RunRouting) -> Box<dyn Harness + '_> {
        Box::new(self.0.clone())
    }
}
