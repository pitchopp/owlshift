//! The committed schemas in `schemas/` match the types, and the fixtures
//! validate against them.
//!
//! Run with `OWLSHIFT_UPDATE_SCHEMAS=1` to rewrite the committed schemas.

use std::fs;
use std::path::PathBuf;

use owlshift_contracts::schema;
use serde_json::{Value, json};

fn schemas_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../schemas")
}

fn fixture(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    // A checkout may turn line endings into CRLF; the tests assume LF.
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .replace("\r\n", "\n")
}

fn committed_schema(stem: &str) -> Value {
    let path = schemas_dir().join(format!("{stem}.schema.json"));
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap()
}

fn validator(stem: &str) -> jsonschema::Validator {
    jsonschema::validator_for(&committed_schema(stem)).unwrap()
}

#[test]
fn committed_schemas_match_the_types() {
    let update = std::env::var_os("OWLSHIFT_UPDATE_SCHEMAS").is_some_and(|v| v == "1");
    let dir = schemas_dir();
    let generated = schema::all();
    let mut drifted = Vec::new();
    for (stem, schema) in &generated {
        let path = dir.join(format!("{stem}.schema.json"));
        let rendered = schema::render(schema);
        if update {
            fs::create_dir_all(&dir).unwrap();
            fs::write(&path, &rendered).unwrap();
            continue;
        }
        // A Windows checkout may turn line endings into CRLF.
        let committed = fs::read_to_string(&path)
            .map(|text| text.replace("\r\n", "\n"))
            .unwrap_or_default();
        if committed != rendered {
            drifted.push(path.display().to_string());
        }
    }
    let known: Vec<String> = generated
        .iter()
        .map(|(stem, _)| format!("{stem}.schema.json"))
        .collect();
    let stale: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| !known.contains(name))
        .collect();
    assert!(
        drifted.is_empty() && stale.is_empty(),
        "schemas out of date: {drifted:?}; unexpected files in schemas/: {stale:?}. \
         Run `OWLSHIFT_UPDATE_SCHEMAS=1 cargo test -p owlshift-contracts --test schemas` \
         and commit the result."
    );
}

#[test]
fn fixtures_validate_against_the_committed_schemas() {
    let cases: [(&str, &str); 20] = [
        ("result", "result-sample.json"),
        ("result", "result-answer-check.json"),
        ("result", "result-counter-question.json"),
        ("result", "result-resolver.json"),
        ("result", "result-intake.json"),
        ("result", "result-design.json"),
        ("result", "result-review.json"),
        ("brief", "brief.json"),
        ("event", "event.json"),
        ("claim", "claim.json"),
        ("ticket-state", "ticket-state.json"),
        ("ticket-questions", "ticket-questions.json"),
        ("ticket-delivery", "ticket-delivery.json"),
        ("ticket-intake", "ticket-intake.json"),
        ("ticket-proposals", "ticket-proposals.json"),
        ("ticket-plan", "ticket-plan.json"),
        ("ticket-review", "ticket-review.json"),
        ("comment-footer", "footer.json"),
        ("project-config", "owlshift.toml"),
        ("personal-config", "personal.toml"),
    ];
    for (stem, name) in cases {
        let text = fixture(name);
        let instance: Value = if name.ends_with(".toml") {
            toml::from_str(&text).unwrap()
        } else {
            serde_json::from_str(&text).unwrap()
        };
        if let Err(error) = validator(stem).validate(&instance) {
            panic!("{name} does not validate against {stem}.schema.json: {error}");
        }
    }
}

/// The unit form `admit = "ready"` (OWL-322) is in the committed schema, and
/// a bare label is not.
#[test]
fn project_config_schema_accepts_the_ready_gesture() {
    let validator = validator("project-config");
    let text = fixture("owlshift.toml");
    let with = |admit: &str| -> Value {
        toml::from_str(&text.replace("admit = { label = \"agent\" }", admit)).unwrap()
    };
    assert!(validator.is_valid(&with("admit = \"ready\"")));
    assert!(validator.is_valid(&with("admit = { state = \"Todo\" }")));
    assert!(!validator.is_valid(&with("admit = \"label\"")));
}

/// The schema carries the rules of `result.json` that JSON Schema can express;
/// question order is checked by `RunResult::parse` only.
#[test]
fn result_schema_carries_the_expressible_rules() {
    let validator = validator("result");
    let question = |id: &str| json!({ "id": id, "category": "scope", "context": "c", "text": "t", "recommendation": { "why": "r" } });
    let base = |status: &str| json!({ "format": 10, "status": status, "summary": "s" });

    let mut no_question = base("questions");
    no_question["questions"] = json!([]);
    assert!(!validator.is_valid(&no_question));
    assert!(!validator.is_valid(&base("questions")));

    let mut pr_not_done = base("failed");
    pr_not_done["pr"] = json!({ "branch": "b", "title": "t", "body": "b" });
    assert!(!validator.is_valid(&pr_not_done));
    let mut pr_done = pr_not_done.clone();
    pr_done["status"] = json!("done");
    assert!(validator.is_valid(&pr_done));

    // A refused choice is named as asked by a result that asks only
    // (OWL-193).
    let mut named = base("questions");
    named["questions"] = json!([question("Q1")]);
    named["refused_choices_asked"] = json!([{ "choice": 1, "question": "Q1" }]);
    assert!(validator.is_valid(&named));
    let mut unasked = named.clone();
    unasked["status"] = json!("blocked");
    assert!(!validator.is_valid(&unasked));
    let mut premise = named.clone();
    premise["status"] = json!("premise_false");
    assert!(validator.is_valid(&premise));
    premise["questions"] = json!([]);
    assert!(!validator.is_valid(&premise));
    named["refused_choices_asked"][0]["choice"] = json!(0);
    assert!(!validator.is_valid(&named));

    for format in [1, 2, 3, 4, 5, 6, 7, 8, 9, 11] {
        let mut other = base("done");
        other["format"] = json!(format);
        assert!(!validator.is_valid(&other), "format {format}");
    }

    // Every question recommends, with an object (OWL-324): absent or null
    // is refused, as `RunResult::parse` refuses it.
    let mut unrecommended = base("questions");
    unrecommended["questions"] = json!([question("Q1")]);
    assert!(validator.is_valid(&unrecommended));
    for recommendation in [Value::Null, json!("r")] {
        unrecommended["questions"][0]["recommendation"] = recommendation;
        assert!(!validator.is_valid(&unrecommended));
    }
    unrecommended["questions"][0]
        .as_object_mut()
        .unwrap()
        .remove("recommendation");
    assert!(!validator.is_valid(&unrecommended));

    // The quality pipeline's fields go with `done` only, and an approval
    // `changed` with its plan, `approved` without one (OWL-300); schema and
    // type agree on each case.
    let design: Value = serde_json::from_str(&fixture("result-design.json")).unwrap();
    let review: Value = serde_json::from_str(&fixture("result-review.json")).unwrap();
    let ruling = json!([{ "finding": "F1", "outcome": "confirmed", "reason": "r" }]);
    let response = json!([{ "finding": "F1", "outcome": "fixed", "reason": "r" }]);
    let with = |mut document: Value, field: &str, value: Value| {
        if value.is_null() {
            document.as_object_mut().unwrap().remove(field);
        } else {
            document[field] = value;
        }
        document
    };
    for (case, document, valid) in [
        ("plan", design.clone(), true),
        (
            "plan, failed",
            with(design.clone(), "status", json!("failed")),
            false,
        ),
        (
            "changed",
            with(design.clone(), "approval", json!("changed")),
            true,
        ),
        (
            "changed without a plan",
            with(
                with(design.clone(), "approval", json!("changed")),
                "plan",
                Value::Null,
            ),
            false,
        ),
        (
            "approved with a plan",
            with(design.clone(), "approval", json!("approved")),
            false,
        ),
        (
            "approved",
            with(
                with(design.clone(), "approval", json!("approved")),
                "plan",
                Value::Null,
            ),
            true,
        ),
        ("findings", review.clone(), true),
        (
            "findings, blocked",
            with(review.clone(), "status", json!("blocked")),
            false,
        ),
        (
            "rulings",
            with(base("done"), "rulings", ruling.clone()),
            true,
        ),
        (
            "rulings, failed",
            with(base("failed"), "rulings", ruling),
            false,
        ),
        (
            "responses",
            with(base("done"), "finding_responses", response.clone()),
            true,
        ),
        (
            "responses, failed",
            with(base("failed"), "finding_responses", response),
            false,
        ),
    ] {
        assert_eq!(validator.is_valid(&document), valid, "schema: {case}");
        let parses = owlshift_contracts::result::RunResult::parse(&document.to_string()).is_ok();
        assert_eq!(parses, valid, "type: {case}");
    }

    // The intake record goes with `done` or `questions` only (OWL-245).
    let intake: Value = serde_json::from_str(&fixture("result-intake.json")).unwrap();
    assert!(validator.is_valid(&intake));
    for status in ["blocked", "failed", "premise_false"] {
        let mut other = intake.clone();
        other["status"] = json!(status);
        assert!(!validator.is_valid(&other), "{status}");
    }

    // Resolutions go with `done` only; a decision carries a decision and a
    // basis, a pass a reason, none of them blank; schema and type agree.
    let parse_ok = |instance: &Value| {
        owlshift_contracts::result::RunResult::parse(&instance.to_string()).is_ok()
    };
    for (resolution, done, valid) in [
        (
            json!({ "outcome": "decided", "question": "Q1", "category": "naming", "decision": "d", "basis": "b" }),
            true,
            true,
        ),
        (
            json!({ "outcome": "decided", "question": "Q1", "category": "naming", "decision": "d", "basis": "b" }),
            false,
            false,
        ),
        (
            json!({ "outcome": "decided", "question": "Q1", "category": "naming", "decision": "d", "basis": " " }),
            true,
            false,
        ),
        (
            json!({ "outcome": "decided", "question": "Q1", "category": "naming", "decision": "d" }),
            true,
            false,
        ),
        (
            json!({ "outcome": "passed_on", "question": "Q1", "reason": "r" }),
            true,
            true,
        ),
        (
            json!({ "outcome": "passed_on", "question": "Q1", "reason": "r", "basis": "b" }),
            true,
            false,
        ),
        (json!({ "question": "Q1", "reason": "r" }), true, false),
        // The resolver's label is required; its form is the runner's to judge.
        (
            json!({ "outcome": "decided", "question": "Q1", "decision": "d", "basis": "b" }),
            true,
            false,
        ),
        (
            json!({ "outcome": "decided", "question": "Q1", "category": "", "decision": "d", "basis": "b" }),
            true,
            true,
        ),
    ] {
        let mut result = base(if done { "done" } else { "failed" });
        result["resolutions"] = json!([resolution.clone()]);
        assert_eq!(
            validator.is_valid(&result),
            valid,
            "schema: {resolution} {done}"
        );
        assert_eq!(parse_ok(&result), valid, "type: {resolution} {done}");
    }

    // Verdicts go with `done` only, each with a reason that is not blank.
    let verdict = |reason: &str| json!({ "question": "Q1", "class": "partial", "reason": reason });
    let mut verdicts_done = base("done");
    verdicts_done["verdicts"] = json!([verdict("Q1 still needs a time zone.")]);
    assert!(validator.is_valid(&verdicts_done));
    let mut verdicts_failed = verdicts_done.clone();
    verdicts_failed["status"] = json!("failed");
    assert!(!validator.is_valid(&verdicts_failed));
    for reason in ["", " \t\n"] {
        let mut blank = base("done");
        blank["verdicts"] = json!([verdict(reason)]);
        assert!(!validator.is_valid(&blank), "reason {reason:?}");
        assert!(owlshift_contracts::result::RunResult::parse(&blank.to_string()).is_err());
    }
    let mut extra = base("done");
    extra["verdicts"] = json!([verdict("r")]);
    extra["verdicts"][0]["extra"] = json!(1);
    assert!(!validator.is_valid(&extra));

    // A reply goes with a counter-question, always and only, and is not
    // blank; schema and type agree on each case, a null reply included.
    let parses = |instance: &Value| {
        owlshift_contracts::result::RunResult::parse(&instance.to_string()).is_ok()
    };
    for (class, reply, valid) in [
        (
            "counter_question",
            Some(json!("It means the reader's.")),
            true,
        ),
        ("counter_question", None, false),
        ("counter_question", Some(Value::Null), false),
        ("counter_question", Some(json!(" \t\n")), false),
        ("partial", Some(json!("It means the reader's.")), false),
        ("partial", Some(Value::Null), false),
        ("answered", None, true),
    ] {
        let mut result = base("done");
        result["verdicts"] = json!([{ "question": "Q1", "class": class, "reason": "r" }]);
        if let Some(reply) = reply.clone() {
            result["verdicts"][0]["reply"] = reply;
        }
        assert_eq!(
            validator.is_valid(&result),
            valid,
            "schema: {class} {reply:?}"
        );
        assert_eq!(parses(&result), valid, "type: {class} {reply:?}");
    }

    let mut out_of_order = base("questions");
    out_of_order["questions"] = json!([question("Q2")]);
    assert!(validator.is_valid(&out_of_order));
    assert!(owlshift_contracts::result::RunResult::parse(&out_of_order.to_string()).is_err());

    // The artifact path pattern agrees with the Rust check.
    for path in [
        "plan.md",
        ".owlshift/plan.md",
        "./plan.md",
        "..plan/x",
        "a/.../b",
        "",
        "/etc/passwd",
        "../x",
        "a/../b",
        "a/..",
        "a//b",
        "a/",
        "C:/x",
        "\\\\server\\share",
        "a\\b",
        "a:b",
    ] {
        let mut result = base("done");
        result["artifacts"] = json!({ "plan": path });
        assert_eq!(
            validator.is_valid(&result),
            owlshift_contracts::ids::RelativePath::new(path).is_ok(),
            "schema and type disagree on {path:?}"
        );
    }
}
