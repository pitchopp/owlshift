//! OWL-399: the steps of `owlshift watch --drain` in real child processes
//! (architecture section 5, "Parallel", "The step, in a child process"):
//! a drain's intake end to end, a stale envelope refused spending nothing,
//! a restarted watch applying a predecessor's hold only where the ticket's
//! ref still is, and since OWL-161 the three ways a watch stops: a drain
//! letting the step under way finish, a checked stop (SIGTERM) checking two
//! Builds that resume later or finding a breach, and a stop now (SIGQUIT,
//! SIGKILL) leaving the run's obligation. The child is `owlshift-step-child`, which calls the entry
//! `owlshift continue <TICKET> --step <ENVELOPE>` calls; the drain's own
//! rules are the bench's to play, in watch's own process
//! (`tests/on_demand.rs`).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use jiff::{SignedDuration, Timestamp};
use serde_json::json;

use owlshift_adapters::tracker::markdown::MarkdownTracker;
use owlshift_contracts::format::{Format, RESULT_FORMAT};
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::step::{RefAfter, StepEnd, StepEnvelope, StepHold, StepKind, StepOutcome};
use owlshift_runner::events::EventLog;
use owlshift_runner::executor::Git;
use owlshift_runner::slots::{Holder, MachineSlots, Slot};
use owlshift_runner::step::{self, Children};
use owlshift_runner::ticket_ref::{self, RefAt};
use owlshift_runner::watch::{POLL_INTERVAL, Watch};
use owlshift_testkit::reply::Reply;
use owlshift_testkit::step_child::{ChildProject, ChildSlots, TICKET};

/// The step's child.
const CHILD: &str = env!("CARGO_BIN_EXE_owlshift-step-child");
/// The fake harness.
const FAKE: &str = env!("CARGO_BIN_EXE_owlshift-fake-harness");

fn ticket() -> TicketId {
    TicketId::new(TICKET).unwrap()
}

/// An intake that ends done, the ticket back in the ready column.
fn intake_done() -> String {
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

/// A drained project under a folder of its own, its paths in a file the
/// child reads, its runs played by `harness`.
struct Setup {
    /// Read by the Unix test alone; held by every test.
    #[cfg_attr(not(unix), allow(dead_code))]
    tmp: tempfile::TempDir,
    project: ChildProject,
    file: PathBuf,
}

fn setup(harness: Option<PathBuf>) -> Setup {
    let tmp = tempfile::Builder::new()
        .prefix("owlshift child ")
        .tempdir()
        .unwrap();
    let harness = harness.unwrap_or_else(|| PathBuf::from(FAKE));
    let project = ChildProject::seed(tmp.path(), harness);
    project.play(Reply::default(), Some(&intake_done()));
    let file = tmp.path().join("project.toml");
    project.save(&file);
    Setup { tmp, project, file }
}

/// The children of the project whose paths `file` keeps.
fn children(file: &Path) -> Children<'static> {
    let file = file.to_owned();
    Children::new(move |ticket, envelope| {
        let mut command = Command::new(CHILD);
        command.arg(&file).arg(ticket.as_str()).arg(envelope);
        command
    })
}

/// One pass of `owlshift watch --drain`, its steps in children: watch's
/// lines, and how many outcomes it reported itself.
fn drain_once(project: &ChildProject, file: &Path) -> (String, usize) {
    let (mut lines, mut events, mut told) = (Vec::new(), Vec::new(), 0);
    let children = children(file);
    project.with_on_demand(&mut events, |on_demand, sink| {
        let watch = Watch {
            on_demand,
            interval: POLL_INTERVAL,
        };
        watch.run_with_children(
            true,
            &children,
            sink,
            &mut lines,
            &mut |_, _, _| told += 1,
            &mut |_| false,
        );
    });
    (String::from_utf8(lines).unwrap(), told)
}

fn comments(project: &ChildProject) -> Vec<String> {
    MarkdownTracker::new(&project.checkout)
        .comments(&ticket())
        .unwrap()
        .into_iter()
        .map(|comment| comment.body)
        .collect()
}

/// The drain's intake, its step in a child: the run plays, the INTAKE
/// comment and the ref's record are kept, the child's lines and outcome
/// are printed after the ticket, its events reach the shared log, its
/// record is read and removed, and watch reports nothing itself.
#[test]
#[cfg_attr(
    windows,
    ignore = "native Windows runs no agent (D9): built, not run, as on_demand's tests"
)]
fn a_drain_intake_runs_in_a_child_end_to_end() {
    let s = setup(None);
    let (lines, told) = drain_once(&s.project, &s.file);
    assert_eq!(told, 0, "the child reports its own outcome:\n{lines}");
    assert!(
        lines.contains(&format!(
            "{TICKET}: {TICKET} is back in the ready column with its intake record"
        )),
        "{lines}"
    );
    let env = s.project.env();
    let git = Git::with_setup("git", move |command| env.apply(command));
    let dirs = s.project.dirs();
    let stored = ticket_ref::read(&git, &dirs.checkout(), &ticket())
        .unwrap()
        .expect("the intake's ref");
    assert!(stored.record.intake.is_some());
    let comments = comments(&s.project);
    assert!(
        comments
            .last()
            .is_some_and(|c| c.starts_with("[owlshift] INTAKE")),
        "{comments:?}"
    );
    assert!(step::leftovers(&dirs).is_empty());
    let log = fs::read_to_string(EventLog::in_dir(&s.project.data).path()).unwrap();
    assert!(log.contains("\"drain\":\"intake\""), "{log}");
}

/// An envelope whose ticket ref moved since watch read it runs nothing: no
/// run, no comment, its outcome stale and holding nothing.
#[test]
#[cfg_attr(
    windows,
    ignore = "native Windows runs no agent (D9): built, not run, as on_demand's tests"
)]
fn a_stale_envelope_is_refused_spending_nothing() {
    let s = setup(None);
    let envelope = StepEnvelope {
        format: Format,
        ticket: ticket(),
        attempt: "1-1-1".to_owned(),
        step: StepKind::Intake,
        commit: Some("0".repeat(40)),
        progress: RefAt::none().progress.as_str().to_owned(),
        answer: None,
        generation: 0,
        kept: Default::default(),
    };
    let dirs = s.project.dirs();
    let mut lines = Vec::new();
    let record = children(&s.file)
        .start(&envelope)
        .unwrap()
        .wait(&dirs, &mut lines)
        .unwrap();
    assert_eq!(record.end, StepEnd::Stale);
    assert_eq!(record.hold, None);
    assert_eq!(record.after, RefAfter::Absent);
    assert!(!dirs.runs(&ticket()).exists(), "a run started");
    assert!(comments(&s.project).is_empty());
    assert!(step::leftovers(&dirs).is_empty());
    assert!(lines.is_empty(), "{}", String::from_utf8_lossy(&lines));
}

/// A watch that starts applies the hold a predecessor's record calls for
/// while the ticket's ref is where the record says it is, and removes the
/// record: the ticket is not taken in. A record whose ref is not the
/// ticket's is removed and holds nothing: the intake runs.
#[test]
#[cfg_attr(
    windows,
    ignore = "native Windows runs no agent (D9): built, not run, as on_demand's tests"
)]
fn a_restarted_watch_applies_a_hold_only_at_its_records_ref() {
    let s = setup(None);
    let dirs = s.project.dirs();
    let record = |attempt: &str, after: RefAfter| StepOutcome {
        format: Format,
        ticket: ticket(),
        attempt: attempt.to_owned(),
        commit: None,
        progress: RefAt::none().progress.as_str().to_owned(),
        answer: None,
        end: StepEnd::Other,
        after,
        hold: Some(StepHold::Held {
            until: Some(
                Timestamp::now()
                    .checked_add(SignedDuration::from_hours(1))
                    .unwrap(),
            ),
        }),
        kept: Default::default(),
        at: Timestamp::now(),
    };

    step::write_outcome(&dirs, &record("1-1-1", RefAfter::Absent)).unwrap();
    let (lines, _) = drain_once(&s.project, &s.file);
    assert!(step::leftovers(&dirs).is_empty());
    assert!(comments(&s.project).is_empty(), "{lines}");
    assert!(!dirs.runs(&ticket()).exists(), "{lines}");

    let elsewhere = RefAfter::Present {
        commit: "f".repeat(40),
        progress: "elsewhere".to_owned(),
    };
    step::write_outcome(&dirs, &record("1-1-2", elsewhere)).unwrap();
    let (lines, _) = drain_once(&s.project, &s.file);
    assert!(step::leftovers(&dirs).is_empty());
    let comments = comments(&s.project);
    assert!(
        comments
            .last()
            .is_some_and(|c| c.starts_with("[owlshift] INTAKE")),
        "{comments:?}\n{lines}"
    );
}

/// Passes of one `owlshift watch --drain` over `project`, its steps in
/// `children`, `between(pass)` called after each and ending the watch when
/// it says so: watch's lines.
fn drain_passes(
    project: &ChildProject,
    children: &Children<'_>,
    between: impl FnMut(u32) -> bool,
) -> String {
    drain_every(project, children, POLL_INTERVAL, between)
}

/// [`drain_passes`], watch waiting `interval` between idle passes.
fn drain_every(
    project: &ChildProject,
    children: &Children<'_>,
    interval: std::time::Duration,
    mut between: impl FnMut(u32) -> bool,
) -> String {
    let (mut lines, mut events, mut pass) = (Vec::new(), Vec::new(), 0);
    project.with_on_demand(&mut events, |on_demand, sink| {
        let watch = Watch {
            on_demand,
            interval,
        };
        watch.run_with_children(
            true,
            children,
            sink,
            &mut lines,
            &mut |_, _, _| {},
            &mut |_| {
                pass += 1;
                between(pass)
            },
        );
    });
    String::from_utf8(lines).unwrap()
}

/// Takes the machine's slot `n` in `data` as another project's step would,
/// tried again for the moment a child forked by another test's thread
/// keeps a lock it inherited (`ProjectDirs::lock`).
fn hold_slot(data: &Path) -> Slot {
    let slots = MachineSlots::new(data, 1);
    let holder = Holder {
        project: "other/project".to_owned(),
        ticket: TicketId::new("OTHER-1").unwrap(),
        pid: std::process::id(),
        since: Timestamp::now(),
    };
    (0..100)
        .find_map(|_| {
            let slot = slots.take(&holder).unwrap();
            if slot.is_none() {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            slot
        })
        .expect("a free slot")
}

/// OWL-409, "Machine run slots": while every slot of the machine is
/// taken, watch starts no child, and a child that finds none after watch's
/// count ends with its "no slot" record, its ticket untouched; either way
/// nothing holds the ticket, so a pass after the slot frees runs the step,
/// with no wait for `RETRY_AFTER`.
#[test]
#[cfg_attr(
    windows,
    ignore = "native Windows runs no agent (D9): built, not run, as on_demand's tests"
)]
fn a_step_waits_for_a_free_run_slot_and_is_not_held() {
    // Watch's count: one slot, held elsewhere.
    let s = setup(None);
    let machine = s.tmp.path().join("machine");
    let dirs = s.project.dirs();
    let ran = || dirs.runs(&ticket()).exists();
    let counted = children(&s.file).with_slots(MachineSlots::new(&machine, 1));
    let mut held = Some(hold_slot(&machine));
    let lines = drain_passes(&s.project, &counted, |pass| {
        if pass == 1 {
            assert!(!ran(), "a run started with no free slot");
            drop(held.take());
        }
        pass < 5 && !ran()
    });
    assert!(ran(), "{lines}");
    assert!(step::leftovers(&dirs).is_empty());

    // The child's own take: watch counts two slots, one held, so it starts
    // the child; the child takes from one, the one held, so it ends with
    // its "no slot" record, read and removed.
    let s = setup(None);
    let machine = s.tmp.path().join("machine");
    let mut project = s.project.clone();
    project.slots = Some(ChildSlots {
        data: machine.clone(),
        count: 1,
    });
    project.save(&s.file);
    let dirs = project.dirs();
    let ran = || dirs.runs(&ticket()).exists();
    let wider = children(&s.file).with_slots(MachineSlots::new(&machine, 2));
    let mut held = Some(hold_slot(&machine));
    let lines = drain_passes(&project, &wider, |pass| {
        if pass == 1 {
            assert!(!ran(), "a run started with no free slot");
            assert!(comments(&project).is_empty());
            drop(held.take());
        }
        pass < 5 && !ran()
    });
    assert!(ran(), "{lines}");
    assert!(step::leftovers(&dirs).is_empty());
}

// A process's state, with no `ps` or `kill` (OWL-110): a program forked by
// one test thread would keep, until it execs, a slot lock another thread's
// probe holds for a moment.
#[cfg(unix)]
#[path = "../../owlshift-platform/src/test_proc.rs"]
#[allow(dead_code)]
mod test_proc;

/// The bound of a test whose watches wait for the machine's slots
/// (OWL-425): `limit` passes, a pass counting only while no process that
/// still runs holds one of the slots in `machine`, so a holder slowed by a
/// loaded machine spends none of it. A slot held with no holder record, or
/// one naming a process gone, is a lock that outlived its step, a slot leak:
/// passes count, and the test fails at the bound once the steps under way
/// end. A probe that fails counts too. A holder that never ends would keep
/// the bound from counting: past [`WEDGED`] the test fails anyway.
#[cfg(unix)]
struct SlotBound<'a> {
    slots: MachineSlots,
    machine: &'a Path,
    limit: u32,
    counted: u32,
    since: std::time::Instant,
}

/// How long a [`SlotBound`] waits on live holders before the test fails:
/// far past any step of these tests, however loaded the machine.
#[cfg(unix)]
const WEDGED: std::time::Duration = std::time::Duration::from_secs(300);

#[cfg(unix)]
impl<'a> SlotBound<'a> {
    fn new(machine: &'a Path, count: u32, limit: u32) -> Self {
        Self {
            slots: MachineSlots::new(machine, count),
            machine,
            limit,
            counted: 0,
            since: std::time::Instant::now(),
        }
    }

    /// Counts the pass that just ended unless a live process holds a slot:
    /// whether the test has passes left.
    fn more(&mut self) -> bool {
        let live = self.slots.holders().is_ok_and(|held| {
            held.iter().any(|slot| {
                slot.holder
                    .as_ref()
                    .is_some_and(|holder| test_proc::is_alive(holder.pid))
            })
        });
        if !live {
            self.counted += 1;
        }
        assert!(
            self.since.elapsed() < WEDGED,
            "{WEDGED:?} of passes, {} of them counted, the slots of {} held meanwhile",
            self.counted,
            self.machine.display()
        );
        self.counted < self.limit
    }
}

/// OWL-409, the acceptance's own case: two watches of two projects, each
/// step in a child taking one of the machine's slots, `concurrent_runs` 1.
/// Each project's intake sleeps a second in its harness, which counts the
/// runs under way: never more than one, and both intakes end up run. The
/// project that waits counts no pass while the other's step holds the slot
/// (OWL-425): on a loaded machine that step outlasted 100 passes.
#[cfg(unix)]
#[test]
fn two_watches_of_two_projects_share_the_machine_s_slots() {
    use std::os::unix::fs::PermissionsExt;
    use std::thread;

    let tmp = tempfile::tempdir().unwrap();
    let machine = tmp.path().join("machine");
    let running = tmp.path().join("running");
    let counts = tmp.path().join("counts");
    fs::create_dir_all(&running).unwrap();
    let script = tmp.path().join("harness.sh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nmkdir '{running}/'$$\nls '{running}' | wc -l >> '{counts}'\nsleep 1\n\
             rmdir '{running}/'$$\nexec '{FAKE}' \"$@\"\n",
            running = running.display(),
            counts = counts.display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let projects: Vec<Setup> = (0..2)
        .map(|_| {
            let s = setup(Some(script.clone()));
            let mut project = s.project.clone();
            project.slots = Some(ChildSlots {
                data: machine.clone(),
                count: 1,
            });
            project.save(&s.file);
            Setup { project, ..s }
        })
        .collect();
    thread::scope(|scope| {
        for s in &projects {
            let machine = &machine;
            scope.spawn(move || {
                let counted = children(&s.file).with_slots(MachineSlots::new(machine, 1));
                let ran = || s.project.dirs().runs(&ticket()).exists();
                let mut bound = SlotBound::new(machine, 1, 100);
                let lines = drain_passes(&s.project, &counted, |_| {
                    thread::sleep(std::time::Duration::from_millis(100));
                    bound.more() && !ran()
                });
                // A quarantine stops every step: it is told first.
                assert_eq!(s.project.dirs().quarantine().unwrap(), None, "{lines}");
                assert!(ran(), "{lines}");
            });
        }
    });
    let counts = fs::read_to_string(&counts).unwrap();
    let counts: Vec<u32> = counts.lines().map(|n| n.trim().parse().unwrap()).collect();
    assert_eq!(counts, [1, 1], "runs under way as each started");
}

/// An intake that ends done, the ticket `trivial`, declaring `zones`.
#[cfg(unix)]
fn intake_declaring(zones: &[&str]) -> String {
    let mut result: serde_json::Value = serde_json::from_str(&intake_done()).unwrap();
    result["intake"]["zones"] = json!(zones);
    result.to_string()
}

/// A Build that asks its decider one question: the step ends with the
/// ticket waiting for the answer, holding what it holds.
#[cfg(unix)]
fn build_asks() -> String {
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
/// `None`), its result `result`, for [`playing_harness`].
#[cfg(unix)]
fn reply_for(dir: &Path, role: &str, ticket: Option<&str>, result: &str) {
    let name = ticket.map_or_else(|| role.to_owned(), |ticket| format!("{role}-{ticket}"));
    let result_file = dir.join(format!("{name}.json"));
    fs::write(&result_file, result).unwrap();
    let reply = Reply {
        result: Some(result_file),
        ..Reply::default()
    };
    fs::write(dir.join(format!("reply-{name}.toml")), reply.render()).unwrap();
}

/// A harness for the tests of several steps at once (OWL-401): a script
/// that plays the fake harness with the reply [`reply_for`] wrote in `dir`
/// for its run's role and ticket, read from its brief. A Build run first
/// marks itself under way in `dir/running` and arrived in `dir/arrived/
/// <group>`, and waits until `builds` Build runs of its group have arrived;
/// then it writes how many runs of every group are under way to
/// `dir/counts`, marks itself counted in `dir/counted/<group>`, and waits
/// until `builds` of its group have counted, so no Build leaves before
/// every count of its group is taken (OWL-422). A Build that waits for no
/// other lasts two seconds more, so the runs of several projects overlap.
/// No wait has a deadline: a test whose Builds wait for each other writes
/// `dir/release` when it runs out of passes, so a Build still waiting goes
/// on, and a harness whose `dir` is gone, or that looked 6000 times, exits
/// with a failure. `typed` is a command the first Build of DEMO-1 runs
/// once between its two waits, its output in `dir/typed/out`.
#[cfg(unix)]
fn playing_harness(dir: &Path, group: &str, builds: usize, typed: Option<&str>) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    for sub in [
        "running".to_owned(),
        format!("arrived/{group}"),
        format!("counted/{group}"),
    ] {
        fs::create_dir_all(dir.join(sub)).unwrap();
    }
    let typed = typed.map_or_else(String::new, |command| {
        format!(
            "  if [ \"$ticket\" = DEMO-1 ] && mkdir \"$dir/typed\" 2>/dev/null; then\n    \
             {command} > \"$dir/typed/out\" 2>&1\n  fi\n"
        )
    });
    let stay = if builds > 1 { "" } else { "  sleep 2\n" };
    let script = dir.join(format!("harness-{group}.sh"));
    fs::write(
        &script,
        format!(
            "#!/bin/sh\ndir='{dir}'\nbrief=\"\"\nreply=\"\"\n\
             while [ $# -gt 0 ]; do\n  case \"$1\" in\n    --brief) brief=\"$2\"; shift 2;;\n    \
             --reply) reply=\"$2\"; shift 2;;\n    *) shift;;\n  esac\ndone\n\
             role=$(sed -n 's/^  \"role\": \"\\([a-z_]*\\)\",$/\\1/p' \"$brief\" | head -n 1)\n\
             ticket=$(sed -n 's/^    \"id\": \"\\([A-Z0-9-]*\\)\",$/\\1/p' \"$brief\" | head -n 1)\n\
             if [ -f \"$dir/reply-$role-$ticket.toml\" ]; then reply=\"$dir/reply-$role-$ticket.toml\"\n\
             elif [ -f \"$dir/reply-$role.toml\" ]; then reply=\"$dir/reply-$role.toml\"; fi\n\
             n=0\ngather() {{\n  \
             while [ \"$(ls \"$dir/$1/{group}\" 2>/dev/null | wc -l)\" -lt {builds} ] \
             && [ ! -e \"$dir/release\" ]; do\n    \
             if [ ! -d \"$dir\" ] || [ $n -ge 6000 ]; then\n      \
             echo \"gave up waiting in $1\" >&2\n      exit 1\n    fi\n    \
             sleep 0.1\n    n=$((n+1))\n  done\n}}\n\
             if [ \"$role\" = build ]; then\n  mkdir \"$dir/running/$$\"\n  \
             touch \"$dir/arrived/{group}/$ticket\"\n  gather arrived\n{typed}  \
             ls \"$dir/running\" | wc -l | tr -d ' ' >> \"$dir/counts\"\n  \
             touch \"$dir/counted/{group}/$ticket\"\n  gather counted\n{stay}  \
             rmdir \"$dir/running/$$\"\nfi\n\
             exec '{FAKE}' --brief \"$brief\" --reply \"$reply\"\n",
            dir = dir.display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    script
}

/// A drained project of tickets DEMO-1 to DEMO-`count`, each admitted and
/// held by DEMO-9, which no gesture admits, until the test moves it to
/// Done; its runs played by `harness`, its children taking the machine's
/// `slots` in `machine`, its capacity `max_in_flight`.
#[cfg(unix)]
fn parallel_setup(
    harness: &Path,
    count: usize,
    max_in_flight: u32,
    (machine, slots): (&Path, u32),
) -> Setup {
    let s = setup(Some(harness.to_owned()));
    let mut project = s.project.clone();
    project.slots = Some(ChildSlots {
        data: machine.to_owned(),
        count: slots,
    });
    project.save(&s.file);
    project.set_policy(&format!("max_in_flight = {max_in_flight}"));
    project.add_ticket("DEMO-9", &[], "");
    for n in 1..=count {
        project.add_ticket(
            &format!("DEMO-{n}"),
            &["agent"],
            "blocked_by = [\"DEMO-9\"]\n",
        );
    }
    Setup { project, ..s }
}

/// The ticket ref of `id` in `project`'s dedicated checkout, if it reads.
#[cfg(unix)]
fn record_of(project: &ChildProject, id: &str) -> Option<ticket_ref::TicketRecord> {
    let env = project.env();
    let git = Git::with_setup("git", move |command| env.apply(command));
    let checkout = project.dirs().checkout();
    ticket_ref::read(&git, &checkout, &TicketId::new(id).unwrap())
        .ok()
        .flatten()
        .map(|stored| stored.record)
}

/// Whether the ticket `id` of `project` waits for its decider's answer.
#[cfg(unix)]
fn asks(project: &ChildProject, id: &str) -> bool {
    use owlshift_core::state::{Status, TicketState};

    record_of(project, id).is_some_and(|record| {
        TicketState::try_from(&record.state)
            .is_ok_and(|state| matches!(state.status(), Status::NeedsInput { .. }))
    })
}

/// OWL-401's acceptance, scenario S5 with three tickets in flight, their
/// steps in real children: with `policy.max_in_flight = 4` and three run
/// slots, DEMO-1, DEMO-2 and DEMO-3 declare the zones `a`, `b` and `c` at
/// intake, one at a time, while DEMO-9 holds them; once it is done, one
/// pass dispatches the three, and their Builds run at once, each holding
/// until all three are under way and none leaving until the three have
/// counted, however long each takes (OWL-422). Meanwhile a person types
/// `owlshift do DEMO-4`, a ticket with no intake, which would hold the
/// whole repository, from within DEMO-1's Build: it is refused, DEMO-1
/// holding `a`. Each Build asks its decider, and the three wait; no
/// quarantine, no obligation and no outcome record is left.
///
/// Its bound stays 200 passes, not counted on the slots' holders as the
/// tests of two projects are (OWL-425, 2026-10-22): its Builds wait for each
/// other while alive, so a Build never dispatched would leave the others
/// holding their slots, and the test would fail only when the harness gives
/// up, some ten minutes on. OWL-422 measured 42 to 55 passes at a two to
/// three times slower machine; in 29 loaded runs this bound never ran out.
#[cfg(unix)]
#[test]
fn three_tickets_build_at_once_and_a_do_typed_meanwhile_waits() {
    let tmp = tempfile::tempdir().unwrap();
    let plays = tmp.path().join("plays");
    let machine = tmp.path().join("machine");
    fs::create_dir_all(&plays).unwrap();
    for (id, zone) in [("DEMO-1", "a"), ("DEMO-2", "b"), ("DEMO-3", "c")] {
        reply_for(&plays, "intake", Some(id), &intake_declaring(&[zone]));
    }
    reply_for(&plays, "build", None, &build_asks());
    // The file the project's paths will be kept in, which `do` reads.
    let file = tmp.path().join("project.toml");
    let typed = format!("'{CHILD}' '{}' DEMO-4 do", file.display());
    let harness = playing_harness(&plays, "one", 3, Some(&typed));
    let s = parallel_setup(&harness, 3, 4, (&machine, 3));
    s.project.add_ticket("DEMO-4", &[], "");
    s.project.save(&file);
    let counted = children(&file).with_slots(MachineSlots::new(&machine, 3));
    let interval = std::time::Duration::from_millis(200);
    let lines = drain_every(&s.project, &counted, interval, |pass| {
        if record_of(&s.project, "DEMO-3").is_some_and(|record| record.intake.is_some()) {
            s.project.move_ticket("DEMO-9", "Done");
        }
        let all = ["DEMO-1", "DEMO-2", "DEMO-3"]
            .iter()
            .all(|id| asks(&s.project, id));
        if !all && pass >= 200 {
            // Out of passes: a Build still waiting goes on, so watch's wait
            // for its children ends and the test fails below.
            fs::write(plays.join("release"), "").unwrap();
        }
        pass < 200 && !all
    });
    // A quarantine stops every step: it is told first, with what it says.
    let dirs = s.project.dirs();
    assert_eq!(dirs.quarantine().unwrap(), None, "{lines}");
    let counts = fs::read_to_string(plays.join("counts")).unwrap_or_default();
    assert!(
        !plays.join("release").exists(),
        "out of passes before the three Builds asked, counts {counts:?}\n{lines}"
    );
    assert_eq!(
        counts.lines().collect::<Vec<_>>(),
        ["3", "3", "3"],
        "{lines}"
    );
    let typed = fs::read_to_string(plays.join("typed/out")).unwrap();
    assert!(
        typed.contains("DEMO-4 waits: DEMO-1 holds zone `a`"),
        "{typed}\n{lines}"
    );
    for (id, zone) in [("DEMO-1", "a"), ("DEMO-2", "b"), ("DEMO-3", "c")] {
        assert!(asks(&s.project, id), "{id}\n{lines}");
        let held = record_of(&s.project, id).unwrap().held.unwrap();
        assert_eq!(held.zones, [zone], "{id}");
    }
    assert_eq!(record_of(&s.project, "DEMO-4"), None, "{lines}");
    assert!(!dirs.obligations_stand(), "{lines}");
    assert!(step::leftovers(&dirs).is_empty());
}

/// OWL-401, scenario S16: two projects, each with `policy.max_in_flight =
/// 2`, share one machine's two run slots. Each would dispatch its two
/// tickets together once their intakes ran, each Build lasting a second:
/// never more than two runs of the machine are under way, the first watch
/// to take a free slot running and the other's dispatches waiting until one
/// frees, and all four Builds run. A rendezvous of a project's Builds
/// would deadlock here, each project holding one slot: the test of three
/// Builds at once is the one above.
#[cfg(unix)]
#[test]
fn two_projects_two_in_flight_each_share_two_slots() {
    use std::thread;

    let tmp = tempfile::tempdir().unwrap();
    let plays = tmp.path().join("plays");
    let machine = tmp.path().join("machine");
    fs::create_dir_all(&plays).unwrap();
    for (id, zone) in [("DEMO-1", "a"), ("DEMO-2", "b")] {
        reply_for(&plays, "intake", Some(id), &intake_declaring(&[zone]));
    }
    reply_for(&plays, "build", None, &build_asks());
    let projects: Vec<Setup> = ["one", "two"]
        .iter()
        .map(|group| {
            let harness = playing_harness(&plays, group, 1, None);
            parallel_setup(&harness, 2, 2, (&machine, 2))
        })
        .collect();
    let watched = std::sync::Mutex::new(Vec::<String>::new());
    thread::scope(|scope| {
        for s in &projects {
            let (machine, watched) = (&machine, &watched);
            scope.spawn(move || {
                let counted = children(&s.file).with_slots(MachineSlots::new(machine, 2));
                let interval = std::time::Duration::from_millis(200);
                let mut bound = SlotBound::new(machine, 2, 300);
                let lines = drain_every(&s.project, &counted, interval, |_| {
                    if record_of(&s.project, "DEMO-2").is_some_and(|r| r.intake.is_some()) {
                        s.project.move_ticket("DEMO-9", "Done");
                    }
                    let both = asks(&s.project, "DEMO-1") && asks(&s.project, "DEMO-2");
                    bound.more() && !both
                });
                // A quarantine stops every step: it is told first.
                assert_eq!(s.project.dirs().quarantine().unwrap(), None, "{lines}");
                assert!(
                    asks(&s.project, "DEMO-1") && asks(&s.project, "DEMO-2"),
                    "{lines}"
                );
                watched.lock().unwrap().push(lines);
            });
        }
    });
    let lines = watched.into_inner().unwrap().join("\n");
    let counts = fs::read_to_string(plays.join("counts")).unwrap();
    let counts: Vec<u32> = counts.lines().map(|n| n.trim().parse().unwrap()).collect();
    assert_eq!(counts.len(), 4, "every Build ran\n{lines}");
    assert!(
        counts.iter().all(|n| (1..=2).contains(n)),
        "runs under way as each Build went on: {counts:?}\n{lines}"
    );
}

/// What a watch process and its step left running, by pid.
#[cfg(unix)]
struct Running {
    pids: Vec<u32>,
}

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

#[cfg(unix)]
fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    use std::thread;
    use std::time::{Duration, Instant};

    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "still waiting for {what}");
        thread::sleep(Duration::from_millis(20));
    }
}

/// Kills, on a failed test's way out, what it left running.
#[cfg(unix)]
impl Drop for Running {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        for pid in &self.pids {
            let _ = Command::new("kill")
                .args(["-s", "KILL", &pid.to_string()])
                .status();
        }
    }
}

#[cfg(unix)]
fn pid_in(file: &Path) -> u32 {
    fs::read_to_string(file).unwrap().trim().parse().unwrap()
}

/// [`wait_for`], within `seconds`, `log`, watch's output, in the failure.
#[cfg(unix)]
fn wait_within(what: &str, seconds: u64, log: &Path, mut done: impl FnMut() -> bool) {
    use std::thread;
    use std::time::{Duration, Instant};

    let deadline = Instant::now() + Duration::from_secs(seconds);
    while !done() {
        assert!(
            Instant::now() < deadline,
            "still waiting for {what}\n{}",
            fs::read_to_string(log).unwrap_or_default()
        );
        thread::sleep(Duration::from_millis(50));
    }
}

/// A harness for the stop tests (OWL-161): a script that, for a run of
/// `role` while `dir/hold` stands, runs `before` (shell text), writes its
/// pid to `dir/<role>-<ticket>.pid`, then runs `hold` (shell text), such as
/// `exec sleep 30`, a run under way until a stop cuts it; past it, and for
/// any other run, it plays the fake harness with the reply [`reply_for`]
/// wrote in `dir`, or the bench's.
#[cfg(unix)]
fn holding_harness(dir: &Path, role: &str, before: &str, hold: &str) -> PathBuf {
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
             exec '{FAKE}' --brief \"$brief\" --reply \"$reply\"\n",
            dir = dir.display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    script
}

/// Starts [`helper_watch_until_stopped`] over the project `file` keeps, in a
/// process group of its own with core dumps off, its output in `log`: its
/// steps in children, or in its own process when `in_process`. A step's
/// child writes its pid to `child_pid` when given.
#[cfg(unix)]
fn start_watch(
    file: &Path,
    in_process: bool,
    child_pid: Option<&Path>,
    log: &Path,
) -> std::process::Child {
    use std::os::unix::process::CommandExt;

    let out = fs::File::create(log).unwrap();
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", "ulimit -c 0; exec \"$0\" \"$@\""])
        .arg(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "helper_watch_until_stopped",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("OWLSHIFT_CHILD_PROJECT", file)
        .stdout(out.try_clone().unwrap())
        .stderr(out)
        .process_group(0);
    if in_process {
        command.env("OWLSHIFT_WATCH_IN_PROCESS", "1");
    }
    if let Some(child_pid) = child_pid {
        command.env("OWLSHIFT_STEP_CHILD_PID", child_pid);
    }
    command.spawn().unwrap()
}

/// Sends `signal` to `watch`.
#[cfg(unix)]
fn signal(watch: &std::process::Child, signal: &str) {
    let sent = Command::new("kill")
        .args(["-s", signal, &watch.id().to_string()])
        .status()
        .unwrap();
    assert!(sent.success(), "kill -s {signal}");
}

/// The tickets whose run ended `outcome`, as their `run_ended` events in
/// `project`'s log say.
#[cfg(unix)]
fn ended(project: &ChildProject, outcome: &str) -> Vec<String> {
    use owlshift_contracts::event::{Event, EventKind};

    let log = EventLog::in_dir(&project.data);
    let mut tickets: Vec<String> = fs::read_to_string(log.path())
        .unwrap_or_default()
        .lines()
        .map(|line| Event::parse(line).unwrap())
        .filter(|event| event.kind == EventKind::RunEnded && event.data["outcome"] == outcome)
        .filter_map(|event| event.ticket.map(|ticket| ticket.as_str().to_owned()))
        .collect();
    tickets.sort();
    tickets
}

/// OWL-161, the acceptance's first case: a drain asked while a step runs
/// lets it finish. DEMO-1's intake, in a child, writes the project's stop
/// file, then ends done a second later: its INTAKE is kept, DEMO-2, admitted
/// too, never starts, and watch ends on its own, the file removed, nothing
/// quarantined and no obligation left.
#[cfg(unix)]
#[test]
fn a_drain_lets_the_step_under_way_finish() {
    let s = setup(None);
    let plays = s.tmp.path().join("plays");
    fs::create_dir_all(&plays).unwrap();
    fs::write(plays.join("hold"), "").unwrap();
    let dirs = s.project.dirs();
    let stop = owlshift_runner::watch::stop::file(&dirs);
    let before = format!("touch '{}'", stop.display());
    let mut project = s.project.clone();
    project.harness = holding_harness(&plays, "intake", &before, "sleep 1");
    project.add_ticket("DEMO-2", &["agent"], "");
    project.save(&s.file);
    let lines = drain_every(&project, &children(&s.file), POLL_INTERVAL, |pass| {
        pass < 20
    });
    assert!(lines.contains("ended: drained"), "{lines}");
    assert!(
        comments(&project)
            .last()
            .is_some_and(|c| c.starts_with("[owlshift] INTAKE")),
        "{lines}"
    );
    assert!(
        !dirs.runs(&TicketId::new("DEMO-2").unwrap()).exists(),
        "{lines}"
    );
    assert!(!stop.exists());
    assert_eq!(dirs.quarantine().unwrap(), None, "{lines}");
    assert!(!dirs.obligations_stand(), "{lines}");
    assert!(step::leftovers(&dirs).is_empty());
}

/// OWL-161, the acceptance's second case, scenario S11's stop: SIGTERM to a
/// watch while two Builds run, each in a child. Watch sends each child
/// SIGTERM; each stops its run, checks it and retires its obligation, its
/// ticket left at Build, and watch ends, nothing quarantined. A later drain
/// resumes both Builds, which ask their decider.
#[cfg(unix)]
#[test]
fn a_sigterm_during_two_builds_checks_both_and_both_resume() {
    use owlshift_core::state::{Status, TicketState};
    use owlshift_core::vocab::Stage;

    let tmp = tempfile::tempdir().unwrap();
    let plays = tmp.path().join("plays");
    let machine = tmp.path().join("machine");
    fs::create_dir_all(&plays).unwrap();
    for (id, zone) in [("DEMO-1", "a"), ("DEMO-2", "b")] {
        reply_for(&plays, "intake", Some(id), &intake_declaring(&[zone]));
    }
    reply_for(&plays, "build", None, &build_asks());
    fs::write(plays.join("hold"), "").unwrap();
    let harness = holding_harness(&plays, "build", ":", "exec sleep 30");
    let s = parallel_setup(&harness, 2, 2, (&machine, 2));
    s.project.move_ticket("DEMO-9", "Done");
    let log = tmp.path().join("watch.log");
    let mut watch = start_watch(&s.file, false, None, &log);
    let mut running = Running {
        pids: vec![watch.id()],
    };
    let builds = [
        plays.join("build-DEMO-1.pid"),
        plays.join("build-DEMO-2.pid"),
    ];
    wait_within("both Builds under way", 120, &log, || {
        builds.iter().all(|pid| pid.exists())
    });
    let runs: Vec<u32> = builds.iter().map(|pid| pid_in(pid)).collect();
    running.pids.extend(&runs);

    signal(&watch, "TERM");
    wait_within("watch to end", 60, &log, || {
        watch.try_wait().unwrap().is_some()
    });
    let status = watch.wait().unwrap();
    let log_text = fs::read_to_string(&log).unwrap_or_default();
    assert!(status.success(), "{status}\n{log_text}");
    for run in runs {
        assert!(!alive(run), "a Build's run outlived the checked stop");
    }
    let dirs = s.project.dirs();
    assert_eq!(dirs.quarantine().unwrap(), None, "{log_text}");
    assert!(!dirs.obligations_stand(), "{log_text}");
    assert_eq!(
        ended(&s.project, "stopped"),
        ["DEMO-1", "DEMO-2"],
        "{log_text}"
    );
    for id in ["DEMO-1", "DEMO-2"] {
        let state = TicketState::try_from(&record_of(&s.project, id).unwrap().state).unwrap();
        assert_eq!(state.status(), Status::Active(Stage::Build), "{id}");
    }
    assert!(log_text.contains("ended: its steps stopped"), "{log_text}");

    fs::remove_file(plays.join("hold")).unwrap();
    let counted = children(&s.file).with_slots(MachineSlots::new(&machine, 2));
    let interval = std::time::Duration::from_millis(200);
    let both = || asks(&s.project, "DEMO-1") && asks(&s.project, "DEMO-2");
    let lines = drain_every(&s.project, &counted, interval, |pass| pass < 200 && !both());
    assert!(both(), "{lines}");
    assert_eq!(dirs.quarantine().unwrap(), None, "{lines}");
}

/// OWL-161, the acceptance's third case: a breach planted by a run, here an
/// intake in the watch's own process as plain `owlshift watch` runs it,
/// writing into the dedicated checkout before it sleeps, is found by the
/// check of the checked stop that cuts it: the project is quarantined, the
/// run's obligation kept.
#[cfg(unix)]
#[test]
fn a_breach_found_at_a_checked_stop_quarantines() {
    let s = setup(None);
    let plays = s.tmp.path().join("plays");
    fs::create_dir_all(&plays).unwrap();
    fs::write(plays.join("hold"), "").unwrap();
    let dirs = s.project.dirs();
    let planted = format!("echo planted > '{}/planted'", dirs.checkout().display());
    let mut project = s.project.clone();
    project.harness = holding_harness(&plays, "intake", &planted, "exec sleep 30");
    project.save(&s.file);
    let log = s.tmp.path().join("watch.log");
    let mut watch = start_watch(&s.file, true, None, &log);
    let mut running = Running {
        pids: vec![watch.id()],
    };
    let pid = plays.join(format!("intake-{TICKET}.pid"));
    wait_within("the intake under way", 60, &log, || pid.exists());
    let run = pid_in(&pid);
    running.pids.push(run);

    signal(&watch, "TERM");
    wait_within("watch to end", 60, &log, || {
        watch.try_wait().unwrap().is_some()
    });
    let log_text = fs::read_to_string(&log).unwrap_or_default();
    assert!(watch.wait().unwrap().success(), "{log_text}");
    assert!(!alive(run), "the intake's run outlived the checked stop");
    let quarantine = dirs.quarantine().unwrap();
    assert!(
        quarantine
            .as_deref()
            .is_some_and(|text| text.contains("broke isolation")),
        "{quarantine:?}\n{log_text}"
    );
    assert!(dirs.obligations_stand(), "{log_text}");
}

/// OWL-161, the acceptance's fourth case, and OWL-399's "Stopping": a stop
/// now, SIGQUIT, stops watch's child through the policy every command
/// shares, and a watch killed outright (SIGKILL) has its sentinel stop it;
/// either way the child's own sentinel then stops the run it started, a
/// harness that would sleep 30 s, its obligation left as before OWL-161:
/// the next pass quarantines the project.
#[cfg(unix)]
#[test]
fn sigquit_and_sigkill_of_watch_stop_now_leaving_the_obligation() {
    for signal_name in ["QUIT", "KILL"] {
        let s = setup(None);
        let plays = s.tmp.path().join("plays");
        fs::create_dir_all(&plays).unwrap();
        fs::write(plays.join("hold"), "").unwrap();
        let mut project = s.project.clone();
        project.harness = holding_harness(&plays, "intake", ":", "exec sleep 30");
        project.save(&s.file);

        let child_pid = s.tmp.path().join("child.pid");
        let log = s.tmp.path().join("watch.log");
        let mut watch = start_watch(&s.file, false, Some(&child_pid), &log);
        let mut running = Running {
            pids: vec![watch.id()],
        };
        let harness_pid = plays.join(format!("intake-{TICKET}.pid"));
        wait_within("the child and its run", 60, &log, || {
            child_pid.exists() && harness_pid.exists()
        });
        let (child, run) = (pid_in(&child_pid), pid_in(&harness_pid));
        running.pids.extend([child, run]);
        assert!(watch.try_wait().unwrap().is_none(), "watch ended early");

        signal(&watch, signal_name);
        wait_for("watch to end", || watch.try_wait().unwrap().is_some());
        wait_for(
            &format!("the child and its run to stop after SIG{signal_name}"),
            || !alive(child) && !alive(run),
        );
        let dirs = project.dirs();
        assert!(dirs.obligations_stand(), "SIG{signal_name}");
        let (lines, _) = drain_once(&project, &s.file);
        assert!(
            dirs.quarantine().unwrap().is_some(),
            "SIG{signal_name}\n{lines}"
        );
    }
}

/// The watch of the stop tests (OWL-161): passes of `owlshift watch
/// --drain` over the project `OWLSHIFT_CHILD_PROJECT` names, its steps in
/// children or, with `OWLSHIFT_WATCH_IN_PROCESS`, in its own process, with
/// the signal policy, the sentinel and the stop by degrees `owlshift`
/// installs, until it is stopped or two minutes have passed.
#[test]
#[ignore = "helper, run by the tests below"]
fn helper_watch_until_stopped() {
    use std::time::{Duration, Instant};

    use owlshift_platform::process;
    use owlshift_runner::watch::stop::CHECKED_STOP_BOUND;

    let Some(file) = std::env::var_os("OWLSHIFT_CHILD_PROJECT") else {
        return;
    };
    process::stop_trees_on_signal().unwrap();
    #[cfg(unix)]
    process::stop_trees_when_killed().unwrap();
    process::stop_by_degrees(Some(CHECKED_STOP_BOUND)).unwrap();
    let file = PathBuf::from(file);
    let project = ChildProject::load(&file);
    let in_process = std::env::var_os("OWLSHIFT_WATCH_IN_PROCESS").is_some();
    let ends = Instant::now() + Duration::from_secs(120);
    let mut sleep = |interval| {
        process::wait_asked(interval);
        Instant::now() < ends
    };
    let mut children = children(&file);
    if let Some(slots) = project.machine_slots() {
        children = children.with_slots(slots);
    }
    project.with_on_demand(&mut std::io::stdout(), |on_demand, sink| {
        let watch = Watch {
            on_demand,
            interval: Duration::from_millis(200),
        };
        let mut out = std::io::stdout();
        if in_process {
            watch.run_drain(sink, &mut out, &mut |_, _, _| {}, &mut sleep);
        } else {
            watch.run_with_children(
                true,
                &children,
                sink,
                &mut out,
                &mut |_, _, _| {},
                &mut sleep,
            );
        }
    });
}
