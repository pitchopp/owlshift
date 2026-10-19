//! The default role prompts in `roles/` stay in step with the contracts: their
//! front matter names the formats this crate reads, and every field, value and
//! example they use exists in the generated schemas.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use owlshift_contracts::Role;
use owlshift_contracts::brief::Brief;
use owlshift_contracts::format::{RESULT_FORMAT, strip_role_front_matter};
use owlshift_contracts::result::{
    AnswerClass, MAX_QUESTION_BYTES, MAX_QUESTIONS, MAX_QUESTIONS_BYTES, Resolution, RunResult,
};
use owlshift_contracts::review::{MAX_FINDING_BYTES, MAX_FINDINGS, MAX_PLAN_BYTES, RulingOutcome};
use owlshift_contracts::schema;
use owlshift_core::floor::FloorCategory;
use owlshift_core::gate::{GatePolicy, Route};
use owlshift_core::vocab::{DelegationLevel, PlanApproval};
use serde_json::{Value, json};

/// The line budget of the build prompt, which the harness re-reads every run.
const BUILD_MAX_LINES: usize = 120;

/// `result.json` fields and values only the answer check writes (OWL-23): the
/// build prompt need not name them, and a build result carrying them is
/// refused.
const ANSWER_CHECK_ONLY: &[&str] = &[
    "verdicts",
    "class",
    "reply",
    "answered",
    "partial",
    "unanswered",
    "counter_question",
];

/// `result.json` fields and values only the resolver writes (OWL-138): the
/// build prompt need not name them, and a build result carrying them is
/// refused.
const RESOLVER_ONLY: &[&str] = &[
    "resolutions",
    "outcome",
    "decided",
    "passed_on",
    "basis_kind",
    "fact",
    "settled",
    "judgment",
];

/// `result.json` fields the build prompt need not name since Build takes no
/// decision of its own (OWL-176): a decision's `basis`, which the resolver's
/// decisions still carry. The prompt names `decisions` itself, to leave it out.
const NOT_BUILD: &[&str] = &["basis"];

/// `result.json` fields and values only the intake role writes (OWL-245): the
/// build prompt need not name them, and a build result carrying them is
/// refused.
const INTAKE_ONLY: &[&str] = &[
    "intake",
    "variant",
    "trivial",
    "risky",
    "routing",
    "harness",
    "claude",
    "codex",
    "tier",
    "deep",
    "standard",
    "fast",
    "effort",
    "low",
    "medium",
    "high",
    "xhigh",
    "max",
    "dependencies",
    "ticket",
    "quote",
];

/// `result.json` fields and values of the quality pipeline (OWL-300) that
/// only other roles write: the design role's plan and approval, the review
/// roles' findings and the arbiter's rulings, which a build result carrying
/// them is refused for. Build's own `replan` and `finding_responses` are
/// in its prompt since OWL-302.
const QUALITY_PIPELINE: &[&str] = &[
    "plan",
    "goal",
    "criteria",
    "approach",
    "alternatives",
    "files",
    "steps",
    "what",
    "test",
    "risks",
    "out_of_scope",
    "forks",
    "choice",
    "approval",
    "approved",
    "changed",
    "findings",
    "severity",
    "blocker",
    "major",
    "minor",
    "claim",
    "evidence",
    "criterion",
    "settle",
    "rulings",
    "confirmed",
];

/// Brief fields the build prompt relies on, as dotted paths from the brief's
/// root; arrays and alternatives are crossed on the way.
const BUILD_BRIEF_FIELDS: &[&str] = &[
    "ticket.description",
    "ticket.author.relation",
    "decider",
    "thread.author.relation",
    "thread.body",
    "thread.type",
    "thread.decision",
    "thread.question",
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
    "always_human",
    "gate_failure.command",
    "gate_failure.reason",
    "gate_failure.output",
    "result_refusal",
    "decisions_refused",
    "refused_choices.question",
    "refused_choices.recorded",
    "plan.plan",
    "plan.base",
    "findings.mode",
    "result_path",
];

/// Brief fields the answer-check prompt relies on.
const ANSWER_CHECK_BRIEF_FIELDS: &[&str] = &[
    "ticket.description",
    "thread.round",
    "thread.questions.id",
    "thread.questions.category",
    "thread.questions.context",
    "thread.questions.text",
    "thread.questions.options",
    "thread.questions.options.label",
    "thread.questions.options.detail",
    "thread.questions.recommendation",
    "thread.questions.recommendation.option",
    "thread.questions.recommendation.why",
    "thread.author.relation",
    "thread.body",
    "result_refusal",
    "result_path",
];

/// Reads `roles/<file>`, with its line endings normalized to LF (a checkout
/// may turn them into CRLF); the tests below assume LF.
fn prompt(file: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../roles")
        .join(file);
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .replace("\r\n", "\n")
}

fn build_prompt() -> String {
    prompt("build.md")
}

fn answer_check_prompt() -> String {
    prompt("answer_check.md")
}

fn resolver_prompt() -> String {
    prompt("resolver.md")
}

/// Brief fields the resolver prompt relies on.
const RESOLVER_BRIEF_FIELDS: &[&str] = &[
    "resolve.id",
    "resolve.context",
    "resolve.text",
    "resolve.options",
    "resolve.options.label",
    "resolve.options.detail",
    "resolve.recommendation",
    "resolve.recommendation.option",
    "resolve.recommendation.why",
    "ticket.description",
    "ticket.author.relation",
    "thread.author.relation",
    "thread.decision",
    "rules.text",
    "rules.source",
    "always_human",
    "delegation",
    "result_refusal",
    "result_path",
    // The asking run (OWL-343).
    "resolve.finding",
    "asked_by",
    "asked_by.role",
    "asked_by.summary",
    "asked_by.premise_false",
    "asked_by.plan",
    "asked_by.findings",
    "asked_by.diff",
];

/// The JSON examples of a prompt, in ```json blocks.
fn json_examples(text: &str) -> Vec<&str> {
    text.split("```json\n")
        .skip(1)
        .map(|after| after.split_once("\n```").expect("an unclosed json block").0)
        .collect()
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
    let prompt = build_prompt();
    strip_role_front_matter(Role::Build, &prompt).unwrap_or_else(|e| panic!("{e}"));
    // The prose names the format the role must write, too.
    assert!(
        prompt.contains(&format!("JSON with `format` {RESULT_FORMAT},")),
        "roles/build.md does not say result.json has `format` {RESULT_FORMAT}"
    );
}

#[test]
fn build_prompt_states_the_question_bounds() {
    let sentence = format!(
        "at most {MAX_QUESTIONS} questions, each at most {MAX_QUESTION_BYTES} bytes, \
         all together at most {MAX_QUESTIONS_BYTES} bytes."
    );
    assert!(
        build_prompt().contains(&sentence),
        "roles/build.md does not state the bounds on a result's questions as: {sentence}"
    );
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
    for word in ANSWER_CHECK_ONLY
        .iter()
        .chain(RESOLVER_ONLY)
        .chain(NOT_BUILD)
        .chain(INTAKE_ONLY)
        .chain(QUALITY_PIPELINE)
    {
        assert!(
            expected.remove(*word),
            "{word} is no longer in the result schema"
        );
    }
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
    let blocks = json_examples(&text);
    assert_eq!(blocks.len(), 1, "roles/build.md holds one json example");
    let result = RunResult::parse(blocks[0]).unwrap_or_else(|e| {
        panic!("the example in roles/build.md is not a valid result.json: {e}")
    });
    // Checked against a Build brief too, so it never lists decisions (OWL-176).
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/brief.json");
    let brief = Brief::parse(&fs::read_to_string(&fixture).unwrap()).unwrap();
    assert_eq!(brief.role, Role::Build);
    if let Err(e) = result.validate_against(&brief) {
        panic!("the example in roles/build.md is refused from Build: {e}");
    }
}

/// Each field exists in the brief, and the prompt in `file` names it by its
/// full path or by a tail of it, such as `author.relation` inside the thread.
fn brief_fields_exist(file: &str, text: &str, fields: &[&str]) {
    let brief = generated("brief");
    for field in fields {
        let path: Vec<&str> = field.split('.').collect();
        assert!(
            !resolve(&brief, &brief, &path).is_empty(),
            "the brief has no {field}"
        );
        assert!(
            (0..path.len()).any(|start| names(text, &path[start..].join("."))),
            "roles/{file} does not name {field}"
        );
    }
}

#[test]
fn build_brief_fields_exist() {
    let text = build_prompt();
    let brief = generated("brief");
    brief_fields_exist("build.md", &text, BUILD_BRIEF_FIELDS);
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
        ("findings.mode", &["answer", "fix"][..]),
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

/// The whole gate runs once per Build, in the runner (OWL-355): the role
/// checks its change and is never asked for the whole gate before `done`.
#[test]
fn build_leaves_the_whole_gate_to_the_runner() {
    let text = build_prompt();
    assert!(
        text.contains("The whole gate is the runner's"),
        "roles/build.md no longer leaves the whole gate to the runner"
    );
    assert!(
        text.contains("Do not run the whole gate yourself"),
        "roles/build.md no longer tells the role to leave the whole gate out"
    );
    for asked in ["the whole `gate`", "whole gate again", "full gate before"] {
        assert!(
            !text.contains(asked),
            "roles/build.md asks for {asked:?}: the runner runs the whole gate"
        );
    }
}

/// A broad change runs a command whole once at most (OWL-366): each step
/// checks what it changed, the check of what uses it runs once before `done`,
/// and a fix after it re-runs only what failed and what the fix touches.
#[test]
fn build_widens_its_checks_once() {
    let text = build_prompt();
    for rule in [
        "any other runs whole once in a run at most",
        "After each step, check what that step changed",
        "not what uses it.",
        "Once, before `done`, widen to the parts that use what your plan's steps changed",
        "Run that wider check once in a run: fix what it finds in a step of its own, \
         then re-run only the part that failed and what the fix touches, \
         never the wider check again",
        "the last of them on your last commit",
    ] {
        assert!(
            text.contains(rule),
            "roles/build.md no longer says {rule:?}"
        );
    }
    for gone in [
        "Widen the scope to the parts that use what you changed",
        "a change after them means running them again.",
    ] {
        assert!(
            !text.contains(gone),
            "roles/build.md says {gone:?} again: each step widened, and each fix re-ran the widened check"
        );
    }
}

/// Every dotted path the prompt writes as code, such as `checkpoint.plan` or
/// `author.relation`, exists in the brief or `result.json`, from their root or
/// from one of their definitions. The project config is left out on purpose:
/// the role reads what it needs from the brief. File names are skipped.
#[test]
fn build_dotted_fields_exist() {
    dotted_fields_exist("build.md", &build_prompt());
}

/// Every dotted path the prompt in `file` writes as code exists in the brief
/// or `result.json`; see [`build_dotted_fields_exist`].
fn dotted_fields_exist(file: &str, text: &str) {
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
        assert!(exists, "roles/{file} names `{code}`, which no contract has");
        checked += 1;
    }
    assert!(checked > 0, "no dotted field found in roles/{file}");
}

#[test]
fn answer_check_front_matter_matches_the_contract_formats() {
    let prompt = answer_check_prompt();
    strip_role_front_matter(Role::AnswerCheck, &prompt).unwrap_or_else(|e| panic!("{e}"));
    assert!(
        prompt.contains(&format!("JSON with `format` {RESULT_FORMAT},")),
        "roles/answer_check.md does not say result.json has `format` {RESULT_FORMAT}"
    );
}

/// The answer check names what it writes, every class the result schema
/// allows (the four of architecture section 4), its two statuses, and the
/// brief fields and author relations it reads.
#[test]
fn answer_check_prompt_names_its_fields_classes_and_inputs() {
    let text = answer_check_prompt();
    let result = generated("result");
    let classes: BTreeSet<String> = resolve(&result, &result, &["verdicts", "class"])
        .into_iter()
        .flat_map(|node| allowed_values(&result, node))
        .collect();
    let four: BTreeSet<String> = ["answered", "partial", "unanswered", "counter_question"]
        .map(String::from)
        .into();
    assert_eq!(classes, four, "the answer check's classes changed");
    let words = [
        "format", "status", "summary", "verdicts", "question", "class", "reason", "reply", "done",
        "failed",
    ];
    let missing: Vec<_> = words
        .iter()
        .map(|w| w.to_string())
        .chain(classes)
        .filter(|w| !names(&text, w))
        .collect();
    assert!(
        missing.is_empty(),
        "roles/answer_check.md does not name these result.json fields or values: {missing:?}"
    );
    brief_fields_exist("answer_check.md", &text, ANSWER_CHECK_BRIEF_FIELDS);
    for relation in ["decider", "owlshift", "other"] {
        assert!(
            names(&text, relation),
            "roles/answer_check.md does not name {relation}"
        );
    }
}

/// The answer check knows the decider's short answers (OWL-324): an option
/// by its question and letter, a bare letter on a one-question ask only, and
/// `ok` taking every recommendation; a recognition only a model's run can
/// show, which this pins in the prompt.
#[test]
fn answer_check_prompt_states_the_short_answers() {
    let text = answer_check_prompt();
    for rule in [
        "lettered A, B, C… in their order",
        "such as `Q1: B`",
        "a bare letter such as `B` when the latest ask has one question only",
        "takes the `recommendation` of every question of the latest ask that has one",
        "`Q2: ok` that of Q2 alone",
        "a bare letter when the latest ask has several questions",
    ] {
        assert!(
            text.contains(rule),
            "roles/answer_check.md does not state: {rule}"
        );
    }
}

/// The answer check takes a choice in the decider's own words (OWL-352): a
/// letter followed by words, such as the approval's `B, fork 1 option C`,
/// plain words, and a fork's option by its letter, which the brief's plan
/// must have.
#[test]
fn answer_check_prompt_takes_a_choice_in_own_words() {
    let text = answer_check_prompt();
    for rule in [
        "such as `B, fork 1 option C`, is a choice with what it says",
        "plain words that settle the question, with no letter, are an answer too",
        "`fork 2 option C` names the plan's second fork in the brief's `plan` and its third \
         option",
        "such as a fork or an option the brief's `plan` does not have",
    ] {
        assert!(
            text.contains(rule),
            "roles/answer_check.md does not state: {rule}"
        );
    }
}

#[test]
fn answer_check_example_result_parses() {
    let text = answer_check_prompt();
    let blocks = json_examples(&text);
    assert_eq!(
        blocks.len(),
        1,
        "roles/answer_check.md holds one json example"
    );
    let result = RunResult::parse(blocks[0]).unwrap_or_else(|e| {
        panic!("the example in roles/answer_check.md is not a valid result.json: {e}")
    });
    assert_eq!(result.status, owlshift_contracts::result::Status::Done);
    assert!(!result.verdicts.is_empty(), "the example gives no verdict");
    assert!(
        result
            .verdicts
            .iter()
            .any(|v| v.class == AnswerClass::CounterQuestion && v.reply.is_some()),
        "the example gives no counter-question with its reply"
    );
}

#[test]
fn answer_check_dotted_fields_exist() {
    dotted_fields_exist("answer_check.md", &answer_check_prompt());
}

#[test]
fn resolver_front_matter_matches_the_contract_formats() {
    let prompt = resolver_prompt();
    strip_role_front_matter(Role::Resolver, &prompt).unwrap_or_else(|e| panic!("{e}"));
    assert!(
        prompt.contains(&format!("JSON with `format` {RESULT_FORMAT},")),
        "roles/resolver.md does not say result.json has `format` {RESULT_FORMAT}"
    );
}

/// The resolver names what it writes, both outcomes the result schema allows,
/// its two statuses, the brief fields and author relations it reads, and
/// every floor category, which it passes on whatever a question's category
/// says.
#[test]
fn resolver_prompt_names_its_fields_outcomes_and_inputs() {
    let text = resolver_prompt();
    let result = generated("result");
    let outcomes: BTreeSet<String> = resolve(&result, &result, &["resolutions", "outcome"])
        .into_iter()
        .flat_map(|node| allowed_values(&result, node))
        .collect();
    let two: BTreeSet<String> = ["decided", "passed_on"].map(String::from).into();
    assert_eq!(outcomes, two, "the resolver's outcomes changed");
    let words = [
        "format",
        "status",
        "summary",
        "resolutions",
        "question",
        "outcome",
        "category",
        "decision",
        "basis",
        "basis_kind",
        "quote",
        "option",
        "reason",
        "done",
        "failed",
    ];
    let missing: Vec<_> = words
        .iter()
        .map(|w| w.to_string())
        .chain(outcomes)
        .filter(|w| !names(&text, w))
        .collect();
    assert!(
        missing.is_empty(),
        "roles/resolver.md does not name these result.json fields or values: {missing:?}"
    );
    brief_fields_exist("resolver.md", &text, RESOLVER_BRIEF_FIELDS);
    for relation in ["decider", "owlshift", "other"] {
        assert!(
            names(&text, relation),
            "roles/resolver.md does not name {relation}"
        );
    }
    let floor: Vec<_> = FloorCategory::ALL
        .iter()
        .map(|c| c.token())
        .filter(|token| !text.contains(&format!("`{token}`")))
        .collect();
    assert!(
        floor.is_empty(),
        "roles/resolver.md does not name these floor categories: {floor:?}"
    );
    // Every delegation level and basis kind the brief and the result carry
    // (OWL-337).
    let result_kinds: BTreeSet<String> = resolve(&result, &result, &["resolutions", "basis_kind"])
        .into_iter()
        .flat_map(|node| allowed_values(&result, node))
        .collect();
    let kinds: BTreeSet<String> = ["fact", "settled", "ticket", "judgment"]
        .map(String::from)
        .into();
    assert_eq!(result_kinds, kinds, "the basis kinds changed");
    let missing: Vec<&str> = DelegationLevel::ALL
        .iter()
        .map(|level| level.as_str())
        .chain(["fact", "settled", "ticket", "judgment"])
        .filter(|word| !names(&text, word))
        .collect();
    assert!(
        missing.is_empty(),
        "roles/resolver.md does not name these levels or basis kinds: {missing:?}"
    );
}

#[test]
fn resolver_example_result_parses() {
    let text = resolver_prompt();
    let blocks = json_examples(&text);
    assert_eq!(blocks.len(), 1, "roles/resolver.md holds one json example");
    let result = RunResult::parse(blocks[0]).unwrap_or_else(|e| {
        panic!("the example in roles/resolver.md is not a valid result.json: {e}")
    });
    assert_eq!(result.status, owlshift_contracts::result::Status::Done);
    let decided = result
        .resolutions
        .iter()
        .filter(|r| matches!(r, Resolution::Decided { .. }))
        .count();
    assert!(
        decided > 0 && decided < result.resolutions.len(),
        "the example shows both a decision and a pass"
    );
    // The example's labels are ones the runner lets stand: a token, outside
    // the floor.
    let policy = GatePolicy::new(Vec::<String>::new(), PlanApproval::Never);
    for resolution in &result.resolutions {
        if let Resolution::Decided {
            category,
            basis_kind,
            ..
        } = resolution
        {
            assert_eq!(
                policy.route_decided("naming", category, *basis_kind),
                Route::Resolver,
                "the example labels a decision {category:?}"
            );
        }
    }
}

#[test]
fn resolver_dotted_fields_exist() {
    dotted_fields_exist("resolver.md", &resolver_prompt());
}

fn intake_prompt() -> String {
    prompt("intake.md")
}

/// Brief fields the intake prompt relies on (OWL-248).
const INTAKE_BRIEF_FIELDS: &[&str] = &[
    "ticket.id",
    "ticket.title",
    "ticket.description",
    "ticket.author.relation",
    "thread.author.relation",
    "thread.decision",
    "decider",
    "rules.text",
    "rules.source",
    "result_path",
];

#[test]
fn intake_prompt_matches_the_contracts() {
    let text = intake_prompt();
    strip_role_front_matter(Role::Intake, &text).unwrap_or_else(|e| panic!("{e}"));
    assert!(
        text.contains(&format!("JSON with `format` {RESULT_FORMAT},")),
        "roles/intake.md does not say result.json has `format` {RESULT_FORMAT}"
    );
    let bounds = format!(
        "at most {MAX_QUESTIONS} questions, each at most {MAX_QUESTION_BYTES} bytes, \
         all together at most {MAX_QUESTIONS_BYTES} bytes."
    );
    assert!(
        text.contains(&bounds),
        "roles/intake.md does not state the question bounds"
    );
    let missing: Vec<_> = FloorCategory::ALL
        .iter()
        .map(|c| c.token())
        .filter(|token| !text.contains(&format!("`{token}`")))
        .collect();
    assert!(
        missing.is_empty(),
        "roles/intake.md does not name these floor categories: {missing:?}"
    );
    brief_fields_exist("intake.md", &text, INTAKE_BRIEF_FIELDS);
    dotted_fields_exist("intake.md", &text);
    // Every routing value it names is one the result takes.
    let result = generated("result");
    let mut values = BTreeSet::new();
    names_and_values(&result, &mut values);
    for value in [
        "trivial", "standard", "risky", "claude", "fast", "deep", "xhigh", "max",
    ] {
        assert!(values.contains(value), "the result takes no {value:?}");
        assert!(names(&text, value), "roles/intake.md does not name {value}");
    }
}

#[test]
fn intake_example_result_parses() {
    let text = intake_prompt();
    let blocks = json_examples(&text);
    assert_eq!(blocks.len(), 1, "roles/intake.md holds one json example");
    let result = RunResult::parse(blocks[0]).unwrap_or_else(|e| {
        panic!("the example in roles/intake.md is not a valid result.json: {e}")
    });
    assert_eq!(result.status, owlshift_contracts::result::Status::Done);
    let intake = result.intake.expect("the example records its intake");
    assert!(
        intake
            .dependencies
            .iter()
            .all(|d| d.quote.contains(d.ticket.as_str())),
        "each example dependency's quote names its ticket"
    );
}

/// The JSON example of `roles/<file>`, which holds exactly one.
fn the_example(file: &str) -> Value {
    let text = prompt(file);
    let blocks = json_examples(&text);
    assert_eq!(blocks.len(), 1, "roles/{file} holds one json example");
    serde_json::from_str(blocks[0])
        .unwrap_or_else(|e| panic!("the example in roles/{file} is not JSON: {e}"))
}

/// A brief of the quality pipeline's `role` (OWL-302): the fixture's, made
/// a brief of that role, with what the role is given at its stage. The plan
/// is design.md's example and the findings design_review.md's, so the
/// prompts' examples read as one ticket's.
fn quality_pipeline_brief(role: Role) -> Brief {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/brief.json");
    let mut brief: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    brief["role"] = role.as_str().into();
    let object = brief.as_object_mut().unwrap();
    for build_only in ["checkpoint", "gate_failure", "result_refusal"] {
        object.remove(build_only);
    }
    brief["permissions"]["level"] = "read_only".into();
    let plan = json!({ "base": "a".repeat(40), "plan": the_example("design.md")["plan"] });
    let diff = json!({ "base": "b".repeat(40), "path": ".owlshift/diff.patch" });
    let review = |diff: Option<&Value>| {
        let mut review = json!({ "mandates": ["critic", "conformance"], "pass": 1 });
        if let Some(diff) = diff {
            review["diff"] = diff.clone();
        }
        review
    };
    match role {
        Role::Design => {}
        Role::DesignReview => {
            brief["plan"] = plan;
            brief["review"] = review(None);
        }
        Role::Verify => {
            brief["plan"] = plan;
            brief["review"] = review(Some(&diff));
        }
        Role::Arbiter => {
            let mut findings = the_example("design_review.md")["findings"].clone();
            let mut again = findings[0].clone();
            again["id"] = "F3".into();
            findings.as_array_mut().unwrap().push(again);
            brief["plan"] = plan;
            brief["findings"] = json!({ "mode": "rule", "findings": findings, "diff": diff });
        }
        other => panic!("{} is not a quality pipeline role", other.as_str()),
    }
    Brief::parse(&brief.to_string()).unwrap_or_else(|e| panic!("{}: {e}", role.as_str()))
}

/// The checks every prompt of the quality pipeline passes (OWL-302): its
/// front matter, the format sentence, the result fields and values it
/// writes, the brief fields it reads, its dotted fields, and its one
/// example, a result its role may give. Returns the prompt and the example.
fn quality_pipeline_prompt(role: Role, writes: &[&str], reads: &[&str]) -> (String, RunResult) {
    let file = format!("{}.md", role.as_str());
    let text = prompt(&file);
    strip_role_front_matter(role, &text).unwrap_or_else(|e| panic!("{e}"));
    assert!(
        text.contains(&format!("JSON with `format` {RESULT_FORMAT},")),
        "roles/{file} does not say result.json has `format` {RESULT_FORMAT}"
    );
    let mut result_words = BTreeSet::new();
    names_and_values(&generated("result"), &mut result_words);
    for word in writes {
        assert!(result_words.contains(*word), "the result takes no {word:?}");
        assert!(names(&text, word), "roles/{file} does not name {word}");
    }
    brief_fields_exist(&file, &text, reads);
    dotted_fields_exist(&file, &text);
    let example = the_example(&file).to_string();
    let result = RunResult::parse_against(&example, &quality_pipeline_brief(role)).unwrap_or_else(
        |refused| {
            panic!(
                "the example in roles/{file} is refused from its role: {}",
                refused.error
            )
        },
    );
    (text, result)
}

/// The words of a review run's result.
const REVIEW_WRITES: &[&str] = &[
    "format",
    "status",
    "summary",
    "findings",
    "id",
    "severity",
    "blocker",
    "major",
    "minor",
    "claim",
    "evidence",
    "criterion",
    "settle",
    "done",
    "blocked",
    "failed",
];

/// A review prompt states the bounds on findings and names every mandate.
fn review_prompt_names_bounds_and_mandates(file: &str, text: &str) {
    let bounds = format!(
        "At most {MAX_FINDINGS} findings, each at most {MAX_FINDING_BYTES} bytes rendered as JSON"
    );
    assert!(
        text.contains(&bounds),
        "roles/{file} does not state: {bounds}"
    );
    let brief = generated("brief");
    let mut mandates = BTreeSet::new();
    names_and_values(&brief["$defs"]["Mandate"], &mut mandates);
    assert_eq!(mandates.len(), 3, "the mandates changed: {mandates:?}");
    for mandate in &mandates {
        assert!(names(text, mandate), "roles/{file} does not name {mandate}");
    }
}

#[test]
fn design_prompt_matches_the_contracts() {
    let (text, result) = quality_pipeline_prompt(
        Role::Design,
        &[
            "format",
            "status",
            "summary",
            "plan",
            "goal",
            "criteria",
            "approach",
            "alternatives",
            "reason",
            "files",
            "steps",
            "what",
            "test",
            "risks",
            "out_of_scope",
            "forks",
            "choice",
            "options",
            "approval",
            "approved",
            "changed",
            "finding_responses",
            "finding",
            "outcome",
            "fixed",
            "rejected",
            "questions",
            "id",
            "category",
            "context",
            "text",
            "recommendation",
            "decisions",
            "done",
            "blocked",
            "premise_false",
            "failed",
        ],
        &[
            "ticket.description",
            "ticket.author.relation",
            "decider",
            "thread.author.relation",
            "thread.type",
            "thread.decision",
            "thread.question",
            "thread.reversed",
            "rules.text",
            "rules.source",
            "rules.applies_to",
            "zones",
            "always_human",
            "plan.plan",
            "plan.base",
            "approval",
            "findings.mode",
            "result_path",
        ],
    );
    let bounds = format!(
        "at most {MAX_QUESTIONS} questions, each at most {MAX_QUESTION_BYTES} bytes, \
         all together at most {MAX_QUESTIONS_BYTES} bytes."
    );
    assert!(
        text.contains(&bounds),
        "roles/design.md does not state: {bounds}"
    );
    let plan_bound = format!("at most {MAX_PLAN_BYTES} bytes rendered as JSON");
    assert!(
        text.contains(&plan_bound),
        "roles/design.md does not state: {plan_bound}"
    );
    let missing: Vec<_> = FloorCategory::ALL
        .iter()
        .map(|c| c.token())
        .filter(|token| !text.contains(&format!("`{token}`")))
        .collect();
    assert!(
        missing.is_empty(),
        "roles/design.md does not name: {missing:?}"
    );
    for mode in ["fix", "answer"] {
        assert!(names(&text, mode), "roles/design.md does not name {mode}");
    }
    let plan = result.plan.expect("the example gives a plan");
    assert!(!plan.forks.is_empty(), "the example names a fork");
}

#[test]
fn design_review_prompt_matches_the_contracts() {
    let (text, result) = quality_pipeline_prompt(
        Role::DesignReview,
        REVIEW_WRITES,
        &[
            "ticket.description",
            "ticket.author.relation",
            "decider",
            "thread.author.relation",
            "thread.decision",
            "rules.text",
            "rules.source",
            "plan.plan",
            "plan.base",
            "review.mandates",
            "review.pass",
            "result_path",
        ],
    );
    review_prompt_names_bounds_and_mandates("design_review.md", &text);
    assert!(!result.findings.is_empty(), "the example gives findings");
}

#[test]
fn verify_prompt_matches_the_contracts() {
    let (text, result) = quality_pipeline_prompt(
        Role::Verify,
        REVIEW_WRITES,
        &[
            "ticket.description",
            "ticket.author.relation",
            "decider",
            "thread.author.relation",
            "thread.decision",
            "rules.text",
            "rules.source",
            "plan.plan",
            "plan.base",
            "review.mandates",
            "review.pass",
            "review.diff.base",
            "review.diff.path",
            "result_path",
        ],
    );
    review_prompt_names_bounds_and_mandates("verify.md", &text);
    assert!(!result.findings.is_empty(), "the example gives findings");
}

#[test]
fn arbiter_prompt_matches_the_contracts() {
    let (_, result) = quality_pipeline_prompt(
        Role::Arbiter,
        &[
            "format",
            "status",
            "summary",
            "rulings",
            "finding",
            "outcome",
            "reason",
            "confirmed",
            "rejected",
            "done",
            "failed",
        ],
        &[
            "ticket.description",
            "ticket.author.relation",
            "decider",
            "thread.author.relation",
            "thread.decision",
            "rules.text",
            "rules.source",
            "findings.findings",
            "findings.diff.base",
            "findings.diff.path",
            "plan.plan",
            "plan.base",
            "result_path",
        ],
    );
    for outcome in [RulingOutcome::Confirmed, RulingOutcome::Rejected] {
        assert!(
            result.rulings.iter().any(|r| r.outcome == outcome),
            "the example gives no {outcome:?} ruling"
        );
    }
}
