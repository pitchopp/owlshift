//! Scenario S5 at Watch under `policy.overlap = "rebase"` (OWL-449;
//! architecture section 5, "Tolerated overlap", "The refresh" and "A review
//! after a resolution"), on the bench of `on_demand.rs`: DEMO-1 is
//! delivered, its pull request followed, and DEMO-2, the ticket it shares a
//! file with, was merged; the fake GitHub follows DEMO-1's pull request,
//! and DEMO-2's merge is a commit on the remote's `main`.

use owlshift_contracts::refs::{Ended, EndedWhy, TicketHeld};

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
    let bench = Bench::with(
        "maintainer",
        "assignee = \"maintainer\"\n",
        "overlap = \"rebase\"\n",
    );
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
    let env = bench.env.clone();
    let git = Git::with_setup("git", move |command| env.apply(command));
    let (demo1, demo2) = (ticket(), TicketId::new("DEMO-2").unwrap());
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
        held.record_overlap(&demo1, &paths, at);
    }
    let sibling = TicketRecord {
        delivery: Some(delivery),
        held: Some(held),
        ..record.clone()
    };
    write_through_run_file(&git, &bench.dirs(), &demo2, &sibling, None);
    if !theirs {
        let mut own = record.held.clone().unwrap();
        own.record_overlap(&demo2, &paths, at);
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

/// The rebase run of a refresh: the delivered branch replayed onto `onto`,
/// with no conflict.
fn replayed_onto(bench: &Bench, onto: String) -> During {
    let (env, worktree) = (bench.env.clone(), bench.worktree());
    (
        1,
        Box::new(move || {
            env.run(&worktree, &["rebase", "--quiet", &onto]).unwrap();
        }),
    )
}

/// The refresh: DEMO-2, merged, shared `AGENTS.md` with DEMO-1, by DEMO-2's
/// record. Its merge changes that file without a conflict, which GitHub
/// reports mergeable: DEMO-1's pull request gets one refresh rebase on the
/// fast tier, its cause the overlap, no Verify pass after it, and its
/// rebased head, pushed, is the one GitHub's checks are read on again. A
/// later move of the base on another path refreshes nothing.
#[test]
fn a_siblings_merge_on_a_shared_file_refreshes_the_pull_request() {
    let bench = overlapping(&[("AGENTS.md", AGENTS_APPENDED)], false);
    merged_sibling(&bench, &["AGENTS.md"], true);
    let first_push = bench.remote_branch().unwrap();
    let main = bench.merge_into_main(&[("AGENTS.md", AGENTS_PREPENDED)]);
    let refresh = replayed_onto(&bench, main.clone());
    let mut pass = 0;
    let (outcomes, lines, printed) = bench.watch_during(
        vec![bench.reply(None, Some(REBASED))],
        vec![refresh],
        &mut |_| {
            pass += 1;
            if pass == 3 {
                bench.merge_into_main(&[("OTHER.md", "Another ticket's file.\n")]);
            }
            pass < 6
        },
    );
    let report = format!("{outcomes:?}\n{lines}\n{printed}");
    assert!(outcomes.iter().all(Result::is_ok), "{report}");
    let runs = rebase_runs(&bench);
    assert_eq!(runs.len(), 1, "{runs:?}\n{report}");
    assert_eq!(
        (
            &runs[0]["cause"],
            &runs[0]["refresh"],
            &runs[0]["trivial"],
            &runs[0]["tier"],
            &runs[0]["review"],
            &runs[0]["onto"]
        ),
        (
            &json!("overlap"),
            &json!(true),
            &json!(true),
            &json!("fast"),
            &json!(false),
            &json!(main)
        )
    );
    let head = bench.remote_branch().unwrap();
    assert_ne!(head, first_push);
    assert!(is_ancestor(&bench, &main, &head), "{report}");
    // GitHub's checks are read again on the rebased head.
    let read_at_head = data_of(&bench.events(), EventKind::Decision, "watch")
        .into_iter()
        .any(|data| data["head"] == json!(head) && data["verdict"] == json!("green"));
    assert!(read_at_head, "{report}");
    let roles: Vec<Role> = bench.briefs().iter().map(|brief| brief.role).collect();
    assert_eq!(roles.last(), Some(&Role::Rebase), "{report}");
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
