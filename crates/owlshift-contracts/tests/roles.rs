//! The default role prompts in `roles/` stay in step with the contracts: their
//! front matter names the formats this crate reads, and every field, value and
//! example they use exists in the generated schemas.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use owlshift_contracts::Role;
use owlshift_contracts::format::strip_role_front_matter;
use owlshift_contracts::result::RunResult;
use owlshift_contracts::schema;
use owlshift_core::floor::FloorCategory;
use serde_json::Value;

/// The line budget of the build prompt, which the harness re-reads every run.
const BUILD_MAX_LINES: usize = 120;

/// Brief fields the build prompt relies on, as dotted paths from the brief's
/// root; arrays and alternatives are crossed on the way.
const BUILD_BRIEF_FIELDS: &[&str] = &[
    "ticket.description",
    "ticket.author.relation",
    "decider",
    "thread.author.relation",
    "thread.body",
    "checkpoint.plan",
    "checkpoint.ledger",
    "zones",
    "rules.text",
    "rules.source",
    "rules.applies_to",
    "permissions.level",
    "permissions.network",
    "permissions.browser",
    "gate",
    "result_path",
];

/// Reads `roles/build.md`, with its line endings normalized to LF (a
/// checkout may turn them into CRLF); the tests below assume LF.
fn build_prompt() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../roles/build.md");
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .replace("\r\n", "\n")
}

/// Whether the prompt names `word` as code (`` `word` ``) or as a JSON string
/// (`"word"`).
fn names(text: &str, word: &str) -> bool {
    text.contains(&format!("`{word}`")) || text.contains(&format!("\"{word}\""))
}

fn generated(stem: &str) -> Value {
    let (_, schema) = schema::all()
        .into_iter()
        .find(|(s, _)| *s == stem)
        .unwrap_or_else(|| panic!("no {stem} schema"));
    schema.to_value()
}

/// Follows a local `$ref` to its definition.
fn deref<'a>(root: &'a Value, node: &'a Value) -> &'a Value {
    match node.get("$ref").and_then(Value::as_str) {
        Some(reference) => {
            let pointer = reference
                .strip_prefix('#')
                .unwrap_or_else(|| panic!("non-local $ref {reference}"));
            deref(
                root,
                root.pointer(pointer)
                    .unwrap_or_else(|| panic!("{reference}")),
            )
        }
        None => node,
    }
}

/// The schemas a dotted path leads to from `node`, crossing references, array
/// items and alternatives (`anyOf`, `oneOf`) on the way.
fn resolve<'a>(root: &'a Value, node: &'a Value, path: &[&str]) -> Vec<&'a Value> {
    let node = deref(root, node);
    let Some((first, rest)) = path.split_first() else {
        return vec![node];
    };
    let mut found = Vec::new();
    if let Some(child) = node.get("properties").and_then(|p| p.get(*first)) {
        found.extend(resolve(root, child, rest));
    }
    if let Some(items) = node.get("items") {
        found.extend(resolve(root, items, path));
    }
    for key in ["anyOf", "oneOf"] {
        for alternative in node
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            found.extend(resolve(root, alternative, path));
        }
    }
    found
}

/// The string values a schema allows, through references and alternatives.
fn allowed_values(root: &Value, node: &Value) -> BTreeSet<String> {
    let node = deref(root, node);
    let mut values: BTreeSet<String> = node
        .get("enum")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .chain(node.get("const"))
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    for key in ["anyOf", "oneOf"] {
        for alternative in node
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            values.extend(allowed_values(root, alternative));
        }
    }
    values
}

/// Every property name, and every string an `enum` or a `const` allows,
/// anywhere in a schema.
fn names_and_values(node: &Value, out: &mut BTreeSet<String>) {
    match node {
        Value::Object(map) => {
            if let Some(Value::Object(properties)) = map.get("properties") {
                out.extend(properties.keys().cloned());
            }
            if let Some(Value::Array(values)) = map.get("enum") {
                out.extend(values.iter().filter_map(Value::as_str).map(str::to_owned));
            }
            if let Some(Value::String(value)) = map.get("const") {
                out.insert(value.clone());
            }
            map.values().for_each(|v| names_and_values(v, out));
        }
        Value::Array(values) => values.iter().for_each(|v| names_and_values(v, out)),
        _ => {}
    }
}

#[test]
fn build_front_matter_matches_the_contract_formats() {
    strip_role_front_matter(Role::Build, &build_prompt()).unwrap_or_else(|e| panic!("{e}"));
}

#[test]
fn build_prompt_stays_short() {
    let lines = build_prompt().lines().count();
    assert!(
        lines <= BUILD_MAX_LINES,
        "roles/build.md has {lines} lines; the budget is {BUILD_MAX_LINES}"
    );
}

#[test]
fn build_prompt_names_every_result_field_and_value() {
    let text = build_prompt();
    let mut expected = BTreeSet::new();
    names_and_values(&generated("result"), &mut expected);
    let missing: Vec<_> = expected.iter().filter(|w| !names(&text, w)).collect();
    assert!(
        missing.is_empty(),
        "roles/build.md does not name these result.json fields or values: {missing:?}"
    );
}

#[test]
fn build_prompt_names_every_floor_category() {
    let text = build_prompt();
    let missing: Vec<_> = FloorCategory::ALL
        .iter()
        .map(|c| c.token())
        .filter(|token| !text.contains(&format!("`{token}`")))
        .collect();
    assert!(
        missing.is_empty(),
        "roles/build.md does not name these floor categories: {missing:?}"
    );
}

#[test]
fn build_example_result_parses() {
    let text = build_prompt();
    let blocks: Vec<&str> = text
        .split("```json\n")
        .skip(1)
        .map(|after| after.split_once("\n```").expect("an unclosed json block").0)
        .collect();
    assert_eq!(blocks.len(), 1, "roles/build.md holds one json example");
    if let Err(e) = RunResult::parse(blocks[0]) {
        panic!("the example in roles/build.md is not a valid result.json: {e}");
    }
}

#[test]
fn build_brief_fields_exist() {
    let text = build_prompt();
    let brief = generated("brief");
    for field in BUILD_BRIEF_FIELDS {
        let path: Vec<&str> = field.split('.').collect();
        assert!(
            !resolve(&brief, &brief, &path).is_empty(),
            "the brief has no {field}"
        );
        // The prompt may name a field by its full path or by a tail of it,
        // such as `author.relation` inside the thread.
        assert!(
            (0..path.len()).any(|start| names(&text, &path[start..].join("."))),
            "roles/build.md does not name {field}"
        );
    }
    for (field, values) in [
        (
            "ticket.author.relation",
            &["decider", "owlshift", "other"][..],
        ),
        (
            "thread.author.relation",
            &["decider", "owlshift", "other"][..],
        ),
        ("permissions.level", &["write_worktree"][..]),
    ] {
        let path: Vec<&str> = field.split('.').collect();
        let allowed: BTreeSet<String> = resolve(&brief, &brief, &path)
            .into_iter()
            .flat_map(|node| allowed_values(&brief, node))
            .collect();
        for value in values {
            assert!(allowed.contains(*value), "{field} no longer allows {value}");
            assert!(names(&text, value), "roles/build.md does not name {value}");
        }
    }
}

/// The gate reaches the role in its brief, written by the runner: the prompt
/// never sends the role to the project config for commands to run.
#[test]
fn build_gate_comes_from_the_brief() {
    let text = build_prompt();
    for word in ["owlshift.toml", "stack.gate", "[stack]"] {
        assert!(
            !text.contains(word),
            "roles/build.md names {word}; the gate comes from the brief's `gate`"
        );
    }
}

/// Every dotted path the prompt writes as code, such as `checkpoint.plan` or
/// `author.relation`, exists in the brief or `result.json`, from their root or
/// from one of their definitions. The project config is left out on purpose:
/// the role reads what it needs from the brief. File names are skipped.
#[test]
fn build_dotted_fields_exist() {
    let text = build_prompt();
    let schemas: Vec<Value> = ["brief", "result"].into_iter().map(generated).collect();
    let dotted = text
        .split('`')
        .skip(1)
        .step_by(2)
        .filter(|code| {
            code.contains('.')
                && code
                    .split('.')
                    .all(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_lowercase() || c == '_'))
        })
        .filter(|code| {
            !code.ends_with(".md") && !code.ends_with(".json") && !code.ends_with(".toml")
        });
    let mut checked = 0;
    for code in dotted {
        let path: Vec<&str> = code.split('.').collect();
        let exists = schemas.iter().any(|root| {
            let definitions = root
                .get("$defs")
                .and_then(Value::as_object)
                .into_iter()
                .flat_map(|defs| defs.values());
            std::iter::once(root)
                .chain(definitions)
                .any(|start| !resolve(root, start, &path).is_empty())
        });
        assert!(
            exists,
            "roles/build.md names `{code}`, which no contract has"
        );
        checked += 1;
    }
    assert!(checked > 0, "no dotted field found in roles/build.md");
}
