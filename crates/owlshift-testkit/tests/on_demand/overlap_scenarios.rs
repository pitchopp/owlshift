//! Scenario S5 at Watch under `policy.overlap = "rebase"` (OWL-449;
//! architecture section 5, "Tolerated overlap", "The refresh" and "A review
//! after a resolution"), on the bench of `on_demand.rs`: DEMO-1 is
//! delivered, its pull request followed, and DEMO-2, the ticket it shares a
//! file with, was merged; the fake GitHub follows DEMO-1's pull request,
//! and DEMO-2's merge is a commit on the remote's `main`.

use owlshift_contracts::refs::{Ended, EndedWhy, TicketHeld};

use super::merge_scenarios::{github_merges, later, merged_comments};
use super::verify_scenarios::{fixed_build, found, ruled};
use super::*;

/// The project's `AGENTS.md` with a line DEMO-1's delivery adds at its end.
const AGENTS_APPENDED: &str =
    "Sign off every commit (`git commit -s`).\nKeep each commit to one change.\n";

/// The same file with a line DEMO-2's merge adds at its start, which merges
/// with DEMO-1's line cleanly.
const AGENTS_PREPENDED: &str = "Read the design first.\nSign off every commit (`git commit -s`).\n";

/// A bench whose project tolerates overlaps, DEMO-1 delivered with
/// `files` beside its greeting; its verify seats play from the queue when
/// `verifying`, a clean pass first.
fn overlapping(files: &[(&str, &str)], verifying: bool) -> Bench {
    overlapping_with(files, verifying, "overlap = \"rebase\"\n")
}

/// [`overlapping`] with the project's `policy` instead (OWL-653).
fn overlapping_with(files: &[(&str, &str)], verifying: bool, policy: &str) -> Bench {
    let bench = Bench::with("maintainer", "assignee = \"maintainer\"\n", policy);
    bench.verify_plays.set(verifying);
    let mut reply = bench.reply(Some("Hello"), Some(DONE));
    for (path, text) in files {
        reply
            .files
            .insert(RelativePath::new(*path).unwrap(), (*text).to_owned());
    }
    let mut replies = vec![reply];
    if verifying {
        replies.push(bench.reviewing(&found(&[])));
    }
    let (outcome, printed) = bench.run(replies, None);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    bench
}

/// DEMO-2's ref, its pull request merged, sharing `paths` with DEMO-1: the
/// overlap recorded by DEMO-2's admission when `theirs`, else by DEMO-1's
/// own record, which the refresh reads all the same.
fn merged_sibling(bench: &Bench, paths: &[&str], theirs: bool) {
    merged_sibling_as(bench, "DEMO-2", paths, theirs);
}

/// [`merged_sibling`] for the sibling `sibling` (OWL-654).
fn merged_sibling_as(bench: &Bench, sibling: &str, paths: &[&str], theirs: bool) {
    let env = bench.env.clone();
    let git = Git::with_setup("git", move |command| env.apply(command));
    let (demo1, demo2) = (ticket(), TicketId::new(sibling).unwrap());
    let paths: Vec<String> = paths.iter().map(|path| (*path).to_owned()).collect();
    let at = bench.clock.get();
    let mut record = bench.record();
    let mut delivery = record.delivery.clone().unwrap();
    delivery.ended = Some(Ended {
        why: EndedWhy::Merged,
        at,
        head: None,
    });
    let mut held = TicketHeld::dispatched(at);
    held.admit(paths.clone(), Vec::new(), None, at);
    if theirs {
        held.record_overlap(&demo1, &paths, &[], at);
    }
    let sibling = TicketRecord {
        delivery: Some(delivery),
        held: Some(held),
        ..record.clone()
    };
    write_through_run_file(&git, &bench.dirs(), &demo2, &sibling, None);
    if !theirs {
        let mut own = record.held.clone().unwrap();
        own.record_overlap(&demo2, &paths, &[], at);
        record.held = Some(own);
        bench.write_record(&record);
    }
}

/// The `rebase_run` decision events.
fn rebase_runs(bench: &Bench) -> Vec<Value> {
    data_of(&bench.events(), EventKind::Decision, "watch")
        .into_iter()
        .filter(|data| data["watch"] == "rebase_run")
        .collect()
}

/// The refresh: DEMO-2, merged, shared `AGENTS.md` with DEMO-1, by DEMO-2's
/// record. Its merge changes that file without a conflict, which GitHub
/// reports mergeable and git merges cleanly: DEMO-1's pull request gets one
/// refresh, the runner's own merge with no run (OWL-655), its cause the
/// overlap, and its merged head, pushed, is the one GitHub's checks are
/// read on again. A
/// later move of the base on another path refreshes nothing.
#[test]
fn a_siblings_merge_on_a_shared_file_refreshes_the_pull_request() {
    let bench = overlapping(&[("AGENTS.md", AGENTS_APPENDED)], false);
    merged_sibling(&bench, &["AGENTS.md"], true);
    let first_push = bench.remote_branch().unwrap();
    let main = bench.merge_into_main(&[("AGENTS.md", AGENTS_PREPENDED)]);
    let runs = bench.briefs().len();
    let mut pass = 0;
    let (outcomes, lines, printed) = bench.watch_during(Vec::new(), Vec::new(), &mut |_| {
        pass += 1;
        if pass == 3 {
            bench.merge_into_main(&[("OTHER.md", "Another ticket's file.\n")]);
        }
        pass < 6
    });
    let report = format!("{outcomes:?}\n{lines}\n{printed}");
    assert!(outcomes.iter().all(Result::is_ok), "{report}");
    let merge = assert_runner_merged(&bench, runs, &first_push, &main, "overlap");
    let head = bench.remote_branch().unwrap();
    assert_eq!(head, merge, "{report}");
    // GitHub's checks are read again on the merged head.
    let read_at_head = data_of(&bench.events(), EventKind::Decision, "watch")
        .into_iter()
        .any(|data| data["head"] == json!(head) && data["verdict"] == json!("green"));
    assert!(read_at_head, "{report}");
}

/// OWL-653: DEMO-1, delivered under merge on green, holds `AGENTS.md`
/// rather than the whole repository, which cannot yield, and its record
/// yielded, as watch writes it after a long round; its own open pull request
/// lists the files its branch changes, `AGENTS.md` among them. DEMO-2,
/// merged, shared that file, and its merge moves `main`: the bench and the
/// `main` it moved to.
fn yielded_and_refreshed() -> (Bench, String) {
    let bench = overlapping_with(
        &[("AGENTS.md", AGENTS_APPENDED)],
        false,
        "overlap = \"rebase\"\n[forge]\nmerge = \"all\"\n",
    );
    let mut record = bench.record();
    let at = bench.clock.get();
    let mut held = TicketHeld::dispatched(at);
    held.admit(vec!["AGENTS.md".to_owned()], Vec::new(), None, at);
    assert!(held.yield_at(at));
    record.held = Some(held);
    bench.write_record(&record);
    let changed = bench
        .env
        .run(
            &bench.remote.bare,
            &["diff", "--name-only", &format!("main...{BRANCH}")],
        )
        .unwrap();
    let changed: Vec<String> = String::from_utf8(changed)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    assert!(
        changed.iter().any(|file| file == "AGENTS.md"),
        "{changed:?}"
    );
    *bench.github.own_files.lock().unwrap() = changed;
    merged_sibling(&bench, &["AGENTS.md"], true);
    let main = bench.merge_into_main(&[("AGENTS.md", AGENTS_PREPENDED)]);
    (bench, main)
}

/// Two minutes later on `bench`'s clock, so that merge on green's next
/// green read is at least a minute after the one before.
fn two_minutes_later(bench: &Bench) {
    let clock = &bench.clock;
    clock.set(
        clock
            .get()
            .checked_add(SignedDuration::from_mins(2))
            .unwrap(),
    );
}

/// The ticket's `yield` = `readmitted` decision events.
fn readmissions(bench: &Bench) -> Vec<Value> {
    data_of(&bench.events(), EventKind::Decision, "yield")
        .into_iter()
        .filter(|data| data["yield"] == "readmitted")
        .collect()
}

/// OWL-653, the shape seen on LOC-2200: a ticket that yielded, then
/// delivered, its base moving on a path its overlap names, is admitted
/// again past its own pull request, which it never waits for: its refresh
/// rebase runs, its cause the overlap, and merge on green then merges the
/// pull request. Before the fix, every pass stopped with "DEMO-1 waits:
/// pull request #1 holds zone `AGENTS.md`".
#[test]
fn a_yielded_delivered_ticket_is_refreshed_past_its_own_pull_request_and_merges_on_green() {
    let (bench, main) = yielded_and_refreshed();
    let mut pass = 0;
    let (outcomes, lines, printed) = bench.watch_during(Vec::new(), Vec::new(), &mut |_| {
        pass += 1;
        two_minutes_later(&bench);
        pass < 6
    });
    let report = format!("{outcomes:?}\n{lines}\n{printed}");
    assert!(outcomes.iter().all(Result::is_ok), "{report}");
    assert!(!report.contains("pull request #1 holds"), "{report}");
    let runs = refreshes_merged(&bench);
    assert_eq!(runs.len(), 1, "{runs:?}\n{report}");
    assert_eq!(
        (&runs[0]["cause"], &runs[0]["onto"]),
        (&json!("overlap"), &json!(main)),
        "{report}"
    );
    assert_eq!(readmissions(&bench).len(), 1, "{report}");
    let record = bench.record();
    assert!(!record.held.unwrap().yielded(), "{report}");
    let delivery = record.delivery.unwrap();
    assert_eq!(
        delivery.ended.as_ref().map(|ended| ended.why),
        Some(EndedWhy::Merged),
        "{report}"
    );
    let merged = data_of(&bench.events(), EventKind::PrMerged, "watch");
    assert_eq!(merged.len(), 1, "{report}");
}

/// Past watch's hold of a step that stopped with its ticket ref as it
/// was ([`owlshift_runner::watch::RETRY_AFTER`]) on `bench`'s clock, so
/// that the next pass tries it again.
fn past_the_retry(bench: &Bench) {
    let clock = &bench.clock;
    let retry = SignedDuration::try_from(owlshift_runner::watch::RETRY_AFTER).unwrap();
    clock.set(
        clock
            .get()
            .checked_add(retry + SignedDuration::from_mins(1))
            .unwrap(),
    );
}

/// The ticket's `watch` = `waits` decision events.
fn watch_waits(bench: &Bench) -> Vec<Value> {
    data_of(&bench.events(), EventKind::Decision, "watch")
        .into_iter()
        .filter(|data| data["watch"] == "waits")
        .collect()
}

/// OWL-653: the same yielded, delivered DEMO-1, its refresh held back at
/// its re-admission by a person's open pull request, #7, that changes
/// `AGENTS.md`, the zone DEMO-1 held before it yielded. Watch's passes stop
/// with the wait, told at each pass, and `owlshift why DEMO-1` shows
/// it as DEMO-1's wait now, naming #7. Once #7 is closed, the refresh runs.
#[test]
fn a_yielded_ticket_held_back_at_its_readmission_is_shown_waiting_by_why() {
    let (bench, main) = yielded_and_refreshed();
    bench.github.others.lock().unwrap().push((
        7,
        "person/agents".to_owned(),
        vec!["AGENTS.md".to_owned()],
    ));
    let why = || {
        let now = bench.clock.get();
        owlshift_runner::why::render(
            &owlshift_runner::why::gather(
                &bench.data,
                &bench.dirs(),
                REPO,
                Some(ticket()),
                2,
                &[],
                now,
            )
            .unwrap(),
        )
    };
    let (mut shown, mut told) = (String::new(), Vec::new());
    let mut pass = 0;
    let (outcomes, lines, printed) = bench.watch_during(Vec::new(), Vec::new(), &mut |_| {
        pass += 1;
        match pass {
            1 => shown = why(),
            // Two passes held back: the wait is told at each.
            2 => {
                told = watch_waits(&bench);
                bench.github.others.lock().unwrap().clear();
            }
            _ => {}
        }
        past_the_retry(&bench);
        pass < 5
    });
    let report = format!("{outcomes:?}\n{lines}\n{printed}");
    let waits = "pull request #7 holds zone `AGENTS.md`, which it held before it yielded: it is \
                 admitted again once pull request #7 no longer holds it";
    for outcome in &outcomes[..2] {
        assert!(
            matches!(outcome, Err(Stop::Waits { said, .. }) if said == waits),
            "{report}"
        );
    }
    assert!(
        shown.contains("now: pull request #7 holds zone `AGENTS.md`, which it would touch\n"),
        "{shown}\n{report}"
    );
    assert!(!shown.contains("not waiting"), "{shown}");
    assert_eq!(told.len(), 2, "{told:?}\n{report}");
    for wait in &told {
        assert_eq!(
            (&wait["pull_request"], &wait["reason"]),
            (&json!(7), &json!("collision")),
            "{report}"
        );
    }
    let runs = refreshes_merged(&bench);
    assert_eq!(runs.len(), 1, "{runs:?}\n{report}");
    assert_eq!(
        (&runs[0]["cause"], &runs[0]["onto"]),
        (&json!("overlap"), &json!(main)),
        "{report}"
    );
    assert_eq!(readmissions(&bench).len(), 1, "{report}");
}

/// OWL-613: DEMO-1, delivered, its own record sharing `AGENTS.md` with
/// DEMO-2, which another runner claims and this one has no ref of; the
/// project claiming on the remote when `remote`. This runner fetches the
/// claim, then DEMO-2's change to that file merges and its holder deletes
/// the claim, as its sweep does a pass after reading the merge, before
/// this runner's watch passes. The rebase runs those passes start.
fn after_a_colleagues_merge(remote: bool) -> (Bench, String, Vec<Value>, String) {
    let bench = Bench::with(
        "maintainer",
        "assignee = \"maintainer\"\n",
        "overlap = \"rebase\"\n",
    );
    if remote {
        bench.claims_on_remote();
    }
    let mut reply = bench.reply(Some("Hello"), Some(DONE));
    reply.files.insert(
        RelativePath::new("AGENTS.md").unwrap(),
        AGENTS_APPENDED.to_owned(),
    );
    let (outcome, printed) = bench.run(vec![reply], None);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let demo2 = TicketId::new("DEMO-2").unwrap();
    let colleague = bench.colleague();
    bench.claimed_by(&colleague, "DEMO-2", &["AGENTS.md"]);
    claims::fetch(&bench.git(), &bench.dirs().checkout()).unwrap();
    let mut record = bench.record();
    let mut own = record.held.clone().unwrap();
    own.record_overlap(&demo2, &["AGENTS.md".to_owned()], &[], bench.clock.get());
    record.held = Some(own);
    bench.write_record(&record);
    let main = bench.merge_into_main(&[("AGENTS.md", AGENTS_PREPENDED)]);
    bench.unclaimed("DEMO-2");
    assert!(bench.claim_commit(&demo2).is_none());
    let mut pass = 0;
    let (outcomes, lines, printed) = bench.watch_during(Vec::new(), Vec::new(), &mut |_| {
        pass += 1;
        pass < 4
    });
    let report = format!("{outcomes:?}\n{lines}\n{printed}");
    assert!(outcomes.iter().all(Result::is_ok), "{report}");
    let runs = refreshes_merged(&bench);
    (bench, main, runs, report)
}

/// OWL-613: under remote claims, the overlap with DEMO-2 still counts once
/// its claim is gone: watch's next pass refreshes DEMO-1's pull request
/// onto DEMO-2's merge, its cause the overlap, though GitHub reports no
/// conflict.
#[test]
fn a_colleagues_merge_refreshes_the_pull_request_once_its_claim_is_gone() {
    let (bench, main, runs, report) = after_a_colleagues_merge(true);
    assert_eq!(runs.len(), 1, "{runs:?}\n{report}");
    assert_eq!(
        (&runs[0]["cause"], &runs[0]["onto"]),
        (&json!("overlap"), &json!(main)),
        "{report}"
    );
    let head = bench.remote_branch().unwrap();
    assert!(is_ancestor(&bench, &main, &head), "{report}");
}

/// Under local claims the same sibling with no ref here is a ticket
/// forgotten here: it counts no more, and the merge refreshes nothing.
#[test]
fn under_local_claims_a_sibling_without_a_ref_refreshes_nothing() {
    let (_, _, runs, report) = after_a_colleagues_merge(false);
    assert!(runs.is_empty(), "{runs:?}\n{report}");
}

/// The `decision` events of the overlaps this runner recorded from another
/// runner's claim (OWL-636).
fn recorded_from_claims(bench: &Bench) -> Vec<Value> {
    data_of(&bench.events(), EventKind::Decision, "overlap")
        .into_iter()
        .filter(|data| data["overlap"] == "recorded" && data["by"] == "claim")
        .collect()
}

/// OWL-636: DEMO-1, delivered, holds `AGENTS.md` and its record names no
/// sibling; DEMO-2, which another runner claims later holding that file,
/// has its overlap recorded by that runner alone. Watch's passes, the claim
/// standing, record it in DEMO-1's own record, once. Then DEMO-2's change
/// merges and its claim is deleted: the next watch, its own fetch pruning
/// the claim, refreshes DEMO-1's pull request onto the merge, its cause the
/// overlap, though GitHub reports no conflict, and records nothing more.
#[test]
fn a_colleagues_later_overlap_is_recorded_here_and_refreshes_at_its_merge() {
    let bench = Bench::with(
        "maintainer",
        "assignee = \"maintainer\"\n",
        "overlap = \"rebase\"\n",
    );
    bench.claims_on_remote();
    let mut reply = bench.reply(Some("Hello"), Some(DONE));
    reply.files.insert(
        RelativePath::new("AGENTS.md").unwrap(),
        AGENTS_APPENDED.to_owned(),
    );
    let (outcome, printed) = bench.run(vec![reply], None);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    // The bench's ticket names no zone, so its Build holds the whole
    // repository, which overlaps nothing: it holds `AGENTS.md` instead.
    let mut record = bench.record();
    let at = bench.clock.get();
    let mut held = TicketHeld::dispatched(at);
    held.admit(vec!["AGENTS.md".to_owned()], Vec::new(), None, at);
    record.held = Some(held);
    bench.write_record(&record);
    // DEMO-1 is delivered, so it waits for no claim: the colleague's claim
    // time, read on another clock than the bench's, orders nothing here.
    let colleague = bench.colleague();
    bench.claimed_by(&colleague, "DEMO-2", &["AGENTS.md"]);
    // Each watch's first pass fetches the claims itself.
    let mut pass = 0;
    let (outcomes, lines, printed) = bench.watch(Vec::new(), &mut |_| {
        pass += 1;
        pass < 3
    });
    let report = format!("{outcomes:?}\n{lines}\n{printed}");
    assert!(outcomes.iter().all(Result::is_ok), "{report}");
    let recorded = recorded_from_claims(&bench);
    assert_eq!(recorded.len(), 1, "{recorded:?}\n{report}");
    assert_eq!(
        (&recorded[0]["tickets"], &recorded[0]["paths"]),
        (&json!(["DEMO-2"]), &json!(["AGENTS.md"]))
    );
    let overlaps = bench.record().held.unwrap().overlaps;
    assert_eq!(overlaps.len(), 1, "{overlaps:?}");
    assert_eq!(
        (overlaps[0].ticket.as_str(), &overlaps[0].paths[..]),
        ("DEMO-2", &["AGENTS.md".to_owned()][..])
    );
    assert!(rebase_runs(&bench).is_empty(), "{report}");
    let main = bench.merge_into_main(&[("AGENTS.md", AGENTS_PREPENDED)]);
    bench.unclaimed("DEMO-2");
    let mut pass = 0;
    let (outcomes, lines, printed) = bench.watch_during(Vec::new(), Vec::new(), &mut |_| {
        pass += 1;
        pass < 3
    });
    let report = format!("{outcomes:?}\n{lines}\n{printed}");
    assert!(outcomes.iter().all(Result::is_ok), "{report}");
    let runs = refreshes_merged(&bench);
    assert_eq!(runs.len(), 1, "{runs:?}\n{report}");
    assert_eq!(
        (&runs[0]["cause"], &runs[0]["onto"]),
        (&json!("overlap"), &json!(main)),
        "{report}"
    );
    let head = bench.remote_branch().unwrap();
    assert!(is_ancestor(&bench, &main, &head), "{report}");
    assert_eq!(recorded_from_claims(&bench).len(), 1, "{report}");
}

/// A review after a resolution: DEMO-1's own record shares `GREETING.md`
/// with DEMO-2, whose merge adds that file too, a conflict GitHub reports
/// and that is not trivial (both sides add it). The rebase run, on the
/// Build routing, resolves it; before its push, a Verify pass on the
/// rebased head, the critic told the rebase, finds the resolution drops
/// DEMO-2's line, the arbiter confirms it, and one last Build at Watch fixes
/// it, the gate passing; that commit, a rebase onto DEMO-2's merge, is
/// what is pushed, its report naming the pass.
#[test]
fn a_non_trivial_resolution_on_an_overlap_is_reviewed_before_its_push() {
    let bench = overlapping(&[], true);
    merged_sibling(&bench, &["GREETING.md"], false);
    let first_push = bench.remote_branch().unwrap();
    let main = bench.merge_into_main(&[("GREETING.md", "Hi from DEMO-2.\n")]);
    bench.github.followed.lock().unwrap().mergeable = "CONFLICTING";
    let rebase = rebase_in(
        &bench,
        main.clone(),
        vec![("GREETING.md", "Hello, reader.\n")],
    );
    let claim = "The resolution drops DEMO-2's greeting.";
    let replies = vec![
        bench.reply(None, Some(REBASED)),
        bench.reviewing(&found(&[("major", claim)])),
        bench.reviewing(&ruled(&["confirmed"])),
        bench.reply(
            Some("Hello, DEMO-2"),
            Some(&fixed_build(&[("F1", "fixed")])),
        ),
    ];
    let delivered_runs = bench.briefs().len();
    let mut pass = 0;
    let (outcomes, lines, printed) = bench.watch_during(replies, vec![rebase], &mut |_| {
        pass += 1;
        // Merged meanwhile: the pull request reads mergeable again.
        bench.github.followed.lock().unwrap().mergeable = "MERGEABLE";
        pass < 3
    });
    let report = format!("{outcomes:?}\n{lines}\n{printed}");
    let roles: Vec<Role> = bench.briefs()[delivered_runs..]
        .iter()
        .map(|brief| brief.role)
        .collect();
    assert_eq!(
        roles,
        [Role::Rebase, Role::Verify, Role::Arbiter, Role::Build],
        "{report}"
    );
    let runs = rebase_runs(&bench);
    assert_eq!(runs.len(), 1, "{runs:?}\n{report}");
    assert_eq!(
        (
            &runs[0]["cause"],
            &runs[0]["trivial"],
            &runs[0]["tier"],
            &runs[0]["review"]
        ),
        (
            &json!("conflict"),
            &json!(false),
            &json!("build"),
            &json!(true)
        )
    );
    // The critic was told the rebase; the last Build the finding to fix.
    let verify = bench.last_of(Role::Verify);
    let told = verify.rebase.as_ref().expect("the rebase reviewed");
    assert_eq!(told.conflicting, ["GREETING.md"]);
    let last = bench.last_of(Role::Build);
    let given = last.findings.as_ref().expect("the finding to fix");
    assert_eq!(given.findings[0].claim, claim);
    let pass_event = data_of(&bench.events(), EventKind::Decision, "review")
        .into_iter()
        .rfind(|data| data["review"] == "pass")
        .unwrap();
    assert_eq!(
        (
            &pass_event["resolution"],
            &pass_event["pass"],
            &pass_event["outcome"]
        ),
        (&json!(true), &json!(2), &json!("last_fix")),
        "{report}"
    );
    // Pushed once, after the review: a rebase onto DEMO-2's merge, with the
    // last fix's greeting.
    let head = bench.remote_branch().unwrap();
    assert_ne!(head, first_push);
    assert!(is_ancestor(&bench, &main, &head), "{report}");
    let pushed = bench
        .env
        .run(
            &bench.remote.bare,
            &["show", &format!("{BRANCH}:GREETING.md")],
        )
        .unwrap();
    assert_eq!(
        String::from_utf8(pushed).unwrap(),
        "Hello, DEMO-2, reader.\n"
    );
    let review = bench.record().review.unwrap();
    let kept = review.passes.last().unwrap();
    assert_eq!(
        (kept.pass.get(), kept.findings[0].outcome),
        (2, Some(owlshift_contracts::review::FindingOutcome::Fixed))
    );
    let rebased = bench
        .comments()
        .into_iter()
        .rfind(|body| body.contains("Rebased onto"))
        .unwrap();
    assert!(
        rebased.contains("Verify pass 2 reviewed the resolution"),
        "{rebased}"
    );
}

/// The bench of a review after a resolution, DEMO-1 delivered with
/// `READER.md` beside its greeting: DEMO-2's merge adds `GREETING.md` too, a
/// conflict that is not trivial, on a path the overlap names. Returns
/// DEMO-1's first push and `main`'s head.
fn a_resolution_to_review(bench: &Bench) -> (String, String) {
    merged_sibling(bench, &["GREETING.md"], false);
    let first_push = bench.remote_branch().unwrap();
    let main = bench.merge_into_main(&[("GREETING.md", "Hi from DEMO-2.\n")]);
    bench.github.followed.lock().unwrap().mergeable = "CONFLICTING";
    (first_push, main)
}

/// The agent of a rebase run that resolves `GREETING.md` but drops
/// `READER.md`, the delivery's file, before continuing: its `run`-th run.
fn rebase_dropping_the_reader(bench: &Bench, run: usize, onto: String) -> During {
    let (env, worktree) = (bench.env.clone(), bench.worktree());
    let act = move || {
        // The conflict stops the rebase: git exits 1.
        let _ = env.run(&worktree, &["rebase", "--quiet", &onto]);
        fs::write(worktree.join("GREETING.md"), "Hello, reader.\n").unwrap();
        env.run(&worktree, &["add", "--", "GREETING.md"]).unwrap();
        env.run(&worktree, &["rm", "-f", "--quiet", "--", "READER.md"])
            .unwrap();
        env.run(
            &worktree,
            &["-c", "core.editor=true", "rebase", "--continue"],
        )
        .unwrap();
    };
    (run, Box::new(act))
}

/// OWL-514, the OWL-460 shape: the delivery adds `READER.md`; the rebase
/// run resolves the conflict and keeps it; Verify asks to drop the second
/// reader, the arbiter confirms, and the last Build at Watch deletes the
/// file on purpose. The push's delivery check reads the rebase run's
/// commit, of which the pushed head descends: the push lands, nothing
/// parks, and the reader is gone from the branch.
#[test]
fn a_last_fix_that_removes_delivered_work_after_a_resolution_is_pushed() {
    let bench = overlapping(&[("READER.md", "A second reader.\n")], true);
    let (first_push, main) = a_resolution_to_review(&bench);
    let rebase = rebase_in(
        &bench,
        main.clone(),
        vec![("GREETING.md", "Hello, reader.\n")],
    );
    let (env, worktree) = (bench.env.clone(), bench.worktree());
    let drop_reader: During = (
        4,
        Box::new(move || {
            env.run(&worktree, &["rm", "--quiet", "--", "READER.md"])
                .unwrap();
            env.run(&worktree, &["commit", "--quiet", "-m", "Drop the reader"])
                .unwrap();
        }),
    );
    let claim = "Drop the second reader: main carries one now.";
    let replies = vec![
        bench.reply(None, Some(REBASED)),
        bench.reviewing(&found(&[("major", claim)])),
        bench.reviewing(&ruled(&["confirmed"])),
        bench.reply(
            Some("Hello, DEMO-2"),
            Some(&fixed_build(&[("F1", "fixed")])),
        ),
    ];
    let delivered_runs = bench.briefs().len();
    let delivered_events = bench.events().len();
    let mut pass = 0;
    let (outcomes, lines, printed) =
        bench.watch_during(replies, vec![rebase, drop_reader], &mut |_| {
            pass += 1;
            // Merged meanwhile: the pull request reads mergeable again.
            bench.github.followed.lock().unwrap().mergeable = "MERGEABLE";
            pass < 3
        });
    let report = format!("{outcomes:?}\n{lines}\n{printed}");
    let roles: Vec<Role> = bench.briefs()[delivered_runs..]
        .iter()
        .map(|brief| brief.role)
        .collect();
    assert_eq!(
        roles,
        [Role::Rebase, Role::Verify, Role::Arbiter, Role::Build],
        "{report}"
    );
    // Pushed: the last fix's commit, on top of the rebase run's, which
    // alone keeps the reader.
    let head = bench.remote_branch().unwrap();
    assert_ne!(head, first_push, "{report}");
    let events = &bench.events()[delivered_events..];
    let gate_commit = |role: &str| {
        let started = events
            .iter()
            .rfind(|e| e.kind == EventKind::RunStarted && e.data["role"] == role)
            .unwrap_or_else(|| panic!("a {role} run\n{report}"));
        events
            .iter()
            .find(|e| e.kind == EventKind::RunEnded && e.run == started.run)
            .and_then(|e| e.data["gate_commit"].as_str())
            .unwrap_or_else(|| panic!("a {role} gate commit\n{report}"))
            .to_owned()
    };
    assert_eq!(gate_commit("build"), head, "{report}");
    let rebased = gate_commit("rebase");
    assert_ne!(rebased, head);
    assert!(is_ancestor(&bench, &rebased, &head), "{report}");
    assert!(is_ancestor(&bench, &main, &head), "{report}");
    for (commit, kept) in [(&rebased, true), (&head, false)] {
        let read = bench.env.run(
            &bench.remote.bare,
            &["cat-file", "-e", &format!("{commit}:READER.md")],
        );
        assert_eq!(read.is_ok(), kept, "READER.md at {commit}\n{report}");
    }
    let given = bench.last_of(Role::Build).findings.unwrap();
    assert_eq!(given.findings[0].claim, claim);
    let delivery = bench.record().delivery.unwrap();
    assert_eq!(
        (
            delivery.pushed.as_deref(),
            delivery.rebased_onto.as_deref(),
            delivery.pushing.is_none()
        ),
        (Some(head.as_str()), Some(main.as_str()), true),
        "{report}"
    );
    let comments = bench.comments();
    assert!(
        !comments.iter().any(|c| c.starts_with("[owlshift] PARKED")),
        "{report}"
    );
    let rebased_report = comments
        .iter()
        .rfind(|body| body.contains("Rebased onto"))
        .unwrap_or_else(|| panic!("a rebase report\n{report}"));
    assert!(
        rebased_report.contains("Verify pass 2 reviewed the resolution"),
        "{rebased_report}"
    );
}

/// OWL-514's regression guard, which passes before it too: a rebase run
/// whose own commit drops `READER.md`, a path the delivery changed, is
/// refused before any review with `rebase_keeps_delivery`'s message, on
/// both attempts; the ticket parks with nothing pushed.
#[test]
fn a_rebase_run_that_drops_delivered_work_is_refused_before_any_review() {
    let bench = overlapping(&[("READER.md", "A second reader.\n")], true);
    let (first_push, main) = a_resolution_to_review(&bench);
    let replies = vec![
        bench.reply(None, Some(REBASED)),
        bench.reply(None, Some(REBASED)),
    ];
    let during = vec![
        rebase_dropping_the_reader(&bench, 1, main.clone()),
        rebase_dropping_the_reader(&bench, 2, main),
    ];
    let delivered_runs = bench.briefs().len();
    let mut pass = 0;
    let (outcomes, lines, printed) = bench.watch_during(replies, during, &mut |_| {
        pass += 1;
        pass < 2
    });
    let report = format!("{outcomes:?}\n{lines}\n{printed}");
    let roles: Vec<Role> = bench.briefs()[delivered_runs..]
        .iter()
        .map(|brief| brief.role)
        .collect();
    assert_eq!(roles, [Role::Rebase, Role::Rebase], "{report}");
    assert_eq!(bench.remote_branch().unwrap(), first_push, "{report}");
    let parked = bench
        .comments()
        .into_iter()
        .rfind(|c| c.starts_with("[owlshift] PARKED"))
        .unwrap_or_else(|| panic!("a PARKED comment\n{report}"));
    assert!(
        parked.contains("no longer changes READER.md, which the delivery changed"),
        "{parked}"
    );
}

/// `AGENTS.md` with a line DEMO-2's merge adds at its end, where DEMO-1's
/// delivery adds its own: git finds the two conflicting (OWL-655).
const AGENTS_CONFLICTING: &str =
    "Sign off every commit (`git commit -s`).\nWrite the test first.\n";

/// DEMO-1 delivered with [`AGENTS_APPENDED`], DEMO-2 merged sharing
/// `AGENTS.md`, under `policy`, and `main` moved by DEMO-2's merge of
/// `files`: the bench, its first push and the `main` it moved to.
fn refreshed_by(policy: &str, files: &[(&str, &str)]) -> (Bench, String, String) {
    let bench = overlapping_with(&[("AGENTS.md", AGENTS_APPENDED)], false, policy);
    merged_sibling(&bench, &["AGENTS.md"], true);
    let first_push = bench.remote_branch().unwrap();
    let main = bench.merge_into_main(files);
    (bench, first_push, main)
}

/// The data of the warnings `what`.
fn warned(bench: &Bench, what: &str) -> Vec<Value> {
    data_of(&bench.events(), EventKind::Warning, "what")
        .into_iter()
        .filter(|data| data["what"] == what)
        .collect()
}

/// OWL-655 (a): a refresh whose base change git finds conflicting, the
/// forge still reporting the pull request mergeable, keeps the rebase run:
/// on the fast tier, the conflicting path named, and no merge of the
/// runner's.
#[test]
fn a_refresh_git_finds_conflicting_keeps_the_rebase_run() {
    let (bench, first_push, main) = refreshed_by(
        "overlap = \"rebase\"\n",
        &[("AGENTS.md", AGENTS_CONFLICTING)],
    );
    let refresh = rebased_keeping_delivery(&bench, &main);
    let (outcomes, lines, printed) = bench.watch_during(
        vec![bench.reply(None, Some(REBASED))],
        vec![refresh],
        &mut |_| false,
    );
    let report = format!("{outcomes:?}\n{lines}\n{printed}");
    assert!(outcomes.iter().all(Result::is_ok), "{report}");
    let runs = rebase_runs(&bench);
    assert_eq!(runs.len(), 1, "{runs:?}\n{report}");
    assert_eq!(
        (
            &runs[0]["refresh"],
            &runs[0]["tier"],
            &runs[0]["conflicting"],
            &runs[0]["cause"]
        ),
        (
            &json!(true),
            &json!("fast"),
            &json!(["AGENTS.md"]),
            &json!("overlap")
        )
    );
    assert!(refreshes_merged(&bench).is_empty(), "{report}");
    let head = bench.remote_branch().unwrap();
    assert_ne!(head, first_push);
    assert!(is_ancestor(&bench, &main, &head), "{report}");
}

/// OWL-655 (b): a remote that refuses the push of the runner's merge, as a
/// rule requiring a linear history or signed commits would: the ticket
/// parks, the forge's answer quoted, and no run is started for it. The
/// refusal is played by a lock held on the branch's ref in the bare remote:
/// the bench, as the runner, runs git with hooks off, so no `pre-receive`
/// hook could refuse it.
#[test]
fn a_forge_that_refuses_the_runner_s_merge_parks_the_ticket() {
    let (bench, first_push, _) =
        refreshed_by("overlap = \"rebase\"\n", &[("AGENTS.md", AGENTS_PREPENDED)]);
    let lock = bench
        .remote
        .bare
        .join("refs/heads")
        .join(format!("{BRANCH}.lock"));
    fs::create_dir_all(lock.parent().unwrap()).unwrap();
    fs::write(&lock, "").unwrap();
    let runs = bench.briefs().len();
    let (outcomes, lines, printed) = bench.watch(Vec::new(), &mut |_| false);
    let report = format!("{outcomes:?}\n{lines}\n{printed}");
    assert_eq!(bench.briefs().len(), runs, "a run started\n{report}");
    assert_eq!(refreshes_merged(&bench).len(), 1, "{report}");
    assert_eq!(bench.remote_branch().unwrap(), first_push, "{report}");
    assert!(
        matches!(state_of(&bench).status(), Status::Parked { .. }),
        "{report}"
    );
    let parked = bench
        .comments()
        .into_iter()
        .rfind(|c| c.starts_with("[owlshift] PARKED"))
        .unwrap_or_else(|| panic!("a PARKED comment\n{report}"));
    assert!(parked.contains("[remote rejected]"), "{parked}");
}

/// OWL-655 (c): the runner's merge whose push a crash cut short is pushed
/// again with no run, and its report still says the runner merged it.
#[test]
fn a_runner_s_merge_cut_short_is_pushed_again_and_told_as_a_refresh() {
    let (bench, first_push, main) =
        refreshed_by("overlap = \"rebase\"\n", &[("AGENTS.md", AGENTS_PREPENDED)]);
    let (env, worktree) = (bench.env.clone(), bench.worktree());
    env.run(&worktree, &["fetch", "--quiet", "origin", "main"])
        .unwrap();
    env.run(
        &worktree,
        &["merge", "--quiet", "--no-ff", "--no-edit", &main],
    )
    .unwrap();
    let out = env.run(&worktree, &["rev-parse", "HEAD"]).unwrap();
    let merge = String::from_utf8(out).unwrap().trim().to_owned();
    // The merge moved the branch: its run file says so.
    let file = bench.dirs().run_file(&ticket());
    let mut run =
        owlshift_contracts::step::TicketRun::parse(&fs::read_to_string(&file).unwrap()).unwrap();
    run.tip = Some(merge.clone());
    run.generation += 1;
    fs::write(&file, run.render()).unwrap();
    let mut record = bench.record();
    record.delivery.as_mut().unwrap().pushing = Some(owlshift_contracts::refs::Pushing {
        commit: merge.clone(),
        kind: owlshift_contracts::refs::PushKind::Rebase,
        base: main.clone(),
        onto: Some(main.clone()),
        rebased: Some(merge.clone()),
        report: Some(owlshift_contracts::refs::KeptReport {
            summary: "Owlshift merged `main` into the branch itself.".to_owned(),
            gate: None,
            checks: Vec::new(),
            questions: Vec::new(),
            notes: Vec::new(),
            followups: Vec::new(),
            reviews: Vec::new(),
            steps: Vec::new(),
            refreshed: true,
        }),
    });
    bench.write_record(&record);
    bench.github.followed.lock().unwrap().mergeable = "CONFLICTING";
    let runs = bench.briefs().len();
    let (outcomes, lines, printed) = bench.watch(Vec::new(), &mut |_| false);
    let report = format!("{outcomes:?}\n{lines}\n{printed}");
    assert_eq!(bench.briefs().len(), runs, "a run started\n{report}");
    assert_ne!(merge, first_push);
    assert_eq!(bench.remote_branch().unwrap(), merge, "{report}");
    let lead = format!(
        "**Refreshed onto `main` at `{}` after delivery,**",
        &main[..7]
    );
    assert!(
        bench
            .comments()
            .iter()
            .any(|c| c.starts_with("[owlshift] DELIVERY") && c.contains(&lead)),
        "{report}"
    );
}

/// OWL-655 (d): a base that already carries a delivered path as the
/// delivery wrote it leaves the runner's merge without that change: the
/// push guard refuses it, a warning says so, and the rebase run follows,
/// dispatched once, the ticket's branch still at the first push when it
/// starts.
#[test]
fn a_merge_the_push_guard_refuses_falls_back_to_the_rebase_run() {
    let (bench, first_push, _) =
        refreshed_by("overlap = \"rebase\"\n", &[("AGENTS.md", AGENTS_APPENDED)]);
    let (env, worktree) = (bench.env.clone(), bench.worktree());
    let at_start = first_push.clone();
    let check: During = (
        1,
        Box::new(move || {
            let out = env.run(&worktree, &["rev-parse", "HEAD"]).unwrap();
            assert_eq!(String::from_utf8(out).unwrap().trim(), at_start);
        }),
    );
    let (outcomes, lines, printed) = bench.watch_during(
        vec![
            bench.reply(None, Some(NOT_REBASED)),
            bench.reply(None, Some(NOT_REBASED)),
        ],
        vec![check],
        &mut |_| false,
    );
    let report = format!("{outcomes:?}\n{lines}\n{printed}");
    let refused = warned(&bench, "refresh_merge_refused");
    assert_eq!(refused.len(), 1, "{refused:?}\n{report}");
    assert!(
        refused[0]["reason"]
            .as_str()
            .unwrap()
            .contains("no longer changes AGENTS.md"),
        "{refused:?}"
    );
    assert!(refreshes_merged(&bench).is_empty(), "{report}");
    let rebases: Vec<Value> = data_of(&bench.events(), EventKind::Dispatch, "command")
        .into_iter()
        .filter(|data| data["command"] == "rebase")
        .collect();
    assert_eq!(rebases.len(), 1, "{rebases:?}\n{report}");
    let runs = rebase_runs(&bench);
    assert!(!runs.is_empty(), "{report}");
    assert_eq!(runs[0]["refresh"], json!(true));
    assert_eq!(bench.remote_branch().unwrap(), first_push, "{report}");
}

/// OWL-655 (e): a sibling's merge that changes the shared path and sets the
/// project's gate to a failing command. The runner's merge runs no gate, so
/// the refresh is pushed and the pull request merges on green, which a
/// gate run would have stopped: the pull request's checks are its guard.
#[test]
fn a_clean_refresh_runs_no_gate_and_merges_on_green() {
    let config = |bench: &Bench| {
        fs::read_to_string(bench.remote.checkout.join("owlshift.toml"))
            .unwrap()
            .replace(GATE_LINE, "gate = [\"false\"]")
    };
    let bench = overlapping_with(
        &[("AGENTS.md", AGENTS_APPENDED)],
        false,
        "overlap = \"rebase\"\n[forge]\nmerge = \"all\"\n",
    );
    merged_sibling(&bench, &["AGENTS.md"], true);
    let first_push = bench.remote_branch().unwrap();
    let gated = config(&bench);
    assert!(gated.contains("gate = [\"false\"]"), "{gated}");
    let main = bench.merge_into_main(&[("AGENTS.md", AGENTS_PREPENDED), ("owlshift.toml", &gated)]);
    let runs = bench.briefs().len();
    let mut pass = 0;
    let (outcomes, lines, printed) = bench.watch_during(Vec::new(), Vec::new(), &mut |_| {
        pass += 1;
        two_minutes_later(&bench);
        pass < 6
    });
    let report = format!("{outcomes:?}\n{lines}\n{printed}");
    assert!(outcomes.iter().all(Result::is_ok), "{report}");
    let merge = assert_runner_merged(&bench, runs, &first_push, &main, "overlap");
    let merges = bench.github.merges.lock().unwrap().clone();
    assert_eq!(
        merges,
        [json!({ "sha": merge, "merge_method": "squash" })],
        "{report}"
    );
}

/// A second change to `AGENTS.md` after [`AGENTS_PREPENDED`], which merges
/// with DEMO-1's line cleanly too (OWL-654).
const AGENTS_PREPENDED_TWICE: &str =
    "Read the design first.\nRead the records too.\nSign off every commit (`git commit -s`).\n";

/// The project files of OWL-654's scenarios: overlaps tolerated, and
/// `[forge] merge` set to `switch`.
fn rebase_and_merge(switch: &str) -> String {
    format!("overlap = \"rebase\"\n[forge]\nmerge = \"{switch}\"\n")
}

/// The `refresh_owed` waits told (OWL-654).
fn refresh_waits(bench: &Bench) -> Vec<Value> {
    watch_waits(bench)
        .into_iter()
        .filter(|wait| wait["reason"] == "refresh_owed")
        .collect()
}

/// `owlshift why DEMO-1` now.
fn why_now(bench: &Bench) -> String {
    owlshift_runner::why::render(
        &owlshift_runner::why::gather(
            &bench.data,
            &bench.dirs(),
            REPO,
            Some(ticket()),
            2,
            &[],
            bench.clock.get(),
        )
        .unwrap(),
    )
}

/// The rebase run of a refresh, the `run`th of the command: the delivered
/// branch replayed onto the remote's `main` as it is when the run works.
fn replayed_onto_main(bench: &Bench, run: usize) -> During {
    let (env, worktree, bare) = (
        bench.env.clone(),
        bench.worktree(),
        bench.remote.bare.clone(),
    );
    (
        run,
        Box::new(move || {
            let out = env.run(&bare, &["rev-parse", "main"]).unwrap();
            let main = String::from_utf8(out).unwrap().trim().to_owned();
            env.run(&worktree, &["rebase", "--quiet", &main]).unwrap();
        }),
    )
}

/// OWL-654, the ticket's acceptance: DEMO-1, delivered under merge on
/// green, waits for its decider, a red fix run having asked
/// (`NeedsInput { return_to: Watch }`), while DEMO-2 and DEMO-3, which
/// share `AGENTS.md` with it, merge one after the other, each changing that
/// file cleanly. Watch reads DEMO-1 only for an end then, and runs no
/// refresh: it tells the owed refresh, onto each new base head, as waiting
/// for the decider's answer, and `owlshift why` shows it. Before OWL-654
/// the decider's wait ran no refresh either, watch not following a ticket
/// whose round waits; the refreshes it saved were those of each sibling
/// merge while the pull request is followed (below). After the answer,
/// exactly one refresh runs, onto `main`'s latest head, and the rebased
/// head merges on green after two green reads.
#[test]
fn two_siblings_merging_while_the_decider_is_asked_get_one_refresh_after_the_answer() {
    let bench = overlapping_with(
        &[("AGENTS.md", AGENTS_APPENDED)],
        false,
        &rebase_and_merge("all"),
    );
    asked_at_watch(&bench);
    merged_sibling_as(&bench, "DEMO-2", &["AGENTS.md"], true);
    merged_sibling_as(&bench, "DEMO-3", &["AGENTS.md"], true);
    bench.github.followed.lock().unwrap().conclusion = "SUCCESS";
    let first = bench.merge_into_main(&[("AGENTS.md", AGENTS_PREPENDED)]);
    let (mut second, mut told, mut shown, mut status) = (None, Vec::new(), String::new(), None);
    let mut pass = 0;
    let (outcomes, lines, printed) = bench.watch_during(
        vec![
            bench.reply(
                None,
                Some(&check(&[("Q1", "answered", "The variable is set.")])),
            ),
            bench.reply(None, Some(REBASED)),
        ],
        vec![replayed_onto_main(&bench, 2)],
        &mut |_| {
            pass += 1;
            match pass {
                1 => second = Some(bench.merge_into_main(&[("AGENTS.md", AGENTS_PREPENDED_TWICE)])),
                2 => {
                    told = refresh_waits(&bench);
                    shown = why_now(&bench);
                    status = Some(state_of(&bench).status());
                    assert!(rebase_runs(&bench).is_empty());
                    bench.answer("Q1: Set, and the check re-run. Go.\n");
                }
                _ => later(&bench.clock),
            }
            pass < 12 && merged_comments(&bench).is_empty()
        },
    );
    let report = format!("{outcomes:?}\n{lines}\n{printed}");
    let second = second.unwrap();
    assert_eq!(
        status,
        Some(Status::NeedsInput {
            return_to: Stage::Watch
        }),
        "{report}"
    );
    // Told once per base head, waiting for the decider's answer.
    let onto: Vec<&Value> = told.iter().map(|wait| &wait["onto"]).collect();
    assert_eq!(onto, [&json!(first), &json!(second)], "{told:?}\n{report}");
    for wait in &told {
        assert_eq!(wait["until"], "question", "{report}");
    }
    let short = &second[..7];
    assert!(
        shown.contains(&format!(
            "pull request #1 owes a refresh rebase onto `{short}`, whose base changed a path it \
             shares with another ticket: it waits until its decider answers its question, then \
             rebases once onto the base head of that moment"
        )),
        "{shown}\n{report}"
    );
    let runs = rebase_runs(&bench);
    assert_eq!(runs.len(), 1, "{runs:?}\n{report}");
    assert_eq!(
        (&runs[0]["cause"], &runs[0]["onto"]),
        (&json!("overlap"), &json!(second)),
        "{report}"
    );
    let head = bench.remote_branch().unwrap();
    assert!(is_ancestor(&bench, &second, &head), "{report}");
    let merges = github_merges(&bench);
    assert_eq!(merges.len(), 1, "{merges:?}\n{report}");
    assert_eq!(merges[0]["sha"], json!(head), "{report}");
    assert_eq!(merged_comments(&bench).len(), 1, "{report}");
}

/// OWL-654, the saving the ticket measured: DEMO-1, delivered under merge
/// on green, is followed at Watch while its checks run, and DEMO-2 then
/// DEMO-3, which share `AGENTS.md` with it, merge one after the other,
/// each changing that file cleanly. Before OWL-654 each merge ran its own
/// refresh at the next read; now no rebase runs while the checks run, the
/// owed refresh told onto each new base head as waiting for them. Once
/// they read green, exactly one refresh runs, onto `main`'s latest head,
/// and the rebased head merges on green.
#[test]
fn two_siblings_merging_while_the_checks_run_get_one_refresh_onto_the_latest_base() {
    let bench = overlapping_with(
        &[("AGENTS.md", AGENTS_APPENDED)],
        false,
        &rebase_and_merge("all"),
    );
    merged_sibling_as(&bench, "DEMO-2", &["AGENTS.md"], true);
    merged_sibling_as(&bench, "DEMO-3", &["AGENTS.md"], true);
    bench.github.followed.lock().unwrap().conclusion = "SUCCESS";
    *bench.github.pending_follow_reads.lock().unwrap() = 0..u32::MAX;
    let first = bench.merge_into_main(&[("AGENTS.md", AGENTS_PREPENDED)]);
    let (mut second, mut held) = (None, None);
    let mut pass = 0;
    let (outcomes, lines, printed) = bench.watch_during(
        vec![bench.reply(None, Some(REBASED))],
        vec![replayed_onto_main(&bench, 1)],
        &mut |_| {
            pass += 1;
            match pass {
                1 => second = Some(bench.merge_into_main(&[("AGENTS.md", AGENTS_PREPENDED_TWICE)])),
                3 => {
                    held = Some((rebase_runs(&bench).len(), refresh_waits(&bench)));
                    *bench.github.pending_follow_reads.lock().unwrap() = 0..0;
                }
                _ => {}
            }
            later(&bench.clock);
            pass < 12 && merged_comments(&bench).is_empty()
        },
    );
    let report = format!("{outcomes:?}\n{lines}\n{printed}");
    let second = second.unwrap();
    let (ran, told) = held.unwrap();
    assert_eq!(ran, 0, "{report}");
    // Told once per base head, waiting for the checks.
    let onto: Vec<&Value> = told.iter().map(|wait| &wait["onto"]).collect();
    assert_eq!(onto, [&json!(first), &json!(second)], "{told:?}\n{report}");
    for wait in &told {
        assert_eq!(wait["until"], "checks", "{report}");
    }
    let runs = rebase_runs(&bench);
    assert_eq!(runs.len(), 1, "{runs:?}\n{report}");
    assert_eq!(
        (&runs[0]["cause"], &runs[0]["onto"]),
        (&json!("overlap"), &json!(second)),
        "{report}"
    );
    let head = bench.remote_branch().unwrap();
    assert!(is_ancestor(&bench, &second, &head), "{report}");
    let merges = github_merges(&bench);
    assert_eq!(merges.len(), 1, "{merges:?}\n{report}");
    assert_eq!(merges[0]["sha"], json!(head), "{report}");
}

/// OWL-654, decided Q1 and Q2: under `[forge] merge = "never"` a refresh
/// owed while the pull request's checks still run waits for them, told as
/// such; once they read green, one refresh runs onto the sibling's head. A
/// later sibling merge on the shared path, the pull request waiting for the
/// person, makes it owed again: a second refresh. Nothing is merged.
#[test]
fn under_never_an_owed_refresh_waits_for_the_checks_and_is_owed_again_after_a_later_sibling() {
    let bench = overlapping(&[("AGENTS.md", AGENTS_APPENDED)], false);
    merged_sibling(&bench, &["AGENTS.md"], true);
    *bench.github.pending_follow_reads.lock().unwrap() = 0..u32::MAX;
    let first = bench.merge_into_main(&[("AGENTS.md", AGENTS_PREPENDED)]);
    let (mut held, mut second) = (None, None);
    let mut pass = 0;
    let (outcomes, lines, printed) = bench.watch_during(
        vec![
            bench.reply(None, Some(REBASED)),
            bench.reply(None, Some(REBASED)),
        ],
        vec![replayed_onto_main(&bench, 1), replayed_onto_main(&bench, 2)],
        &mut |_| {
            pass += 1;
            match pass {
                2 => {
                    held = Some((rebase_runs(&bench).len(), refresh_waits(&bench)));
                    *bench.github.pending_follow_reads.lock().unwrap() = 0..0;
                }
                // The refresh ran and its head was read green.
                5 => second = Some(bench.merge_into_main(&[("AGENTS.md", AGENTS_PREPENDED_TWICE)])),
                _ => {}
            }
            later(&bench.clock);
            pass < 9
        },
    );
    let report = format!("{outcomes:?}\n{lines}\n{printed}");
    let (ran, told) = held.unwrap();
    assert_eq!(ran, 0, "{report}");
    assert_eq!(told.len(), 1, "{told:?}\n{report}");
    assert_eq!(
        (&told[0]["onto"], &told[0]["until"]),
        (&json!(first), &json!("checks")),
        "{report}"
    );
    let runs = rebase_runs(&bench);
    let onto: Vec<(&Value, &Value)> = runs
        .iter()
        .map(|run| (&run["cause"], &run["onto"]))
        .collect();
    let second = second.unwrap();
    assert_eq!(
        onto,
        [
            (&json!("overlap"), &json!(first)),
            (&json!("overlap"), &json!(second))
        ],
        "{report}"
    );
    assert!(github_merges(&bench).is_empty(), "{report}");
}

/// OWL-654, decided Q1: under `[forge] merge = "labeled"`, the ticket
/// without the label, a sibling's merge on the shared path gets one refresh
/// once the pull request reads green, then nothing more: no merge, and
/// `continue` says the next pass would not merge it for the label.
#[test]
fn labeled_without_its_label_gets_one_refresh_once_ready() {
    let bench = overlapping_with(
        &[("AGENTS.md", AGENTS_APPENDED)],
        false,
        &rebase_and_merge("labeled"),
    );
    merged_sibling(&bench, &["AGENTS.md"], true);
    let main = bench.merge_into_main(&[("AGENTS.md", AGENTS_PREPENDED)]);
    let mut pass = 0;
    let (outcomes, lines, printed) = bench.watch_during(
        vec![bench.reply(None, Some(REBASED))],
        vec![replayed_onto_main(&bench, 1)],
        &mut |_| {
            pass += 1;
            later(&bench.clock);
            pass < 5
        },
    );
    let report = format!("{outcomes:?}\n{lines}\n{printed}");
    let runs = rebase_runs(&bench);
    assert_eq!(runs.len(), 1, "{runs:?}\n{report}");
    assert_eq!(
        (&runs[0]["cause"], &runs[0]["onto"]),
        (&json!("overlap"), &json!(main)),
        "{report}"
    );
    let read_green_first = data_of(&bench.events(), EventKind::Decision, "watch")
        .into_iter()
        .take_while(|data| data["watch"] != "rebase_run")
        .any(|data| data["verdict"] == json!("green"));
    assert!(read_green_first, "{report}");
    assert!(github_merges(&bench).is_empty(), "{report}");
}

/// OWL-654: `owlshift continue` on a green pull request that owes a refresh
/// runs none: its merge note says the refresh comes first, and watch's next
/// pass runs it.
#[test]
fn continue_names_an_owed_refresh_and_runs_none() {
    let bench = overlapping_with(
        &[("AGENTS.md", AGENTS_APPENDED)],
        false,
        &rebase_and_merge("all"),
    );
    merged_sibling(&bench, &["AGENTS.md"], true);
    bench.merge_into_main(&[("AGENTS.md", AGENTS_PREPENDED)]);
    let (outcome, printed) = bench.continue_ticket(Vec::new());
    let report = format!("{outcome:?}\n{printed}");
    assert!(
        matches!(&outcome, Err(Stop::Watched { merge: Some(note), .. })
            if note.contains("a refresh rebase onto the base comes first")),
        "{report}"
    );
    assert!(rebase_runs(&bench).is_empty(), "{report}");
}

/// OWL-654: a red read after a sibling's merge on the shared path gets its
/// fix run as before, with no refresh first. Its push records the base it
/// went onto, which holds the sibling's change, so no refresh is owed any
/// more, and under merge on green the fixed head merges once read green
/// twice.
#[test]
fn a_fix_push_onto_a_base_holding_the_siblings_change_clears_the_owed_refresh() {
    let bench = overlapping_with(
        &[("AGENTS.md", AGENTS_APPENDED)],
        false,
        &rebase_and_merge("all"),
    );
    merged_sibling(&bench, &["AGENTS.md"], true);
    let main = bench.merge_into_main(&[("AGENTS.md", AGENTS_PREPENDED)]);
    bench.github.followed.lock().unwrap().conclusion = "FAILURE";
    let github = bench.github.clone();
    let mut pass = 0;
    let (outcomes, lines, printed) = bench.watch(
        vec![bench.reply(Some("Hello again"), Some(FIXED))],
        &mut |_| {
            pass += 1;
            if pass == 1 {
                github.followed.lock().unwrap().conclusion = "SUCCESS";
            }
            later(&bench.clock);
            pass < 6 && github_merges(&bench).is_empty()
        },
    );
    let report = format!("{outcomes:?}\n{lines}\n{printed}");
    let roles: Vec<Role> = bench.briefs().iter().map(|brief| brief.role).collect();
    assert_eq!(
        roles.iter().filter(|role| **role == Role::Build).count(),
        2,
        "{roles:?}\n{report}"
    );
    assert!(rebase_runs(&bench).is_empty(), "{report}");
    let delivery = bench.record().delivery.unwrap();
    assert_eq!(
        delivery.base_at_push.as_deref(),
        Some(main.as_str()),
        "{report}"
    );
    let merges = github_merges(&bench);
    assert_eq!(merges.len(), 1, "{merges:?}\n{report}");
    assert_eq!(
        merges[0]["sha"],
        json!(bench.remote_branch().unwrap()),
        "{report}"
    );
}
