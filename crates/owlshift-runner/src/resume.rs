//! Resuming the asking run's session (OWL-394, architecture section 4,
//! "Resuming the asking run's session"): the id a run's own session starts
//! under, and the rule that says whether the next Design or Build run
//! resumes the session the ticket ref keeps, or why it starts fresh.
//! Pure: the caller reads the branch head, the clock and the kept
//! transcript.

use jiff::{SignedDuration, Timestamp};
use sha2::{Digest, Sha256};

use owlshift_contracts::ids::{SessionId, TicketId};
use owlshift_contracts::refs::TicketSession;
use owlshift_contracts::{Role, Stage};

/// How long the prompt cache keeps a session's context after its last
/// request: every cache write of live check C18 was a one-hour one
/// (`ephemeral_1h`). A resume past it is cold.
pub const CACHE_LIFETIME: SignedDuration = SignedDuration::from_hours(1);

/// The largest context a cold resume takes, in tokens. A cold resume writes
/// the whole context to the cache again, about 3.7 USD per million tokens
/// on Sonnet 5.5 in C18, so 200,000 tokens cost about 0.75 USD, under half
/// of a restarted Build's 1.6 USD average on the P5 gate; past it the
/// rewrite alone nears what a restart costs. A warm resume reads the
/// context at a tenth of that price, so the cap is not its.
pub const RESUME_CONTEXT_CAP_TOKENS: u64 = 200_000;

/// The id of the session run `run` of `ticket` starts when it resumes none:
/// a UUID made from the SHA-256 of both, so it is the run's alone and the
/// same each time it is computed.
pub fn session_id(ticket: &TicketId, run: &str) -> SessionId {
    let digest = Sha256::new()
        .chain_update(b"owlshift-session\0")
        .chain_update(ticket.as_str())
        .chain_update([0])
        .chain_update(run)
        .finalize();
    SessionId::from_bytes(
        digest[..16]
            .try_into()
            .expect("a SHA-256 holds sixteen bytes"),
    )
}

/// Why the next run starts fresh rather than resume the kept session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fresh {
    /// The session is another role's or stage's.
    OtherStage,
    /// Its questions are not all decided or answered.
    NotSettled,
    /// The harness or the model the run asks for changed.
    HarnessChanged,
    /// The ticket's branch moved since the asking run ended.
    HeadMoved,
    /// The session's cost at the asking run's end is unknown, so the
    /// resumed run's own could not be counted.
    CostUnknown,
    /// A cold resume of a context of unknown size.
    ContextUnknown,
    /// A cold resume of a context past [`RESUME_CONTEXT_CAP_TOKENS`].
    ContextOverCap,
    /// The transcript is not kept on this machine.
    TranscriptMissing,
}

impl Fresh {
    /// The reason as the `run_started` event's `fresh` names it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OtherStage => "other_stage",
            Self::NotSettled => "not_settled",
            Self::HarnessChanged => "harness_changed",
            Self::HeadMoved => "head_moved",
            Self::CostUnknown => "cost_unknown",
            Self::ContextUnknown => "context_unknown",
            Self::ContextOverCap => "context_over_cap",
            Self::TranscriptMissing => "transcript_missing",
        }
    }

    /// Whether a resume was due, so the event says why it did not happen:
    /// every reason but a session of another stage or not settled.
    pub fn due(self) -> bool {
        !matches!(self, Self::OtherStage | Self::NotSettled)
    }
}

/// The run about to start, as the rule compares it with the kept session.
#[derive(Clone, Copy, Debug)]
pub struct Next<'a> {
    pub role: Role,
    pub stage: Stage,
    /// What its harness names its sessions by (`Harness::session_key`).
    pub harness: &'a str,
    /// The ticket branch's head now, `None` when it could not be read.
    pub head: Option<&'a str>,
    pub now: Timestamp,
}

/// Whether `next` resumes `kept`, the transcript's presence aside: the
/// first reason it does not, in the order of [`Fresh`].
pub fn decide(kept: &TicketSession, next: &Next<'_>) -> Result<(), Fresh> {
    if kept.role != next.role || kept.stage != next.stage {
        return Err(Fresh::OtherStage);
    }
    if !kept.settled {
        return Err(Fresh::NotSettled);
    }
    if kept.harness != next.harness {
        return Err(Fresh::HarnessChanged);
    }
    if next.head != Some(kept.head.as_str()) {
        return Err(Fresh::HeadMoved);
    }
    if kept.cost_micros.is_none() {
        return Err(Fresh::CostUnknown);
    }
    if next.now.duration_since(kept.at) >= CACHE_LIFETIME {
        match kept.context_tokens {
            None => return Err(Fresh::ContextUnknown),
            Some(tokens) if tokens > RESUME_CONTEXT_CAP_TOKENS => {
                return Err(Fresh::ContextOverCap);
            }
            Some(_) => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use owlshift_contracts::format::Format;

    use super::*;

    const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

    fn kept() -> TicketSession {
        TicketSession {
            format: Format,
            run: "20261020T121956Z".to_owned(),
            session: SessionId::new("75744e41-7eac-4fdf-aeea-df03acb72bff").unwrap(),
            role: Role::Build,
            stage: Stage::Build,
            harness: "claude-code:sonnet".to_owned(),
            head: HEAD.to_owned(),
            at: "2026-10-20T12:20:00Z".parse().unwrap(),
            cost_micros: Some(233_669),
            context_tokens: Some(RESUME_CONTEXT_CAP_TOKENS + 1),
            round: None,
            settled: true,
        }
    }

    fn next(minutes: i64) -> Next<'static> {
        Next {
            role: Role::Build,
            stage: Stage::Build,
            harness: "claude-code:sonnet",
            head: Some(HEAD),
            now: kept().at + SignedDuration::from_mins(minutes),
        }
    }

    /// A settled session of the same role, stage, harness and head resumes
    /// within the cache's hour whatever its context, and past it only with
    /// a context it knows within the cap; each other reason starts fresh,
    /// and says so when a resume was due.
    #[test]
    fn a_settled_session_resumes_unless_a_reason_says_otherwise() {
        assert_eq!(decide(&kept(), &next(59)), Ok(()));
        assert_eq!(decide(&kept(), &next(60)), Err(Fresh::ContextOverCap));
        let within = TicketSession {
            context_tokens: Some(RESUME_CONTEXT_CAP_TOKENS),
            ..kept()
        };
        assert_eq!(decide(&within, &next(600)), Ok(()));
        let unknown = TicketSession {
            context_tokens: None,
            ..kept()
        };
        assert_eq!(decide(&unknown, &next(5)), Ok(()));
        assert_eq!(decide(&unknown, &next(60)), Err(Fresh::ContextUnknown));

        let design = Next {
            role: Role::Design,
            stage: Stage::Design,
            ..next(1)
        };
        let unsettled = TicketSession {
            settled: false,
            ..kept()
        };
        let costless = TicketSession {
            cost_micros: None,
            ..kept()
        };
        let cases = [
            (decide(&kept(), &design), Fresh::OtherStage),
            (decide(&unsettled, &next(1)), Fresh::NotSettled),
            (
                decide(
                    &kept(),
                    &Next {
                        harness: "claude-code:opus",
                        ..next(1)
                    },
                ),
                Fresh::HarnessChanged,
            ),
            (
                decide(
                    &kept(),
                    &Next {
                        head: Some("1123456789abcdef0123456789abcdef01234567"),
                        ..next(1)
                    },
                ),
                Fresh::HeadMoved,
            ),
            (
                decide(
                    &kept(),
                    &Next {
                        head: None,
                        ..next(1)
                    },
                ),
                Fresh::HeadMoved,
            ),
            (decide(&costless, &next(1)), Fresh::CostUnknown),
        ];
        for (decided, reason) in cases {
            assert_eq!(decided, Err(reason));
        }
        assert!(!Fresh::OtherStage.due() && !Fresh::NotSettled.due());
        assert!(Fresh::HeadMoved.due() && Fresh::TranscriptMissing.due());
    }

    /// A run's own session id is a UUID of its own, the same each time.
    #[test]
    fn a_runs_session_id_is_its_own() {
        let ticket = TicketId::new("OWL-394").unwrap();
        let id = session_id(&ticket, "20261020T121956Z");
        assert_eq!(id, session_id(&ticket, "20261020T121956Z"));
        assert_ne!(id, session_id(&ticket, "20261020T121956Z-2"));
        assert_ne!(
            id,
            session_id(&TicketId::new("OWL-395").unwrap(), "20261020T121956Z")
        );
        assert_eq!(SessionId::new(id.as_str()).unwrap(), id);
        assert_eq!(&id.as_str()[14..15], "4");
    }
}
