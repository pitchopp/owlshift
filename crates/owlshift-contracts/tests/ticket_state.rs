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

use owlshift_contracts::refs::{PersistedState, Waiting};
use owlshift_contracts::{ContractError, Stage, Variant};
use owlshift_core::pipeline::Pipeline;
use owlshift_core::state::{Event, Status, TicketState, Transition};
use serde_json::{Value, json};

/// Every state reachable from admission in `pipeline`, with question rounds
/// bounded at three: past that, only the round counter grows.
fn reachable(pipeline: Pipeline) -> HashSet<TicketState> {
    let mut seen = HashSet::from([TicketState::admitted()]);
    let mut queue = VecDeque::from([TicketState::admitted()]);
    while let Some(current) = queue.pop_front() {
        for event in Event::ALL {
            let next = match current.apply(pipeline, event) {
                Ok(Transition::To(next) | Transition::Parked { state: next, .. }) => next,
                Ok(Transition::Finished(_)) | Err(_) => continue,
            };
            if next.round() <= 3 && seen.insert(next) {
                queue.push_back(next);
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
    for variant in [Variant::Standard, Variant::Trivial] {
        let states = reachable(Pipeline::new(variant));
        for state in &states {
            assert_eq!(written_and_read(state, &schema), *state, "{variant:?}");
        }
        if variant == Variant::Standard {
            let statuses: HashSet<_> = states.iter().map(TicketState::status).collect();
            assert_eq!(statuses.len(), 29, "every status the machine has");
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
