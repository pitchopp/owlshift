//! The automatic re-run of a red check (OWL-513; architecture section 5,
//! "The automatic re-run"), on the bench of `on_demand.rs`, each scenario
//! driven by `owlshift watch`'s passes: the Markdown tracker, and the real
//! GitHub adapter over its fake GitHub, whose jobs carry their workflow run,
//! which answers the re-run call and lists the new attempts after it.

use owlshift_contracts::refs::RerunOutcomeKind;

use super::*;

/// The workflow run of the bench's jobs.
const RUN: u64 = 7;

/// A delivered ticket whose pull request's checks are `first`, jobs of
/// workflow run [`RUN`].
fn red_jobs(first: Vec<Job>) -> Bench {
    let bench = delivered();
    let mut jobs = bench.github.jobs.lock().unwrap();
    jobs.run = Some(RUN);
    jobs.first = first;
    drop(jobs);
    bench
}

fn job(name: &'static str, id: u64, conclusion: &'static str) -> Job {
    Job {
        name,
        id,
        conclusion,
    }
}

/// The link of check run `id` on the fake GitHub.
fn run_link(id: u64) -> String {
    format!("https://github.com/demo/project/runs/{id}")
}

/// The ticket's comments of `kind`.
fn marked(bench: &Bench, kind: &str) -> Vec<String> {
    bench
        .comments()
        .into_iter()
        .filter(|c| c.starts_with(&format!("[owlshift] {kind}")))
        .collect()
}

/// The warnings recorded of `what`.
fn warnings(bench: &Bench, what: &str) -> Vec<Value> {
    data_of(&bench.events(), EventKind::Warning, "what")
        .into_iter()
        .filter(|data| data["what"] == what)
        .collect()
}

/// A red of a failed leg and its failed aggregate `ci` is re-run once by the
/// runner; the rollup lists the newest attempts alone, `ci` only once the
/// legs ended. Once green, the ticket stays at Watch with no run, question
/// or park, and one RERUN comment tells the flake with both attempts.
#[test]
fn a_red_check_passing_its_rerun_is_delivered_with_no_run_question_or_park() {
    let bench = red_jobs(vec![
        job("check (ubuntu-latest)", 10, "FAILURE"),
        job("check (macos-latest)", 11, "SUCCESS"),
        job("ci", 12, "FAILURE"),
    ]);
    let github = bench.github.clone();
    let mut pass = 0;
    let (outcomes, lines, printed) = bench.watch_passes(Vec::new(), &mut |_| {
        pass += 1;
        // Nothing is merged, nor told green, while `ci` has not reported.
        if github
            .jobs
            .lock()
            .unwrap()
            .reads
            .is_some_and(|reads| reads <= 2)
        {
            assert!(github.merges.lock().unwrap().is_empty());
        }
        pass < 6
    });
    assert!(
        outcomes
            .iter()
            .all(|outcome| !matches!(outcome, Err(Stop::Parked { .. } | Stop::NeedsInput { .. }))),
        "{outcomes:?}\n{lines}\n{printed}"
    );
    assert_eq!(
        github.jobs.lock().unwrap().calls,
        [RUN],
        "{lines}\n{printed}"
    );
    // The delivery's Build and Verify alone: no fix run.
    assert_eq!(bench.briefs().len(), 2, "{lines}\n{printed}");
    assert!(marked(&bench, "QUESTIONS").is_empty());
    assert!(marked(&bench, "PARKED").is_empty());
    let reruns = marked(&bench, "RERUN");
    assert_eq!(reruns.len(), 1, "{reruns:?}\n{lines}\n{printed}");
    let told = &reruns[0];
    assert!(
        told.contains("**Outcome:** passed on the re-run: a flake, nothing to fix."),
        "{told}"
    );
    for (first, again) in [(10, 13), (12, 14)] {
        assert!(
            told.contains(&format!(
                "[first attempt]({}), [re-run]({})",
                run_link(first),
                run_link(again)
            )),
            "{told}"
        );
    }
    let state = state_of(&bench);
    assert_eq!(state.status(), Status::Active(Stage::Watch));
    assert_eq!((state.fix_passes(), state.repairs()), (0, 0));
    assert_eq!(bench.stage(), "In Review");
    let rerun = bench.record().delivery.unwrap().rerun.unwrap();
    assert_eq!(rerun.outcome, Some(RerunOutcomeKind::Passed));
    assert!(rerun.told);
    let decided = data_of(&bench.events(), EventKind::Decision, "watch");
    assert_eq!(
        decided.iter().filter(|d| d["watch"] == "rerun").count(),
        1,
        "{decided:?}"
    );
}

/// A check red again after its re-run, the rollup listing both attempts:
/// one RERUN comment says so, then one fix run is briefed with the first
/// attempt in `check_failure.rerun` and the second alone in its checks, its
/// log read. Its question opens one round at Watch; the head is never
/// re-run again.
#[test]
fn a_check_red_again_after_its_rerun_gets_one_fix_run_that_asks_once() {
    let bench = red_jobs(vec![job("ci", 10, "FAILURE")]);
    {
        let mut jobs = bench.github.jobs.lock().unwrap();
        jobs.lists_first = true;
        jobs.again = "FAILURE";
    }
    let github = bench.github.clone();
    let mut pass = 0;
    let (outcomes, lines, printed) = bench.watch_passes(variable_asked(&bench), &mut |_| {
        pass += 1;
        pass < 7
    });
    assert_eq!(
        github.jobs.lock().unwrap().calls,
        [RUN],
        "{lines}\n{printed}"
    );
    let reruns = marked(&bench, "RERUN");
    assert_eq!(reruns.len(), 1, "{reruns:?}\n{lines}\n{printed}");
    assert!(
        reruns[0].contains("**Outcome:** failed again: a fix run reads the logs next."),
        "{}",
        reruns[0]
    );
    assert!(
        reruns[0].contains(&format!(
            "[first attempt]({}), [re-run]({})",
            run_link(10),
            run_link(11)
        )),
        "{}",
        reruns[0]
    );
    // The delivery's Build and Verify, the fix run, its question's resolver.
    let briefs = bench.briefs();
    assert_eq!(briefs.len(), 4, "{outcomes:?}\n{lines}\n{printed}");
    let failure = briefs[2].check_failure.as_ref().expect("a fix run");
    let rerun = failure.rerun.as_ref().expect("the re-run's first attempt");
    assert_eq!(rerun.first.len(), 1);
    assert_eq!(rerun.first[0].name, "ci");
    assert_eq!(rerun.first[0].url.as_deref(), Some(run_link(10).as_str()));
    assert_eq!(failure.checks.len(), 1, "{failure:?}");
    assert_eq!(
        failure.checks[0].url.as_deref(),
        Some(run_link(11).as_str())
    );
    assert!(failure.checks[0].excerpt.is_some(), "{failure:?}");
    assert_eq!(github.jobs.lock().unwrap().logs, [11]);
    // One question round at Watch, no park.
    let asked = marked(&bench, "QUESTIONS");
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert!(asked[0].starts_with("[owlshift] QUESTIONS · round 1"));
    assert!(marked(&bench, "PARKED").is_empty());
    assert_eq!(
        state_of(&bench).status(),
        Status::NeedsInput {
            return_to: Stage::Watch
        }
    );
}

/// A head red again after its re-run whose RERUN comment the tracker
/// refuses: the fix run goes on, and its head is red too. When that head's
/// re-run is claimed, the first head's comment is tried once more: posted
/// when the tracker `accepts` it then, otherwise dropped with a
/// `rerun_report_dropped` warning; the ref keeps the new head's re-run
/// either way. Returns the bench and the first head.
fn replaced_untold(accepts: bool) -> (Bench, String) {
    let bench = red_jobs(vec![job("ci", 10, "FAILURE")]);
    {
        let mut jobs = bench.github.jobs.lock().unwrap();
        jobs.lists_first = true;
        jobs.again = "FAILURE";
    }
    bench.refuse.set(Some("[owlshift] RERUN"));
    let first_push = bench.remote_branch().unwrap();
    let github = bench.github.clone();
    let mut pass = 0;
    let mut moved = false;
    let (outcomes, lines, printed) = bench.watch_passes(
        vec![bench.reply(Some("Hello again"), Some(FIXED))],
        &mut |_| {
            pass += 1;
            if !moved && bench.remote_branch().unwrap() != first_push {
                // CI ran on the fix's head: red, another job, no re-run yet.
                moved = true;
                let mut jobs = github.jobs.lock().unwrap();
                jobs.first = vec![job("ci", 20, "FAILURE")];
                jobs.reads = None;
                drop(jobs);
                if accepts {
                    bench.refuse.set(None);
                }
            }
            github.jobs.lock().unwrap().calls.len() < 2 && pass < 8
        },
    );
    assert_eq!(
        github.jobs.lock().unwrap().calls,
        [RUN, RUN],
        "{outcomes:?}\n{lines}\n{printed}"
    );
    // The first head's outcome was owed, and its post refused, before the
    // fix run.
    assert_eq!(
        warnings(&bench, "rerun_report_unposted").len(),
        1,
        "{lines}\n{printed}"
    );
    assert_eq!(bench.briefs().len(), 3, "{outcomes:?}\n{lines}\n{printed}");
    let rerun = bench.record().delivery.unwrap().rerun.unwrap();
    assert_eq!(rerun.head, bench.remote_branch().unwrap());
    assert_ne!(rerun.head, first_push);
    assert_eq!(rerun.outcome, None);
    (bench, first_push)
}

/// The tracker takes the first head's RERUN comment when the next head's
/// re-run is claimed: it is posted then, with no `rerun_report_dropped`.
#[test]
fn an_untold_rerun_is_told_when_the_next_heads_rerun_is_claimed() {
    let (bench, first_push) = replaced_untold(true);
    let reruns = marked(&bench, "RERUN");
    assert_eq!(reruns.len(), 1, "{reruns:?}");
    assert!(
        reruns[0].contains(&format!("head `{}`", &first_push[..7])),
        "{}",
        reruns[0]
    );
    assert!(
        reruns[0].contains("**Outcome:** failed again: a fix run reads the logs next."),
        "{}",
        reruns[0]
    );
    assert!(warnings(&bench, "rerun_report_dropped").is_empty());
}

/// The tracker still refuses the first head's RERUN comment when the next
/// head's re-run is claimed: it is dropped, with one warning naming its
/// head, and the claim is written all the same.
#[test]
fn an_untold_rerun_still_refused_is_dropped_when_the_next_heads_rerun_is_claimed() {
    let (bench, first_push) = replaced_untold(false);
    assert!(marked(&bench, "RERUN").is_empty());
    let dropped = warnings(&bench, "rerun_report_dropped");
    assert_eq!(dropped.len(), 1, "{dropped:?}");
    assert_eq!(dropped[0]["head"], first_push.as_str());
}

/// A red the base shares waits, with no re-run and no run; once the base
/// passes, its checks having failed at the push, a refresh rebase runs.
#[test]
fn a_red_the_base_shares_waits_then_refreshes_once_the_base_passes() {
    let bench = red_jobs(vec![job("ci", 10, "FAILURE")]);
    bench.github.followed.lock().unwrap().base_conclusion = "FAILURE";
    let first_push = bench.remote_branch().unwrap();
    let (env, worktree) = (bench.env.clone(), bench.worktree());
    let rebase: During = (
        1,
        Box::new(move || {
            env.run(&worktree, &["rebase", "--quiet", "origin/main"])
                .unwrap();
        }),
    );
    let github = bench.github.clone();
    let mut pass = 0;
    let (outcomes, lines, printed) = bench.watch_passes_during(
        vec![bench.reply(None, Some(REBASED))],
        vec![rebase],
        &mut |_| {
            pass += 1;
            if pass == 2 {
                // Red on the base: nothing was re-run or run.
                assert!(github.jobs.lock().unwrap().calls.is_empty());
                assert_eq!(bench.briefs().len(), 2);
                // The base's fix lands: its head passes, the push's base
                // still failed.
                bench.merge_into_main(&[("FIXED.md", "fixed\n")]);
                let mut followed = github.followed.lock().unwrap();
                followed.base_conclusion = "SUCCESS";
                followed.at_push_conclusion = Some("FAILURE");
            }
            if bench.remote_branch().as_deref() != Some(first_push.as_str()) {
                // CI ran again on the refreshed head, green.
                github.jobs.lock().unwrap().first = vec![job("ci", 20, "SUCCESS")];
                github.followed.lock().unwrap().at_push_conclusion = None;
            }
            pass < 5
        },
    );
    assert_eq!(
        github.jobs.lock().unwrap().calls,
        Vec::<u64>::new(),
        "{outcomes:?}\n{lines}\n{printed}"
    );
    let decided = data_of(&bench.events(), EventKind::Decision, "watch");
    assert!(
        decided.iter().any(|d| d["action"] == "wait_base_red"),
        "{decided:?}"
    );
    assert!(
        decided
            .iter()
            .any(|d| d["action"] == "refresh_rebase" && d["cause"] == "base_fixed"),
        "{decided:?}\n{lines}\n{printed}"
    );
    let rebases: Vec<&Value> = decided
        .iter()
        .filter(|d| d["watch"] == "rebase_run")
        .collect();
    assert_eq!(rebases.len(), 1, "{decided:?}\n{lines}\n{printed}");
    assert_ne!(bench.remote_branch().unwrap(), first_push);
}

/// A re-run GitHub answers without running anything, its workflow run still
/// in progress (OWL-525, its words of 2026-11-11) or its rate limit spent:
/// the claim is given back, no fix run starts and nothing is warned of as
/// refused; once watch's hold of a step that left the ref as it was has
/// passed, a pass re-runs, and the head, green on its re-run, gets no fix
/// run at all.
#[test]
fn a_rerun_github_ran_nothing_for_is_tried_again_after_the_hold() {
    for refusal in [
        "This workflow is already running",
        "API rate limit exceeded",
    ] {
        let bench = red_jobs(vec![job("ci", 10, "FAILURE")]);
        bench.github.jobs.lock().unwrap().answer = (403, refusal);
        let github = bench.github.clone();
        let clock = bench.clock.clone();
        let mut given_back = false;
        let mut pass = 0;
        let (outcomes, lines, printed) = bench.watch_passes(Vec::new(), &mut |_| {
            pass += 1;
            let mut jobs = github.jobs.lock().unwrap();
            if jobs.calls.len() == 1 && !given_back {
                let delivery = bench.record().delivery.unwrap();
                assert!(delivery.rerun.is_none(), "{refusal}: {delivery:?}");
                assert_eq!(bench.briefs().len(), 2, "{refusal}: a fix run");
                given_back = true;
                jobs.answer = (201, "");
                // Past watch's hold of a step that left the ref as it was.
                clock.set(
                    clock
                        .get()
                        .checked_add(SignedDuration::from_mins(11))
                        .unwrap(),
                );
            }
            pass < 8
        });
        assert!(given_back, "{refusal}: {outcomes:?}\n{lines}\n{printed}");
        assert_eq!(
            github.jobs.lock().unwrap().calls,
            [RUN, RUN],
            "{refusal}: {outcomes:?}\n{lines}\n{printed}"
        );
        assert!(
            outcomes.iter().any(|outcome| matches!(
                outcome,
                Err(Stop::Refused(why))
                    if why.contains(refusal) && why.contains("a later read tries again")
            )),
            "{refusal}: {outcomes:?}"
        );
        assert!(warnings(&bench, "rerun_refused").is_empty(), "{refusal}");
        assert_eq!(bench.briefs().len(), 2, "{refusal}: {lines}\n{printed}");
        assert_eq!(state_of(&bench).fix_passes(), 0);
        let rerun = bench.record().delivery.unwrap().rerun.unwrap();
        assert_eq!(rerun.outcome, Some(RerunOutcomeKind::Passed), "{refusal}");
    }
}

/// A re-run GitHub refuses for its permission: one `rerun_refused`
/// warning, and the fix run in the same pass, as before OWL-513; the ref
/// keeps the refusal. The fix's head, red again and refused again, gets
/// its own call and fix run, and no second warning while watch runs.
#[test]
fn a_rerun_refused_for_its_permission_runs_the_fix_with_one_warning() {
    let bench = red_jobs(vec![job("ci", 10, "FAILURE")]);
    bench.github.jobs.lock().unwrap().answer = (403, "Resource not accessible by integration");
    let first_push = bench.remote_branch().unwrap();
    let github = bench.github.clone();
    let mut pass = 0;
    let mut second_push = None;
    let (outcomes, lines, printed) = bench.watch_passes(
        vec![
            bench.reply(Some("Hello again"), Some(FIXED)),
            bench.reply(Some("Hello once more"), Some(FIXED)),
        ],
        &mut |_| {
            pass += 1;
            let pushed = bench.remote_branch().unwrap();
            if pushed != first_push && second_push.is_none() {
                // CI ran on the fix's head: red again, another job.
                github.jobs.lock().unwrap().first = vec![job("ci", 20, "FAILURE")];
                second_push = Some(pushed);
            }
            github.jobs.lock().unwrap().calls.len() < 2 && pass < 6
        },
    );
    assert_eq!(
        bench.github.jobs.lock().unwrap().calls,
        [RUN, RUN],
        "{outcomes:?}\n{lines}\n{printed}"
    );
    let refused = warnings(&bench, "rerun_refused");
    assert_eq!(refused.len(), 1, "{refused:?}\n{lines}\n{printed}");
    assert_eq!(refused[0]["permission"], true);
    assert_eq!(refused[0]["head"], first_push.as_str());
    // A fix run for each head, in the pass that called, briefed with no
    // re-run.
    let briefs = bench.briefs();
    assert_eq!(briefs.len(), 4, "{outcomes:?}\n{lines}\n{printed}");
    for brief in &briefs[2..] {
        let failure = brief.check_failure.as_ref().expect("a fix run");
        assert!(failure.rerun.is_none(), "{failure:?}");
    }
    let second_push = second_push.expect("the first fix pushed");
    assert_ne!(bench.remote_branch().unwrap(), second_push);
    assert_eq!(state_of(&bench).fix_passes(), 2);
    let rerun = bench.record().delivery.unwrap().rerun.unwrap();
    assert_eq!(rerun.head, second_push);
    assert_eq!(rerun.outcome, Some(RerunOutcomeKind::Refused));
    assert!(
        rerun
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("Resource not accessible by integration")),
        "{rerun:?}"
    );
    assert!(marked(&bench, "RERUN").is_empty());
}
