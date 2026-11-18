//! The operator, and whose tickets a runner takes (P10, OWL-583; build plan
//! "P10 · Teams", "The operator, and whose tickets a runner takes";
//! architecture section 7, "Which runner takes which ticket").
//!
//! A runner's operator is the account of the tracker key `init` stored for
//! the project (`key_account`), read once when `owlshift watch --drain`
//! starts; on the Markdown tracker, whose key has no account, it is the
//! personal file's `identity.tracker`. The personal `takes` says which
//! admitted tickets the drain takes: `"mine"`, the default, those whose
//! admission gesture is the operator's, or `"all"`. [`resolve`] gives the
//! drain its [`Taking`]; the step children get it in their envelope.
//!
//! A ticket found not the operator's keeps a record in its folder,
//! `not_mine.json` ([`TicketNotMine`]), which the draining watch writes and
//! reads: while it [`rests`], the pass starts no step for the ticket.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use jiff::Timestamp;
use owlshift_adapters::tracker::{ErrorKind, Tracker};
use owlshift_contracts::config::{PersonalConfig, Takes, TrackerKind};
use owlshift_contracts::format::Format;
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::step::TicketNotMine;

use crate::project::ProjectDirs;
use crate::refusal::Refusal;
use crate::step::write_atomically;

/// The record's file name in a ticket's folder.
pub const NOT_MINE_FILE: &str = "not_mine.json";

/// How long a not-mine record rests at least before the gesture is read
/// again, once the ticket's update time has moved: ten minutes, as a
/// gesture that does not admit waits ([`crate::watch::RETRY_AFTER`]).
pub const REREAD_AFTER: Duration = crate::watch::RETRY_AFTER;

/// Which admitted tickets the drain takes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Taking {
    /// Every admitted ticket, as before P10: `takes = "all"`, a Markdown
    /// project whose personal file names no `identity.tracker`, and every
    /// command but the drain, which reads no gesture (`owlshift do`).
    #[default]
    All,
    /// Only those whose admission gesture is this operator's tracker
    /// account.
    Mine(String),
}

impl Taking {
    /// What a step's envelope carries: the operator under `Mine`.
    pub fn operator(&self) -> Option<&str> {
        match self {
            Taking::All => None,
            Taking::Mine(operator) => Some(operator),
        }
    }

    /// The taking a step's envelope says, `operator` being its field.
    pub fn of_envelope(operator: Option<&str>) -> Self {
        operator.map_or(Taking::All, |operator| Taking::Mine(operator.to_owned()))
    }
}

/// What the drain of the project on `kind`'s tracker, of `repository`
/// (`github.com/<owner>/<name>`), takes, under the personal file `personal`
/// (`None` when none is loaded, which reads as its defaults): under
/// `takes = "mine"`, the account of `tracker`'s key, read now. A key that
/// cannot be read refuses the drain's start, as a missing admitters list
/// does: waiting for the network when the tracker gave no answer at all,
/// passing when it answered with an error a retry may clear, lasting
/// otherwise, and lasting for a key that is an app's. On the Markdown
/// tracker the operator is `identity.tracker`, and a personal file without
/// it takes every admitted ticket, as before (decided 2026-11-18: a
/// one-person project behaves as before).
pub fn resolve(
    tracker: &dyn Tracker,
    kind: TrackerKind,
    personal: Option<&PersonalConfig>,
    repository: &str,
) -> Result<Taking, Refusal> {
    let takes = personal.map_or(Takes::Mine, |personal| personal.takes_for(Some(repository)));
    if takes == Takes::All {
        return Ok(Taking::All);
    }
    if kind == TrackerKind::Markdown {
        let identity = personal.and_then(|personal| personal.identity.tracker.as_deref());
        return Ok(identity
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map_or(Taking::All, |id| Taking::Mine(id.to_owned())));
    }
    const FIX: &str = "the drain takes only the tickets its operator admitted (`takes = \"mine\"` \
                       in the personal file); set `takes = \"all\"` there to take every admitted \
                       ticket";
    match tracker.operator() {
        Ok(Some(account)) if account.app => Err(format!(
            "the tracker key is the account of an app, {:?}, not a person's, so this runner has no \
             operator: {FIX}",
            account.name
        )
        .into()),
        Ok(Some(account)) => Ok(Taking::Mine(account.id)),
        Ok(None) => Err(format!("the tracker key has no account to read: {FIX}").into()),
        Err(error) => {
            let message = format!("the tracker key's account could not be read ({error}): {FIX}");
            Err(match error.kind {
                ErrorKind::Unreachable => Refusal::unreachable(message),
                ErrorKind::Other => Refusal::passing(message),
                ErrorKind::Unauthorized | ErrorKind::NotFound | ErrorKind::NoGesture => {
                    message.into()
                }
            })
        }
    }
}

/// The record of `ticket` in the project of `dirs`.
pub fn record_file(dirs: &ProjectDirs, ticket: &TicketId) -> PathBuf {
    dirs.ticket_dir(ticket).join(NOT_MINE_FILE)
}

/// The not-mine record of `ticket`; `None` when there is none or it does
/// not read, which counts as none: its ticket is read again.
pub fn read(dirs: &ProjectDirs, ticket: &TicketId) -> Option<TicketNotMine> {
    let text = fs::read_to_string(record_file(dirs, ticket)).ok()?;
    TicketNotMine::parse(&text).ok()
}

/// Writes the not-mine record of `ticket`: the gesture's `account`, read
/// against `operator` by a step that started at `read_at`, whose pass
/// listed the ticket updated at `updated_at`.
pub fn write(
    dirs: &ProjectDirs,
    ticket: &TicketId,
    account: &str,
    operator: &str,
    read_at: Timestamp,
    updated_at: Option<Timestamp>,
) -> Result<(), String> {
    let record = TicketNotMine {
        format: Format,
        account: account.to_owned(),
        operator: operator.to_owned(),
        read_at,
        updated_at,
    };
    let path = record_file(dirs, ticket);
    write_atomically(&path, &record.render()).map_err(|e| format!("{}: {e}", path.display()))
}

/// Removes the not-mine record of `ticket`, if any.
pub fn remove(dirs: &ProjectDirs, ticket: &TicketId) {
    let _ = fs::remove_file(record_file(dirs, ticket));
}

/// Whether `record` keeps its ticket out of this pass: a record of this
/// operator's (`operator`), unless the tracker's update time of the ticket
/// the pass listed, `updated_at`, has moved since (an unknown one counting
/// as moved) and [`REREAD_AFTER`] has passed since it was read, at `now`.
pub fn rests(
    record: &TicketNotMine,
    operator: &str,
    updated_at: Option<Timestamp>,
    now: Timestamp,
) -> bool {
    if record.operator != operator {
        return false;
    }
    let moved = updated_at.is_none() || updated_at != record.updated_at;
    let due = record
        .read_at
        .checked_add(REREAD_AFTER)
        .is_ok_and(|due| now >= due);
    !(moved && due)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use owlshift_adapters::tracker::{Capability, Comment, Error, KeyAccount, Ticket};

    use super::*;

    struct Keyed(RefCell<Option<Result<Option<KeyAccount>, Error>>>);

    impl Tracker for Keyed {
        fn capabilities(&self) -> &'static [Capability] {
            &[]
        }
        fn ticket(&self, _: &TicketId) -> Result<Ticket, Error> {
            unreachable!()
        }
        fn comments(&self, _: &TicketId) -> Result<Vec<Comment>, Error> {
            unreachable!()
        }
        fn post_comment(&self, _: &TicketId, _: &str) -> Result<Comment, Error> {
            unreachable!()
        }
        fn set_stage(&self, _: &TicketId, _: &str) -> Result<(), Error> {
            unreachable!()
        }
        fn operator(&self) -> Result<Option<KeyAccount>, Error> {
            self.0.borrow_mut().take().expect("read once")
        }
    }

    fn key(answer: Result<Option<KeyAccount>, Error>) -> Keyed {
        Keyed(RefCell::new(Some(answer)))
    }

    fn account(id: &str, app: bool) -> KeyAccount {
        KeyAccount {
            id: id.to_owned(),
            name: format!("{id}'s name"),
            app,
            workspace: String::new(),
        }
    }

    const REPO: &str = "github.com/acme/api";

    #[test]
    fn the_operator_is_the_keys_account_and_all_reads_no_key() {
        let personal = |text: &str| PersonalConfig::parse(text).unwrap();
        let linear = TrackerKind::Linear;
        let mine = resolve(&key(Ok(Some(account("u-1", false)))), linear, None, REPO);
        assert_eq!(mine.unwrap(), Taking::Mine("u-1".to_owned()));
        // Under `all` the key is not read: `Keyed` would panic on a second
        // read, and none is given.
        let all = personal("[repositories.\"github.com/acme/api\"]\ntakes = \"all\"\n");
        let none = Keyed(RefCell::new(None));
        assert_eq!(
            resolve(&none, linear, Some(&all), REPO).unwrap(),
            Taking::All
        );
        let app = resolve(&key(Ok(Some(account("app-1", true)))), linear, None, REPO);
        let app = app.unwrap_err();
        assert!(app.lasting() && app.message.contains("an app"), "{app:?}");
    }

    #[test]
    fn a_key_that_cannot_be_read_refuses_by_what_may_mend_it() {
        let refused = |kind| {
            resolve(
                &key(Err(Error::new(kind, "x"))),
                TrackerKind::Github,
                None,
                REPO,
            )
            .unwrap_err()
        };
        let unreachable = refused(ErrorKind::Unreachable);
        assert!(unreachable.unanswered(), "{unreachable:?}");
        assert!(
            unreachable.message.contains("`takes = \"all\"`"),
            "{unreachable:?}"
        );
        assert!(!refused(ErrorKind::Other).lasting());
        assert!(refused(ErrorKind::Unauthorized).lasting());
    }

    #[test]
    fn on_markdown_the_operator_is_the_identity_and_none_takes_all() {
        let none = Keyed(RefCell::new(None));
        let markdown = TrackerKind::Markdown;
        assert_eq!(resolve(&none, markdown, None, REPO).unwrap(), Taking::All);
        let named = PersonalConfig::parse("[identity]\ntracker = \"lea\"\n").unwrap();
        assert_eq!(
            resolve(&none, markdown, Some(&named), REPO).unwrap(),
            Taking::Mine("lea".to_owned())
        );
    }

    #[test]
    fn a_record_rests_until_the_ticket_moved_and_ten_minutes_passed() {
        let at = |time: &str| -> Timestamp { time.parse().unwrap() };
        let record = TicketNotMine {
            format: Format,
            account: "lea".to_owned(),
            operator: "me".to_owned(),
            read_at: at("2026-11-18T10:00:00Z"),
            updated_at: Some(at("2026-11-18T09:00:00Z")),
        };
        let same = Some(at("2026-11-18T09:00:00Z"));
        let moved = Some(at("2026-11-18T10:05:00Z"));
        assert!(rests(&record, "me", same, at("2026-11-18T12:00:00Z")));
        assert!(rests(&record, "me", moved, at("2026-11-18T10:09:59Z")));
        assert!(!rests(&record, "me", moved, at("2026-11-18T10:10:00Z")));
        assert!(!rests(&record, "me", None, at("2026-11-18T10:10:00Z")));
        assert!(!rests(
            &record,
            "someone else",
            same,
            at("2026-11-18T10:01:00Z")
        ));
    }
}
