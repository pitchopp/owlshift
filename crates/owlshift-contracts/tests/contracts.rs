//! Round trips and rejections for every contract.

use std::fmt::Debug;
use std::fs;
use std::num::NonZeroU32;
use std::path::PathBuf;

use owlshift_contracts::ContractError;
use owlshift_contracts::brief::Brief;
use owlshift_contracts::comment::{Footer, Header, MarkedComment, MarkerKind};
use owlshift_contracts::config::{Admit, PersonalConfig, ProjectConfig, peek_requires};
use owlshift_contracts::event::Event;
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::refs::{Claim, PersistedState, claim_ref, ticket_ref};
use owlshift_contracts::result::{RunResult, Status};
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

    round_trip("brief.json", Brief::parse, Brief::render);
    round_trip("event.json", Event::parse, Event::render);
    round_trip("claim.json", Claim::parse, Claim::render);
    round_trip(
        "ticket-state.json",
        PersistedState::parse,
        PersistedState::render,
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
            v["format"] = json!(2);
            v["verdicts"] = json!([]);
        }),
        "upgrade Owlshift",
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
        RunResult::parse(r#"{"format": 2, "status""#),
        "upgrade Owlshift",
    );
    rejects(
        "truncated document",
        RunResult::parse(r#"{"format": 1, "status""#),
        "EOF",
    );
    let newer = edited("result-sample.json", |v| v["format"] = json!(2));
    assert!(matches!(
        RunResult::parse(&newer),
        Err(ContractError::NewerFormat {
            found: 2,
            supported: 1,
            ..
        })
    ));
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
        parse(|v| v["format"] = json!(3)),
        "upgrade Owlshift",
    );
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
