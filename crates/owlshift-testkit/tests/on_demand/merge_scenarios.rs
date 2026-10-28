//! Scenario S18, merge on green (D14, OWL-445; architecture section 5,
//! "Merge on green"), on the bench of `on_demand.rs`: the Markdown tracker,
//! and the real GitHub adapter over its fake GitHub, which answers the base
//! branch's head, the last read before a merge and the merge call. Under
//! `never`, `watch_follows_a_delivered_pull_request_until_it_is_merged`
//! shows that nothing is read for a merge and nothing is merged.

use owlshift_contracts::refs::{EndedWhy, TicketQuestions};

use super::*;

/// A delivered ticket of a project whose `[forge] merge` is `switch`, read
/// at the base branch's head, as the remote holds it from the seed.
fn merging(switch: &str) -> Bench {
    deliver(Bench::with(
        "maintainer",
        "assignee = \"maintainer\"\n",
        &format!("[forge]\nmerge = \"{switch}\"\n"),
    ))
}

/// Two minutes later on the bench's clock: a green read now is at least a
/// minute after one made before.
fn later(clock: &Cell<Timestamp>) {
    clock.set(
        clock
            .get()
            .checked_add(SignedDuration::from_mins(2))
            .unwrap(),
    );
}

/// The MERGED comments on the ticket.
fn merged_comments(bench: &Bench) -> Vec<String> {
    bench
        .comments()
        .into_iter()
        .filter(|body| body.starts_with("[owlshift] MERGED"))
        .collect()
}

/// The data of the `pr_merged` events.
fn merged_events(bench: &Bench) -> Vec<Value> {
    data_of(&bench.events(), EventKind::PrMerged, "watch")
}

/// The `fact` of each `merge_facts_unread` warning.
fn unread_facts(bench: &Bench) -> Vec<Value> {
    data_of(&bench.events(), EventKind::Warning, "fact")
        .into_iter()
        .filter(|data| data["what"] == "merge_facts_unread")
        .map(|data| data["fact"].clone())
        .collect()
}

/// The ticket's following ended merged, with no merge under way left.
fn assert_ended_merged(bench: &Bench) {
    let delivery = bench.record().delivery.unwrap();
    assert_eq!(
        delivery.ended.as_ref().map(|ended| ended.why),
        Some(EndedWhy::Merged)
    );
    assert!(delivery.merging.is_none(), "{delivery:?}");
}

/// S18 under `labeled`: no merge without the label, none while its labels
/// do not read (warned of once), none while a question of the ticket was
/// asked under a category the project keeps for a person; then, its label
/// on and that question gone, one merge at the head Owlshift pushed, by the
/// project's method, told by a MERGED comment and a `pr_merged` event, and
/// no notification.
#[test]
fn labeled_merges_once_its_label_is_on_and_no_question_is_kept_for_a_person() {
    let bench = merging("labeled");
    let pushed = bench.remote_branch().unwrap();
    // The DELIVERY report says who merges.
    let report = bench.comments().into_iter().next().unwrap();
    assert!(
        report.contains(
            "Owlshift merges [#1](https://github.com/demo/project/pull/1) once it is green only \
             if the ticket carries the label `owlshift:merge` (merge on green); otherwise \
             review and merge it."
        ),
        "{report}"
    );
    // A question asked under `billing`, a category the project sends to a
    // person.
    let config = bench.remote.checkout.join("owlshift.toml");
    let text = fs::read_to_string(&config).unwrap();
    fs::write(
        &config,
        text.replacen("always_human = []", "always_human = [\"billing\"]", 1),
    )
    .unwrap();
    let mut asked = bench.record();
    let plain = asked.clone();
    asked.questions = TicketQuestions::parse(
        r#"{"format":17,"asks":[{"kind":"questions","round":1,"at":"2026-10-02T09:00:00Z",
        "comment":"c1","decider":{"account":"maintainer","by":"assignee"},"questions":[
        {"id":"Q1","category":"billing","context":"The plan charges for it.",
        "text":"Who pays for the greeting?","options":[]}]}]}"#,
    )
    .unwrap();

    let github = bench.github.clone();
    let clock = bench.clock.clone();
    let file = bench.ticket_file();
    let ticket_text = fs::read_to_string(&file).unwrap();
    let mut pass = 0;
    let (_, lines, printed) = bench.watch(Vec::new(), &mut |_| {
        pass += 1;
        later(&clock);
        let reads = *github.merge_reads.lock().unwrap();
        match pass {
            // Unlabeled at passes 1 and 2: nothing read for a merge.
            2 => {
                assert_eq!(reads, 0);
                fs::write(&file, "not a ticket\n").unwrap();
            }
            // The labels did not read at passes 3 and 4.
            4 => {
                let labeled =
                    ticket_text.replacen("\n+++\n", "\nlabels = [\"owlshift:merge\"]\n+++\n", 1);
                fs::write(&file, labeled).unwrap();
                bench.write_record(&asked);
            }
            // Labeled, but a question was asked under `billing`.
            5 => {
                assert_eq!(reads, 0);
                bench.write_record(&plain);
            }
            _ => {}
        }
        pass < 7
    });
    let merges = github.merges.lock().unwrap().clone();
    assert_eq!(
        merges,
        [json!({ "sha": pushed, "merge_method": "squash" })],
        "{lines}\n{printed}"
    );
    // The read that said it was not a draft, then the last read.
    assert_eq!(*github.merge_reads.lock().unwrap(), 2);
    assert_eq!(unread_facts(&bench), [json!("label")]);
    let merged = merged_comments(&bench);
    assert_eq!(merged.len(), 1, "{merged:?}");
    assert!(
        merged[0].contains(&format!(
            "**Pull request:** [#1](https://github.com/demo/project/pull/1), head `{}`",
            &pushed[..7]
        )) && merged[0].contains("by `squash`")
            && merged[0].contains(&format!("**Merge commit:** `{}`", &MERGE_COMMIT[..7])),
        "{}",
        merged[0]
    );
    let events = merged_events(&bench);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(
        (
            &events[0]["head"],
            &events[0]["method"],
            &events[0]["commit"]
        ),
        (&json!(pushed), &json!("squash"), &json!(MERGE_COMMIT))
    );
    assert_ended_merged(&bench);
    assert!(bench.notified.0.borrow().is_empty());
    assert!(lines.contains("merged by Owlshift (squash)"), "{lines}");
}

/// The rebase run of a refresh: the delivered branch replayed onto `onto`,
/// with no conflict.
fn refresh_in(bench: &Bench, onto: String) -> During {
    let (env, worktree) = (bench.env.clone(), bench.worktree());
    (
        1,
        Box::new(move || {
            env.run(&worktree, &["rebase", "--quiet", &onto]).unwrap();
        }),
    )
}

/// S18, a base that moved since the push: no merge, a refresh rebase run
/// instead, and a merge only once the rebased head reads green twice.
#[test]
fn a_moved_base_gets_a_refresh_rebase_then_two_green_reads_before_the_merge() {
    refresh_then_merge(&merging("all"));
}

/// LOC-1454 on the bench (OWL-457): the same moved base while two other
/// tickets' runs are in flight throughout, their obligations standing and
/// their locks held, beside a third ticket's entry whose folder is gone. The
/// rebase runs and the merge follows with no one waiting for them to end,
/// and the third ticket's entry, which a run in flight's check reads, is
/// left as it was: no `worktree prune` ran.
#[test]
fn a_moved_base_is_rebased_and_merged_while_other_runs_are_in_flight() {
    let bench = merging("all");
    let dirs = bench.dirs();
    let in_flight: Vec<_> = [
        ("DEMO-2", "20261025T210000Z"),
        ("DEMO-3", "20261025T210100Z"),
    ]
    .into_iter()
    .map(|(id, run)| {
        let id = TicketId::new(id).unwrap();
        let lock = dirs.ticket_lock(&id).unwrap().expect("its lock is free");
        owlshift_runner::ticket_run::write_obligation(
            &dirs,
            Some(&id),
            run,
            &format!("Run {run} of {id}"),
        )
        .unwrap();
        lock
    })
    .collect();
    let checkout = dirs.checkout();
    let stale = bench.tmp.path().join("stale");
    let stale_path = stale.to_str().unwrap();
    let add = [
        "worktree",
        "add",
        "--quiet",
        "-b",
        "owlshift/demo-4",
        stale_path,
    ];
    bench.env.run(&checkout, &add).unwrap();
    fs::remove_dir_all(&stale).unwrap();
    let entry = checkout.join(".git/worktrees/stale");
    let kept = fs::read(entry.join("gitdir")).unwrap();

    refresh_then_merge(&bench);
    assert!(dirs.obligations_stand());
    assert_eq!(fs::read(entry.join("gitdir")).unwrap(), kept, "pruned");
    drop(in_flight);
}

/// A moved base's refresh rebase run, then the merge once the rebased head
/// read green twice, played by `watch` on `bench`, a delivered ticket of a
/// project whose `[forge] merge` is `all`.
fn refresh_then_merge(bench: &Bench) {
    let first_push = bench.remote_branch().unwrap();
    let main = bench.merge_into_main(&[("OTHER.md", "Another ticket's file.\n")]);
    let refresh = refresh_in(bench, main.clone());
    let github = bench.github.clone();
    let clock = bench.clock.clone();
    let mut pushed_at: Option<u32> = None;
    let mut pass = 0;
    let (outcomes, lines, printed) = bench.watch_during(
        vec![bench.reply(None, Some(REBASED))],
        vec![refresh],
        &mut |_| {
            pass += 1;
            later(&clock);
            let merges = github.merges.lock().unwrap().len();
            if pushed_at.is_none() && bench.remote_branch().as_deref() != Some(&first_push) {
                pushed_at = Some(pass);
                assert_eq!(merges, 0, "merged before the rebased head read green twice");
            }
            pass < 6
        },
    );
    assert!(
        outcomes.iter().all(Result::is_ok),
        "{outcomes:?}\n{lines}\n{printed}"
    );
    let rebased = data_of(&bench.events(), EventKind::Decision, "refresh");
    assert_eq!(rebased.len(), 1, "{rebased:?}");
    assert_eq!(
        (&rebased[0]["refresh"], &rebased[0]["onto"]),
        (&json!(true), &json!(main))
    );
    let head = bench.remote_branch().unwrap();
    assert!(is_ancestor(bench, &main, &head));
    assert!(pushed_at.is_some(), "{lines}");
    let merges = github.merges.lock().unwrap().clone();
    assert_eq!(merges, [json!({ "sha": head, "merge_method": "squash" })]);
    assert_eq!(merged_comments(bench).len(), 1);
    assert_ended_merged(bench);
}

/// S18's refusals: one that may pass is tried again at the next pass; one
/// that will not is notified once and not tried again on that head until a
/// person's `continue` asks (which never merges and says what watch would
/// do); a review the last read finds required is notified once and lets
/// the merge go on once approved on the same head; a merge with no verdict
/// that GitHub made is told once by the next read.
#[test]
fn refusals_are_retried_notified_once_or_held_and_a_merge_with_no_verdict_is_told_once() {
    let bench = merging("all");
    let pushed = bench.remote_branch().unwrap();
    // `continue` at Watch never merges, keeps a person's asking for one,
    // and says what watch's next pass would do.
    let (outcome, printed) = bench.continue_ticket(Vec::new());
    let said = match &outcome {
        Err(stop @ Stop::Watched { .. }) => stop.to_string(),
        other => panic!("{other:?}\n{printed}"),
    };
    assert!(
        said.ends_with(
            "`continue` never merges: the next pass of `owlshift watch` would merge it, once \
             its read is green again (merge on green)."
        ),
        "{said}"
    );
    assert!(github_merges(&bench).is_empty());
    let asked = bench.record().delivery.unwrap().merge_retry;
    assert!(asked.is_some());

    let final_refusal = "Merge commits are not allowed on this repository.";
    bench.github.merge_answers.lock().unwrap().extend([
        MergeAnswer::Refused(
            405,
            "Base branch was modified. Review and try the merge again.",
        ),
        MergeAnswer::Refused(405, final_refusal),
        MergeAnswer::MergedUnanswered,
    ]);
    let github = bench.github.clone();
    let clock = bench.clock.clone();
    let waited = bench.waited.get();
    let mut pass = 0;
    let (_, lines, printed) = bench.watch(Vec::new(), &mut |_| {
        pass += 1;
        later(&clock);
        let merges = github.merges.lock().unwrap().len();
        match pass {
            // Green once, then a refusal that may pass, then one that will
            // not, then nothing tried at pass 4.
            4 => {
                assert_eq!(merges, 2);
                // As a person's `continue` asks again, on the clock watch
                // reads.
                let mut record = bench.record();
                record.delivery.as_mut().unwrap().merge_retry =
                    Some(clock.get().checked_add(waited).unwrap());
                bench.write_record(&record);
                github.followed.lock().unwrap().review = Some("REVIEW_REQUIRED");
            }
            // The last read found a review required, twice: no call.
            6 => {
                assert_eq!(merges, 2);
                github.followed.lock().unwrap().review = Some("APPROVED");
            }
            _ => {}
        }
        pass < 9
    });
    let merges = github_merges(&bench);
    assert_eq!(merges.len(), 3, "{lines}\n{printed}");
    assert!(
        merges
            .iter()
            .all(|body| *body == json!({ "sha": pushed, "merge_method": "squash" }))
    );
    let refused: Vec<(Value, Value)> = data_of(&bench.events(), EventKind::Decision, "watch")
        .into_iter()
        .filter(|data| data["watch"] == "merge_refused")
        .map(|data| (data["final"].clone(), data["status"].clone()))
        .collect();
    assert_eq!(
        refused,
        [
            (json!(false), json!(405)),
            (json!(true), json!(405)),
            (json!(true), Value::Null),
        ]
    );
    let notified = bench.notified.0.borrow().clone();
    assert_eq!(notified.len(), 2, "{notified:?}");
    assert!(
        notified[0].starts_with(&format!(
            "DEMO-1: GitHub refused to merge pull request #1: {final_refusal}"
        )),
        "{notified:?}"
    );
    assert!(notified[1].contains("a review required"), "{notified:?}");
    let unanswered = data_of(&bench.events(), EventKind::Warning, "what")
        .into_iter()
        .filter(|data| data["what"] == "merge_unanswered")
        .count();
    assert_eq!(unanswered, 1);
    // Told once, by the read that found it merged.
    assert_eq!(merged_comments(&bench).len(), 1);
    let events = merged_events(&bench);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].get("commit"), None);
    assert_ended_merged(&bench);
}

/// The merge calls the fake GitHub received.
fn github_merges(bench: &Bench) -> Vec<Value> {
    bench.github.merges.lock().unwrap().clone()
}

/// S18, a crash between GitHub's merge and the ref write, played by the
/// ticket ref's lock held from the call on: the MERGED comment is posted
/// with the merge, the end is kept by the next pass, which finds it posted,
/// so it is told exactly once.
#[test]
fn a_crash_between_the_merge_and_the_ref_write_still_tells_one_merged() {
    let bench = merging("all");
    let lock = bench.ref_lock();
    let held = lock.clone();
    *bench.github.during_merge.lock().unwrap() = Some(Box::new(move || {
        fs::create_dir_all(held.parent().unwrap()).unwrap();
        fs::write(&held, "").unwrap();
    }));
    let github = bench.github.clone();
    let clock = bench.clock.clone();
    let mut pass = 0;
    let (_, lines, printed) = bench.watch(Vec::new(), &mut |_| {
        pass += 1;
        later(&clock);
        if pass == 2 {
            assert_eq!(github.merges.lock().unwrap().len(), 1);
            assert!(bench.record().delivery.unwrap().merging.is_some());
            fs::remove_file(&lock).unwrap();
        }
        pass < 4
    });
    assert_eq!(github_merges(&bench).len(), 1, "{lines}\n{printed}");
    assert_eq!(merged_comments(&bench).len(), 1, "{lines}\n{printed}");
    assert_eq!(merged_events(&bench).len(), 1);
    assert_ended_merged(&bench);
}
