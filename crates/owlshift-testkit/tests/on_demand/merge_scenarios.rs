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
/// do not read (warned of once), none while a question of the ticket under a
/// category the project keeps for a person has no answer; then, its label
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
    // A base that did not move is never read for its own checks (OWL-521).
    assert_eq!(*github.base_reads.lock().unwrap(), 0);
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
    // The merge notifies nothing; each hold only a person lifts was told
    // once at its first green read (OWL-464): the label missing, then the
    // `billing` question.
    let notified = bench.notified.0.borrow().clone();
    assert_eq!(notified.len(), 2, "{notified:?}");
    assert!(
        notified[0].contains("the ticket does not carry the label `owlshift:merge`")
            && notified[1].contains("question Q1 of round 1 under `billing`"),
        "{notified:?}"
    );
    assert_eq!(
        waits_for_a_person(&bench),
        [json!("unlabeled"), json!("unanswered")]
    );
    assert!(lines.contains("merged by Owlshift (squash)"), "{lines}");
}

/// The `blocker` of each `merge_waits` event (OWL-464).
fn waits_for_a_person(bench: &Bench) -> Vec<Value> {
    data_of(&bench.events(), EventKind::Decision, "watch")
        .into_iter()
        .filter(|data| data["watch"] == "merge_waits")
        .map(|data| data["blocker"].clone())
        .collect()
}

/// A Build round asking one question under `security`.
const SECURITY_ROUND: &str = r#"{"format":14,"status":"questions",
"summary":"Who may read the greeting is not given.",
"questions":[
  {"id":"Q1","category":"security","context":"The greeting file is public.",
   "text":"May anyone read the greeting?","options":[{"label":"Yes"},{"label":"No"}],
   "recommendation":{"option":"A","why":"It holds no secret."}}]}"#;

/// OWL-464: a ticket whose round asked a question under `security`, which
/// its decider answered, is delivered saying that Owlshift will not merge
/// it, naming the question, in the DELIVERY report and the delivery line;
/// its first green read is told once, an event, a line and a notification,
/// and no later pass tells it again or merges it.
#[test]
fn a_question_kept_for_a_person_is_said_at_delivery_and_once_at_watch() {
    let bench = Bench::with(
        "maintainer",
        "assignee = \"maintainer\"\n",
        "[forge]\nmerge = \"all\"\n",
    );
    let (asked, printed) = bench.run(vec![bench.reply(None, Some(SECURITY_ROUND))], None);
    assert!(
        matches!(&asked, Err(Stop::NeedsInput { posted: Ok(round), .. }) if round.get() == 1),
        "{asked:?}\n{printed}"
    );
    bench.answer("Q1: A");
    let answered = check(&[("Q1", "answered", "A, anyone.")]);
    let (delivered, printed) = bench.continue_ticket(vec![
        bench.reply(None, Some(&answered)),
        bench.reply(Some("Hello"), Some(DONE)),
    ]);
    let line = delivered.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let held = "question Q1 of round 1 was asked under `security`, and a person merges a security \
                question whatever the answer. Review and merge it yourself.";
    assert!(
        line.to_string()
            .ends_with(&format!("Owlshift will not merge it: {held}")),
        "{line}"
    );
    let report = bench
        .comments()
        .into_iter()
        .rfind(|body| body.starts_with("[owlshift] DELIVERY"))
        .unwrap();
    assert!(
        report.contains(&format!(
            "Owlshift will not merge [#1](https://github.com/demo/project/pull/1): {held}"
        )),
        "{report}"
    );

    let clock = bench.clock.clone();
    let mut pass = 0;
    let (_, lines, printed) = bench.watch(Vec::new(), &mut |_| {
        pass += 1;
        later(&clock);
        pass < 4
    });
    assert!(github_merges(&bench).is_empty(), "{lines}\n{printed}");
    let waits: Vec<Value> = data_of(&bench.events(), EventKind::Decision, "watch")
        .into_iter()
        .filter(|data| data["watch"] == "merge_waits")
        .collect();
    assert_eq!(waits.len(), 1, "{waits:?}");
    assert_eq!(
        (
            &waits[0]["blocker"],
            &waits[0]["category"],
            &waits[0]["question"],
            &waits[0]["round"],
            &waits[0]["head"],
        ),
        (
            &json!("security"),
            &json!("security"),
            &json!("Q1"),
            &json!(1),
            &json!(bench.remote_branch().unwrap()),
        )
    );
    let notified = bench.notified.0.borrow().clone();
    let told = notified
        .iter()
        .filter(|line| line.contains("is green and waits for a person"))
        .collect::<Vec<_>>();
    assert_eq!(told.len(), 1, "{notified:?}");
    assert!(
        told[0].starts_with(
            "DEMO-1: pull request #1 is green and waits for a person: Owlshift will not merge \
             it, as question Q1 of round 1 was asked under `security`"
        ),
        "{told:?}"
    );
    assert_eq!(
        lines.matches("is green and waits for a person").count(),
        1,
        "{lines}"
    );
}

/// `continue` at Watch says what merge on green makes of the ticket's
/// questions (OWL-465). LOC-1037's shape, an `irreversible` question its
/// decider answered, lets watch's next pass merge; a `security` question
/// answered, an `irreversible` one with no answer on record and a decision
/// the resolver kept under `money`, which the project's level keeps for a
/// person, each hold it, said in a person's words.
#[test]
fn continue_at_watch_says_which_questions_still_hold_the_merge() {
    let bench = merging("all");
    let plain = bench.record();
    let say = |questions: String| {
        let mut asked = plain.clone();
        asked.questions = TicketQuestions::parse(&questions).unwrap();
        bench.write_record(&asked);
        let (outcome, printed) = bench.continue_ticket(Vec::new());
        match &outcome {
            Err(stop @ Stop::Watched { .. }) => stop.to_string(),
            other => panic!("{other:?}\n{printed}"),
        }
    };
    let asked = |category: &str, verdicts: &str| {
        r#"{"format":17,"asks":[{"kind":"questions","round":1,"at":"2026-10-02T09:00:00Z",
        "comment":"c1","decider":{"account":"maintainer","by":"assignee"},"questions":[
        {"id":"Q1","category":"CATEGORY","context":"The letter is legally binding.",
        "text":"Send it from the sandbox only, or for real now?","options":[
        {"label":"Sandbox only"},{"label":"Prod-ready now"}],
        "recommendation":{"option":"A","why":"Nothing leaves the sandbox."}}]VERDICTS}]}"#
            .replace("CATEGORY", category)
            .replace("VERDICTS", verdicts)
    };
    let answered = r#","verdicts":[{"question":"Q1","class":"answered","reason":"Sandbox only."}]"#;
    let held = |words: &str| {
        format!(
            "`continue` never merges, and the next pass of `owlshift watch` would not merge it: \
             a question of the ticket {words}."
        )
    };
    let cases = [
        (
            asked("irreversible", answered),
            "`continue` never merges: the next pass of `owlshift watch` would merge it, once its \
             read is green again (merge on green)."
                .to_owned(),
        ),
        (
            asked("security", answered),
            held(
                "was asked under `security`, and a person merges a security question whatever \
                 the answer",
            ),
        ),
        (
            asked("irreversible", ""),
            held(
                "under `irreversible`, a category the project keeps for a person, has no answer \
                 of its decider on record",
            ),
        ),
        (
            r#"{"format":17,"decisions":[{"at":"2026-10-02T09:00:00Z","comment":"c2",
            "question":{"id":"Q1","category":"money","context":"c","text":"t","options":[]},
            "decision":"d","basis":"b"}]}"#
                .to_owned(),
            held(
                "under `money`, a category the project keeps for a person, was decided by the \
                 resolver rather than answered by its decider",
            ),
        ),
    ];
    for (questions, expected) in cases {
        let said = say(questions);
        assert!(said.ends_with(&expected), "{said}\nexpected: {expected}");
    }
    assert!(github_merges(&bench).is_empty());
}

/// The rebase run of a refresh: the delivered branch replayed onto the base
/// its step fetched, with no conflict.
fn refresh_in(bench: &Bench) -> During {
    let (env, worktree) = (bench.env.clone(), bench.worktree());
    (
        1,
        Box::new(move || {
            env.run(&worktree, &["rebase", "--quiet", "origin/main"])
                .unwrap();
        }),
    )
}

/// A sibling's merge into `main`, writing a file of its own: a move that
/// leaves the delivery's paths alone (OWL-520).
fn sibling_merged(bench: &Bench) -> String {
    bench.merge_into_main(&[("OTHER.md", "Another ticket's file.\n")])
}

/// `NOTES.md` as `main` holds it before the ticket's Build: the delivery
/// changes its last line, a sibling its first, so their merge is clean and
/// shares a path (OWL-520).
const NOTES: &str = "one\ntwo\nthree\nfour\nfive\n";

/// `NOTES.md` seeded on `main`, then a Build's reply: the greeting and the
/// last line of `NOTES.md` changed, its result `result`.
fn sharing_reply(bench: &Bench, result: &str) -> Reply {
    bench.merge_into_main(&[("NOTES.md", NOTES)]);
    let mut reply = bench.reply(Some("Hello"), Some(result));
    reply.files.insert(
        RelativePath::new("NOTES.md").unwrap(),
        NOTES.replace("five", "FIVE"),
    );
    reply
}

/// [`merging`], the delivery also changing `NOTES.md` ([`sharing_reply`]).
fn sharing(switch: &str) -> Bench {
    let bench = Bench::with(
        "maintainer",
        "assignee = \"maintainer\"\n",
        &format!("[forge]\nmerge = \"{switch}\"\n"),
    );
    let reply = sharing_reply(&bench, DONE);
    let (outcome, printed) = bench.run(vec![reply], None);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    bench
}

/// A sibling's merge into `main` changing the first line of `NOTES.md`, a
/// path the delivery of [`sharing`] changed: a clean merge whose move needs
/// a refresh rebase (OWL-520).
fn sibling_sharing(bench: &Bench) -> String {
    bench.merge_into_main(&[("NOTES.md", &NOTES.replace("one", "ONE"))])
}

/// The data of the rebase runs (`watch` `rebase_run`).
fn rebase_runs(bench: &Bench) -> Vec<Value> {
    data_of(&bench.events(), EventKind::Decision, "watch")
        .into_iter()
        .filter(|data| data["watch"] == "rebase_run")
        .collect()
}

/// The data of the moved bases merge on green told (OWL-520).
fn moved_bases(bench: &Bench) -> Vec<Value> {
    data_of(&bench.events(), EventKind::Decision, "watch")
        .into_iter()
        .filter(|data| data["watch"] == "base_moved")
        .collect()
}

/// The data of the waits merge on green told (OWL-499).
fn waits_told(bench: &Bench) -> Vec<Value> {
    data_of(&bench.events(), EventKind::Decision, "watch")
        .into_iter()
        .filter(|data| data["watch"] == "waits")
        .collect()
}

/// S18, a base that moved since the push on a path the delivery changed:
/// no merge, a refresh rebase run instead, and a merge only once the
/// rebased head reads green twice.
#[test]
fn a_moved_base_gets_a_refresh_rebase_then_two_green_reads_before_the_merge() {
    refresh_then_merge(&sharing("all"), false);
}

/// OWL-499: a base a sibling's merge moves on a path the delivery changed
/// while watch follows a green pull request, after its first green read,
/// gets the refresh rebase at the next pass, then the merge, with no wait
/// told on the way.
#[test]
fn a_base_moved_by_a_sibling_s_merge_while_watch_follows_gets_its_refresh() {
    refresh_then_merge(&sharing("all"), true);
}

/// LOC-1454 on the bench (OWL-457): the same moved base while two other
/// tickets' runs are in flight throughout, their obligations standing and
/// their locks held, beside a third ticket's entry whose folder is gone. The
/// rebase runs and the merge follows with no one waiting for them to end,
/// and the third ticket's entry, which a run in flight's check reads, is
/// left as it was: no `worktree prune` ran.
#[test]
fn a_moved_base_is_rebased_and_merged_while_other_runs_are_in_flight() {
    let bench = sharing("all");
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

    refresh_then_merge(&bench, false);
    assert!(dirs.obligations_stand());
    assert_eq!(fs::read(entry.join("gitdir")).unwrap(), kept, "pruned");
    drop(in_flight);
}

/// A moved base's refresh rebase run, then the merge once the rebased head
/// read green twice, played by `watch` on `bench`, a delivered ticket of a
/// project whose `[forge] merge` is `all` ([`sharing`]): the base moved on a
/// path the delivery changed before watch starts, or, `while_watching`,
/// after its first pass read the pull request green (OWL-499). The move is
/// told once, its shared path named (OWL-520).
fn refresh_then_merge(bench: &Bench, while_watching: bool) {
    let first_push = bench.remote_branch().unwrap();
    let at_push = bench.remote_main();
    let mut main = (!while_watching).then(|| sibling_sharing(bench));
    let refresh = refresh_in(bench);
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
            if main.is_none() {
                assert_eq!(bench.remote_branch().as_deref(), Some(first_push.as_str()));
                main = Some(sibling_sharing(bench));
            }
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
    let main = main.unwrap();
    // Each wait the next pass settled said nothing (OWL-499).
    assert_eq!(waits_told(bench), Vec::<Value>::new(), "{lines}");
    let rebased = rebase_runs(bench);
    assert_eq!(rebased.len(), 1, "{rebased:?}");
    assert_eq!(
        (
            &rebased[0]["refresh"],
            &rebased[0]["onto"],
            &rebased[0]["cause"]
        ),
        (&json!(true), &json!(main), &json!("merge"))
    );
    let moved = moved_bases(bench);
    assert_eq!(moved.len(), 1, "{moved:?}");
    assert_eq!(
        (
            &moved[0]["reason"],
            &moved[0]["refresh"],
            &moved[0]["shared"],
            &moved[0]["head"],
            &moved[0]["base_at_push"],
            &moved[0]["base"]
        ),
        (
            &json!("shared"),
            &json!(true),
            &json!(["NOTES.md"]),
            &json!(first_push),
            &json!(at_push),
            &json!(main)
        )
    );
    let line = "DEMO-1: pull request #1's base moved since the push, on paths it changes too \
                (NOTES.md): a refresh rebase comes first";
    assert_eq!(lines.matches(line).count(), 1, "{lines}");
    let head = bench.remote_branch().unwrap();
    assert!(is_ancestor(bench, &main, &head));
    assert!(pushed_at.is_some(), "{lines}");
    let merges = github.merges.lock().unwrap().clone();
    assert_eq!(merges, [json!({ "sha": head, "merge_method": "squash" })]);
    assert_eq!(merged_comments(bench).len(), 1);
    assert_ended_merged(bench);
}

/// OWL-520, S18: a base two siblings' merges moved since the push, each on
/// paths the delivery leaves alone. `continue` says the next pass would
/// merge; watch tells each move once as needing no refresh, runs no rebase,
/// and merges the pushed head on the base it read last, which the
/// `pr_merged` event names beside the base at the push.
#[test]
fn a_base_moved_on_paths_the_delivery_leaves_alone_merges_without_a_refresh() {
    let bench = merging("all");
    let pushed = bench.remote_branch().unwrap();
    let at_push = bench.remote_main();
    let first = sibling_merged(&bench);
    let (outcome, printed) = bench.continue_ticket(Vec::new());
    let said = match &outcome {
        Err(stop @ Stop::Watched { .. }) => stop.to_string(),
        other => panic!("{other:?}\n{printed}"),
    };
    assert!(
        said.ends_with("the next pass of `owlshift watch` would merge it, once its read is green again (merge on green)."),
        "{said}"
    );
    let clock = bench.clock.clone();
    let mut second = None;
    let mut pass = 0;
    let (outcomes, lines, printed) = bench.watch(Vec::new(), &mut |_| {
        pass += 1;
        later(&clock);
        // Between the two green reads, a second sibling moves the base.
        if pass == 1 {
            second = Some(bench.merge_into_main(&[("THIRD.md", "A third ticket's file.\n")]));
        }
        pass < 4
    });
    assert!(
        outcomes.iter().all(Result::is_ok),
        "{outcomes:?}\n{lines}\n{printed}"
    );
    let second = second.unwrap();
    assert_eq!(rebase_runs(&bench), Vec::<Value>::new(), "{lines}");
    let moved: Vec<Value> = moved_bases(&bench)
        .iter()
        .map(|data| {
            json!([
                data["reason"],
                data["refresh"],
                data["head"],
                data["base_at_push"],
                data["base"]
            ])
        })
        .collect();
    assert_eq!(
        moved,
        [
            json!(["disjoint", false, pushed, at_push, first]),
            json!(["disjoint", false, pushed, at_push, second]),
        ],
        "{lines}"
    );
    let line = "DEMO-1: pull request #1's base moved since the push, on paths it does not \
                change: no refresh rebase, it merges once read green twice";
    assert_eq!(lines.matches(line).count(), 2, "{lines}");
    assert_eq!(
        github_merges(&bench),
        [json!({ "sha": pushed, "merge_method": "squash" })],
        "{lines}"
    );
    let merged = merged_events(&bench);
    assert_eq!(
        (&merged[0]["base_at_push"], &merged[0]["base"]),
        (&json!(at_push), &json!(second))
    );
    assert_ended_merged(&bench);
}

/// OWL-521, S18: a base a sibling's merge moved since the push, on paths
/// the delivery leaves alone, whose head's own checks fail (they passed at
/// `base_at_push`). Watch reads them only once the pull request is read
/// green twice, merges nothing and runs no rebase while they fail, tells the
/// wait once, naming the check, and merges the pushed head once they pass.
#[test]
fn a_red_base_moved_apart_from_the_delivery_holds_the_merge_until_it_is_green() {
    let bench = merging("all");
    let pushed = bench.remote_branch().unwrap();
    sibling_merged(&bench);
    {
        let mut followed = bench.github.followed.lock().unwrap();
        followed.base_conclusion = "FAILURE";
        followed.at_push_conclusion = Some("SUCCESS");
    }
    let base_reads = || *bench.github.base_reads.lock().unwrap();
    let before = base_reads();
    let clock = bench.clock.clone();
    let mut reads = Vec::new();
    let mut pass = 0;
    let (outcomes, lines, printed) = bench.watch(Vec::new(), &mut |_| {
        pass += 1;
        later(&clock);
        reads.push(base_reads() - before);
        if pass < 4 {
            assert_eq!(github_merges(&bench), Vec::<Value>::new(), "pass {pass}");
        }
        if pass == 3 {
            bench.github.followed.lock().unwrap().base_conclusion = "SUCCESS";
        }
        pass < 4
    });
    assert!(
        outcomes.iter().all(Result::is_ok),
        "{outcomes:?}\n{lines}\n{printed}"
    );
    // Nothing read at the first green read; one read a pass after it.
    assert_eq!(reads, [0, 1, 2, 3], "{lines}");
    assert_eq!(rebase_runs(&bench), Vec::<Value>::new(), "{lines}");
    let told: Vec<Value> = waits_told(&bench)
        .iter()
        .map(|data| json!([data["reason"], data["checks"], data["head"]]))
        .collect();
    assert_eq!(told, [json!(["merge_base_red", ["ci"], pushed])], "{lines}");
    let line = "DEMO-1: pull request #1 is green and not merged yet: its base moved since the \
                push, and the base head's own checks fail (ci); it merges once they pass";
    assert_eq!(lines.matches(line).count(), 1, "{lines}");
    assert_eq!(
        github_merges(&bench),
        [json!({ "sha": pushed, "merge_method": "squash" })],
        "{lines}"
    );
    assert_ended_merged(&bench);
}

/// What `continue` says of the next pass of watch on a ticket at Watch.
fn continue_says(bench: &Bench) -> String {
    let (outcome, printed) = bench.continue_ticket(Vec::new());
    match &outcome {
        Err(stop @ Stop::Watched { .. }) => stop.to_string(),
        other => panic!("{other:?}\n{printed}"),
    }
}

/// OWL-526, S18: a base a sibling's merge moved since the push, on paths
/// the delivery leaves alone, whose head carries a deployment still in
/// progress beside a passed `ci`. The pull request does not run `deploy`,
/// so it holds nothing: `continue` says the next pass would merge it, and
/// watch merges the pushed head without telling a pending base.
#[test]
fn a_pending_base_check_the_pull_request_does_not_carry_holds_nothing() {
    let bench = merging("all");
    let pushed = bench.remote_branch().unwrap();
    sibling_merged(&bench);
    bench.github.followed.lock().unwrap().base_head_checks =
        Some(vec![("ci", Some("SUCCESS")), ("deploy", None)]);
    let said = continue_says(&bench);
    assert!(
        said.ends_with(
            "the next pass of `owlshift watch` would merge it, once its read is green again \
             (merge on green)."
        ),
        "{said}"
    );
    let clock = bench.clock.clone();
    let mut pass = 0;
    let (outcomes, lines, printed) = bench.watch(Vec::new(), &mut |_| {
        pass += 1;
        later(&clock);
        pass < 3
    });
    assert!(
        outcomes.iter().all(Result::is_ok),
        "{outcomes:?}\n{lines}\n{printed}"
    );
    let pending: Vec<Value> = waits_told(&bench)
        .into_iter()
        .filter(|data| data["reason"] == "merge_base_pending")
        .collect();
    assert_eq!(pending, Vec::<Value>::new(), "{lines}");
    assert_eq!(
        github_merges(&bench),
        [json!({ "sha": pushed, "merge_method": "squash" })],
        "{lines}"
    );
    assert_ended_merged(&bench);
}

/// OWL-526, S18: the same moved base, whose head's `ci`, a check the pull
/// request carries too, is still in progress. `continue` says the next pass
/// would not merge it, naming `ci`; watch merges nothing while it runs,
/// tells the wait once, naming it, and merges the pushed head once it
/// passes.
#[test]
fn a_pending_base_check_the_pull_request_carries_holds_the_merge_until_it_passes() {
    let bench = merging("all");
    let pushed = bench.remote_branch().unwrap();
    sibling_merged(&bench);
    bench.github.followed.lock().unwrap().base_head_checks = Some(vec![("ci", None)]);
    let said = continue_says(&bench);
    assert!(
        said.ends_with(
            "the next pass of `owlshift watch` would not merge it: its base moved since the \
             push, and the base head's own checks still run: ci."
        ),
        "{said}"
    );
    let clock = bench.clock.clone();
    let mut pass = 0;
    let (outcomes, lines, printed) = bench.watch(Vec::new(), &mut |_| {
        pass += 1;
        later(&clock);
        if pass < 4 {
            assert_eq!(github_merges(&bench), Vec::<Value>::new(), "pass {pass}");
        }
        if pass == 3 {
            bench.github.followed.lock().unwrap().base_head_checks =
                Some(vec![("ci", Some("SUCCESS"))]);
        }
        pass < 4
    });
    assert!(
        outcomes.iter().all(Result::is_ok),
        "{outcomes:?}\n{lines}\n{printed}"
    );
    assert_eq!(rebase_runs(&bench), Vec::<Value>::new(), "{lines}");
    let told: Vec<Value> = waits_told(&bench)
        .iter()
        .map(|data| json!([data["reason"], data["checks"], data["head"]]))
        .collect();
    assert_eq!(
        told,
        [json!(["merge_base_pending", ["ci"], pushed])],
        "{lines}"
    );
    assert_eq!(
        github_merges(&bench),
        [json!({ "sha": pushed, "merge_method": "squash" })],
        "{lines}"
    );
    assert_ended_merged(&bench);
}

/// OWL-520, S18: GitHub refuses the merge of a pull request on a base that
/// moved since the push on paths the delivery leaves alone, as a repository
/// that requires its branches up to date would (the words below are not
/// GitHub's, which were not observed, and the fallback reads none). No one
/// is notified: one `merge_refused` event, not final, saying a refresh
/// comes; at the next pass the move is told again with its new reason and
/// the refresh rebase runs. On the base its checks ran on, the refreshed
/// head's refusal is taken as before: final, and notified once.
#[test]
fn a_merge_refused_on_a_moved_base_falls_back_to_the_refresh() {
    let bench = merging("all");
    let pushed = bench.remote_branch().unwrap();
    let main = sibling_merged(&bench);
    let final_refusal = "Merge commits are not allowed on this repository.";
    bench.github.merge_answers.lock().unwrap().extend([
        MergeAnswer::Refused(405, "Head branch is not up to date with the base branch."),
        MergeAnswer::Refused(405, final_refusal),
    ]);
    let refresh = refresh_in(&bench);
    let clock = bench.clock.clone();
    let mut pass = 0;
    let (outcomes, lines, printed) = bench.watch_during(
        vec![bench.reply(None, Some(REBASED))],
        vec![refresh],
        &mut |_| {
            pass += 1;
            later(&clock);
            pass < 9
        },
    );
    assert!(
        outcomes.iter().all(Result::is_ok),
        "{outcomes:?}\n{lines}\n{printed}"
    );
    let head = bench.remote_branch().unwrap();
    assert_ne!(head, pushed, "no refresh rebase was pushed\n{lines}");
    assert_eq!(
        github_merges(&bench),
        [
            json!({ "sha": pushed, "merge_method": "squash" }),
            json!({ "sha": head, "merge_method": "squash" }),
        ],
        "{lines}"
    );
    let refused: Vec<Value> = data_of(&bench.events(), EventKind::Decision, "watch")
        .into_iter()
        .filter(|data| data["watch"] == "merge_refused")
        .map(|data| json!([data["head"], data["final"], data["status"], data["refresh"]]))
        .collect();
    assert_eq!(
        refused,
        [
            json!([pushed, false, 405, true]),
            json!([head, true, 405, null]),
        ],
        "{lines}"
    );
    let moved: Vec<Value> = moved_bases(&bench)
        .iter()
        .map(|data| json!([data["reason"], data["refresh"], data["head"], data["base"]]))
        .collect();
    assert_eq!(
        moved,
        [
            json!(["disjoint", false, pushed, main]),
            json!(["refused", true, pushed, main]),
        ],
        "{lines}"
    );
    let rebased = rebase_runs(&bench);
    assert_eq!(rebased.len(), 1, "{rebased:?}\n{lines}");
    assert_eq!(
        (&rebased[0]["cause"], &rebased[0]["onto"]),
        (&json!("merge"), &json!(main))
    );
    let line = "DEMO-1: GitHub refused to merge pull request #1 on a base that moved since the \
                push (Head branch is not up to date with the base branch); the next pass gets it \
                a refresh rebase";
    assert_eq!(lines.matches(line).count(), 1, "{lines}");
    let notified = bench.notified.0.borrow().clone();
    assert_eq!(notified.len(), 1, "{notified:?}");
    assert!(notified[0].contains(final_refusal), "{notified:?}");
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
/// OWL-438: under an operator's pause a green, mergeable pull request is
/// not merged, nor read for a merge; once the pause is lifted, the next
/// passes merge it as usual.
#[test]
fn a_pause_holds_merge_on_green_until_it_is_lifted() {
    let bench = merging("all");
    let pushed = bench.remote_branch().unwrap();
    let pause = owlshift_runner::daemon::requests::pause_file(&bench.dirs());
    fs::create_dir_all(pause.parent().unwrap()).unwrap();
    fs::write(&pause, "").unwrap();
    let github = bench.github.clone();
    let clock = bench.clock.clone();
    let mut pass = 0;
    let (_, lines, printed) = bench.watch(Vec::new(), &mut |_| {
        pass += 1;
        later(&clock);
        if pass == 4 {
            assert!(
                github.merges.lock().unwrap().is_empty(),
                "merged while paused"
            );
            assert_eq!(*github.merge_reads.lock().unwrap(), 0, "read for a merge");
            fs::remove_file(&pause).unwrap();
        }
        pass < 8
    });
    assert_eq!(
        github_merges(&bench),
        [json!({ "sha": pushed, "merge_method": "squash" })],
        "{lines}\n{printed}"
    );
    assert_eq!(merged_comments(&bench).len(), 1, "{lines}\n{printed}");
    assert_ended_merged(&bench);
    assert!(lines.contains("nor a merge on green"), "{lines}");
}

/// OWL-485: a dark wake holds the dispatches and the steps' starts, not
/// Owlshift's merge on green, which starts no run and spends nothing: a
/// green, mergeable pull request is merged while the machine is in a dark
/// wake.
#[test]
fn a_dark_wake_leaves_merge_on_green_alone() {
    let bench = merging("all");
    let pushed = bench.remote_branch().unwrap();
    bench.wake.set(Wake::Dark);
    let clock = bench.clock.clone();
    let mut pass = 0;
    let (_, lines, printed) = bench.watch(Vec::new(), &mut |_| {
        pass += 1;
        later(&clock);
        pass < 6
    });
    assert_eq!(
        github_merges(&bench),
        [json!({ "sha": pushed, "merge_method": "squash" })],
        "{lines}\n{printed}"
    );
    assert_eq!(merged_comments(&bench).len(), 1, "{lines}\n{printed}");
    assert_ended_merged(&bench);
    assert!(lines.contains("is in a dark wake"), "{lines}");
}

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

/// LOC-1513 and LOC-1530 on the bench (OWL-501; architecture section 5,
/// "Merge on green", "An owed follow-up does not hold"): a green pull
/// request on a base a sibling's merge moved, whose ticket owes follow-ups
/// the tracker refuses. While its DELIVERY report is owed too, it gets no
/// refresh rebase, which is told once (OWL-499). Once the report is posted,
/// the follow-ups, still refused, hold nothing: the refresh rebase runs and
/// the merge follows, with no wait told for them. After the merge, watch
/// files them again at most every 10 minutes, a refusal kept again changing
/// nothing, and files them once the tracker takes them, with no second
/// report.
#[test]
fn owed_follow_ups_the_tracker_refuses_hold_neither_the_refresh_nor_the_merge() {
    let bench = Bench::with(
        "maintainer",
        "assignee = \"maintainer\"\n",
        "[forge]\nmerge = \"all\"\n",
    );
    {
        let mut inbox = bench.inbox.borrow_mut();
        inbox.on = true;
        inbox.refuse = Some("the plan holds 250 issues");
    }
    bench.refuse.set(Some("[owlshift] DELIVERY"));
    let (outcome, printed) = bench.run(vec![sharing_reply(&bench, PROPOSING)], None);
    assert!(
        matches!(&outcome, Err(Stop::Delivery(why)) if why.contains("the next pass posts it")),
        "{outcome:?}\n{printed}"
    );
    let refused = ProposalOutcome::Owed {
        refusal: Some("tracker: the plan holds 250 issues".to_owned()),
    };
    assert_eq!(
        proposal_outcomes(&bench),
        [refused.clone(), refused.clone()]
    );
    let first_push = bench.remote_branch().unwrap();
    let main = sibling_sharing(&bench);
    let refresh = refresh_in(&bench);
    let clock = bench.clock.clone();
    let mut pass = 0;
    // From the first idle pass that reads the following ended: the filings
    // tried by then and after each pass, the comments and the ref then.
    let mut ended: Option<(u32, usize, String)> = None;
    let mut tries = Vec::new();
    let mut refs = Vec::new();
    let (outcomes, lines, printed) = bench.watch_during(
        vec![bench.reply(None, Some(REBASED))],
        vec![refresh],
        &mut |_| {
            pass += 1;
            let attempts = bench.inbox.borrow().attempts;
            if ended.is_none() && bench.record().delivery.unwrap().ended.is_some() {
                assert_eq!(
                    proposal_outcomes(&bench),
                    [refused.clone(), refused.clone()]
                );
                ended = Some((pass, bench.comments().len(), bench.ref_commit()));
            }
            let Some((at, _, _)) = ended else {
                // The report, refused twice, holds the refresh, then posts.
                if pass == 2 {
                    assert_eq!(bench.remote_branch().as_deref(), Some(first_push.as_str()));
                    bench.refuse.set(None);
                }
                later(&clock);
                return pass < 12;
            };
            tries.push(attempts);
            refs.push(bench.ref_commit());
            if pass == at + 3 {
                bench.inbox.borrow_mut().refuse = None;
            }
            // Five minutes a pass: a try is due every second pass.
            clock.set(
                clock
                    .get()
                    .checked_add(SignedDuration::from_mins(5))
                    .unwrap(),
            );
            pass < at + 5
        },
    );
    assert!(
        outcomes.iter().all(Result::is_ok),
        "{outcomes:?}\n{lines}\n{printed}"
    );
    let (_, comments, _) = ended.expect("the following ended");
    // The report alone was told as owed first; the follow-ups never were.
    let told = waits_told(&bench);
    assert_eq!(told.len(), 1, "{told:?}\n{lines}");
    assert_eq!(
        (&told[0]["reason"], &told[0]["head"], &told[0]["report"]),
        (&json!("merge_owes_first"), &json!(first_push), &json!(true))
    );
    let line = "DEMO-1: pull request #1 is green and not merged yet: the ticket owes its \
                DELIVERY report on the tracker, which comes first. Owlshift tries again at each \
                pass, then gets it a refresh rebase or merges it";
    assert_eq!(lines.matches(line).count(), 1, "{lines}");
    let rebased = rebase_runs(&bench);
    assert_eq!(rebased.len(), 1, "{rebased:?}\n{lines}");
    assert_eq!(
        (&rebased[0]["onto"], &rebased[0]["cause"]),
        (&json!(main), &json!("merge"))
    );
    let head = bench.remote_branch().unwrap();
    assert_eq!(
        github_merges(&bench),
        [json!({ "sha": head, "merge_method": "squash" })],
        "{lines}"
    );
    assert_ended_merged(&bench);
    // After the end, two filings a try: tried at the next pass, held while
    // under 10 minutes, tried again at 10 (`RETRY_AFTER` reached) and
    // refused, each refusal kept again leaving the ref as it was; then, the
    // tracker taking them, filed at the next try.
    let made: Vec<usize> = tries.windows(2).map(|w| w[1] - w[0]).collect();
    assert_eq!(made, [2, 0, 2, 0, 2], "{tries:?}\n{lines}");
    assert_eq!(refs[0], refs[3], "a refusal kept again wrote the ref");
    assert_eq!(
        proposal_outcomes(&bench)
            .iter()
            .filter(|outcome| matches!(outcome, ProposalOutcome::Filed { .. }))
            .count(),
        2,
        "{:?}",
        proposal_outcomes(&bench)
    );
    assert_eq!(
        proposal_events(&bench.events()),
        ["proposal_unfiled", "proposal_unfiled", "filed", "filed"]
    );
    assert_eq!(bench.comments().len(), comments, "a second report");
}

/// OWL-499: a refresh rebase whose own read does not find the pull request
/// green, its check running again, says what it read and that it ran
/// nothing, never that Owlshift does not merge; its ticket is held as any
/// step's that left its ref as it was, which the passes after it tell once;
/// after the hold, the refresh rebase runs and the merge follows.
#[test]
fn a_refresh_that_ran_nothing_is_held_and_told() {
    let bench = sharing("all");
    let at_push = bench.remote_main();
    let main = sibling_sharing(&bench);
    // Watch's first read is green; the refresh's own read, the next one,
    // finds the check running.
    let reads = *bench.github.follow_reads.lock().unwrap();
    *bench.github.pending_follow_read.lock().unwrap() = Some(reads + 2);
    let refresh = refresh_in(&bench);
    let clock = bench.clock.clone();
    let mut pass = 0;
    let (outcomes, lines, printed) = bench.watch_during(
        vec![bench.reply(None, Some(REBASED))],
        vec![refresh],
        &mut |_| {
            pass += 1;
            later(&clock);
            pass < 12
        },
    );
    let watched: Vec<String> = outcomes
        .iter()
        .filter_map(|outcome| outcome.as_ref().err().map(ToString::to_string))
        .collect();
    assert_eq!(watched.len(), 1, "{outcomes:?}\n{lines}\n{printed}");
    assert!(
        watched[0].contains("refresh rebase ran nothing") && !watched[0].contains("never merges"),
        "{}",
        watched[0]
    );
    let unneeded: Vec<Value> = data_of(&bench.events(), EventKind::Decision, "watch")
        .into_iter()
        .filter(|data| data["watch"] == "refresh_unneeded")
        .collect();
    assert_eq!(unneeded.len(), 1, "{unneeded:?}");
    assert_eq!(
        (
            &unneeded[0]["action"],
            &unneeded[0]["base"],
            &unneeded[0]["base_at_push"]
        ),
        (&json!("wait_checks"), &json!(main), &json!(at_push))
    );
    let told = waits_told(&bench);
    assert_eq!(told.len(), 1, "{told:?}\n{lines}");
    assert_eq!(told[0]["reason"], "refresh_held");
    let until = told[0]["until"].as_str().expect("a time");
    let line = format!(
        "DEMO-1: pull request #1 is green on a base that moved since the push, and its refresh \
         rebase waits: its last step left its ticket ref as it was, so it is tried again at \
         {until}, or sooner once its ref moves"
    );
    assert_eq!(lines.matches(&line).count(), 1, "{lines}");
    let rebased = rebase_runs(&bench);
    assert_eq!(rebased.len(), 1, "{rebased:?}\n{lines}");
    assert_eq!(github_merges(&bench).len(), 1, "{lines}");
    assert_ended_merged(&bench);
}

/// OWL-558: an agent's commit message closes no issue. The Build's "Fixes
/// #5" reaches the remote as `Fi\u{200B}xes #5`, which GitHub reads as no
/// keyword (live check of 2026-11-14, `owl-558.sh`; a bench sees messages,
/// not GitHub's closing); a fix run's "Closes #6" lands as a fast-forward
/// of that image, broken too; the ticket's branch follows each push, its
/// run file with it, or the fix run would not have started; and the merge
/// on green is of the head so pushed.
#[test]
fn an_agent_s_closing_keywords_are_broken_in_every_commit_pushed_and_merged() {
    let bench = Bench::with(
        "maintainer",
        "assignee = \"maintainer\"\n",
        "[forge]\nmerge = \"all\"\n",
    );
    let main = bench.remote_main();
    let mut build = bench.reply(Some("Hello"), Some(DONE));
    build.commit = Some("Add a greeting\n\nFixes #5".to_owned());
    let (outcome, printed) = bench.run(vec![build], None);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let delivered = bench.remote_branch().unwrap();
    assert_eq!(
        pushed_messages(&bench, &main, &delivered),
        ["Add a greeting\n\nFi\u{200B}xes #5"]
    );
    assert_eq!(bench.branch_head(), delivered);

    let github = bench.github.clone();
    let clock = bench.clock.clone();
    github.followed.lock().unwrap().conclusion = "FAILURE";
    let mut fix = bench.reply(Some("Hello again"), Some(FIXED));
    fix.commit = Some("Closes #6 with a second greeting".to_owned());
    let mut pass = 0;
    let (outcomes, lines, printed) = bench.watch(vec![fix], &mut |_| {
        pass += 1;
        later(&clock);
        github.followed.lock().unwrap().conclusion = "SUCCESS";
        pass < 6
    });
    assert!(
        outcomes.iter().all(Result::is_ok),
        "{outcomes:?}\n{lines}\n{printed}"
    );
    let head = bench.remote_branch().unwrap();
    assert_eq!(
        pushed_messages(&bench, &main, &head),
        [
            "Cl\u{200B}oses #6 with a second greeting",
            "Add a greeting\n\nFi\u{200B}xes #5"
        ]
    );
    let parent = bench
        .env
        .run(&bench.remote.bare, &["rev-parse", &format!("{head}^")])
        .unwrap();
    assert_eq!(String::from_utf8(parent).unwrap().trim(), delivered);
    assert_eq!(bench.branch_head(), head);
    assert_eq!(
        github_merges(&bench),
        [json!({ "sha": head, "merge_method": "squash" })],
        "{lines}\n{printed}"
    );
    assert_ended_merged(&bench);
}

/// OWL-558 on a refresh rebase (OWL-520): the rebase run's own commit says
/// "Closes #7"; the push sends its neutral image, the delivery check made
/// on that image as the rebase run's commit, and the merge is of it.
#[test]
fn a_rebase_run_s_closing_keyword_is_broken_before_its_push() {
    let bench = sharing("all");
    let main = sibling_sharing(&bench);
    let (env, worktree) = (bench.env.clone(), bench.worktree());
    let rebase: During = (
        1,
        Box::new(move || {
            env.run(&worktree, &["rebase", "--quiet", "origin/main"])
                .unwrap();
            env.run(
                &worktree,
                &[
                    "commit",
                    "--quiet",
                    "--amend",
                    "-m",
                    "Add a greeting\n\nCloses #7",
                ],
            )
            .unwrap();
        }),
    );
    let clock = bench.clock.clone();
    let mut pass = 0;
    let (outcomes, lines, printed) = bench.watch_during(
        vec![bench.reply(None, Some(REBASED))],
        vec![rebase],
        &mut |_| {
            pass += 1;
            later(&clock);
            pass < 6
        },
    );
    assert!(
        outcomes.iter().all(Result::is_ok),
        "{outcomes:?}\n{lines}\n{printed}"
    );
    assert_eq!(rebase_runs(&bench).len(), 1, "{lines}");
    let head = bench.remote_branch().unwrap();
    assert_eq!(
        pushed_messages(&bench, &main, &head),
        ["Add a greeting\n\nCl\u{200B}oses #7"]
    );
    assert_eq!(bench.branch_head(), head);
    assert_eq!(
        github_merges(&bench),
        [json!({ "sha": head, "merge_method": "squash" })],
        "{lines}\n{printed}"
    );
}
