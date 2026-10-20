//! OWL-399: the steps of `owlshift watch --drain` in real child processes
//! (architecture section 5, "Parallel", "The step, in a child process"):
//! a drain's intake end to end, a stale envelope refused spending nothing,
//! SIGTERM and SIGKILL of watch stopping the child and its run, and a
//! restarted watch applying a predecessor's hold only where the ticket's
//! ref still is. The child is `owlshift-step-child`, which calls the entry
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
use owlshift_runner::slots::{MachineSlots, Slot};
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
    let record = children(&s.file).run(&dirs, &envelope, &mut lines).unwrap();
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
    mut between: impl FnMut(u32) -> bool,
) -> String {
    let (mut lines, mut events, mut pass) = (Vec::new(), Vec::new(), 0);
    project.with_on_demand(&mut events, |on_demand, sink| {
        let watch = Watch {
            on_demand,
            interval: POLL_INTERVAL,
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
    (0..100)
        .find_map(|_| {
            let slot = slots.take().unwrap();
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

/// OWL-409, the acceptance's own case: two watches of two projects, each
/// step in a child taking one of the machine's slots, `concurrent_runs` 1.
/// Each project's intake sleeps a second in its harness, which counts the
/// runs under way: never more than one, and both intakes end up run.
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
                let lines = drain_passes(&s.project, &counted, |pass| {
                    thread::sleep(std::time::Duration::from_millis(100));
                    pass < 100 && !ran()
                });
                assert!(ran(), "{lines}");
            });
        }
    });
    let counts = fs::read_to_string(&counts).unwrap();
    let counts: Vec<u32> = counts.lines().map(|n| n.trim().parse().unwrap()).collect();
    assert_eq!(counts, [1, 1], "runs under way as each started");
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

/// OWL-399, "Stopping": a watch told to end (SIGTERM) stops its child
/// through the policy every command shares, and a watch killed outright
/// (SIGKILL) has its sentinel stop it; either way the child's own sentinel
/// then stops the run it started, a harness that would sleep 30 s.
#[cfg(unix)]
#[test]
fn sigterm_and_sigkill_of_watch_stop_the_child_and_its_run() {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;

    for signal in ["TERM", "KILL"] {
        let s = setup(None);
        let harness_pid = s.tmp.path().join("harness.pid");
        let script = s.tmp.path().join("harness.sh");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\necho $$ > '{pid}.partial'\nmv '{pid}.partial' '{pid}'\nexec sleep 30\n",
                pid = harness_pid.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let mut project = s.project.clone();
        project.harness = script;
        project.save(&s.file);

        let child_pid = s.tmp.path().join("child.pid");
        let mut watch = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "helper_watch_drain",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("OWLSHIFT_CHILD_PROJECT", &s.file)
            .env("OWLSHIFT_STEP_CHILD_PID", &child_pid)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let mut running = Running {
            pids: vec![watch.id()],
        };
        wait_for("the child and its run", || {
            child_pid.exists() && harness_pid.exists()
        });
        let pid =
            |file: &Path| -> u32 { fs::read_to_string(file).unwrap().trim().parse().unwrap() };
        let (child, run) = (pid(&child_pid), pid(&harness_pid));
        running.pids.extend([child, run]);
        assert!(watch.try_wait().unwrap().is_none(), "watch ended early");

        let sent = Command::new("kill")
            .args(["-s", signal, &watch.id().to_string()])
            .status()
            .unwrap();
        assert!(sent.success(), "kill -s {signal}");
        wait_for("watch to end", || watch.try_wait().unwrap().is_some());
        wait_for(
            &format!("the child and its run to stop after SIG{signal}"),
            || !alive(child) && !alive(run),
        );
    }
}

/// The watch of the signal test: one pass of `owlshift watch --drain` over
/// the project `OWLSHIFT_CHILD_PROJECT` names, with the signal policy and
/// the sentinel `owlshift` installs first.
#[test]
#[ignore = "helper, run by the test above"]
fn helper_watch_drain() {
    let Some(file) = std::env::var_os("OWLSHIFT_CHILD_PROJECT") else {
        return;
    };
    owlshift_platform::process::stop_trees_on_signal().unwrap();
    #[cfg(unix)]
    owlshift_platform::process::stop_trees_when_killed().unwrap();
    let file = PathBuf::from(file);
    drain_once(&ChildProject::load(&file), &file);
}
