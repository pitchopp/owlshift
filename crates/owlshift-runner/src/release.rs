//! `owlshift release TICKET` (P10 part 8, OWL-588; build plan "P10 · Teams",
//! "Releasing a claim"): a person lifts a ticket's claim on the git remote,
//! so that another runner can take the ticket. A runner never takes a claim
//! over; a person releases it.
//!
//! [`read`] reads the claim `origin` holds and says whether this machine may
//! release it ([`standing`]): at any time on the holder's machine, or when
//! the claim's operator is this machine's operator (from any of their
//! machines, which also frees the claims of a lost data directory); once it
//! expired by this machine's clock, by a person the project lists. Any other
//! live claim is refused, naming its holder and its expiry.
//!
//! [`apply`] releases what [`read`] read. On the holder's machine the
//! ticket's step under way is stopped first, as a lost claim stops it
//! (`claim_lost.json`, which `cancel::Watcher` watches), and the ticket
//! leaves every count here, until `owlshift forget`. The claim is then
//! deleted, leased on the commit read ([`claims::lift`]): a holder that
//! renewed in between keeps it. A RELEASED comment says on the ticket who
//! released it, whose claim it was and how to have it worked again; the
//! ticket's stage is left as it is, and nothing is parked.

use std::fs;
use std::time::Duration;

use jiff::Timestamp;
use owlshift_adapters::tracker::Tracker;
use owlshift_contracts::config::ProjectConfig;
use owlshift_contracts::event::EventKind;
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::refs::Claim;
use owlshift_contracts::service::BY_UI;
use serde_json::json;

use crate::claims::{self, ClaimUnread, ClaimsAt, Found, Lifted, RunnerId};
use crate::events::{EventSink, data};
use crate::executor::Git;
use crate::project::{self, ProjectDirs};
use crate::ticket_run::{self, Look};
use crate::watch::stop::CHECKED_STOP_BOUND;
use crate::writer::{ReleasedComment, Writer};

/// How long a release on the holder's machine waits for the ticket's step
/// it stopped to end: the checked stop's bound and the step's own end.
pub const STEP_WAIT: Duration = Duration::from_secs(CHECKED_STOP_BOUND.as_secs() + 15);

/// This machine's operator, as a release compares and names it: the
/// tracker account of the project's key, and its handle in `[people]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Operator {
    pub account: String,
    pub handle: Option<String>,
}

impl Operator {
    /// The operator of `account` in the project `config`.
    pub fn of(config: &ProjectConfig, account: &str) -> Self {
        Self {
            account: account.to_owned(),
            handle: config.handle_of(account).map(str::to_owned),
        }
    }

    /// Its name, as a claim names an operator: its handle, else its account.
    pub fn name(&self) -> &str {
        self.handle.as_deref().unwrap_or(&self.account)
    }

    /// Whether a claim's `operator` is this one: by handle, or by account,
    /// as a claim taken under a file without `[people]` names it.
    pub fn names(&self, operator: &str) -> bool {
        operator == self.account || self.handle.as_deref() == Some(operator)
    }

    /// Whether the project `config` lists this operator among its people,
    /// who may release an expired claim: in `[people]`, or, in a file
    /// without the table, in `[tracker].admitters`, which then lists the
    /// same accounts.
    pub fn listed(&self, config: &ProjectConfig) -> bool {
        match &config.people {
            Some(_) => self.handle.is_some(),
            None => config
                .tracker
                .admitters
                .iter()
                .flatten()
                .any(|admitter| *admitter == self.account),
        }
    }
}

/// Why this machine may release a claim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Standing {
    /// This machine's runner holds it.
    Holder,
    /// Its operator is this machine's operator.
    OwnOperator,
    /// It expired by this machine's clock, and the project lists this
    /// machine's operator.
    Expired,
}

/// Whether this machine, whose runner is `runner` (none made yet) and whose
/// operator is `operator`, listed by the project when `listed`, may release
/// `claim` of `ticket` at `now`; the refusal otherwise, naming its holder
/// and its expiry.
pub fn standing(
    ticket: &TicketId,
    claim: &Claim,
    runner: Option<&RunnerId>,
    operator: &Operator,
    listed: bool,
    now: Timestamp,
) -> Result<Standing, String> {
    if runner.is_some_and(|runner| runner.as_str() == claim.runner) {
        return Ok(Standing::Holder);
    }
    if operator.names(&claim.operator) {
        return Ok(Standing::OwnOperator);
    }
    let holder = format!("{}'s runner {}", claim.operator, claim.runner);
    let expires = utc(claim.expires_at);
    if claim.expires_at >= now {
        return Err(format!(
            "{ticket} is claimed by {holder} until {expires}, a live claim: that runner works \
             it. Ask {}, who may release it from any of their machines; a person the project \
             lists may release it once it has expired",
            claim.operator
        ));
    }
    if listed {
        return Ok(Standing::Expired);
    }
    Err(format!(
        "{ticket}'s claim, {holder}'s, expired at {expires}, and only a person the project \
         lists may release it: this machine's operator, {}, is not listed in `[people]`, or in \
         `[tracker].admitters` in a file without it",
        operator.name()
    ))
}

/// What a release works with.
#[derive(Clone, Copy)]
pub struct Release<'a> {
    pub git: &'a Git,
    pub dirs: &'a ProjectDirs,
    /// `origin`'s URL, which a missing dedicated checkout is cloned from.
    pub remote_url: &'a str,
    /// The project file the command loaded: its people, its admission
    /// gesture and its ready column.
    pub config: &'a ProjectConfig,
    /// The tracker the RELEASED comment is posted on.
    pub tracker: &'a dyn Tracker,
    pub operator: &'a Operator,
    pub clock: &'a dyn Fn() -> Timestamp,
    /// How long the holder's step under way is waited for: [`STEP_WAIT`].
    pub step_wait: Duration,
    /// Released from the web UI's button.
    pub from_ui: bool,
}

/// A claim [`read`] read and this machine may release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Read {
    pub ticket: TicketId,
    /// The commit read, which the delete is leased on.
    pub commit: String,
    pub claim: Claim,
    /// Whether the ticket's branch is on `origin`.
    pub branch: bool,
    pub standing: Standing,
}

/// What a release did, in words for a person.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Released {
    pub lines: Vec<String>,
    /// Why the RELEASED comment could not be posted: the release stands.
    pub warning: Option<String>,
}

/// Releases the claim of `ticket`: [`read`], then [`apply`].
pub fn release(
    r: &Release<'_>,
    ticket: &TicketId,
    sink: &mut EventSink<'_>,
) -> Result<Released, String> {
    let read = read(r, ticket)?;
    apply(r, read, sink)
}

/// Reads the claim `origin` holds of `ticket` and whether this machine may
/// release it ([`standing`]). Refused while the project is quarantined
/// here, as every command is; a project with no dedicated checkout here is
/// cloned first, under the project's lock, as `owlshift do` clones it.
pub fn read(r: &Release<'_>, ticket: &TicketId) -> Result<Read, String> {
    prepare(r)?;
    let checkout = r.dirs.checkout();
    let branch = crate::on_demand::branch_for(ticket);
    let Some(Found {
        commit,
        claim,
        branch: on_origin,
    }) = claims::read_on_origin(r.git, &checkout, ticket, &branch)?
    else {
        return Err(format!(
            "nothing to release: origin holds no claim of {ticket}"
        ));
    };
    let claim = claim.map_err(|unread| unread_refusal(ticket, &unread))?;
    let runner = claims::this_runner(r.dirs.data_dir());
    let standing = standing(
        ticket,
        &claim,
        runner.as_ref(),
        r.operator,
        r.operator.listed(r.config),
        (r.clock)(),
    )?;
    Ok(Read {
        ticket: ticket.clone(),
        commit,
        claim,
        branch: on_origin,
        standing,
    })
}

/// Releases the claim `read` read. On the holder's machine its step under
/// way is stopped first and waited for, [`Release::step_wait`] at most;
/// then the claim is deleted, leased on the commit read, a `decision` event
/// records it, and the RELEASED comment is posted. A claim moved since it
/// was read is kept, and the release refused: its holder renewed it. On
/// the holder's machine, a claim this runner itself renewed since is read
/// again and deleted at that commit, once.
pub fn apply(r: &Release<'_>, read: Read, sink: &mut EventSink<'_>) -> Result<Released, String> {
    let ticket = &read.ticket;
    let holder = read.standing == Standing::Holder;
    let at = ClaimsAt {
        git: r.git,
        dirs: r.dirs,
    };
    // The ticket's lock, held across the delete on the holder's machine: no
    // step of it starts here meanwhile, and none after, its claim lost.
    let _ticket_lock = if holder {
        claims::record_lost(r.dirs, ticket, None, (r.clock)())
            .map_err(|e| format!("{ticket}'s step could not be asked to stop: {e}"))?;
        match r.dirs.ticket_lock_waiting(ticket, r.step_wait) {
            Ok(Some(lock)) => Some(lock),
            Ok(None) => return Err(not_stopped(ticket, r.step_wait)),
            Err(error) => {
                return Err(format!(
                    "{ticket}'s lock could not be taken: {error}.{}",
                    stopped_tail(ticket)
                ));
            }
        }
    } else {
        None
    };
    let tail = |text: String| {
        if holder {
            format!("{text}.{}", stopped_tail(ticket))
        } else {
            text
        }
    };
    let (mut commit, mut claim) = (read.commit.clone(), read.claim.clone());
    for again in [false, true] {
        let moved = match claims::lift(at, ticket, &commit) {
            Ok(Lifted::Deleted) => None,
            Ok(Lifted::Moved(None)) => {
                return Err(tail(format!(
                    "{ticket}'s claim is gone from origin since it was read: someone else \
                     released or deleted it"
                )));
            }
            Ok(Lifted::Moved(Some(head))) => Some(head),
            Err(why) => {
                return Err(tail(format!(
                    "{ticket}'s claim could not be deleted on origin ({why}): it is kept"
                )));
            }
        };
        let Some(head) = moved else {
            break;
        };
        // On the holder's machine, its own renewal may have moved it: read
        // again, and deleted at that commit while it is still this
        // runner's.
        let found = if holder && !again {
            let branch = crate::on_demand::branch_for(ticket);
            claims::read_on_origin(r.git, &r.dirs.checkout(), ticket, &branch)
                .ok()
                .flatten()
                .filter(|found| found.commit == head)
                .and_then(|found| found.claim.ok().map(|claim| (found.commit, claim)))
                .filter(|(_, found)| found.runner == read.claim.runner)
        } else {
            None
        };
        match found {
            Some((head, found)) => (commit, claim) = (head, found),
            None => {
                return Err(tail(format!(
                    "{ticket}'s claim moved on origin since it was read, to {}: its holder \
                     renewed it, or another runner took it since, and keeps it",
                    short(&head)
                )));
            }
        }
    }
    let now = (r.clock)();
    let expired = claim.expires_at < now;
    let mut event = data([
        ("claim", json!("released")),
        ("operator", json!(claim.operator)),
        ("runner", json!(claim.runner)),
        ("expires_at", json!(claim.expires_at.to_string())),
        ("expired", json!(expired)),
        ("commit", json!(commit)),
        ("released_by", json!(r.operator.name())),
    ]);
    if r.from_ui {
        event.insert("by".to_owned(), json!(BY_UI));
    }
    sink.emit(ticket, None, EventKind::Decision, event);
    let branch = read.branch.then(|| crate::on_demand::branch_for(ticket));
    let comment = ReleasedComment {
        ticket: ticket.clone(),
        commit: commit.clone(),
        claim: claim.clone(),
        expired,
        by: r.operator.name().to_owned(),
        from_ui: r.from_ui,
        at: now,
        admit: r.config.tracker.admit.clone(),
        ready: r.config.tracker.states.ready.clone(),
        branch: branch.clone(),
    };
    let lease = if expired {
        format!("which expired at {}", utc(claim.expires_at))
    } else {
        format!("which held it until {}", utc(claim.expires_at))
    };
    let mut released = Released {
        lines: vec![format!(
            "Released {ticket}'s claim on origin, {}'s runner {}'s, {lease}: no runner works it \
             now, and its stage on the tracker is left as it is.",
            claim.operator, claim.runner
        )],
        warning: None,
    };
    match Writer::new(r.tracker).post_released(&comment) {
        Ok(_) => released.lines.push(format!(
            "Posted a RELEASED comment on {ticket}: the person whose runner should take it admits \
             it again, and that runner starts from the ticket's text and its thread."
        )),
        Err(error) => {
            let why = error.to_string();
            sink.emit(
                ticket,
                None,
                EventKind::Warning,
                data([("what", json!("release_not_told")), ("reason", json!(why))]),
            );
            released.warning = Some(format!(
                "the RELEASED comment could not be posted on {ticket} ({why}): the claim is \
                 released all the same; say on the ticket who should take it"
            ));
        }
    }
    if let Some(branch) = &branch {
        released.lines.push(format!(
            "The branch {branch} is still on origin: close its pull request and delete the \
             branch, or finish that pull request by hand, before a runner takes {ticket}."
        ));
    }
    if holder {
        released.lines.push(format!(
            "This runner's step of {ticket} was stopped, and the ticket is left out of every \
             count here: `owlshift forget {ticket}` starts it over here, should this runner take \
             it again."
        ));
    }
    Ok(released)
}

/// Refuses a release while the project is quarantined here, as every
/// command is, and clones its dedicated checkout when there is none, under
/// the project's lock. A checkout already there is not fetched: the claim's
/// read fetches the claim alone.
fn prepare(r: &Release<'_>) -> Result<(), String> {
    let _lock = match r.dirs.lock() {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            return Err(
                "another command works the project on this machine: run `owlshift release` \
                 again once it is done"
                    .to_owned(),
            );
        }
        Err(error) => return Err(format!("the project's lock: {error}")),
    };
    match ticket_run::look(r.dirs, None) {
        Ok(Look::Clear { .. }) => {}
        Ok(Look::Quarantined { .. }) => {
            return Err(
                "the project is quarantined on this machine, and every command refuses it: \
                 `owlshift unquarantine` lifts it once a person has checked it"
                    .to_owned(),
            );
        }
        Err(error) => return Err(format!("the project's quarantine: {error}")),
    }
    if fs::symlink_metadata(r.dirs.checkout()).is_err() {
        project::sync_checkout(r.git, r.dirs, r.remote_url)?;
    }
    Ok(())
}

/// Why a claim that does not read is not released.
fn unread_refusal(ticket: &TicketId, unread: &ClaimUnread) -> String {
    if unread.newer {
        format!(
            "{ticket}'s claim on origin was written by a later Owlshift ({}): upgrade Owlshift \
             to read it, then release it",
            unread.reason
        )
    } else {
        format!(
            "{ticket}'s claim on origin does not read ({}), so its holder and its expiry are \
             unknown and Owlshift does not release it: should it be no runner's, a person \
             deletes it with `git push origin --delete refs/owlshift/claims/{ticket}`",
            unread.reason
        )
    }
}

/// The refusal of a release whose step under way here did not end within
/// `wait`.
fn not_stopped(ticket: &TicketId, wait: Duration) -> String {
    format!(
        "{ticket}'s step under way here was asked to stop and has not ended within {} s: \
         run `owlshift release {ticket}` again once it has. A run typed in a terminal \
         (`owlshift do`, `owlshift continue`) is not stopped by a release: end it there first. \
         The claim is kept; this runner's watch starts nothing more of {ticket}, and leaves it \
         out of every count here",
        wait.as_secs()
    )
}

/// What a refusal on the holder's machine adds: the step was stopped and
/// the ticket left out of every count all the same.
fn stopped_tail(ticket: &TicketId) -> String {
    format!(
        " This runner stopped working {ticket} all the same: `owlshift release {ticket}` again \
         releases its claim, or `owlshift forget {ticket}` starts it over here"
    )
}

fn utc(at: Timestamp) -> String {
    at.strftime("%Y-%m-%d %H:%M UTC").to_string()
}

fn short(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use owlshift_contracts::config::Person;
    use owlshift_contracts::format::Format;
    use owlshift_contracts::refs::ClaimHeld;

    const RUNNER: &str = "0123456789abcdef";

    fn claim(operator: &str, expires: &str) -> Claim {
        Claim {
            format: Format,
            runner: RUNNER.to_owned(),
            operator: operator.to_owned(),
            claimed_at: "2026-11-18T09:00:00Z".parse().unwrap(),
            expires_at: expires.parse().unwrap(),
            held: ClaimHeld::default(),
        }
    }

    fn me() -> Operator {
        Operator {
            account: "acct-ada".to_owned(),
            handle: Some("ada".to_owned()),
        }
    }

    /// Who may release which claim: the holder and its operator at any
    /// time, a listed person once it expired; anyone else is refused,
    /// naming the holder, its runner and its expiry.
    #[test]
    fn who_may_release_a_claim() {
        let ticket = TicketId::new("OWL-1").unwrap();
        let now: Timestamp = "2026-11-20T10:00:00Z".parse().unwrap();
        let live = claim("lea", "2026-11-21T09:00:00Z");
        let expired = claim("lea", "2026-11-19T09:00:00Z");
        let ours = RunnerId::parse(RUNNER).unwrap();
        let other = RunnerId::parse("fedcba9876543210").unwrap();
        let of = |claim: &Claim, runner: Option<&RunnerId>, listed| {
            standing(&ticket, claim, runner, &me(), listed, now)
        };
        assert_eq!(of(&live, Some(&ours), false), Ok(Standing::Holder));
        for operator in ["ada", "acct-ada"] {
            let mine = claim(operator, "2026-11-21T09:00:00Z");
            assert_eq!(of(&mine, Some(&other), false), Ok(Standing::OwnOperator));
            assert_eq!(of(&mine, None, false), Ok(Standing::OwnOperator));
        }
        assert_eq!(of(&expired, Some(&other), true), Ok(Standing::Expired));
        let refused = of(&live, Some(&other), true).unwrap_err();
        assert!(
            refused.starts_with(
                "OWL-1 is claimed by lea's runner 0123456789abcdef until 2026-11-21 09:00 UTC, \
                 a live claim"
            ),
            "{refused}"
        );
        let unlisted = of(&expired, None, false).unwrap_err();
        assert!(
            unlisted.contains("expired at 2026-11-19 09:00 UTC"),
            "{unlisted}"
        );
        assert!(
            unlisted.contains("this machine's operator, ada, is not listed"),
            "{unlisted}"
        );
    }

    /// The people who may release an expired claim: `[people]`, or the
    /// admitters in a file without the table.
    #[test]
    fn the_people_listed_are_the_table_else_the_admitters() {
        let mut config = ProjectConfig::parse(
            "requires = \">=0.0\"\n[tracker]\nkind = \"linear\"\nteam = \"DEMO\"\n\
             admit = \"delegation\"\nstates = { ready = \"Todo\", working = \"In Progress\", \
             needs_input = \"c\", review = \"d\" }\n[stack]\ngate = []\n[pipeline]\n\
             default = \"trivial\"\nplan_approval = \"never\"\n[models]\n[policy]\n\
             always_human = []\n",
        )
        .unwrap();
        config.tracker.admitters = Some(vec!["acct-ada".to_owned()]);
        assert!(Operator::of(&config, "acct-ada").listed(&config));
        assert!(!Operator::of(&config, "acct-sam").listed(&config));
        config.people = Some(
            [(
                "sam".to_owned(),
                Person {
                    tracker: "acct-sam".to_owned(),
                    forge: None,
                },
            )]
            .into(),
        );
        let sam = Operator::of(&config, "acct-sam");
        assert_eq!(sam.name(), "sam");
        assert!(sam.listed(&config));
        assert!(!Operator::of(&config, "acct-ada").listed(&config));
    }
}
