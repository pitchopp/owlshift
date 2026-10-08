//! Round trips and rejections for every contract.

use std::fmt::Debug;
use std::fs;
use std::num::NonZeroU32;
use std::path::PathBuf;

use owlshift_contracts::ContractError;
use owlshift_contracts::brief::{
    Brief, MAX_CHOICE_BYTES, MAX_REFUSED_CHOICES, MAX_RESULT_REFUSAL_BYTES, ThreadEntry,
};
use owlshift_contracts::comment::{Footer, Header, MarkedComment, MarkerKind};
use owlshift_contracts::config::{Admit, PersonalConfig, ProjectConfig, peek_requires};
use owlshift_contracts::event::Event;
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::refs::{
    AskKind, BuildRefusal, Claim, PersistedState, TicketQuestions, claim_ref, ticket_ref,
};
use owlshift_contracts::result::{
    AnswerClass, DecisionsRefusal, RefusedChoice, Resolution, RunResult, Status,
};
use serde_json::{Value, json};

/// Artifact paths that could leave the worktree, on any platform.
const BAD_PATHS: [&str; 13] = [
    "",
    "/etc/passwd",
    "../../secrets",
    "plans/../../x",
    "plans/..",
    "a//b",
    "a/",
    "C:/Windows",
    "C:plan.md",
    "\\\\server\\share\\x",
    "\\Windows",
    "plans\\..\\..\\x",
    "plan.md:stream",
];
const GOOD_PATHS: [&str; 5] = [
    "plan.md",
    ".owlshift/plan.md",
    "./plan.md",
    "..plan/notes.md",
    "a/.../b",
];

fn fixture(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    // A checkout may turn line endings into CRLF; the tests assume LF.
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .replace("\r\n", "\n")
}

/// Parses, renders, parses again, and requires the two values to be equal.
fn round_trip<T: PartialEq + Debug>(
    name: &str,
    parse: fn(&str) -> Result<T, ContractError>,
    render: fn(&T) -> String,
) -> T {
    let first = parse(&fixture(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
    let second = parse(&render(&first)).unwrap_or_else(|e| panic!("{name} rendered: {e}"));
    assert_eq!(first, second, "{name} changed through a round trip");
    first
}

/// Requires `result` to fail with an error whose message contains `needle`.
fn rejects<T: Debug>(case: &str, result: Result<T, ContractError>, needle: &str) {
    match result {
        Ok(value) => panic!("{case}: accepted {value:?}"),
        Err(error) => {
            let message = error.to_string();
            assert!(
                message.contains(needle),
                "{case}: expected {needle:?} in {message:?}"
            );
        }
    }
}

/// A JSON fixture, changed by `edit`, as text.
fn edited(name: &str, edit: impl FnOnce(&mut Value)) -> String {
    let mut value: Value = serde_json::from_str(&fixture(name)).unwrap();
    edit(&mut value);
    value.to_string()
}

/// A TOML fixture with `from` replaced by `to`.
fn replaced(name: &str, from: &str, to: &str) -> String {
    let text = fixture(name);
    assert!(text.contains(from), "{name} does not contain {from:?}");
    text.replacen(from, to, 1)
}

#[test]
fn every_contract_round_trips() {
    let result = round_trip("result-sample.json", RunResult::parse, RunResult::render);
    assert_eq!(result.status, Status::Questions);
    assert_eq!(result.questions[0].id.as_str(), "Q1");
    assert!(result.pr.is_none());
    assert!(result.verdicts.is_empty());
    let checked = round_trip(
        "result-answer-check.json",
        RunResult::parse,
        RunResult::render,
    );
    assert_eq!(checked.verdicts[0].class, AnswerClass::Unanswered);
    assert_eq!(checked.verdicts[0].reply, None);
    let counter = round_trip(
        "result-counter-question.json",
        RunResult::parse,
        RunResult::render,
    );
    assert_eq!(counter.verdicts[0].class, AnswerClass::CounterQuestion);
    assert!(
        counter.verdicts[0]
            .reply
            .as_deref()
            .is_some_and(|reply| reply.starts_with("\"Local\" is the reader's"))
    );

    let resolved = round_trip("result-resolver.json", RunResult::parse, RunResult::render);
    assert!(matches!(
        &resolved.resolutions[..],
        [Resolution::Decided { .. }, Resolution::PassedOn { .. }]
    ));
    assert_eq!(resolved.resolutions[1].question().as_str(), "Q3");

    let brief = round_trip("brief.json", Brief::parse, Brief::render);
    assert!(matches!(
        brief.thread.last(),
        Some(ThreadEntry::Decision { question, .. }) if question.category == "naming"
    ));
    // A decision is not an ask: the latest ask is still the re-ask of Q2.
    assert_eq!(brief.latest_ask().map(|(_, q)| q.len()), Some(1));
    let event = round_trip("event.json", Event::parse, Event::render);
    // One line of the event log reads back as the same event.
    let line = event.render_line();
    assert!(!line.contains('\n'), "{line}");
    assert_eq!(Event::parse(&line).unwrap(), event);
    round_trip("claim.json", Claim::parse, Claim::render);
    round_trip(
        "ticket-state.json",
        PersistedState::parse,
        PersistedState::render,
    );
    let asked = round_trip(
        "ticket-questions.json",
        TicketQuestions::parse,
        TicketQuestions::render,
    );
    assert_eq!(asked.asks[1].kind, AskKind::Reask);
    assert!(matches!(
        asked.asks[1].entry(),
        ThreadEntry::Reask { questions, .. } if questions.len() == 1
    ));
    assert_eq!(asked.asks[0].verdicts[1].class, AnswerClass::Partial);
    assert!(asked.asks[1].verdicts.is_empty());
    // Build's kept refusal tells the advice for its typed choices first,
    // then a list an older format kept as text, then the latest reason
    // (OWL-192, OWL-195, OWL-198); the typed choices go in the brief's
    // `refused_choices`.
    let kept = asked.build_refusal.as_ref().unwrap();
    assert!(kept.holds());
    assert_eq!(
        kept.choices,
        [RefusedChoice {
            question: "Which tone".to_owned(),
            recorded: "Formal".to_owned()
        }]
    );
    let told = kept.told();
    assert!(told.starts_with("invalid result.json: decisions are given but"));
    assert!(told.contains("are in `refused_choices`, oldest first\n\nEarlier results were refused too, their choices quoted here and not in `refused_choices`: invalid result.json: decisions are given but the run's role is build. The choices listed: \"Which file\" (recorded: \"GREETING.md\")\n\nA later result was refused too: "), "{told}");
    assert!(!told.contains("Which tone"), "{told}");
    assert!(told.ends_with("A later result was refused too: invalid result.json: status is done, but the previous run's decisions were refused"));
    assert!(matches!(
        asked.decisions[0].entry(),
        ThreadEntry::Decision { decision, .. } if decision.starts_with("GREETING.md")
    ));
    // A kept decision never moves the time answers are measured against.
    assert_eq!(
        asked.answers_after(),
        Some("2026-09-28T11:00:00Z".parse().unwrap())
    );
    round_trip("footer.json", Footer::parse_payload, |f| {
        serde_json::to_string(f).unwrap()
    });

    let project = round_trip("owlshift.toml", ProjectConfig::parse, ProjectConfig::render);
    assert_eq!(project.tracker.admit, Admit::Label("agent".into()));
    assert_eq!(project.stack.gate, ["make lint", "make test"]);
    round_trip(
        "personal.toml",
        PersonalConfig::parse,
        PersonalConfig::render,
    );
}

#[test]
fn result_rejections() {
    let parse = |edit: fn(&mut Value)| RunResult::parse(&edited("result-sample.json", edit));
    rejects(
        "unknown field",
        parse(|v| v["extra"] = json!(1)),
        "unknown field `extra`",
    );
    rejects(
        "unknown nested field",
        parse(|v| v["questions"][0]["extra"] = json!(1)),
        "unknown field `extra`",
    );
    rejects(
        "unknown artifact",
        parse(|v| v["artifacts"]["notes"] = json!("n.md")),
        "unknown field `notes`",
    );
    rejects(
        "bad status",
        parse(|v| v["status"] = json!("maybe")),
        "unknown variant `maybe`",
    );
    rejects(
        "question id with a leading zero",
        parse(|v| v["questions"][0]["id"] = json!("Q01")),
        "invalid question id",
    );
    rejects(
        "lower-case question id",
        parse(|v| v["questions"][0]["id"] = json!("q1")),
        "invalid question id",
    );
    rejects(
        "questions out of order",
        parse(|v| v["questions"][0]["id"] = json!("Q2")),
        "out of order",
    );
    rejects(
        "questions status without a question",
        parse(|v| v["questions"] = json!([])),
        "no question",
    );
    rejects(
        "pr without done",
        parse(|v| v["pr"] = json!({ "branch": "b", "title": "t", "body": "b" })),
        "pr is given",
    );
    rejects(
        "newer format with unknown fields",
        parse(|v| {
            v["format"] = json!(8);
            v["confidence"] = json!(0.9);
        }),
        "upgrade Owlshift",
    );
    rejects(
        "format 5, before Build's decisions were refused",
        parse(|v| v["format"] = json!(5)),
        "unknown format 5",
    );
    rejects(
        "format 4, before a decision carried the resolver's category",
        parse(|v| v["format"] = json!(4)),
        "unknown format 4",
    );
    rejects(
        "format 3, before result.json carried resolutions",
        parse(|v| v["format"] = json!(3)),
        "unknown format 3",
    );
    rejects(
        "format 2, before a verdict carried its reply",
        parse(|v| v["format"] = json!(2)),
        "unknown format 2",
    );
    rejects(
        "format 1, before result.json carried verdicts",
        parse(|v| v["format"] = json!(1)),
        "unknown format 1",
    );
    rejects(
        "format 0",
        parse(|v| v["format"] = json!(0)),
        "unknown format 0",
    );
    rejects(
        "format as a string",
        parse(|v| v["format"] = json!("1")),
        "invalid type",
    );
    rejects(
        "missing format",
        parse(|v| {
            v.as_object_mut().unwrap().remove("format");
        }),
        "missing field `format`",
    );
    rejects(
        "truncated document",
        RunResult::parse(r#"{"format": 8, "status""#),
        "upgrade Owlshift",
    );
    rejects(
        "truncated document",
        RunResult::parse(r#"{"format": 7, "status""#),
        "EOF",
    );
    let newer = edited("result-sample.json", |v| v["format"] = json!(8));
    assert!(matches!(
        RunResult::parse(&newer),
        Err(ContractError::NewerFormat {
            found: 8,
            supported: 7,
            ..
        })
    ));
    // Verdicts: known classes, in question order, each with a reason, and
    // with `done` only.
    let verdicts =
        |edit: fn(&mut Value)| RunResult::parse(&edited("result-answer-check.json", edit));
    rejects(
        "unknown class",
        verdicts(|v| v["verdicts"][0]["class"] = json!("settled")),
        "unknown variant `settled`",
    );
    rejects(
        "unknown field in a verdict",
        verdicts(|v| v["verdicts"][0]["extra"] = json!(1)),
        "unknown field `extra`",
    );
    rejects(
        "verdict without a reason",
        verdicts(|v| v["verdicts"][0]["reason"] = json!(" \t\n")),
        "the verdict for Q2 has no reason",
    );
    rejects(
        "verdict repeated",
        verdicts(|v| {
            let first = v["verdicts"][0].clone();
            v["verdicts"].as_array_mut().unwrap().push(first);
        }),
        "the verdict for Q2 is repeated or out of order",
    );
    rejects(
        "verdicts out of order",
        verdicts(|v| {
            let mut q1 = v["verdicts"][0].clone();
            q1["question"] = json!("Q1");
            v["verdicts"].as_array_mut().unwrap().push(q1);
        }),
        "the verdict for Q1 is repeated or out of order",
    );
    rejects(
        "verdicts without done",
        verdicts(|v| v["status"] = json!("blocked")),
        "verdicts are given but status is not done",
    );
    // A reply goes with a counter-question, always and only, and is not
    // blank: the runner posts it as written.
    let counter =
        |edit: fn(&mut Value)| RunResult::parse(&edited("result-counter-question.json", edit));
    rejects(
        "counter-question without a reply",
        counter(|v| {
            v["verdicts"][0].as_object_mut().unwrap().remove("reply");
        }),
        "the verdict for Q2 is a counter-question without a reply",
    );
    rejects(
        "counter-question with a null reply",
        counter(|v| v["verdicts"][0]["reply"] = Value::Null),
        "invalid type: null",
    );
    rejects(
        "blank reply",
        counter(|v| v["verdicts"][0]["reply"] = json!(" \t\n")),
        "the verdict for Q2 has a blank reply",
    );
    rejects(
        "reply on an answered question",
        counter(|v| v["verdicts"][0]["class"] = json!("answered")),
        "the verdict for Q2 has a reply but is not a counter-question",
    );
    rejects(
        "reply on an unanswered question",
        verdicts(|v| v["verdicts"][0]["reply"] = json!("Here is what it means.")),
        "the verdict for Q2 has a reply but is not a counter-question",
    );
    // Artifact paths come from a model: they must stay inside the worktree.
    for path in BAD_PATHS {
        let input = edited("result-sample.json", |v| {
            v["artifacts"]["plan"] = json!(path)
        });
        rejects(path, RunResult::parse(&input), "invalid relative path");
    }
    for path in GOOD_PATHS {
        let input = edited("result-sample.json", |v| {
            v["artifacts"]["report"] = json!(path)
        });
        let result = RunResult::parse(&input).unwrap_or_else(|e| panic!("{path}: {e}"));
        assert_eq!(result.artifacts.report.unwrap().as_str(), path);
    }
    // Deserializing the type directly still refuses another format.
    let direct = serde_json::from_str::<RunResult>(&newer).unwrap_err();
    assert!(direct.to_string().contains("upgrade Owlshift"));
}

/// Verdicts against the brief of the run: from the answer check only, one for
/// each question of the thread's latest ask (OWL-23).
#[test]
fn result_against_the_brief() {
    // The fixture is Build's brief: an answer check's carries no refused
    // result.
    let brief = |edit: fn(&mut Value)| {
        let input = edited("brief.json", |v| {
            v.as_object_mut().unwrap().remove("result_refusal");
            edit(v);
        });
        Brief::parse(&input).unwrap()
    };
    let answer_check = brief(|v| v["role"] = json!("answer_check"));
    let checked =
        |edit: fn(&mut Value)| RunResult::parse(&edited("result-answer-check.json", edit)).unwrap();

    // brief.json's latest ask is the re-ask of Q2 alone, before a comment.
    for class in ["answered", "partial", "unanswered", "counter_question"] {
        let mut value: Value = serde_json::from_str(&fixture("result-answer-check.json")).unwrap();
        value["verdicts"][0]["class"] = json!(class);
        if class == "counter_question" {
            value["verdicts"][0]["reply"] = json!("It means the reader's time zone.");
        }
        let result = RunResult::parse(&value.to_string()).unwrap();
        result.validate_against(&answer_check).unwrap();
    }
    rejects(
        "a verdict on a question the re-ask settled earlier",
        checked(|v| {
            let mut q1 = v["verdicts"][0].clone();
            q1["question"] = json!("Q1");
            v["verdicts"].as_array_mut().unwrap().insert(0, q1);
        })
        .validate_against(&answer_check),
        "the verdict for Q1 names a question the latest ask of round 1 did not ask",
    );
    rejects(
        "a done answer check without verdicts",
        checked(|v| v["verdicts"] = json!([])).validate_against(&answer_check),
        "no verdict for Q2 of round 1",
    );
    // A later round is the latest ask, whole: Q1 and Q2 of round 2.
    let round_two = brief(|v| {
        v["role"] = json!("answer_check");
        let mut round = v["thread"][0].clone();
        round["round"] = json!(2);
        v["thread"].as_array_mut().unwrap().push(round);
    });
    rejects(
        "a verdict missing for the latest round",
        checked(|_| {}).validate_against(&round_two),
        "no verdict for Q1 of round 2",
    );
    rejects(
        "an answer check on a thread that asks nothing",
        checked(|_| {}).validate_against(&brief(|v| {
            v["role"] = json!("answer_check");
            v["thread"]
                .as_array_mut()
                .unwrap()
                .retain(|e| e["type"] == "comment");
        })),
        "asks no question",
    );
    // An answer check that did not finish gives no verdict, and needs none.
    checked(|v| {
        v["status"] = json!("failed");
        v["verdicts"] = json!([]);
    })
    .validate_against(&answer_check)
    .unwrap();

    // Any other role gives no verdict.
    let build = brief(|_| {});
    rejects(
        "verdicts from the build role",
        checked(|_| {}).validate_against(&build),
        "verdicts are given but the run's role is build, not answer_check",
    );
}

/// The build role takes no decision of its own, whatever its status
/// (OWL-176): a choice it would record is asked, and the reason it is told
/// says so. A Build run that only follows the decider's answers or a kept
/// decision lists nothing, and passes.
#[test]
fn decisions_against_the_brief() {
    let build = Brief::parse(&fixture("brief.json")).unwrap();
    for status in ["done", "questions", "blocked", "premise_false", "failed"] {
        let result = RunResult::parse(&edited("result-sample.json", |v| {
            v["status"] = json!(status)
        }))
        .unwrap();
        assert!(!result.decisions.is_empty());
        let error = result.validate_against(&build).unwrap_err().to_string();
        for needle in [
            "decisions are given but the run's role is build",
            "ask each of these choices as a question",
            "`scope` when its answer changes what the ticket delivers",
            "they stay open even though the work is committed",
            "the next run's result is refused if it is `done`",
            r#"The choices listed: "Test framework" (recorded: "Reuse the existing one")"#,
        ] {
            assert!(
                error.contains(needle),
                "{status}: {needle:?} not in {error}"
            );
        }
    }
    let mut followed = RunResult::parse(&fixture("result-sample.json")).unwrap();
    followed.decisions.clear();
    followed.validate_against(&build).unwrap();

    // The Build run told that its predecessor's decisions were refused may
    // end with any status but `done` (OWL-186).
    let told = Brief::parse(&edited("brief.json", |v| {
        v["decisions_refused"] = json!(true);
    }))
    .unwrap();
    for status in ["done", "questions", "blocked", "premise_false", "failed"] {
        let result = RunResult::parse(&edited("result-sample.json", |v| {
            v["status"] = json!(status);
            v.as_object_mut().unwrap().remove("decisions");
        }))
        .unwrap();
        let checked = result.validate_against(&told);
        if status == "done" {
            rejects(
                "a done after refused decisions",
                checked,
                "status is done, but the previous run's decisions were refused",
            );
        } else {
            checked.unwrap_or_else(|e| panic!("{status}: {e}"));
        }
    }

    // The refusal says when it is the build role's decisions, which its next
    // run is told of; a result that lists some is refused for them first,
    // whatever else it breaks.
    let kind = |input: &str, brief: &Brief| RunResult::parse_against(input, brief).unwrap_err();
    let misnumbered = |decisions: bool| {
        edited("result-sample.json", |v| {
            v["questions"][0]["id"] = json!("Q2");
            if !decisions {
                v.as_object_mut().unwrap().remove("decisions");
            }
        })
    };
    // Its kind tells a result that lists choices from a refused `done`
    // (OWL-195).
    // A result that lists choices carries them typed, kept apart from the
    // advice, each text cut to its bound (OWL-198).
    let listed = DecisionsRefusal::Listed(
        RunResult::parse(&fixture("result-sample.json"))
            .unwrap()
            .decisions
            .iter()
            .map(|d| RefusedChoice {
                question: d.question.clone(),
                recorded: d.decision.clone(),
            })
            .collect(),
    );
    let refused = kind(&misnumbered(true), &build);
    assert_eq!(
        refused.decisions.as_ref(),
        Some(&listed),
        "{}",
        refused.error
    );
    let long = edited("result-sample.json", |v| {
        v["decisions"][0]["question"] = json!("é".repeat(MAX_CHOICE_BYTES));
    });
    let Some(DecisionsRefusal::Listed(cut)) = kind(&long, &build).decisions else {
        panic!("not a listed refusal");
    };
    assert!(
        cut[0].question.len() <= MAX_CHOICE_BYTES && cut[0].question.ends_with("é [cut]"),
        "{:?}",
        cut[0]
    );
    let full = refused.error.to_string();
    assert!(
        full.contains("decisions are given but") && full.contains("filed under `scope`"),
        "{full}"
    );
    let refused = kind(&misnumbered(false), &build);
    assert_eq!(refused.decisions, None, "{}", refused.error);
    let done = edited("result-sample.json", |v| {
        v["status"] = json!("done");
        v.as_object_mut().unwrap().remove("decisions");
    });
    assert_eq!(kind(&done, &told).decisions, Some(DecisionsRefusal::Done));
    RunResult::parse_against(&done, &build).unwrap();
    // A told run that lists choices again gets a short advice, its next
    // run being told the first list's full advice first; the choices are
    // quoted the same way.
    let again = kind(&misnumbered(true), &told);
    assert_eq!(again.decisions, Some(listed));
    let short = again.error.to_string();
    assert!(
        short.contains("decisions are given again") && !short.contains("filed under `scope`"),
        "{short}"
    );
    let quoted = |reason: &str| {
        reason
            .split_once("The choices listed: ")
            .unwrap()
            .1
            .to_owned()
    };
    assert_eq!(quoted(&short), quoted(&full));
}

/// A Build run told of refused choices names, when it asks, the question
/// that asks each of them, by its place in the brief's `refused_choices`
/// (OWL-193): a retry that asks some and drops others is refused.
#[test]
fn refused_choices_asked_against_the_brief() {
    let held = |choices: Vec<Value>| {
        Brief::parse(&edited("brief.json", |v| {
            v["decisions_refused"] = json!(true);
            v["refused_choices"] = json!(choices);
        }))
        .unwrap()
    };
    let choice = |question: &str| json!({ "question": question, "recorded": "r" });
    let two = held(vec![
        choice("Every key or only the required ones"),
        choice("Where the README lists it"),
    ]);
    let question = |id: &str| json!({ "id": id, "category": "scope", "context": "c", "text": "t" });
    let named = |pairs: &[(u32, &str)]| {
        json!(
            pairs
                .iter()
                .map(|(choice, question)| json!({ "choice": choice, "question": question }))
                .collect::<Vec<_>>()
        )
    };
    let result = |status: &str, questions: &[&str], asked: Option<Value>| {
        edited("result-sample.json", |v| {
            let object = v.as_object_mut().unwrap();
            object.remove("decisions");
            object.insert("status".to_owned(), json!(status));
            object.insert(
                "questions".to_owned(),
                json!(questions.iter().map(|id| question(id)).collect::<Vec<_>>()),
            );
            if let Some(asked) = asked.clone() {
                object.insert("refused_choices_asked".to_owned(), asked);
            }
        })
    };
    let checked = |input: &str, brief: &Brief| RunResult::parse_against(input, brief).map(|_| ());

    // Every place named, each by its own question or several by one: the
    // runner checks the declaration, not that the question asks the choice.
    for asked in [
        named(&[(1, "Q1"), (2, "Q2")]),
        named(&[(1, "Q1"), (2, "Q1")]),
    ] {
        for status in ["questions", "premise_false"] {
            checked(&result(status, &["Q1", "Q2"], Some(asked.clone())), &two)
                .unwrap_or_else(|e| panic!("{status} {asked}: {}", e.error));
        }
    }
    // A result that asks nothing names nothing, and keeps the hold.
    for status in ["blocked", "failed", "premise_false"] {
        checked(&result(status, &[], None), &two).unwrap();
    }

    // A retry that asks one of the two, or every one without naming them.
    let one = checked(
        &result("questions", &["Q1"], Some(named(&[(1, "Q1")]))),
        &two,
    )
    .unwrap_err();
    assert_eq!(one.decisions, None);
    let reason = one.error.to_string();
    for needle in [
        "the refused choices are not all named as asked",
        "names in `refused_choices_asked`, for each in order, its place from 1",
        ". Not named: 2. The choices not named: 2 \"Where the README lists it\"",
    ] {
        assert!(reason.contains(needle), "{needle:?} not in {reason}");
    }
    rejects(
        "a retry asking every choice without naming them",
        checked(&result("premise_false", &["Q1", "Q2"], None), &two).map_err(|r| r.error),
        "Not named: 1, 2.",
    );
    for (asked, needle) in [
        (
            named(&[(1, "Q1"), (1, "Q2"), (2, "Q2")]),
            "Repeated, out of order or past the 2 given: 1",
        ),
        (
            named(&[(2, "Q1"), (1, "Q2")]),
            "Repeated, out of order or past the 2 given: 1",
        ),
        (
            named(&[(1, "Q1"), (2, "Q1"), (3, "Q2")]),
            "Repeated, out of order or past the 2 given: 3",
        ),
        (
            named(&[(1, "Q1"), (2, "Q3")]),
            "refused choice 2 is named as asked by Q3, which the result does not ask",
        ),
    ] {
        rejects(
            "a misnamed refused choice",
            checked(&result("questions", &["Q1", "Q2"], Some(asked)), &two).map_err(|r| r.error),
            needle,
        );
    }
    for status in ["blocked", "premise_false"] {
        rejects(
            "a refused choice named by a result that asks nothing",
            checked(&result(status, &[], Some(named(&[(1, "Q1")]))), &two).map_err(|r| r.error),
            "refused choices are named as asked but the result asks no question",
        );
    }
    // Listed decisions are refused for them first.
    let decided = edited("result-sample.json", |_| {});
    assert!(matches!(
        RunResult::parse_against(&decided, &two)
            .unwrap_err()
            .decisions,
        Some(DecisionsRefusal::Listed(_))
    ));

    // A brief with no typed choice, a hold kept as text included, has no
    // place to name.
    let text_hold = Brief::parse(&edited("brief.json", |v| {
        v["decisions_refused"] = json!(true)
    }))
    .unwrap();
    let build = Brief::parse(&fixture("brief.json")).unwrap();
    for brief in [&text_hold, &build] {
        checked(&result("questions", &["Q1"], None), brief).unwrap();
        rejects(
            "a refused choice named without one in the brief",
            checked(
                &result("questions", &["Q1"], Some(named(&[(1, "Q1")]))),
                brief,
            )
            .map_err(|r| r.error),
            "refused choices are named as asked but the brief gives none",
        );
    }

    // The instruction and the places come first, within the kept reason's
    // cut, whatever the choices' length.
    let full = held(vec![
        choice(&"x".repeat(MAX_CHOICE_BYTES));
        MAX_REFUSED_CHOICES
    ]);
    let none = checked(&result("questions", &["Q1"], None), &full)
        .unwrap_err()
        .error
        .to_string();
    let places = (1..=MAX_REFUSED_CHOICES)
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let head = format!("Not named: {places}. The choices not named: 1 \"x");
    let at = none.find(&head).unwrap_or_else(|| panic!("{none}"));
    assert!(at + head.len() < MAX_RESULT_REFUSAL_BYTES, "{at}");
}

/// The resolver's brief carries the questions it settles, without the
/// category the raising run gave them (OWL-144), and its result one
/// resolution for each of them, no more (OWL-138): a decision on a question
/// it was not given, such as an always-human one the runner kept from it, is
/// refused.
#[test]
fn resolutions_against_the_brief() {
    let question = |id: &str| json!({ "id": id, "context": "c", "text": "t" });
    let resolver = |resolve: Value| {
        Brief::parse(
            &edited("brief.json", |v| {
                v["role"] = json!("resolver");
                v["permissions"]["level"] = json!("read_only");
                v.as_object_mut().unwrap().remove("result_refusal");
            })
            .replace(
                "\"role\":\"resolver\"",
                &format!("\"role\":\"resolver\",\"resolve\":{resolve}"),
            ),
        )
    };
    let given = resolver(json!([question("Q1"), question("Q3")])).unwrap();
    assert_eq!(given.resolve.len(), 2);
    let resolved =
        |edit: fn(&mut Value)| RunResult::parse(&edited("result-resolver.json", edit)).unwrap();
    resolved(|_| {}).validate_against(&given).unwrap();
    rejects(
        "a decision on a question the resolver was not given",
        resolved(|v| v["resolutions"][1]["question"] = json!("Q2")).validate_against(&given),
        "the resolution of Q2 names a question the resolver was not given",
    );
    rejects(
        "a question left without a resolution",
        resolved(|v| {
            v["resolutions"].as_array_mut().unwrap().pop();
        })
        .validate_against(&given),
        "no resolution for Q3",
    );
    // A resolver that did not finish resolves nothing, and needs not.
    resolved(|v| {
        v["status"] = json!("failed");
        v["resolutions"] = json!([]);
    })
    .validate_against(&given)
    .unwrap();
    rejects(
        "resolutions from the build role",
        resolved(|_| {}).validate_against(&Brief::parse(&fixture("brief.json")).unwrap()),
        "resolutions are given but the run's role is build, not resolver",
    );

    // The brief: questions to resolve in a resolver's brief only, at least
    // one, in increasing order.
    rejects(
        "a resolver's brief with nothing to resolve",
        resolver(json!([])),
        "a resolver's brief has no question to resolve",
    );
    rejects(
        "questions to resolve out of order",
        resolver(json!([question("Q3"), question("Q1")])),
        "question Q1 to resolve is repeated or out of order",
    );
    rejects(
        "a question to resolve with the raising run's category",
        resolver(json!([{ "id": "Q1", "category": "naming", "context": "c", "text": "t" }])),
        "unknown field `category`",
    );
    rejects(
        "questions to resolve in a build brief",
        Brief::parse(&edited("brief.json", |v| {
            v["resolve"] = json!([{ "id": "Q1", "context": "c", "text": "t" }]);
        })),
        "questions to resolve are given but the role is build, not resolver",
    );

    // The result's own rules.
    let parse = |edit: fn(&mut Value)| RunResult::parse(&edited("result-resolver.json", edit));
    rejects(
        "resolutions without done",
        parse(|v| v["status"] = json!("failed")),
        "resolutions are given but status is not done",
    );
    rejects(
        "resolutions out of order",
        parse(|v| v["resolutions"][1]["question"] = json!("Q1")),
        "the resolution of Q1 is repeated or out of order",
    );
    rejects(
        "a decision without the resolver's category",
        parse(|v| {
            v["resolutions"][0]
                .as_object_mut()
                .unwrap()
                .remove("category");
        }),
        "missing field `category`",
    );
    // The label's form is the runner's to judge, per question
    // (`GatePolicy::route_decided`): any string parses.
    for category in ["", "Data-Loss", "sécurité"] {
        let input = edited("result-resolver.json", |v| {
            v["resolutions"][0]["category"] = json!(category);
        });
        RunResult::parse(&input).unwrap_or_else(|e| panic!("{category:?}: {e}"));
    }
    rejects(
        "a decision without a basis",
        parse(|v| v["resolutions"][0]["basis"] = json!("  ")),
        "the resolution of Q1 has no basis",
    );
    rejects(
        "a pass without a reason",
        parse(|v| v["resolutions"][1]["reason"] = json!("")),
        "the resolution of Q3 has no reason",
    );
    rejects(
        "a pass with a decision",
        parse(|v| v["resolutions"][1]["decision"] = json!("d")),
        "unknown field `decision`",
    );
    rejects(
        "an unknown outcome",
        parse(|v| v["resolutions"][0]["outcome"] = json!("guessed")),
        "unknown variant `guessed`",
    );
}

#[test]
fn brief_rejections() {
    let parse = |edit: fn(&mut Value)| Brief::parse(&edited("brief.json", edit));
    rejects(
        "unknown field in a tagged thread entry",
        parse(|v| v["thread"][1]["extra"] = json!(1)),
        "unknown field `extra`",
    );
    rejects(
        "unknown thread entry type",
        parse(|v| v["thread"][1]["type"] = json!("note")),
        "unknown variant `note`",
    );
    rejects(
        "ticket id with a slash",
        parse(|v| v["ticket"]["id"] = json!("OWL/1")),
        "invalid ticket id",
    );
    rejects(
        "ticket id with dots",
        parse(|v| v["ticket"]["id"] = json!("..")),
        "invalid ticket id",
    );
    rejects(
        "ticket id with a leading dash",
        parse(|v| v["ticket"]["id"] = json!("-1")),
        "invalid ticket id",
    );
    rejects(
        "round 0",
        parse(|v| v["thread"][0]["round"] = json!(0)),
        "nonzero",
    );
    rejects(
        "questions of a round not starting at Q1",
        parse(|v| {
            v["thread"][0]["questions"]
                .as_array_mut()
                .unwrap()
                .remove(0);
        }),
        "out of order",
    );
    rejects(
        "re-ask naming a question twice",
        parse(|v| {
            let q = v["thread"][2]["questions"][0].clone();
            v["thread"][2]["questions"].as_array_mut().unwrap().push(q);
        }),
        "repeated or out of order",
    );
    rejects(
        "re-ask of a question the round did not ask",
        parse(|v| v["thread"][2]["questions"][0]["id"] = json!("Q3")),
        "was not asked in that round",
    );
    rejects(
        "re-ask of a round that does not exist",
        parse(|v| v["thread"][2]["round"] = json!(2)),
        "not earlier in the thread",
    );
    rejects(
        "re-ask before its round",
        parse(|v| {
            let thread = v["thread"].as_array_mut().unwrap();
            let reask = thread.remove(2);
            thread.insert(0, reask);
        }),
        "not earlier in the thread",
    );
    rejects(
        "rounds not increasing",
        parse(|v| {
            let round = v["thread"][0].clone();
            v["thread"].as_array_mut().unwrap().push(round);
        }),
        "round 1 comes after round 1",
    );
    rejects(
        "bad relation",
        parse(|v| v["thread"][3]["author"]["relation"] = json!("admin")),
        "unknown variant `admin`",
    );
    rejects(
        "bad permission level",
        parse(|v| v["permissions"]["level"] = json!("root")),
        "unknown variant `root`",
    );
    rejects(
        "newer format",
        parse(|v| v["format"] = json!(12)),
        "upgrade Owlshift",
    );
    rejects(
        "format 10, before Build's brief listed the refused choices",
        parse(|v| v["format"] = json!(10)),
        "unknown format 10",
    );
    rejects(
        "format 9, before the resolver's brief carried its refused result",
        parse(|v| v["format"] = json!(9)),
        "unknown format 9",
    );
    rejects(
        "format 8, before Build's brief said its predecessor's decisions were refused",
        parse(|v| v["format"] = json!(8)),
        "unknown format 8",
    );
    rejects(
        "format 6, before the brief carried the refusal of the previous result",
        parse(|v| v["format"] = json!(6)),
        "unknown format 6",
    );
    rejects(
        "format 5, before the brief carried the project's always-human categories",
        parse(|v| v["format"] = json!(5)),
        "unknown format 5",
    );
    rejects(
        "format 4, before the questions to resolve left out their category",
        parse(|v| v["format"] = json!(4)),
        "unknown format 4",
    );
    rejects(
        "format 3, before the brief carried decisions and questions to resolve",
        parse(|v| v["format"] = json!(3)),
        "unknown format 3",
    );
    rejects(
        "format 1, before the brief carried the gate",
        parse(|v| v["format"] = json!(1)),
        "unknown format 1",
    );
    rejects(
        "format 2, before the brief carried the gate failure",
        parse(|v| v["format"] = json!(2)),
        "unknown format 2",
    );
    rejects(
        "unknown field in the gate failure",
        parse(|v| v["gate_failure"]["exit_code"] = json!(1)),
        "unknown field `exit_code`",
    );
    rejects(
        "missing gate",
        parse(|v| {
            v.as_object_mut().unwrap().remove("gate");
        }),
        "missing field `gate`",
    );
    rejects(
        "missing always-human categories",
        parse(|v| {
            v.as_object_mut().unwrap().remove("always_human");
        }),
        "missing field `always_human`",
    );
    // The refusal of the previous result: Build's (OWL-180), the answer
    // check's (OWL-184) and the resolver's (OWL-191), no other role's,
    // within its cap.
    rejects(
        "a refused result in a verifier's brief",
        parse(|v| v["role"] = json!("verify")),
        "a refused result is given but the role is verify, not build, answer_check or resolver",
    );
    Brief::parse(&edited("brief.json", |v| v["role"] = json!("answer_check"))).unwrap();
    Brief::parse(&edited("brief.json", |v| {
        v["role"] = json!("resolver");
        v["resolve"] = json!([{ "id": "Q1", "context": "c", "text": "t" }]);
    }))
    .unwrap();
    rejects(
        "a refused result past its cap",
        parse(|v| v["result_refusal"] = json!("x".repeat(MAX_RESULT_REFUSAL_BYTES + 1))),
        "the refused result's reason exceeds 2048 bytes",
    );
    Brief::parse(&edited("brief.json", |v| {
        v["result_refusal"] = json!("x".repeat(MAX_RESULT_REFUSAL_BYTES));
    }))
    .unwrap();
    // Refused decisions (OWL-186): Build's alone, with the reason that says
    // why.
    rejects(
        "refused decisions in an answer check's brief",
        parse(|v| {
            v["role"] = json!("answer_check");
            v["decisions_refused"] = json!(true);
        }),
        "refused decisions are given but the role is answer_check, not build",
    );
    rejects(
        "refused decisions without the reason",
        parse(|v| {
            v.as_object_mut().unwrap().remove("result_refusal");
            v["decisions_refused"] = json!(true);
        }),
        "refused decisions are given without the refused result's reason",
    );
    Brief::parse(&edited("brief.json", |v| {
        v["decisions_refused"] = json!(true)
    }))
    .unwrap();
    // Refused choices (OWL-198): with the hold, within their bounds.
    let choice = |question: String| json!({ "question": question, "recorded": "" });
    let held_with = |choices: Vec<Value>| {
        Brief::parse(&edited("brief.json", |v| {
            v["decisions_refused"] = json!(true);
            v["refused_choices"] = json!(choices);
        }))
    };
    rejects(
        "refused choices without the hold",
        parse(|v| v["refused_choices"] = json!([{ "question": "Which file", "recorded": "" }])),
        "refused choices are given without `decisions_refused`",
    );
    rejects(
        "too many refused choices",
        held_with(vec![choice(String::new()); MAX_REFUSED_CHOICES + 1]),
        "more than 64 refused choices are given",
    );
    rejects(
        "a refused choice past its cap",
        held_with(vec![
            choice(String::new()),
            choice("x".repeat(MAX_CHOICE_BYTES + 1)),
        ]),
        "refused choice 2 has a text over 512 bytes",
    );
    let full = held_with(vec![
        choice("x".repeat(MAX_CHOICE_BYTES));
        MAX_REFUSED_CHOICES
    ])
    .unwrap();
    assert_eq!(full.refused_choices.len(), MAX_REFUSED_CHOICES);
    // The checkpoint's paths and the result path come from the runner, but
    // are checked the same way as a model's artifact paths (OWL-25).
    for path in BAD_PATHS {
        let input = edited("brief.json", |v| v["checkpoint"]["plan"] = json!(path));
        rejects(path, Brief::parse(&input), "invalid relative path");
    }
    for path in BAD_PATHS {
        let input = edited("brief.json", |v| v["checkpoint"]["ledger"] = json!(path));
        rejects(path, Brief::parse(&input), "invalid relative path");
    }
    for path in BAD_PATHS {
        let input = edited("brief.json", |v| v["result_path"] = json!(path));
        rejects(path, Brief::parse(&input), "invalid relative path");
    }
    for path in GOOD_PATHS {
        let input = edited("brief.json", |v| v["result_path"] = json!(path));
        let result = Brief::parse(&input).unwrap_or_else(|e| panic!("{path}: {e}"));
        assert_eq!(result.result_path.as_str(), path);
    }
}

#[test]
fn event_claim_and_state_rejections() {
    let event = |edit: fn(&mut Value)| Event::parse(&edited("event.json", edit));
    rejects(
        "unknown event kind",
        event(|v| v["kind"] = json!("coffee")),
        "unknown variant `coffee`",
    );
    rejects(
        "unknown event field",
        event(|v| v["extra"] = json!(1)),
        "unknown field `extra`",
    );
    rejects(
        "bad timestamp",
        event(|v| v["at"] = json!("yesterday")),
        "invalid",
    );
    rejects(
        "data not an object",
        event(|v| v["data"] = json!([1])),
        "invalid type",
    );

    // The log is append-only: a line of format 1 still reads, as format 2.
    let old = Event::parse(&edited("event.json", |v| v["format"] = json!(1))).unwrap();
    assert_eq!(old, Event::parse(&fixture("event.json")).unwrap());
    rejects(
        "newer event",
        event(|v| v["format"] = json!(3)),
        "upgrade Owlshift",
    );
    rejects(
        "unknown event format",
        event(|v| v["format"] = json!(0)),
        "unknown format 0",
    );

    let claim = |edit: fn(&mut Value)| Claim::parse(&edited("claim.json", edit));
    rejects(
        "unknown claim field",
        claim(|v| v["pid"] = json!(1)),
        "unknown field `pid`",
    );
    rejects(
        "newer claim",
        claim(|v| v["format"] = json!(2)),
        "upgrade Owlshift",
    );

    let state = |edit: fn(&mut Value)| PersistedState::parse(&edited("ticket-state.json", edit));
    rejects(
        "unknown stage",
        state(|v| v["stage"] = json!("qa")),
        "unknown variant `qa`",
    );
    rejects(
        "unknown waiting reason",
        state(|v| v["waiting"] = json!("lunch")),
        "unknown variant `lunch`",
    );
    rejects(
        "unknown state field",
        state(|v| v["extra"] = json!(1)),
        "unknown field `extra`",
    );

    let asked =
        |edit: fn(&mut Value)| TicketQuestions::parse(&edited("ticket-questions.json", edit));
    rejects(
        "newer questions",
        asked(|v| v["format"] = json!(10)),
        "upgrade Owlshift",
    );
    rejects(
        "older questions",
        asked(|v| v["format"] = json!(1)),
        "unknown format 1",
    );
    rejects(
        "a format-2 document with verdicts",
        asked(|v| v["format"] = json!(2)),
        "format 2 keeps no verdicts",
    );
    rejects(
        "a verdict on a question the ask did not ask",
        asked(|v| {
            v["asks"][1]["verdicts"] =
                json!([{"question": "Q1", "class": "answered", "reason": "r"}])
        }),
        "an ask of round 1 keeps a verdict for Q1, which it did not ask",
    );
    rejects(
        "a repeated verdict",
        asked(|v| v["asks"][0]["verdicts"][1]["question"] = json!("Q1")),
        "the verdict for Q1 is repeated or out of order",
    );
    rejects(
        "a verdict without a reason",
        asked(|v| v["asks"][0]["verdicts"][0]["reason"] = json!(" ")),
        "the verdict for Q1 has no reason",
    );
    rejects(
        "a counter-question kept without its reply",
        asked(|v| v["asks"][0]["verdicts"][1]["class"] = json!("counter_question")),
        "the verdict for Q2 is a counter-question without a reply",
    );
    rejects(
        "unknown ask field",
        asked(|v| v["asks"][0]["extra"] = json!(1)),
        "unknown field `extra`",
    );

    rejects(
        "a format-3 document with decisions",
        asked(|v| v["format"] = json!(3)),
        "format 3 keeps no decisions",
    );
    // A field a later format added is refused in an older one, whatever
    // its value (OWL-184).
    rejects(
        "a format-2 document with decisions",
        asked(|v| {
            v["format"] = json!(2);
            for ask in v["asks"].as_array_mut().unwrap() {
                ask.as_object_mut().unwrap().remove("verdicts");
            }
        }),
        "format 2 keeps no decisions",
    );
    rejects(
        "a format-3 document with a null refusal",
        asked(|v| {
            v["format"] = json!(3);
            v.as_object_mut().unwrap().remove("decisions");
            v["asks"][1]["result_refusal"] = Value::Null;
        }),
        "format 3 keeps no refusal",
    );
    rejects(
        "a format-4 document with a refusal",
        asked(|v| v["format"] = json!(4)),
        "format 4 keeps no refusal",
    );
    rejects(
        "a format-5 document with Build's refusal",
        asked(|v| {
            v["format"] = json!(5);
            v["build_refusal"] = Value::Null;
        }),
        "format 5 keeps no Build refusal",
    );
    // A format-2 document, written before the asks kept their verdicts, is
    // read as the current format without them, and written back as it.
    let format_2 = edited("ticket-questions.json", |v| {
        v["format"] = json!(2);
        v.as_object_mut().unwrap().remove("build_refusal");
        v.as_object_mut().unwrap().remove("resolver_refusal");
        v.as_object_mut().unwrap().remove("decisions");
        for ask in v["asks"].as_array_mut().unwrap() {
            ask.as_object_mut().unwrap().remove("verdicts");
            ask.as_object_mut().unwrap().remove("result_refusal");
        }
    });
    let read = TicketQuestions::parse(&format_2).unwrap();
    assert_eq!(read.asks.len(), 2);
    assert!(read.asks.iter().all(|ask| ask.verdicts.is_empty()));
    assert!(read.render().starts_with("{\n  \"format\": 9,"));
    // A format-3 document, written before the resolver's decisions were
    // kept, is read with none, its verdicts kept.
    let format_3 = edited("ticket-questions.json", |v| {
        v["format"] = json!(3);
        v.as_object_mut().unwrap().remove("build_refusal");
        v.as_object_mut().unwrap().remove("resolver_refusal");
        v.as_object_mut().unwrap().remove("decisions");
        v["asks"][1]
            .as_object_mut()
            .unwrap()
            .remove("result_refusal");
    });
    let read = TicketQuestions::parse(&format_3).unwrap();
    assert!(read.decisions.is_empty());
    assert_eq!(read.asks[0].verdicts.len(), 2);
    assert!(read.render().starts_with("{\n  \"format\": 9,"));
    // A format-4 document, written before an ask kept its check's refusal,
    // is read with none, its decisions kept.
    let format_4 = edited("ticket-questions.json", |v| {
        v["format"] = json!(4);
        v.as_object_mut().unwrap().remove("build_refusal");
        v.as_object_mut().unwrap().remove("resolver_refusal");
        v["asks"][1]
            .as_object_mut()
            .unwrap()
            .remove("result_refusal");
    });
    let read = TicketQuestions::parse(&format_4).unwrap();
    assert!(read.asks.iter().all(|ask| ask.result_refusal.is_none()));
    assert_eq!(read.decisions.len(), 1);
    assert!(read.render().starts_with("{\n  \"format\": 9,"));
    // A format-5 document, written before Build's refusal was kept
    // (OWL-192), is read with none, its asks' refusals kept.
    let format_5 = edited("ticket-questions.json", |v| {
        v["format"] = json!(5);
        v.as_object_mut().unwrap().remove("build_refusal");
        v.as_object_mut().unwrap().remove("resolver_refusal");
    });
    let read = TicketQuestions::parse(&format_5).unwrap();
    assert_eq!(read.build_refusal, None);
    assert!(read.asks[1].result_refusal.is_some());
    assert!(read.render().starts_with("{\n  \"format\": 9,"));
    // Formats 6 and 7 kept Build's refusal in their own shape (OWL-192,
    // OWL-194): the hold's first list in `decisions_reason`, the latest
    // reason, of a kind they did not keep, in `reason`. Each is read as
    // format 8, the latest reason kept as a list when a held one precedes
    // it, so no later refusal drops it, and told as before.
    let held = "invalid result.json: decisions are given. The choices listed: \"Which file\"";
    let latest = "invalid result.json: status is done";
    let legacy = |format: u64, refusal: Value| {
        edited("ticket-questions.json", |v| {
            v["format"] = json!(format);
            if format == 6 {
                v.as_object_mut().unwrap().remove("resolver_refusal");
            }
            v["build_refusal"] = refusal;
        })
    };
    let upgraded = |format: u64, refusal: Value| {
        let read = TicketQuestions::parse(&legacy(format, refusal)).unwrap();
        assert!(read.render().starts_with("{\n  \"format\": 9,"));
        read.build_refusal.unwrap()
    };
    for format in [6, 7] {
        let both = upgraded(
            format,
            json!({"reason": latest, "decisions": true, "decisions_reason": held}),
        );
        assert_eq!(
            (&both.reason, &both.lists[..]),
            (&None, &[held.to_owned(), latest.to_owned()][..])
        );
        assert_eq!(
            both.told(),
            format!("{held}\n\nA later result was refused too: {latest}")
        );
        let started = upgraded(format, json!({"reason": held, "decisions": true}));
        assert_eq!(
            (started.reason, started.lists),
            (None, vec![held.to_owned()])
        );
        let plain = upgraded(
            format,
            json!({"reason": latest, "decisions": false, "decisions_reason": null}),
        );
        assert_eq!(
            (plain.reason.as_deref(), plain.holds()),
            (Some(latest), false)
        );
        // The older shape keeps its own rules.
        rejects(
            "an older Build refusal without its reason",
            TicketQuestions::parse(&legacy(
                format,
                json!({"decisions": true, "decisions_reason": held}),
            )),
            "missing field `reason`",
        );
        rejects(
            "an older Build refusal with a null reason",
            TicketQuestions::parse(&legacy(
                format,
                json!({"reason": null, "decisions": true, "decisions_reason": held}),
            )),
            "invalid type: null",
        );
        rejects(
            "an older Build refusal with format 8's lists",
            TicketQuestions::parse(&legacy(
                format,
                json!({"reason": latest, "decisions": true, "lists": [held]}),
            )),
            "unknown field `lists`",
        );
        rejects(
            "an older refusal of decisions without the hold",
            TicketQuestions::parse(&legacy(
                format,
                json!({"reason": latest, "decisions": false, "decisions_reason": held}),
            )),
            "keeps a refusal of decisions without `decisions`",
        );
        rejects(
            "an older blank Build refusal",
            TicketQuestions::parse(&legacy(format, json!({"reason": " ", "decisions": true}))),
            "Build's kept list of choices 1 is blank",
        );
    }
    // A format-8 document, written before the refused choices were typed
    // (OWL-198), keeps its lists as text, told as before, and is written
    // back as format 9; typed choices are refused in it.
    let format_8 = |refusal: Value| {
        edited("ticket-questions.json", |v| {
            v["format"] = json!(8);
            v["build_refusal"] = refusal;
        })
    };
    let read =
        TicketQuestions::parse(&format_8(json!({"reason": latest, "lists": [held]}))).unwrap();
    assert!(read.render().starts_with("{\n  \"format\": 9,"));
    let kept = read.build_refusal.unwrap();
    assert!(kept.holds() && kept.choices.is_empty() && kept.lists == [held]);
    assert_eq!(
        kept.told(),
        format!("{held}\n\nA later result was refused too: {latest}")
    );
    for typed in [
        json!({"lists": [held], "choices": [{"question": "q", "recorded": "r"}]}),
        json!({"lists": [held], "left_out": 0}),
    ] {
        rejects(
            "a format-8 Build refusal with typed choices",
            TicketQuestions::parse(&format_8(typed)),
            "format 8 keeps no typed refused choices",
        );
    }
    // A format-6 document, written before the resolver's refusal was kept
    // (OWL-191), is read with none; the field is refused in it, whatever
    // its value. A format-7 document keeps it.
    let read =
        TicketQuestions::parse(&legacy(6, json!({"reason": latest, "decisions": false}))).unwrap();
    assert_eq!(read.resolver_refusal, None);
    let read = TicketQuestions::parse(&legacy(7, Value::Null)).unwrap();
    assert!(read.resolver_refusal.is_some() && read.build_refusal.is_none());
    let null_resolver = edited("ticket-questions.json", |v| {
        v["format"] = json!(7);
        v.as_object_mut().unwrap().remove("build_refusal");
        v["resolver_refusal"] = Value::Null;
    });
    assert_eq!(
        TicketQuestions::parse(&null_resolver)
            .unwrap()
            .resolver_refusal,
        None
    );
    rejects(
        "a format-6 document with the resolver's refusal",
        asked(|v| v["format"] = json!(6)),
        "format 6 keeps no resolver refusal",
    );
    rejects(
        "a format-5 document with a null resolver refusal",
        asked(|v| {
            v["format"] = json!(5);
            v.as_object_mut().unwrap().remove("build_refusal");
            v["resolver_refusal"] = Value::Null;
        }),
        "format 5 keeps no resolver refusal",
    );
    // The resolver's kept refusal keeps the same bounds (OWL-191).
    rejects(
        "a blank resolver refusal",
        asked(|v| v["resolver_refusal"] = json!(" ")),
        "the resolver's kept refusal reason is blank",
    );
    rejects(
        "a resolver refusal past the cap",
        asked(|v| v["resolver_refusal"] = json!("x".repeat(MAX_RESULT_REFUSAL_BYTES + 1))),
        "the resolver's kept refusal reason is over the cap",
    );
    // Build's kept refusal keeps the same bounds on its reason and each of
    // its lists, and keeps one or the other; format 8 knows no
    // `decisions_reason`.
    rejects(
        "a blank Build refusal",
        asked(|v| v["build_refusal"]["reason"] = json!("")),
        "Build's kept refusal reason is blank",
    );
    rejects(
        "a blank list of choices",
        asked(|v| v["build_refusal"]["lists"][0] = json!(" ")),
        "Build's kept list of choices 1 is blank",
    );
    rejects(
        "a list of choices past the cap",
        asked(|v| v["build_refusal"]["lists"][0] = json!("x".repeat(MAX_RESULT_REFUSAL_BYTES + 1))),
        "Build's kept list of choices 1 is over the cap",
    );
    rejects(
        "a Build refusal that keeps nothing",
        asked(|v| v["build_refusal"] = json!({})),
        "keeps neither a reason nor a list of choices",
    );
    rejects(
        "format 7's held reason in format 9",
        asked(|v| v["build_refusal"]["decisions_reason"] = json!("x")),
        "unknown field `decisions_reason`",
    );
    // Its typed choices (OWL-198): at most 64, each text within its bound,
    // the ones past them counted only once full.
    rejects(
        "too many kept choices",
        asked(|v| {
            v["build_refusal"]["choices"] = json!(vec![
                json!({"question": "q", "recorded": "r"});
                MAX_REFUSED_CHOICES + 1
            ])
        }),
        "Build's kept refusal keeps more than 64 choices",
    );
    rejects(
        "a kept choice past its cap",
        asked(|v| {
            v["build_refusal"]["choices"][0]["recorded"] = json!("x".repeat(MAX_CHOICE_BYTES + 1))
        }),
        "Build's kept choice 1 has a text over 512 bytes",
    );
    rejects(
        "choices left out with room for them",
        asked(|v| v["build_refusal"]["left_out"] = json!(1)),
        "counts choices left out with room for them",
    );
    TicketQuestions::parse(&edited("ticket-questions.json", |v| {
        v.as_object_mut().unwrap().remove("build_refusal");
    }))
    .unwrap();
    // The most a hold keeps stays far within the ticket ref's 1 MiB read
    // cap (`owlshift_runner::ticket_ref`), even as JSON escapes its text;
    // the lists formats 6 to 8 kept as text are never added to.
    let mut worst = TicketQuestions::parse(&fixture("ticket-questions.json")).unwrap();
    let escaped = |bytes: usize| "\u{1}".repeat(bytes);
    worst.build_refusal = Some(BuildRefusal {
        reason: Some(escaped(MAX_RESULT_REFUSAL_BYTES)),
        lists: vec![escaped(MAX_RESULT_REFUSAL_BYTES); 3],
        choices: vec![
            RefusedChoice {
                question: escaped(MAX_CHOICE_BYTES),
                recorded: escaped(MAX_CHOICE_BYTES),
            };
            MAX_REFUSED_CHOICES
        ],
        left_out: u32::MAX,
    });
    let rendered = worst.render();
    assert!(rendered.len() < 512 * 1024, "{}", rendered.len());
    assert_eq!(TicketQuestions::parse(&rendered).unwrap(), worst);
    // A kept refusal is the runner's message: never blank, within the
    // brief's cap.
    rejects(
        "a blank kept refusal",
        asked(|v| v["asks"][1]["result_refusal"] = json!(" ")),
        "an ask of round 1 names no refusal reason",
    );
    rejects(
        "a kept refusal past the cap",
        asked(|v| v["asks"][1]["result_refusal"] = json!("x".repeat(MAX_RESULT_REFUSAL_BYTES + 1))),
        "keeps a refusal reason over 2048 bytes",
    );
    rejects(
        "a decision without its comment",
        asked(|v| v["decisions"][0]["comment"] = json!(" ")),
        "decision 1 names no comment",
    );
    rejects(
        "a decision without a basis",
        asked(|v| v["decisions"][0]["basis"] = json!("")),
        "decision 1 names no basis",
    );
    rejects(
        "decisions out of time order",
        asked(|v| {
            let mut earlier = v["decisions"][0].clone();
            earlier["at"] = json!("2026-09-28T08:00:00Z");
            v["decisions"].as_array_mut().unwrap().push(earlier);
        }),
        "decision 2 is older than the one before it",
    );
    rejects(
        "unknown decision field",
        asked(|v| v["decisions"][0]["round"] = json!(1)),
        "unknown field `round`",
    );
    rejects(
        "an ask without its comment",
        asked(|v| v["asks"][1]["comment"] = json!(" ")),
        "an ask of round 1 names no comment",
    );
    rejects(
        "an ask without its decider",
        asked(|v| v["asks"][0]["decider"]["account"] = json!("")),
        "an ask of round 1 names no decider",
    );
    rejects(
        "a re-ask of a round not asked",
        asked(|v| v["asks"][1]["round"] = json!(2)),
        "re-ask of round 2, which is not earlier in the thread",
    );
    rejects(
        "a round out of order",
        asked(|v| {
            let mut later = v["asks"][0].clone();
            later["round"] = json!(1);
            v["asks"].as_array_mut().unwrap().push(later);
        }),
        "round 1 comes after round 1",
    );
    rejects(
        "an ask with no question",
        asked(|v| v["asks"][0]["questions"] = json!([])),
        "round 1 has no question",
    );

    // An answer must be newer than the latest ask and than what the last
    // answer check read.
    let at = |text: &str| text.parse::<jiff::Timestamp>().unwrap();
    let mut questions = TicketQuestions::parse(&fixture("ticket-questions.json")).unwrap();
    assert_eq!(questions.answers_after(), Some(at("2026-09-28T11:00:00Z")));
    questions.checked_through = Some(at("2026-09-28T09:30:00Z"));
    assert_eq!(questions.answers_after(), Some(at("2026-09-28T10:00:00Z")));
    questions.checked_through = None;
    assert_eq!(questions.answers_after(), Some(at("2026-09-28T10:00:00Z")));
    assert_eq!(TicketQuestions::new().answers_after(), None);
}

#[test]
fn ref_names() {
    let id = TicketId::new("OWL-12").unwrap();
    assert_eq!(claim_ref(&id), "refs/owlshift/claims/OWL-12");
    assert_eq!(ticket_ref(&id), "refs/owlshift/tickets/OWL-12");
    for bad in ["", "a b", "a.lock", "a/b", "é", &"x".repeat(65)] {
        assert!(TicketId::new(bad).is_err(), "{bad:?} accepted");
    }
    assert!(TicketId::new("x".repeat(64)).is_ok());
}

#[test]
fn comment_headers() {
    let round = NonZeroU32::new(2);
    let questions = Header {
        kind: MarkerKind::Questions,
        round,
    };
    assert_eq!(questions.render(), "[owlshift] QUESTIONS · round 2");
    assert_eq!(
        Header::parse("[owlshift] QUESTIONS · round 2\r\n").unwrap(),
        questions
    );
    let reask = Header {
        kind: MarkerKind::ReAsk,
        round,
    };
    assert_eq!(reask.render(), "[owlshift] RE-ASK · round 2");
    assert_eq!(Header::parse(&reask.render()).unwrap(), reask);
    let delivery = Header {
        kind: MarkerKind::Delivery,
        round: None,
    };
    assert_eq!(delivery.render(), "[owlshift] DELIVERY");
    assert_eq!(Header::parse(&delivery.render()).unwrap(), delivery);

    for (line, needle) in [
        ("[owlshift] QUESTIONS", "needs a round"),
        ("[owlshift] DELIVERY · round 1", "takes no round"),
        ("[owlshift] QUESTIONS · round 0", "invalid round"),
        ("[owlshift] QUESTIONS · round 02", "invalid round"),
        ("[owlshift] QUESTIONS · round two", "invalid round"),
        ("[owlshift] questions · round 1", "unknown kind"),
        ("[owlshift] LUNCH", "unknown kind"),
        ("[agent] QUESTIONS · round 1", "not a header"),
    ] {
        rejects(line, Header::parse(line), needle);
    }
}

#[test]
fn comment_footers() {
    let footer = Footer::parse_payload(&fixture("footer.json")).unwrap();
    let body = format!(
        "{}\r\n\r\nQ1. Local time or UTC?\r\n\r\n{}\r\n",
        Header {
            kind: MarkerKind::Questions,
            round: NonZeroU32::new(2),
        }
        .render(),
        footer.render()
    );
    let marked = MarkedComment::parse(&body).unwrap().unwrap();
    assert_eq!(marked.footer.as_ref(), Some(&footer));
    assert_eq!(marked.header.round, NonZeroU32::new(2));

    // A value cannot close the HTML comment early.
    let mut tricky = footer.clone();
    tricky.run = Some("a-->b".into());
    let rendered = tricky.render();
    assert_eq!(rendered.matches("-->").count(), 1);
    assert_eq!(Footer::find(&rendered).unwrap(), Some(tricky));

    // The header alone is enough where the tracker dropped the footer.
    let bare = MarkedComment::parse("[owlshift] DELIVERY\n\nShipped.").unwrap();
    assert!(bare.unwrap().footer.is_none());
    // An unmarked comment, even one quoting a footer, is not ours.
    let quoted = format!("Look at this:\n{}\nWhy?", footer.render());
    assert!(MarkedComment::parse(&quoted).unwrap().is_none());
    assert!(MarkedComment::parse("").unwrap().is_none());

    let questions_round_2 = "[owlshift] QUESTIONS · round 2\n\ntext\n";
    let with_footer = |payload: &str| format!("{questions_round_2}<!-- owlshift:{payload} -->");
    rejects(
        "footer disagreeing with the header",
        MarkedComment::parse(&with_footer(
            r#"{"format":1,"kind":"QUESTIONS","ticket":"OWL-42","round":3}"#,
        )),
        "disagrees",
    );
    rejects(
        "footer without a header",
        MarkedComment::parse(&format!("hello\n{}", footer.render())),
        "no header",
    );
    rejects(
        "newer footer",
        MarkedComment::parse(&with_footer(
            r#"{"format":2,"kind":"QUESTIONS","ticket":"OWL-42","round":2,"new":true}"#,
        )),
        "upgrade Owlshift",
    );
    rejects(
        "malformed footer",
        MarkedComment::parse(&with_footer(r#"{"format":1,"kind":"#)),
        "invalid comment footer",
    );
    rejects(
        "footer with an unknown field",
        MarkedComment::parse(&with_footer(
            r#"{"format":1,"kind":"QUESTIONS","ticket":"OWL-42","round":2,"x":1}"#,
        )),
        "unknown field `x`",
    );
    rejects(
        "footer missing its round",
        MarkedComment::parse(&with_footer(
            r#"{"format":1,"kind":"QUESTIONS","ticket":"OWL-42"}"#,
        )),
        "needs a round",
    );
    rejects(
        "unclosed footer",
        Footer::find("<!-- owlshift:{\"format\":1}"),
        "not closed",
    );
    rejects(
        "malformed header",
        MarkedComment::parse("[owlshift] QUESTIONS\n\ntext"),
        "needs a round",
    );
}

#[test]
fn project_config_rejections() {
    let parse = |from: &str, to: &str| ProjectConfig::parse(&replaced("owlshift.toml", from, to));
    rejects(
        "unknown top-level key",
        parse("[stack]", "color = \"blue\"\n\n[stack]"),
        "unknown field `color`",
    );
    rejects(
        "unknown nested key",
        parse("kind = \"linear\"", "kind = \"linear\"\nboard = \"x\""),
        "unknown field `board`",
    );
    rejects(
        "unknown key in an inline table",
        parse(
            "review = \"In Review\"",
            "review = \"In Review\", done = \"Done\"",
        ),
        "unknown field `done`",
    );
    rejects(
        "unknown tier",
        parse("fast     =", "huge = { claude = \"x\" }\nfast     ="),
        "unknown field `huge`",
    );
    rejects(
        "unknown harness in a tier",
        parse("{ claude = \"claude-haiku-4-5\" }", "{ gemini = \"g\" }"),
        "unknown field `gemini`",
    );
    rejects(
        "bad requires",
        parse("requires = \">=0.1\"", "requires = \"soon\""),
        "requires",
    );
    rejects(
        "missing requires",
        parse("requires = \">=0.1\"", ""),
        "missing field `requires`",
    );
    rejects(
        "linear without a team",
        parse("team = \"LOC\"\n", ""),
        "tracker.team is required",
    );
    rejects(
        "two admission gestures",
        parse(
            "admit = { label = \"agent\" }",
            "admit = { label = \"agent\", state = \"Todo\" }",
        ),
        "wanted exactly 1 element",
    );
    rejects(
        "unknown admission gesture",
        parse(
            "admit = { label = \"agent\" }",
            "admit = { emoji = \"owl\" }",
        ),
        "unknown variant `emoji`",
    );
    rejects(
        "bad plan approval",
        parse("\"on-fork\"", "\"sometimes\""),
        "unknown variant `sometimes`",
    );
    rejects(
        "bad variant",
        parse("default = \"standard\"", "default = \"yolo\""),
        "unknown variant `yolo`",
    );

    let delegation = ProjectConfig::parse(&replaced(
        "owlshift.toml",
        "admit = { label = \"agent\" }",
        "admit = \"delegation\"",
    ))
    .unwrap();
    assert_eq!(delegation.tracker.admit, Admit::Delegation);
    let state = ProjectConfig::parse(&replaced(
        "owlshift.toml",
        "admit = { label = \"agent\" }",
        "admit = { state = \"Todo\" }",
    ))
    .unwrap();
    assert_eq!(state.tracker.admit, Admit::State("Todo".into()));

    // `requires` stays readable in a file a newer binary wrote.
    let newer = replaced(
        "owlshift.toml",
        "requires = \">=0.1\"",
        "requires = \">=9\"\n[ui]\nport = 1",
    );
    assert!(ProjectConfig::parse(&newer).is_err());
    assert_eq!(peek_requires(&newer).unwrap().unwrap().to_string(), ">=9");
}

#[test]
fn personal_config_rejections() {
    let parse = |from: &str, to: &str| PersonalConfig::parse(&replaced("personal.toml", from, to));
    rejects(
        "unknown top-level key",
        parse("keep_awake = true", "keep_awake = true\ntheme = \"dark\""),
        "unknown field `theme`",
    );
    rejects(
        "unknown harness",
        parse("[harnesses.codex]", "[harnesses.gemini]"),
        "unknown field `gemini`",
    );
    rejects(
        "unknown harness setting",
        parse(
            "fallback = \"claude\"",
            "fallback = \"claude\"\nmodel = \"x\"",
        ),
        "unknown field `model`",
    );
    rejects(
        "fallback to an unknown harness",
        parse("fallback = \"claude\"", "fallback = \"gemini\""),
        "unknown variant `gemini`",
    );
    rejects(
        "fallback to itself",
        parse("fallback = \"claude\"", "fallback = \"codex\""),
        "falls back to itself",
    );
    rejects(
        "negative budget",
        parse("budget_usd = 20.0", "budget_usd = -1.0"),
        "budget_usd",
    );
    rejects(
        "budget not a number",
        parse("budget_usd = 20.0", "budget_usd = nan"),
        "budget_usd",
    );
    rejects(
        "infinite budget",
        parse("budget_usd = 20.0", "budget_usd = inf"),
        "budget_usd",
    );
    rejects(
        "zero concurrent runs",
        parse("concurrent_runs = 2", "concurrent_runs = 0"),
        "nonzero",
    );
    rejects(
        "unknown notification setting",
        parse("desktop = false", "desktop = false\nsound = true"),
        "unknown field `sound`",
    );
    let out_of_range = "harness Claude: usage_cap_percent must be a whole number from 1 to 100";
    for (cap, needle) in [
        ("0", out_of_range),
        ("101", out_of_range),
        ("80.5", "invalid type"),
    ] {
        rejects(
            &format!("usage cap {cap}"),
            parse(
                "usage_cap_percent = 100",
                &format!("usage_cap_percent = {cap}"),
            ),
            needle,
        );
    }
    rejects(
        "bad requires",
        parse("requires = \">=0.1\"", "requires = \"1.0 or so\""),
        "requires",
    );

    // Every key of the personal file is optional.
    assert_eq!(
        PersonalConfig::parse("").unwrap(),
        PersonalConfig::parse("[identity]\n[harnesses]\n").unwrap()
    );
}

/// OWL-195: what a held Build run is told always fits the brief's cap.
/// Items that do not fit whole share the room, a short one kept whole;
/// past ten, the first list and the newest items are told and a note
/// counts the lists left out between them. With typed choices (OWL-198),
/// their advice is told whole first, and the rest share what is left.
#[test]
fn a_build_refusal_is_told_within_the_cap() {
    let long = |n: usize| format!("list {n}: {}", "é".repeat(MAX_RESULT_REFUSAL_BYTES));
    let kept = |lists: usize, reason: Option<&str>| BuildRefusal {
        reason: reason.map(str::to_owned),
        lists: (1..=lists).map(long).collect(),
        ..BuildRefusal::default()
    };
    let fits = |told: &str| {
        assert!(told.len() <= MAX_RESULT_REFUSAL_BYTES, "{}", told.len());
        told.to_owned()
    };

    let told = fits(&kept(3, Some("status is done")).told());
    for n in 1..=3 {
        assert!(told.contains(&format!("list {n}: é")), "{told}");
    }
    assert_eq!(told.matches("é [cut]").count(), 3, "{told}");
    assert!(told.ends_with("A later result was refused too: status is done"));

    // Ten items fit; past them, the first list, a note and the eight
    // newest items, the latest reason included.
    let told = fits(&kept(10, None).told());
    assert!((1..=10).all(|n| told.contains(&format!("list {n}: é"))));
    for (lists, left_out) in [(11, 3), (60, 52)] {
        let told = fits(&kept(lists, Some("unknown field")).told());
        assert!(told.starts_with("list 1: é"), "{told}");
        assert!(
            told.contains(&format!(
                " [cut]\n\n[{left_out} more refused lists of choices are left out here for lack of room]\n\nA later result was refused too: list {}: é",
                left_out + 2
            )),
            "{told}"
        );
        assert!(told.contains(&format!("list {lists}: é")), "{told}");
        assert!(told.ends_with("A later result was refused too: unknown field"));
    }

    // Typed choices: the advice that points to `refused_choices`, and
    // counts the choices left out, is never cut, whatever shares the rest.
    let choice = RefusedChoice {
        question: "q".to_owned(),
        recorded: "r".to_owned(),
    };
    let mut typed = kept(0, Some("unknown field"));
    typed.keep_choices(&vec![choice; MAX_REFUSED_CHOICES + 2]);
    assert_eq!((typed.reason.as_deref(), typed.left_out), (None, 2));
    typed.reason = Some(long(0));
    let pointer = "are in `refused_choices`, oldest first. 2 more choices they listed are not \
                   kept, past the 64 there: ask too about every other choice of what the work \
                   delivers that the branch's commits took without the decider";
    let told = fits(&typed.told());
    assert!(
        told.contains(&format!(
            "{pointer}\n\nA later result was refused too: list 0: é"
        )),
        "{told}"
    );
    assert!(told.ends_with("é [cut]"), "{told}");
    for lists in [1, 30] {
        let mut mixed = kept(lists, Some("unknown field"));
        mixed.choices = typed.choices.clone();
        mixed.left_out = 2;
        let told = fits(&mixed.told());
        assert!(
            told.starts_with("invalid result.json: decisions are given but")
                && told.contains(&format!(
                    "{pointer}\n\nEarlier results were refused too, their choices quoted here and not in `refused_choices`: list 1: é"
                )),
            "{told}"
        );
        assert!(told.ends_with("A later result was refused too: unknown field"));
    }
}
