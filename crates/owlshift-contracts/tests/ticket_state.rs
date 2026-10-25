//! The persisted ticket state against the core state machine: every reachable
//! state survives a write and a read back, and a document the machine could
//! never be in is refused.
//!
//! Structural rejections (unknown stage, waiting reason or field, newer
//! format) live in `contracts.rs`; this file covers the conversion and the
//! machine invariants `PersistedState::validate` adds.

use std::collections::{HashSet, VecDeque};
use std::fmt::Debug;
use std::fs;
use std::path::PathBuf;

use owlshift_contracts::ids::SessionId;
use owlshift_contracts::refs::{
    MAX_HELD_FILES, PersistedState, TicketCost, TicketHeld, TicketSession, Waiting,
};
use owlshift_contracts::{ContractError, PlanApproval, Stage, Variant};
use owlshift_core::gate::{GatePolicy, TicketSpend};
use owlshift_core::pipeline::Pipeline;
use owlshift_core::state::{Event, MAX_REVIEW_PASSES, Status, TicketState, Transition};
use serde_json::{Value, json};

/// Every state reachable from admission in `pipeline`, each event applied
/// without a plan gate and with the gate of an approved plan (OWL-32), with
/// question rounds bounded at three and review passes at two past the last
/// one: past that, only the round or pass counter grows, a review round the
/// resolver decides counting a pass without a round (OWL-342).
fn reachable(pipeline: Pipeline) -> HashSet<TicketState> {
    let gate = GatePolicy::new(Vec::<String>::new(), PlanApproval::Always).plan_gate(
        pipeline,
        true,
        "plan",
        Some("plan"),
    );
    let mut seen = HashSet::from([TicketState::admitted()]);
    let mut queue = VecDeque::from([TicketState::admitted()]);
    while let Some(current) = queue.pop_front() {
        for event in Event::ALL {
            for transition in [
                current.apply(pipeline, event),
                current.apply_gated(pipeline, event, gate),
            ] {
                let next = match transition {
                    Ok(Transition::To(next) | Transition::Parked { state: next, .. }) => next,
                    Ok(Transition::Finished(_)) | Err(_) => continue,
                };
                if next.round() <= 3
                    && next.review_passes() <= MAX_REVIEW_PASSES + 2
                    && seen.insert(next)
                {
                    queue.push_back(next);
                }
            }
        }
    }
    seen
}

fn schema_validator() -> jsonschema::Validator {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../schemas/ticket-state.schema.json");
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    jsonschema::validator_for(&serde_json::from_str(&text).unwrap()).unwrap()
}

/// Writes `state` as `state.json`, checks it against the committed schema,
/// and reads it back.
fn written_and_read(state: &TicketState, schema: &jsonschema::Validator) -> TicketState {
    let text = PersistedState::from(state).render();
    let value: Value = serde_json::from_str(&text).unwrap();
    assert!(
        schema.is_valid(&value),
        "{state:?} rendered off-schema: {text}"
    );
    let parsed = PersistedState::parse(&text).unwrap_or_else(|e| panic!("{state:?}: {e}"));
    TicketState::try_from(&parsed).unwrap_or_else(|e| panic!("{state:?}: {e}"))
}

#[test]
fn every_reachable_state_survives_a_write_and_a_read_back() {
    let schema = schema_validator();
    for variant in [Variant::Standard, Variant::Trivial, Variant::Risky] {
        let states = reachable(Pipeline::new(variant));
        for state in &states {
            assert_eq!(written_and_read(state, &schema), *state, "{variant:?}");
        }
        if variant == Variant::Standard {
            let statuses: HashSet<_> = states.iter().map(TicketState::status).collect();
            assert_eq!(statuses.len(), 25, "every status the machine has");
            assert!(
                states.iter().any(|state| state.review_passes() > 0),
                "review passes are written and read back"
            );
        }
    }
}

#[test]
fn each_status_maps_onto_its_own_fields() {
    let parked = |at, awaiting_input| Status::Parked { at, awaiting_input };
    // Distinct counters, so a swapped field cannot cancel out.
    let cases = [
        (Status::Active(Stage::Build), 3, 0, 1, 0, 0, None),
        (
            Status::NeedsInput {
                return_to: Stage::Ready,
            },
            u32::MAX,
            2,
            1,
            0,
            0,
            Some(Waiting::NeedsInput),
        ),
        (
            parked(Stage::Verify, false),
            4,
            0,
            2,
            0,
            0,
            Some(Waiting::Parked),
        ),
        (
            parked(Stage::Watch, true),
            5,
            3,
            1,
            2,
            6,
            Some(Waiting::ParkedAwaitingInput),
        ),
    ];
    let schema = schema_validator();
    for (status, round, reasks, failed_runs, fix_passes, repairs, waiting) in cases {
        let state =
            TicketState::restore(status, round, reasks, failed_runs, fix_passes, repairs).unwrap();
        let persisted = PersistedState::from(&state);
        assert_eq!(
            (
                persisted.stage,
                persisted.waiting,
                persisted.round,
                persisted.reasks,
                persisted.failed_runs,
                persisted.fix_passes,
                persisted.repairs
            ),
            (
                status.stage(),
                waiting,
                round,
                reasks,
                failed_runs,
                fix_passes,
                repairs
            ),
            "{status:?}"
        );
        assert_eq!(written_and_read(&state, &schema), state);
    }

    let parked_awaiting = TicketState::restore(parked(Stage::Build, true), 1, 0, 0, 0, 0).unwrap();
    let rendered: Value =
        serde_json::from_str(&PersistedState::from(&parked_awaiting).render()).unwrap();
    assert_eq!(rendered["waiting"], json!("parked_awaiting_input"));
}

/// The reason of an invalid ticket state, or a panic for any other outcome.
fn invalid_reason<T: Debug>(result: Result<T, ContractError>) -> String {
    match result {
        Err(ContractError::Invalid { contract, reason }) => {
            assert_eq!(contract, "ticket state");
            reason
        }
        other => panic!("expected an invalid ticket state, got {other:?}"),
    }
}

#[test]
fn a_state_the_machine_cannot_be_in_is_refused() {
    let document = json!({
        "format": 3,
        "stage": "intake",
        "waiting": "needs_input",
        "round": 1,
        "reasks": 0,
        "failed_runs": 0,
        "fix_passes": 0,
        "repairs": 0,
    })
    .to_string();
    // Waiting at Intake is a round of intake's own (OWL-269); waiting with no
    // round is not.
    assert!(PersistedState::parse(&document).is_ok());
    assert!(
        invalid_reason(PersistedState::parse(
            &document.replace("\"round\":1", "\"round\":0")
        ))
        .ends_with("waiting for input with no question round")
    );

    let built = PersistedState {
        stage: Stage::Build,
        waiting: None,
        reasks: 1,
        ..PersistedState::parse(&document.replace("intake", "build")).unwrap()
    };
    assert!(
        invalid_reason(TicketState::try_from(&built))
            .ends_with("re-asks while not waiting for input")
    );

    let repaired_at_build = document
        .replace("intake", "build")
        .replace("\"repairs\":0", "\"repairs\":1");
    assert!(
        invalid_reason(PersistedState::parse(&repaired_at_build))
            .ends_with("fix passes or repairs outside watch")
    );
}

/// A `state.json` the runner wrote before OWL-55, in format 2, reads with no
/// fix pass and no repair, and is written back in the current format.
#[test]
fn a_format_2_state_reads_with_no_fix_pass_or_repair() {
    let document = json!({
        "format": 2,
        "stage": "watch",
        "waiting": "parked",
        "round": 2,
        "reasks": 0,
        "failed_runs": 2,
    })
    .to_string();
    let persisted = PersistedState::parse(&document).unwrap();
    let state = TicketState::try_from(&persisted).unwrap();
    assert_eq!(
        state,
        TicketState::restore(
            Status::Parked {
                at: Stage::Watch,
                awaiting_input: false,
            },
            2,
            0,
            2,
            0,
            0,
        )
        .unwrap()
    );
    let rendered: Value = serde_json::from_str(&persisted.render()).unwrap();
    assert!(schema_validator().is_valid(&rendered), "{rendered}");
    assert_eq!(
        (
            &rendered["format"],
            &rendered["fix_passes"],
            &rendered["repairs"]
        ),
        (&json!(4), &json!(0), &json!(0))
    );
}

/// What a ticket's runs cost (OWL-378): written and read back as the core's
/// spend, valid against its schema, and refused when its parts contradict
/// each other.
#[test]
fn a_ticket_cost_reads_back_and_refuses_parts_that_contradict() {
    let mut spend = TicketSpend::default();
    spend.ran(Some(2.5));
    spend.ran(None);
    spend.restarted();
    spend.ran(Some(0.25));
    let rendered = TicketCost::from(spend).render();
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../schemas/ticket-cost.schema.json");
    let schema: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let document: Value = serde_json::from_str(&rendered).unwrap();
    assert!(
        jsonschema::validator_for(&schema)
            .unwrap()
            .is_valid(&document)
    );
    assert_eq!(
        document,
        json!({"format": 1, "runs": 3, "unreported_runs": 1, "spent_micros": 2_750_000,
               "restarted_at_micros": 2_500_000})
    );
    let read = TicketCost::parse(&rendered).unwrap();
    assert_eq!(TicketSpend::try_from(&read).unwrap(), spend);
    for (field, value) in [("unreported_runs", 4), ("restarted_at_micros", 2_750_001)] {
        let mut broken = document.clone();
        broken[field] = json!(value);
        match TicketCost::parse(&broken.to_string()) {
            Err(ContractError::Invalid { contract, .. }) => assert_eq!(contract, "ticket cost"),
            other => panic!("{field}: expected an invalid ticket cost, got {other:?}"),
        }
    }
}

/// The session of the latest asking run (OWL-394): written and read back,
/// valid against its schema, and refused when its run is no plain name of a
/// run directory, its role and stage keep no session, its harness is blank
/// or its head is no commit id. A session id is a lowercase UUID, and one
/// made from bytes carries the version and variant of a random UUID.
#[test]
fn a_ticket_session_reads_back_and_refuses_what_no_run_keeps() {
    let document = json!({
        "format": 1, "run": "20261020T121956Z-2",
        "session": "75744e41-7eac-4fdf-aeea-df03acb72bff", "role": "build", "stage": "build",
        "harness": "claude-code:sonnet", "head": "a".repeat(40), "at": "2026-10-20T12:20:10Z",
        "cost_micros": 233_669, "context_tokens": 118_828, "round": 1, "settled": false,
    });
    let read = TicketSession::parse(&document.to_string()).unwrap();
    let rendered: Value = serde_json::from_str(&read.render()).unwrap();
    assert_eq!(rendered, document);
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../schemas/ticket-session.schema.json");
    let schema: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert!(
        jsonschema::validator_for(&schema)
            .unwrap()
            .is_valid(&document)
    );
    for (field, value) in [
        ("run", json!("../20261020T121956Z")),
        ("run", json!("")),
        ("stage", json!("verify")),
        ("role", json!("design")),
        ("harness", json!(" ")),
        ("head", json!("main")),
    ] {
        let mut broken = document.clone();
        broken[field] = value.clone();
        match TicketSession::parse(&broken.to_string()) {
            Err(ContractError::Invalid { contract, .. }) => {
                assert_eq!(contract, "ticket session");
            }
            other => panic!("{field} {value}: expected an invalid session, got {other:?}"),
        }
    }
    let mut broken = document.clone();
    broken["session"] = json!("75744E41-7EAC-4FDF-AEEA-DF03ACB72BFF");
    assert!(TicketSession::parse(&broken.to_string()).is_err());

    let made = SessionId::from_bytes([0xff; 16]);
    assert_eq!(made.as_str(), "ffffffff-ffff-4fff-bfff-ffffffffffff");
    assert_eq!(SessionId::new(made.as_str()).unwrap(), made);
    assert!(SessionId::new("ffffffff-ffff-4fff-bfff-fffffffffff").is_err());
}

/// A ticket admitted at `at` with `zones` (none for the whole repository)
/// and `resources`, no plan (OWL-448).
fn admitted(zones: &[&str], resources: &[&str], at: jiff::Timestamp) -> TicketHeld {
    let owned = |names: &[&str]| names.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>();
    let mut held = TicketHeld::dispatched(at);
    held.admit(owned(zones), owned(resources), None, at);
    held
}

/// What a ticket holds (OWL-403): written and read back, valid against its
/// schema; widened by the files its branch changed, each once and sorted,
/// with the widening's time; the whole repository past its bounds; refused
/// when a zone is no folder, a resource no name, the files unsorted, the
/// plan's hash or the mark malformed. A return to Design's admission adds
/// to what it holds, a whole repository staying whole (OWL-448).
#[test]
fn a_ticket_held_widens_reads_back_and_refuses_what_it_cannot_hold() {
    let at = |minute: i64| -> jiff::Timestamp {
        jiff::Timestamp::from_second(1_790_000_000 + minute * 60).unwrap()
    };
    let mut held = admitted(&["app"], &["migrations"], at(0));
    let files = |names: &[&str]| names.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>();
    assert_eq!(
        held.widen(files(&["b.rs", "a.rs"]), at(1)),
        ["b.rs", "a.rs"]
    );
    assert_eq!(held.widen(files(&["a.rs"]), at(2)), Vec::<String>::new());
    assert_eq!(held.files, ["a.rs", "b.rs"]);
    assert_eq!(held.widened_at, [at(1)]);
    held.verified = Some("c".repeat(40));
    held.plan_sha256 = Some("d".repeat(64));
    let document: Value = serde_json::from_str(&held.render()).unwrap();
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../schemas/ticket-held.schema.json");
    let schema: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert!(
        jsonschema::validator_for(&schema)
            .unwrap()
            .is_valid(&document)
    );
    assert_eq!(TicketHeld::parse(&held.render()).unwrap(), held);
    for (field, value) in [
        ("zones", json!(["../app"])),
        ("zones", json!(["app/*"])),
        ("resources", json!(["two words"])),
        ("files", json!(["b.rs", "a.rs"])),
        ("files", json!(["a.rs", "a.rs"])),
        ("plan_sha256", json!("D".repeat(64))),
        ("verified", json!("main")),
    ] {
        let mut broken = document.clone();
        broken[field] = value.clone();
        match TicketHeld::parse(&broken.to_string()) {
            Err(ContractError::Invalid { contract, .. }) => assert_eq!(contract, "ticket held"),
            other => panic!("{field} {value}: expected an invalid held, got {other:?}"),
        }
    }
    // Past the files it lists, or a path it cannot keep: the whole
    // repository, its admitted resources kept.
    let many: Vec<String> = (0..=MAX_HELD_FILES).map(|n| format!("f{n:05}")).collect();
    let mut wide = held.clone();
    wide.widen(many, at(3));
    assert!(wide.whole() && wide.files.is_empty(), "{wide:?}");
    assert_eq!(wide.resources, ["migrations"]);
    let mut odd = held.clone();
    odd.widen(files(&["x\0y"]), at(4));
    assert!(odd.whole());
    // A return to Design's admission adds the new plan's zones, its time
    // and its plan; the whole repository stays whole.
    let hash = "e".repeat(64);
    held.admit(
        files(&["app", "docs/a.md"]),
        Vec::new(),
        Some(hash.clone()),
        at(5),
    );
    assert_eq!(
        (
            &held.zones,
            &held.files,
            held.admitted_at,
            held.dispatched_at
        ),
        (
            &files(&["app", "docs/a.md"]),
            &files(&["a.rs", "b.rs"]),
            Some(at(5)),
            Some(at(0))
        )
    );
    assert_eq!(held.plan_sha256, Some(hash));
    wide.admit(files(&["docs"]), Vec::new(), None, at(6));
    assert!(wide.whole() && wide.files.is_empty(), "{wide:?}");
    held.admit(Vec::new(), Vec::new(), None, at(7));
    assert!(held.whole() && held.files.is_empty(), "{held:?}");
}

/// A ticket that holds the whole repository, admitted so or past its
/// bounds, lists no file: a widening then adds none, keeps no time and
/// changes nothing, and a record that lists files anyway is refused
/// (OWL-403).
#[test]
fn a_ticket_holding_the_whole_repository_is_widened_by_nothing() {
    let at = jiff::Timestamp::from_second(1_790_000_000).unwrap();
    let mut whole = admitted(&[], &[], at);
    let before = whole.clone();
    assert!(whole.widen(["src/lib.rs".to_owned()], at).is_empty());
    assert!(!whole.widen_whole(at));
    assert_eq!(whole, before);
    let unrecorded = TicketHeld::unrecorded();
    assert!(unrecorded.whole() && unrecorded.dispatched_at.is_none());
    let mut overflowed = admitted(&["app"], &[], at);
    overflowed.widen(["".to_owned()], at);
    assert!(overflowed.whole() && overflowed.files.is_empty());
    let after = overflowed.clone();
    assert!(overflowed.widen(["app/x.rs".to_owned()], at).is_empty());
    assert_eq!(overflowed, after);
    let mut listed: Value = serde_json::from_str(&whole.render()).unwrap();
    listed["files"] = json!(["src/lib.rs"]);
    assert!(TicketHeld::parse(&listed.to_string()).is_err());
}

/// A ticket dispatched since OWL-448 holds nothing until its Build's
/// admission: no zone, not the whole repository, and a record keeping a
/// zone, a resource or a plan while nothing is admitted is refused. What
/// cannot be read is never free: a widening of it keeps its files, and one
/// past its bounds or unread admits the whole repository at that time. A
/// format-1 record reads as admitted at its dispatch, holding what its
/// dispatch declared, and is written back as format 2; format 1 keeps no
/// admission.
#[test]
fn a_ticket_held_admits_nothing_at_its_dispatch_and_reads_format_1_as_admitted() {
    let at = |minute: i64| -> jiff::Timestamp {
        jiff::Timestamp::from_second(1_790_000_000 + minute * 60).unwrap()
    };
    let nothing = TicketHeld::dispatched(at(0));
    assert!(!nothing.admitted() && !nothing.whole() && nothing.zones.is_empty());
    assert_eq!(TicketHeld::parse(&nothing.render()).unwrap(), nothing);
    let document: Value = serde_json::from_str(&nothing.render()).unwrap();
    assert_eq!(document["format"], json!(2));
    for (field, value) in [
        ("zones", json!(["app"])),
        ("resources", json!(["migrations"])),
        ("plan_sha256", json!("d".repeat(64))),
    ] {
        let mut broken = document.clone();
        broken[field] = value.clone();
        assert!(
            TicketHeld::parse(&broken.to_string()).is_err(),
            "{field} {value}"
        );
    }
    let mut widened = nothing.clone();
    assert_eq!(widened.widen(["a.rs".to_owned()], at(1)), ["a.rs"]);
    assert!(!widened.admitted() && !widened.whole());
    let mut past = nothing.clone();
    past.widen([String::new()], at(2));
    assert!(past.whole() && past.admitted_at == Some(at(2)), "{past:?}");
    let mut unread = nothing.clone();
    assert!(unread.widen_whole(at(3)));
    assert!(
        unread.whole() && unread.admitted_at == Some(at(3)),
        "{unread:?}"
    );

    let old = json!({
        "format": 1,
        "zones": ["app"],
        "resources": ["migrations"],
        "files": ["app/a.rs"],
        "dispatched_at": at(0).to_string(),
    });
    let read = TicketHeld::parse(&old.to_string()).unwrap();
    assert!(read.admitted() && !read.whole(), "{read:?}");
    assert_eq!(
        (&read.zones, read.admitted_at, read.plan_sha256.as_ref()),
        (&vec!["app".to_owned()], Some(at(0)), None)
    );
    let rendered: Value = serde_json::from_str(&read.render()).unwrap();
    assert_eq!(rendered["format"], json!(2));
    assert_eq!(TicketHeld::parse(&read.render()).unwrap(), read);
    // One kept for a ref from before P6, with no dispatch time, holds the
    // whole repository.
    let first = TicketHeld::parse(r#"{"format":1}"#).unwrap();
    assert!(first.whole() && first.admitted_at.is_none());
    let mut admitting = old.clone();
    admitting["admitted_at"] = json!(at(1).to_string());
    assert!(TicketHeld::parse(&admitting.to_string()).is_err());
}
