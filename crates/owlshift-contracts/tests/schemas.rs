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
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
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
    let cases: [(&str, &str); 8] = [
        ("result", "result-sample.json"),
        ("brief", "brief.json"),
        ("event", "event.json"),
        ("claim", "claim.json"),
        ("ticket-state", "ticket-state.json"),
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

/// The schema carries the rules of `result.json` that JSON Schema can express;
/// question order is checked by `RunResult::parse` only.
#[test]
fn result_schema_carries_the_expressible_rules() {
    let validator = validator("result");
    let question = |id: &str| json!({ "id": id, "category": "scope", "context": "c", "text": "t" });
    let base = |status: &str| json!({ "format": 1, "status": status, "summary": "s" });

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

    let mut newer = base("done");
    newer["format"] = json!(2);
    assert!(!validator.is_valid(&newer));

    let mut out_of_order = base("questions");
    out_of_order["questions"] = json!([question("Q2")]);
    assert!(validator.is_valid(&out_of_order));
    assert!(owlshift_contracts::result::RunResult::parse(&out_of_order.to_string()).is_err());
}
