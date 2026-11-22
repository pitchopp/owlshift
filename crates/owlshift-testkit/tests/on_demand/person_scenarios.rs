//! A person's steps (OWL-619; architecture section 4, "A person's steps"),
//! on the bench of `on_demand.rs`: the Markdown tracker, the fake harness
//! playing every role, the fake GitHub. Part 1 of the ticket: the plan
//! holds a person's steps, the Build passes them, the delivery lists them,
//! and merge on green leaves the pull request to a person while one due
//! before the merge is owed.

use std::num::NonZeroU32;

use owlshift_contracts::review::StepBy;

use super::design_scenarios::{designed, reviewed};
use super::*;

/// A plan whose last step is a person's, due before the merge, as is its
/// second, with the agent's third step waiting on the second's result.
const PERSON_PLAN: &str = r#"{"goal":"Add GREETING.md saying Hello.",
"criteria":["GREETING.md says Hello.","The greeting reads well on staging."],
"approach":"Write GREETING.md with the line the gate greps, then have a person read it deployed.",
"steps":[
 {"what":"Add GREETING.md.","test":"The gate's grep for Hello."},
 {"what":"Open GREETING.md on staging in a browser, which no run can reach.",
  "test":"It reads Hello.","by":"person","when":"before_merge"},
 {"what":"Note the staging check in the README.","test":"The README names it."},
 {"what":"Ask the team on staging whether the tone fits.","test":"They say yes.",
  "by":"person","when":"before_merge"}]}"#;

/// The Build's `done`, the agent's third step left for the second's result.
const OWING: &str = r#"{"format":16,"status":"done","summary":"Added GREETING.md; step 3 waits on step 2.",
"owed":[3],
"pr":{"branch":"owlshift/demo-1","title":"Add a greeting","body":"Says hello. Gate: green."}}"#;

/// Two minutes later on the bench's clock.
fn later(clock: &Cell<Timestamp>) {
    clock.set(
        clock
            .get()
            .checked_add(SignedDuration::from_mins(2))
            .unwrap(),
    );
}

/// OWL-619's first acceptance, the part before Watch's round: a standard
/// ticket whose plan's last step is a person's is designed, reviewed and
/// built, its Build given the person's steps and passing them; it is
/// delivered with its owed steps under "For a person" in the pull request
/// and in the DELIVERY report; under `[forge] merge = "all"`, watch reads
/// it green and leaves it to a person, told once, and merges nothing.
#[test]
fn a_plan_whose_last_step_is_a_person_s_is_built_delivered_and_held_from_merging() {
    let bench = Bench::with(
        "maintainer",
        "assignee = \"maintainer\"\n",
        "[forge]\nmerge = \"all\"\n",
    );
    bench.add_labels(&["variant:standard"]);
    let replies = vec![
        bench.read(&designed(PERSON_PLAN)),
        bench.read(&reviewed(&[])),
        bench.read(&reviewed(&[])),
        bench.reply(Some("Hello"), Some(OWING)),
    ];
    let (outcome, printed) = bench.run(replies, None);
    let line = outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let held = "the ticket owes 3 steps a person checks before the merge, listed under \"For a \
                person\" in its pull request";
    assert!(
        line.to_string().ends_with(&format!(
            "Owlshift will not merge it: {held}. Review and merge it yourself."
        )),
        "{line}"
    );
    // The Build was given the person's steps, and its owed step is kept
    // with the plan.
    let briefs = bench.briefs();
    assert_eq!(briefs[3].role, Role::Build);
    let given = &briefs[3].plan.as_ref().unwrap().plan.steps;
    let by: Vec<StepBy> = given.iter().map(|step| step.by).collect();
    assert_eq!(
        by,
        [StepBy::Agent, StepBy::Person, StepBy::Agent, StepBy::Person]
    );
    let kept = bench.record().plan.unwrap();
    assert_eq!(kept.owed, [NonZeroU32::new(3).unwrap()]);
    // The PLAN comment lists the person's steps apart.
    let pointer = bench.plan_pointer();
    assert!(
        pointer.contains("**For a person:** step 2, before the merge; step 4, before the merge."),
        "{pointer}"
    );
    // The pull request lists the owed steps after the agent's text.
    let body = bench.github.created.lock().unwrap()[0]["body"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        body.starts_with("Says hello. Gate: green.\n\n## For a person\n\n")
            && body.contains(
                "- **Step 2, before the merge, a person's:** Open GREETING.md on staging in a \
                 browser, which no run can reach. Result to report: It reads Hello.\n- **Step \
                 3, before the merge, left for a person's result:** Note the staging check in \
                 the README.\n- **Step 4, before the merge, a person's:**"
            ),
        "{body}"
    );
    let report = bench
        .comments()
        .into_iter()
        .rfind(|body| body.starts_with("[owlshift] DELIVERY"))
        .unwrap();
    assert!(
        report.contains(&format!(
            "Owlshift will not merge [#1](https://github.com/demo/project/pull/1): {held}."
        )) && report.contains("\n- **Step 4, before the merge, a person's:** Ask the team"),
        "{report}"
    );

    let clock = bench.clock.clone();
    let mut pass = 0;
    let (_, lines, printed) = bench.watch(Vec::new(), &mut |_| {
        pass += 1;
        later(&clock);
        pass < 4
    });
    assert!(
        bench.github.merges.lock().unwrap().is_empty(),
        "{lines}\n{printed}"
    );
    let waits: Vec<Value> = data_of(&bench.events(), EventKind::Decision, "watch")
        .into_iter()
        .filter(|data| data["watch"] == "merge_waits")
        .collect();
    assert_eq!(waits.len(), 1, "{waits:?}");
    assert_eq!(
        (&waits[0]["blocker"], &waits[0]["reason"]),
        (&json!("person_steps"), &json!(held))
    );
}

/// A Build that meets a step only a person can do outside any plan asks it
/// under `person_step`, which goes to the decider with no resolver run, at
/// the level `all` too (OWL-619).
#[test]
fn a_person_s_step_met_outside_the_plan_is_asked_of_the_decider() {
    let bench = Bench::new(true);
    bench.policy("resolver_delegation = \"all\"");
    let asked = r#"{"format":16,"status":"questions",
"summary":"Added GREETING.md; its look on staging needs a person.",
"questions":[{"id":"Q1","category":"person_step",
 "context":"Staging sits behind a login no run holds.",
 "text":"Open GREETING.md on staging: does it read Hello?",
 "recommendation":{"why":"It reads Hello, as the file says."}}]}"#;
    let (outcome, printed) = bench.run(vec![bench.reply(Some("Hello"), Some(asked))], None);
    assert!(
        matches!(&outcome, Err(Stop::NeedsInput { posted: Ok(round), .. }) if round.get() == 1),
        "{outcome:?}\n{printed}"
    );
    assert_eq!(bench.roles(), [Role::Build]);
    let round = bench.marked("[owlshift] QUESTIONS");
    assert!(
        round.contains("Open GREETING.md on staging: does it read Hello?"),
        "{round}"
    );
}
