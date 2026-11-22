//! A ticket handed back to a person (OWL-619; architecture section 4, "A
//! ticket with no step for a run"), as `owlshift status`, `owlshift status
//! --waits`, `owlshift why` and the web UI show it (OWL-626): when, what its
//! run read, and the gesture that gives it back, in the HANDBACK comment's
//! own words.
//!
//! What was read is told from what the ticket ref keeps beside its handback,
//! `handback.json` keeping its time alone: the plan of a person's steps alone
//! a design run wrote, or the words of the ticket intake read as a person's
//! whole ([`of`]).

use jiff::Timestamp;
use owlshift_contracts::config::ProjectConfig;
use owlshift_contracts::ids::TicketId;
use owlshift_core::state::{Status, TicketState};
use owlshift_core::vocab::Stage;

use crate::project::ProjectDirs;
use crate::served;
use crate::ticket_ref::TicketRecord;
use crate::voice::Voice;
use crate::writer::{HandbackRead, code, flatten, readmit_words};

/// The gesture's words when the project file the project is served on does
/// not read.
pub const ANY_GESTURE: &str = "make the project's admission gesture again";

/// A ticket handed back to a person, as shown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandedBack {
    /// When it was handed back (`handback.json`).
    pub at: Timestamp,
    /// What its run read, as its HANDBACK comment says it; `None` when the
    /// ref keeps neither a plan of a person's steps alone nor intake's
    /// quote.
    pub read: Option<HandbackRead>,
    /// The gesture that admits it again ([`gesture`]).
    pub gesture: String,
}

/// The ticket of `record` as handed back, while it is: its ref keeps a
/// handback and its state is at Ready, as `owlshift continue`'s refusal
/// reads it. `gesture` gives the gesture's words, asked only then.
///
/// A design handback keeps its plan, every step a person's, hashed from the
/// text the design run read; an intake handback keeps the quote in its
/// intake record. A ref may keep both: a ticket handed back at intake and
/// admitted again on the same text goes on to Design with that record, and
/// its plan may be handed back in turn, the plan then hashed as the record
/// is. A plan hashed otherwise is older than the record: the text changed
/// after a design handback, which ran intake again, and intake handed the
/// ticket back.
pub fn of(record: &TicketRecord, gesture: &mut dyn FnMut() -> String) -> Option<HandedBack> {
    let handback = record.handback.as_ref()?;
    let state = TicketState::try_from(&record.state).ok()?;
    if state.status() != Status::Active(Stage::Ready) {
        return None;
    }
    let plan = record
        .plan
        .as_ref()
        .filter(|plan| !plan.plan.has_agent_step());
    let person_only = record
        .intake
        .as_ref()
        .and_then(|intake| intake.person_only.as_ref().map(|p| (intake, p)));
    let read = match (plan, person_only) {
        (Some(plan), Some((intake, _))) if intake.text_sha256 == plan.text_sha256 => {
            Some(HandbackRead::Design {
                steps: plan.owed_steps(),
            })
        }
        (_, Some((_, person_only))) => Some(HandbackRead::Intake {
            quote: person_only.quote.clone(),
        }),
        (Some(plan), None) => Some(HandbackRead::Design {
            steps: plan.owed_steps(),
        }),
        (None, None) => None,
    };
    Some(HandedBack {
        at: handback.at,
        read,
        gesture: gesture(),
    })
}

/// The gesture that admits a handed-back ticket of the project of `dirs`
/// again, as its HANDBACK comment words it: from the project file its
/// `served.json` keeps, the one its watch runs on; [`ANY_GESTURE`] when
/// none reads.
pub fn gesture(dirs: &ProjectDirs) -> String {
    served::read_record(dirs)
        .ok()
        .flatten()
        .and_then(|record| record.kept)
        .and_then(|kept| ProjectConfig::parse(&kept.text).ok())
        .map_or_else(
            || ANY_GESTURE.to_owned(),
            |config| readmit_words(&config.tracker.admit, &config.tracker.states.ready),
        )
}

impl HandedBack {
    /// What was read and the gesture that gives `ticket` back, in `voice`:
    /// the terminal's names `owlshift do` too, which the web UI has no
    /// button for.
    pub fn words(&self, ticket: &TicketId, voice: Voice) -> String {
        let read = match &self.read {
            Some(HandbackRead::Intake { quote }) => format!(
                "intake read it so in the ticket's own words: {}",
                code(&flatten(quote))
            ),
            Some(HandbackRead::Design { steps }) => format!(
                "every step of its plan is a person's ({} step{})",
                steps.len(),
                if steps.len() == 1 { "" } else { "s" }
            ),
            None => "its HANDBACK comment says what was read".to_owned(),
        };
        let command = match voice {
            Voice::Terminal => format!(", or `owlshift do {ticket}`"),
            Voice::Ui => String::new(),
        };
        format!(
            "{read}; to give it back to Owlshift, {} after its HANDBACK comment{command}",
            self.gesture
        )
    }
}

#[cfg(test)]
mod tests {
    use owlshift_contracts::drain::{TicketHandback, TicketIntake};
    use owlshift_contracts::format::Format;
    use owlshift_contracts::refs::{PersistedState, TicketQuestions};
    use owlshift_contracts::review::TicketPlan;
    use serde_json::json;

    use super::*;

    fn record(status: Status, intake: Option<(&str, bool)>, plan: Option<&str>) -> TicketRecord {
        let state = TicketState::restore(status, 1, 0, 0, 0, 0).unwrap();
        let intake = intake.map(|(hash, person_only)| {
            let mut value = json!({
                "format": 3,
                "text_sha256": hash.repeat(64),
                "gesture": { "kind": "label", "name": "agent", "account": "u-ann",
                             "at": "2026-11-22T08:00:00Z" },
                "variant": "standard",
                "routing": { "harness": "claude", "tier": "standard", "effort": "high" }
            });
            if person_only {
                value["person_only"] = json!({ "quote": "I will check it myself." });
            }
            TicketIntake::parse(&value.to_string()).unwrap()
        });
        let plan = plan.map(|hash| {
            let value = json!({
                "format": 3,
                "plan": {
                    "goal": "G", "criteria": ["C1"], "approach": "A", "alternatives": [],
                    "files": [], "risks": [], "out_of_scope": [], "forks": [],
                    "steps": [
                        { "what": "Open it.", "test": "It opens.", "by": "person",
                          "when": "before_merge" },
                        { "what": "Tell them.", "test": "Told.", "by": "person",
                          "when": "after_merge" }
                    ]
                },
                "text_sha256": hash.repeat(64),
                "base": "b".repeat(40)
            });
            TicketPlan::parse(&value.to_string()).unwrap()
        });
        TicketRecord {
            state: PersistedState::from(&state),
            questions: TicketQuestions::new(),
            delivery: None,
            intake,
            plan,
            review: None,
            cost: None,
            session: None,
            held: None,
            handback: Some(TicketHandback {
                format: Format,
                at: "2026-11-22T10:00:00Z".parse().unwrap(),
            }),
        }
    }

    /// What was read is told from the ref: a plan of a person's steps
    /// hashed as the intake record (or with none) is the design run's; a
    /// person's quote with no such plan is intake's; a ticket shown handed
    /// back only at Ready and while its handback stands. Each voice words
    /// the gesture, the terminal's naming `owlshift do` too.
    #[test]
    fn what_was_read_is_told_from_the_ref() {
        let ready = Status::Active(Stage::Ready);
        let ticket = TicketId::new("OWL-1").unwrap();
        let shown = |record: &TicketRecord| {
            of(record, &mut || "move it back to `Todo`".to_owned())
                .map(|handed_back| handed_back.words(&ticket, Voice::Terminal))
        };
        let design = "every step of its plan is a person's (2 steps)";
        let intake = "intake read it so in the ticket's own words: `I will check it myself.`";
        for (record, read) in [
            (record(ready, None, Some("a")), Some(design)),
            (record(ready, Some(("a", false)), Some("a")), Some(design)),
            // Handed back at intake, admitted on the same text, then at
            // Design.
            (record(ready, Some(("a", true)), Some("a")), Some(design)),
            // Handed back at Design, the text edited, then at intake.
            (record(ready, Some(("c", true)), Some("a")), Some(intake)),
            (record(ready, Some(("c", true)), None), Some(intake)),
            (
                record(ready, Some(("c", false)), None),
                Some("its HANDBACK comment says what was read"),
            ),
            (record(Status::Active(Stage::Design), None, Some("a")), None),
        ] {
            let words = shown(&record);
            assert_eq!(
                words
                    .as_deref()
                    .map(|words| words.split(';').next().unwrap()),
                read,
                "{words:?}"
            );
        }
        let mut gone = record(ready, None, Some("a"));
        gone.handback = None;
        assert_eq!(shown(&gone), None);

        let handed_back = of(&record(ready, None, Some("a")), &mut || "G".to_owned()).unwrap();
        assert_eq!(
            handed_back.words(&ticket, Voice::Terminal),
            format!(
                "{design}; to give it back to Owlshift, G after its HANDBACK comment, or `owlshift do OWL-1`"
            )
        );
        assert_eq!(
            handed_back.words(&ticket, Voice::Ui),
            format!("{design}; to give it back to Owlshift, G after its HANDBACK comment")
        );
    }
}
