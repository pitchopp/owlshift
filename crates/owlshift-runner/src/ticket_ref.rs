//! The ticket ref: what the runner keeps about a ticket between two
//! commands (P2, OWL-122). `refs/owlshift/tickets/<ticket>` points to a
//! commit whose tree holds the ticket's core state ([`STATE_FILE`]) and the
//! questions the runner asked on it ([`QUESTIONS_FILE`]), so `owlshift
//! continue` knows the round, its re-asks and failed runs, and which comments
//! are the runner's asks; once a delivery was made, the pull request
//! `owlshift watch` follows ([`DELIVERY_FILE`], OWL-209); and once intake
//! ran, its record ([`INTAKE_FILE`], OWL-248); once a design run wrote a
//! plan, the plan ([`PLAN_FILE`]) and its review passes ([`REVIEW_FILE`],
//! OWL-303); once a run ended, what the ticket's runs cost ([`COST_FILE`],
//! OWL-378, written right after each run since OWL-397, which watch's
//! [`Progress`] leaves out); once a Design or Build run asked, its session
//! ([`SESSION_FILE`], OWL-394, which [`Progress`] leaves out too and
//! [`forget`] drops); from its dispatch, what it holds against the
//! project's other tickets ([`HELD_FILE`], OWL-403, which [`Progress`] leaves
//! out and [`forget`] drops); and once a run proposed a follow-up, the
//! proposals and what became of them ([`PROPOSALS_FILE`], OWL-253), which no
//! write drops but [`forget`]'s of an invalid one.
//!
//! The ref lives in the dedicated checkout only (decided 2026-10-02): it is
//! not pushed, which architecture section 7 plans for once claims exist
//! (P7). Deleting the dedicated checkout loses it; the comments stay on the
//! ticket. Each write is a commit whose parent is the previous one, and the
//! ref moves only from the commit it was read at (`update-ref` with the old
//! value). The runner writes between runs; a run that touches the ref breaks
//! the isolation check, which covers every ref but the run's branch and the
//! remote-tracking refs.
//!
//! Every git command is the runner's own ([`Git`]): no hook, no file-system
//! monitor, no filter on the blobs, and no signature on the commits.
//! `commit-tree` did not read `commit.gpgSign` when checked (2026-10-02, git
//! 2.54.0: it wrote a commit with `commit.gpgSign=true` and a `gpg.program`
//! that always fails); `--no-gpg-sign` is passed anyway, so a person's
//! signing setup never stops the runner.
//!
//! A ticket ref that has no room for its next write, or cannot be read, is
//! started over by [`forget`] (`owlshift forget TICKET`, OWL-202): a new
//! record on top of the old one, keeping the round count and the refusals
//! the next runs are told.

use std::path::Path;

use sha2::{Digest, Sha256};

use owlshift_contracts::drain::{TicketIntake, TicketProposals};
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::refs::{
    COST_FILE, DELIVERY_FILE, HELD_FILE, INTAKE_FILE, PLAN_FILE, PROPOSALS_FILE, PersistedState,
    QUESTIONS_FILE, REVIEW_FILE, SESSION_FILE, STATE_FILE, TICKETS_PREFIX, TicketCost,
    TicketDelivery, TicketHeld, TicketQuestions, TicketSession, ticket_ref,
};
use owlshift_contracts::review::{TicketPlan, TicketReview};
use owlshift_core::state::{Status, TicketState};
use owlshift_core::vocab::Stage;

use crate::executor::Git;

/// The most a file of the ticket ref may hold, in bytes: [`read()`] refuses
/// a larger one, so [`write()`] refuses to write one (OWL-200). The runner's own
/// files are far smaller: a round of three questions with their verdicts
/// takes about 3 KB of `questions.json`, a decision about 1.2 KB (OWL-202).
pub const MAX_FILE: u64 = 1024 * 1024;

/// What the ticket ref holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TicketRecord {
    pub state: PersistedState,
    pub questions: TicketQuestions,
    /// The delivery, once one was made: `None` when the tree holds no
    /// [`DELIVERY_FILE`].
    pub delivery: Option<TicketDelivery>,
    /// The intake record, once intake ran: `None` when the tree holds no
    /// [`INTAKE_FILE`] (OWL-248).
    pub intake: Option<TicketIntake>,
    /// The latest plan of the ticket's design runs, once one wrote it:
    /// `None` when the tree holds no [`PLAN_FILE`] (OWL-303).
    pub plan: Option<TicketPlan>,
    /// The review passes of the ticket, once one ran: `None` when the tree
    /// holds no [`REVIEW_FILE`] (OWL-303).
    pub review: Option<TicketReview>,
    /// What the ticket's runs cost, once a write followed a run: `None` when
    /// the tree holds no [`COST_FILE`] (OWL-378).
    pub cost: Option<TicketCost>,
    /// The session of the latest Design or Build run that asked, until the
    /// next run of its role spends it: `None` when the tree holds no
    /// [`SESSION_FILE`] (OWL-394).
    pub session: Option<TicketSession>,
    /// What the ticket holds against the project's other tickets, from its
    /// dispatch: `None` when the tree holds no [`HELD_FILE`] (OWL-403), as
    /// for a ticket dispatched before it was kept.
    pub held: Option<TicketHeld>,
}

/// The hash an intake record keeps of a ticket's text (architecture section
/// 5, "Intake"): the SHA-256 of its title, a NUL byte and its description,
/// in lowercase hex. Intake runs again once the text no longer has it.
pub fn text_sha256(title: &str, description: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(title.as_bytes());
    hash.update([0]);
    hash.update(description.as_bytes());
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A ticket record and the commit it was read from, or last written as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stored {
    pub record: TicketRecord,
    pub commit: String,
    /// The follow-ups the ticket's runs proposed (OWL-253): `None` when the
    /// tree holds no [`PROPOSALS_FILE`]. Kept out of [`TicketRecord`]: no
    /// write drops it, so a write that does not name it keeps it as it is.
    pub proposals: Option<TicketProposals>,
}

impl Stored {
    /// What this record keeps but its runs' cost ([`progress`], OWL-397).
    pub fn progress(&self) -> Progress {
        progress(&self.record, self.proposals.as_ref())
    }

    /// Where this record is: its commit and its progress.
    pub fn at(&self) -> RefAt {
        RefAt {
            commit: self.commit.clone(),
            progress: self.progress(),
        }
    }
}

/// Where a ticket ref is (OWL-397): its commit, which the reads that make
/// sure no other command wrote it since compare, and its [`Progress`],
/// which watch reads as progress.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefAt {
    pub commit: String,
    pub progress: Progress,
}

impl RefAt {
    /// Where a ticket without a ref is: no commit, and the progress of
    /// [`blank`], which keeps nothing either, so the ref a first run makes
    /// for its cost alone is no progress.
    pub fn none() -> Self {
        RefAt {
            commit: String::new(),
            progress: progress(&blank(None), None),
        }
    }
}

/// What a ticket ref keeps but what its runs cost (OWL-397, architecture
/// section 5, "A move that is only the spend"): the runner writes
/// [`COST_FILE`] right after every run, so watch reads a move of the ref as
/// progress only when this changes. A type of its own, never a commit: the
/// reads that make sure no other command wrote the ref since compare the
/// commit instead. Kept in memory only, so how it is computed may change
/// with any release.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Progress(pub(crate) String);

impl Progress {
    /// As a step's envelope and outcome record carry it (OWL-399).
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The [`Progress`] of `record` with `proposals`: the SHA-256, in lowercase
/// hex, of each file a write renders but [`COST_FILE`] and [`SESSION_FILE`]
/// (OWL-394), which runs write and spend beside what they keep, and
/// [`HELD_FILE`] (OWL-403), which a run widens beside them, in a fixed order,
/// each as its name, then a 0 byte when absent, or a 1 byte, its length as
/// eight little-endian bytes and its text. Entries Owlshift never writes are
/// left out: no command changes them.
pub fn progress(record: &TicketRecord, proposals: Option<&TicketProposals>) -> Progress {
    let files = [
        (STATE_FILE, Some(record.state.render())),
        (QUESTIONS_FILE, Some(record.questions.render())),
        (DELIVERY_FILE, record.delivery.as_ref().map(|d| d.render())),
        (INTAKE_FILE, record.intake.as_ref().map(|i| i.render())),
        (PLAN_FILE, record.plan.as_ref().map(|p| p.render())),
        (REVIEW_FILE, record.review.as_ref().map(|r| r.render())),
        (PROPOSALS_FILE, proposals.map(|p| p.render())),
    ];
    let mut hash = Sha256::new();
    for (name, text) in files {
        hash.update(name.as_bytes());
        match text {
            None => hash.update([0]),
            Some(text) => {
                hash.update([1]);
                hash.update((text.len() as u64).to_le_bytes());
                hash.update(text.as_bytes());
            }
        }
    }
    Progress(
        hash.finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    )
}

/// The record of a ticket that keeps nothing but what its runs cost
/// (OWL-397): Ready at round 0, nothing asked, and `cost`, the record
/// [`forget`] leaves of a ticket at round 0, which `owlshift do` starts
/// from. The first run of a ticket without a ref makes its ref with it.
pub fn blank(cost: Option<TicketCost>) -> TicketRecord {
    // `TicketState::restore` refuses counters a state cannot have; Ready
    // with every counter at zero has none of them.
    kept(0, None, cost).expect("Ready at round 0 with no counter is a state of the machine")
}

/// What a write does with [`PROPOSALS_FILE`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProposalsFile<'a> {
    /// Keeps the file the previous commit holds, if any.
    Keep,
    /// Writes these proposals.
    Write(&'a TicketProposals),
    /// Leaves it out: [`forget`] of a file that no longer parses.
    Drop,
}

/// Reads the ticket ref of `ticket` in `checkout`: `None` when there is
/// none. A symbolic ref (Owlshift never writes one), a ref that does not
/// point to a commit, a tree without both files as regular blobs, a file
/// that does not parse, or asks that do not match the state (a ticket
/// waiting for answers to a round the asks do not end with) is an error.
/// [`DELIVERY_FILE`], [`INTAKE_FILE`], [`PLAN_FILE`], [`REVIEW_FILE`],
/// [`COST_FILE`], [`SESSION_FILE`], [`HELD_FILE`] and [`PROPOSALS_FILE`] are optional, checked like the two
/// others when present.
/// Other entries of the tree are left alone.
pub fn read(git: &Git, checkout: &Path, ticket: &TicketId) -> Result<Option<Stored>, String> {
    let name = ticket_ref(ticket);
    let Some(listed) = listed(git, checkout, &name)? else {
        return Ok(None);
    };
    match read_listed(git, checkout, &name, &listed) {
        Ok(stored) => Ok(Some(stored)),
        Err(ReadError::Invalid(why) | ReadError::Git(why)) => Err(why),
    }
}

/// A ref as `for-each-ref` lists it.
struct Listed {
    /// The object it points to, through a symbolic ref.
    oid: String,
    kind: String,
    /// The ref a symbolic ref points to.
    symref: Option<String>,
}

/// The ref named `name` exactly, `None` when there is none.
fn listed(git: &Git, checkout: &Path, name: &str) -> Result<Option<Listed>, String> {
    // `for-each-ref` answers an absent ref with nothing; `show-ref --verify
    // --hash` exits 128 on one (checked 2026-10-02, git 2.54.0). The exact
    // name is kept: a pattern also lists the refs below it. `%(symref)` is
    // empty for a ref that is not symbolic, so the line has four fields.
    let out = git
        .run(
            checkout,
            &[
                "for-each-ref",
                "--format=%(objectname) %(objecttype) %(symref) %(refname)",
                name,
            ],
        )
        .map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&out).lines().find_map(|line| {
        let mut fields = line.splitn(4, ' ');
        let (oid, kind, symref, refname) = (
            fields.next()?,
            fields.next()?,
            fields.next()?,
            fields.next()?,
        );
        (refname == name).then(|| Listed {
            oid: oid.to_owned(),
            kind: kind.to_owned(),
            symref: (!symref.is_empty()).then(|| symref.to_owned()),
        })
    }))
}

/// Why a ticket ref could not be read.
enum ReadError {
    /// What it holds is not a ticket record: the ref is broken.
    Invalid(String),
    /// Git could not show it: nothing is known of the record.
    Git(String),
}

/// Reads the record `listed` points to; [`read`] says what is refused.
fn read_listed(
    git: &Git,
    checkout: &Path,
    name: &str,
    listed: &Listed,
) -> Result<Stored, ReadError> {
    let broken = |why: String| ReadError::Invalid(format!("the ticket ref {name} {why}"));
    if let Some(target) = &listed.symref {
        return Err(broken(format!(
            "is a symbolic ref to {target}, which Owlshift never writes"
        )));
    }
    if listed.kind != "commit" {
        return Err(broken(format!("points to a {}, not a commit", listed.kind)));
    }
    let entries = tree_entries(git, checkout, &listed.oid).map_err(ReadError::Git)?;
    let has = |file: &str| entries.iter().any(|entry| entry.name == file);
    let optional: Vec<&str> = [
        DELIVERY_FILE,
        INTAKE_FILE,
        PLAN_FILE,
        REVIEW_FILE,
        COST_FILE,
        SESSION_FILE,
        HELD_FILE,
        PROPOSALS_FILE,
    ]
    .into_iter()
    .filter(|file| has(file))
    .collect();
    let wanted: Vec<&str> = [STATE_FILE, QUESTIONS_FILE]
        .into_iter()
        .chain(optional)
        .collect();
    // The files are shown in one call, then judged in this order: the first
    // that is refused or does not parse is the error, as with one call each.
    let mut texts = read_files(git, checkout, &entries, &wanted, false)
        .map_err(ReadError::Git)?
        .into_iter();
    let mut next = |file: &str| match texts.next() {
        Some(Ok(text)) => Ok(text),
        Some(Err(why)) => Err(broken(why)),
        None => unreachable!("{file} was asked for and no earlier file was refused"),
    };
    let text = next(STATE_FILE)?;
    let state = PersistedState::parse(&text)
        .map_err(|e| broken(format!("holds an invalid {STATE_FILE}: {e}")))?;
    let text = next(QUESTIONS_FILE)?;
    let questions = TicketQuestions::parse(&text)
        .map_err(|e| broken(format!("holds an invalid {QUESTIONS_FILE}: {e}")))?;
    let delivery = if has(DELIVERY_FILE) {
        let text = next(DELIVERY_FILE)?;
        Some(
            TicketDelivery::parse(&text)
                .map_err(|e| broken(format!("holds an invalid {DELIVERY_FILE}: {e}")))?,
        )
    } else {
        None
    };
    let intake = if has(INTAKE_FILE) {
        let text = next(INTAKE_FILE)?;
        Some(
            TicketIntake::parse(&text)
                .map_err(|e| broken(format!("holds an invalid {INTAKE_FILE}: {e}")))?,
        )
    } else {
        None
    };
    let plan = if has(PLAN_FILE) {
        let text = next(PLAN_FILE)?;
        Some(
            TicketPlan::parse(&text)
                .map_err(|e| broken(format!("holds an invalid {PLAN_FILE}: {e}")))?,
        )
    } else {
        None
    };
    let review = if has(REVIEW_FILE) {
        let text = next(REVIEW_FILE)?;
        Some(
            TicketReview::parse(&text)
                .map_err(|e| broken(format!("holds an invalid {REVIEW_FILE}: {e}")))?,
        )
    } else {
        None
    };
    let cost = if has(COST_FILE) {
        let text = next(COST_FILE)?;
        Some(
            TicketCost::parse(&text)
                .map_err(|e| broken(format!("holds an invalid {COST_FILE}: {e}")))?,
        )
    } else {
        None
    };
    let session = if has(SESSION_FILE) {
        let text = next(SESSION_FILE)?;
        Some(
            TicketSession::parse(&text)
                .map_err(|e| broken(format!("holds an invalid {SESSION_FILE}: {e}")))?,
        )
    } else {
        None
    };
    let held = if has(HELD_FILE) {
        let text = next(HELD_FILE)?;
        Some(
            TicketHeld::parse(&text)
                .map_err(|e| broken(format!("holds an invalid {HELD_FILE}: {e}")))?,
        )
    } else {
        None
    };
    let proposals = if has(PROPOSALS_FILE) {
        let text = next(PROPOSALS_FILE)?;
        Some(
            TicketProposals::parse(&text)
                .map_err(|e| broken(format!("holds an invalid {PROPOSALS_FILE}: {e}")))?,
        )
    } else {
        None
    };
    check_consistent(&state, &questions).map_err(broken)?;
    Ok(Stored {
        record: TicketRecord {
            state,
            questions,
            delivery,
            intake,
            plan,
            review,
            cost,
            session,
            held,
        },
        commit: listed.oid.clone(),
        proposals,
    })
}

/// The text of each of `files` among `entries`, in order: a regular blob of
/// at most [`MAX_FILE`] bytes, in UTF-8, or why the ref is broken for that
/// file. The blobs that pass those checks are read by one `git cat-file
/// --batch`; none when no file passes. Unless `past_invalid`, the files after
/// the first that is refused are not read: [`read_listed`] stops there, and a
/// git failure on a later blob must not come before that refusal.
fn read_files(
    git: &Git,
    checkout: &Path,
    entries: &[Entry],
    files: &[&str],
    past_invalid: bool,
) -> Result<Vec<Result<String, String>>, String> {
    let checked: Vec<Result<&str, String>> = files
        .iter()
        .map(|&file| {
            let entry = entries
                .iter()
                .find(|entry| entry.name == file)
                .ok_or_else(|| format!("has no {file}"))?;
            if entry.mode != "100644" || entry.kind != "blob" {
                return Err(format!(
                    "holds {file} as {} {}, not a regular file",
                    entry.mode, entry.kind
                ));
            }
            if entry.size.is_none_or(|size| size > MAX_FILE) {
                return Err(format!("holds {file} past {MAX_FILE} bytes"));
            }
            Ok(entry.oid.as_str())
        })
        .collect();
    let stop = if past_invalid {
        checked.len()
    } else {
        checked
            .iter()
            .position(Result::is_err)
            .unwrap_or(checked.len())
    };
    let oids: Vec<&str> = checked[..stop]
        .iter()
        .filter_map(|checked| checked.as_ref().ok().copied())
        .collect();
    let mut blobs = if oids.is_empty() {
        Vec::new().into_iter()
    } else {
        let input: String = oids.iter().map(|oid| format!("{oid}\n")).collect();
        let out = git
            .run_with_input(checkout, &["cat-file", "--batch"], input.as_bytes())
            .map_err(|e| e.to_string())?;
        parse_batch(&out, &oids)
            .map_err(|e| format!("git cat-file --batch: {e}"))?
            .into_iter()
    };
    Ok(checked
        .into_iter()
        .zip(files)
        .take(stop + 1)
        .map(|(checked, file)| {
            checked.and_then(|_| {
                let bytes = blobs.next().expect("one blob per checked file");
                String::from_utf8(bytes).map_err(|_| format!("holds {file} not in UTF-8"))
            })
        })
        .collect())
}

/// The contents `git cat-file --batch` printed for `oids`, one per id, in
/// order. Each answer is `<oid> <type> <size>\n`, the content and a `\n`, or
/// `<oid> missing\n` (checked 2026-10-15, git 2.43.0, which also answers
/// `<name> missing` for a name that is no object and exits 0): a missing
/// object, or any other type than a blob, is an error here, as a failing
/// `cat-file blob` is. A blob may hold newlines; its size says where it ends.
fn parse_batch(out: &[u8], oids: &[&str]) -> Result<Vec<Vec<u8>>, String> {
    let mut rest = out;
    let mut blobs = Vec::with_capacity(oids.len());
    for oid in oids {
        let end = rest
            .iter()
            .position(|&byte| byte == b'\n')
            .ok_or_else(|| format!("no answer for {oid}"))?;
        let header = String::from_utf8_lossy(&rest[..end]).into_owned();
        rest = &rest[end + 1..];
        let mut fields = header.split(' ');
        let (Some(answered), Some(kind), size, None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return Err(format!("unexpected answer {header:?} for {oid}"));
        };
        if answered != *oid {
            return Err(format!("answered {answered} where {oid} was asked"));
        }
        if kind == "missing" && size.is_none() {
            return Err(format!("{oid} is missing"));
        }
        let size = size.and_then(|size| size.parse::<usize>().ok());
        let (true, Some(size)) = (kind == "blob", size) else {
            return Err(format!("unexpected answer {header:?} for {oid}"));
        };
        if rest.len() <= size || rest[size] != b'\n' {
            return Err(format!("the content of {oid} is cut short"));
        }
        blobs.push(rest[..size].to_vec());
        rest = &rest[size + 1..];
    }
    Ok(blobs)
}

/// The tickets that have a ticket ref in `checkout`, in the order git lists
/// the refs (OWL-152). A ref below the namespace whose name is not a ticket
/// id is left out: no ticket's ref is named so.
pub fn list(git: &Git, checkout: &Path) -> Result<Vec<TicketId>, String> {
    let listed = git
        .run(
            checkout,
            &["for-each-ref", "--format=%(refname)", TICKETS_PREFIX],
        )
        .map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&listed)
        .lines()
        .filter_map(|name| name.strip_prefix(TICKETS_PREFIX))
        .filter_map(|id| TicketId::new(id).ok())
        .collect())
}

/// A ticket ref as the drain's slot weighs it (OWL-291).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefState {
    /// Its record reads: its state, `None` when the core accepts none from
    /// it.
    Read(Option<TicketState>),
    /// It cannot be read, for this reason: [`forget`] starts it over.
    Unread(String),
}

/// Every ticket ref of `checkout` with its state, in [`list`]'s order. A ref
/// that cannot be read is kept, with why; one gone since the listing is
/// left out. The error says why the refs could not be listed.
pub fn states(git: &Git, checkout: &Path) -> Result<Vec<(TicketId, RefState)>, String> {
    let tickets = list(git, checkout)?;
    Ok(tickets
        .into_iter()
        .filter_map(|ticket| {
            let state = match read(git, checkout, &ticket) {
                Ok(Some(stored)) => {
                    RefState::Read(TicketState::try_from(&stored.record.state).ok())
                }
                Ok(None) => return None,
                Err(why) => RefState::Unread(why),
            };
            Some((ticket, state))
        })
        .collect())
}

/// A ticket ref as read: its ticket, and its record or why it cannot be
/// read.
pub type RefRead = (TicketId, Result<Stored, String>);

/// Every ticket ref of `checkout` but `own`'s, read now, in [`list`]'s
/// order: why a ref cannot be read in its place, one gone since the listing
/// left out. A command that holds the project's lock reads them so, for the
/// slot, the open pull request cap (OWL-313) and the collisions (OWL-403).
/// The error says why the refs could not be listed.
pub fn others(git: &Git, checkout: &Path, own: &TicketId) -> Result<Vec<RefRead>, String> {
    let tickets = list(git, checkout)?;
    Ok(tickets
        .into_iter()
        .filter(|ticket| ticket != own)
        .filter_map(|ticket| match read(git, checkout, &ticket) {
            Ok(Some(stored)) => Some((ticket, Ok(stored))),
            Ok(None) => None,
            Err(why) => Some((ticket, Err(why))),
        })
        .collect())
}

/// Writes `record` as the ticket ref of `ticket` in `checkout` and returns
/// the new commit. `previous` is the commit the ref was read at, `None` when
/// it did not exist: the ref moves only from there, and the new commit has
/// it as its parent and keeps every other entry of its tree but
/// [`DELIVERY_FILE`], [`INTAKE_FILE`], [`PLAN_FILE`], [`REVIEW_FILE`],
/// [`COST_FILE`], [`SESSION_FILE`] and [`HELD_FILE`], each written when the
/// record has it and left out when it has none.
/// [`PROPOSALS_FILE`] is kept as it is:
/// [`write_proposals`] writes it.
///
/// A file that would hold more than [`MAX_FILE`] bytes is refused before
/// any git command runs (OWL-200): [`read`] would refuse it, leaving the
/// ticket where no command reads it. Nothing is written, the ref keeps its
/// previous state, and the error names `owlshift forget` (OWL-202).
pub fn write(
    git: &Git,
    checkout: &Path,
    ticket: &TicketId,
    record: &TicketRecord,
    previous: Option<&str>,
) -> Result<String, String> {
    write_proposals(git, checkout, ticket, record, ProposalsFile::Keep, previous)
}

/// [`write()`], doing `proposals` with [`PROPOSALS_FILE`] (OWL-253): the
/// proposals are kept in the very write that keeps the delivery or the
/// repair that owes them, and as filed in the write after their filing.
pub fn write_proposals(
    git: &Git,
    checkout: &Path,
    ticket: &TicketId,
    record: &TicketRecord,
    proposals: ProposalsFile<'_>,
    previous: Option<&str>,
) -> Result<String, String> {
    let name = ticket_ref(ticket);
    let mut files = vec![
        (STATE_FILE, record.state.render()),
        (QUESTIONS_FILE, record.questions.render()),
    ];
    if let Some(delivery) = &record.delivery {
        files.push((DELIVERY_FILE, delivery.render()));
    }
    if let Some(intake) = &record.intake {
        files.push((INTAKE_FILE, intake.render()));
    }
    if let Some(plan) = &record.plan {
        files.push((PLAN_FILE, plan.render()));
    }
    if let Some(review) = &record.review {
        files.push((REVIEW_FILE, review.render()));
    }
    if let Some(cost) = &record.cost {
        files.push((COST_FILE, cost.render()));
    }
    if let Some(session) = &record.session {
        files.push((SESSION_FILE, session.render()));
    }
    if let Some(held) = &record.held {
        files.push((HELD_FILE, held.render()));
    }
    if let ProposalsFile::Write(proposals) = proposals {
        files.push((PROPOSALS_FILE, proposals.render()));
    }
    for (file, content) in &files {
        // The blob holds these bytes exactly (`--no-filters`), so their
        // count is the size `read` checks.
        let len = content.len();
        if u64::try_from(len).map_or(true, |len| len > MAX_FILE) {
            return Err(format!(
                "the ticket ref {name} would hold {file} of {len} bytes, past the {MAX_FILE} \
                 a ticket ref file may hold: nothing is written, the ref keeps its previous \
                 state, and `owlshift forget {ticket}` makes room, starting the ticket's record \
                 over with its round count and the refusals its next runs are told"
            ));
        }
    }
    let mut blobs = Vec::new();
    for (file, content) in &files {
        blobs.push((*file, blob(git, checkout, content.as_bytes())?));
    }
    let mut tree = Vec::new();
    if let Some(previous) = previous {
        for entry in tree_entries(git, checkout, previous)? {
            let replaced = entry.name == PROPOSALS_FILE && proposals != ProposalsFile::Keep;
            if !replaced
                && ![
                    STATE_FILE,
                    QUESTIONS_FILE,
                    DELIVERY_FILE,
                    INTAKE_FILE,
                    PLAN_FILE,
                    REVIEW_FILE,
                    COST_FILE,
                    SESSION_FILE,
                    HELD_FILE,
                ]
                .contains(&entry.name.as_str())
            {
                tree.extend_from_slice(
                    format!(
                        "{} {} {}\t{}\0",
                        entry.mode, entry.kind, entry.oid, entry.name
                    )
                    .as_bytes(),
                );
            }
        }
    }
    for (file, oid) in &blobs {
        tree.extend_from_slice(format!("100644 blob {oid}\t{file}\0").as_bytes());
    }
    let tree = with_input(git, checkout, &["mktree", "-z"], &tree)?;
    let message = format!(
        "{ticket}: {:?}, round {}",
        record.state.stage, record.state.round
    );
    let mut args = vec![
        "-c",
        "user.name=Owlshift",
        "-c",
        "user.email=owlshift@localhost",
        "commit-tree",
        "--no-gpg-sign",
        &tree,
        "-m",
        &message,
    ];
    if let Some(previous) = previous {
        args.extend(["-p", previous]);
    }
    let commit = text(git.run(checkout, &args).map_err(|e| e.to_string())?);
    git.run(
        checkout,
        &[
            "update-ref",
            "-m",
            "owlshift: the ticket's state",
            &name,
            &commit,
            previous.unwrap_or(""),
        ],
    )
    .map_err(|e| format!("moving {name}: {e}"))?;
    Ok(commit)
}

/// What [`forget`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Forgot {
    /// `kept` was written on top of `previous`, which stays its parent.
    Rewritten {
        previous: String,
        commit: String,
        from: Previous,
        kept: TicketRecord,
    },
    /// The ref already held only what forget keeps: nothing was written.
    Unchanged { commit: String, kept: TicketRecord },
    /// The ref was symbolic or did not point to a commit, `why`: it was
    /// deleted from `previous`, the object it pointed to, and nothing kept.
    Deleted { previous: String, why: String },
}

/// The record a [`Forgot::Rewritten`] replaced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Previous {
    /// Read whole.
    Read(Box<TicketRecord>),
    /// Broken, for this reason: what was kept is what still read on its own.
    Unreadable(String),
}

/// Starts over what the ticket ref of `ticket` keeps (OWL-202), for a ref
/// that has no room for the next write or cannot be read; `None` when there
/// is no ref. The caller holds the project's lock.
///
/// A ref that points to a commit gets a new record on top of it, the old
/// commit its parent: Ready, its round count kept and every other counter at
/// zero, and of `questions.json` only Build's and the resolver's kept
/// refusals, which the next runs are told. `delivery.json` goes: nothing is
/// followed any more, and `owlshift do` delivers again (OWL-209). So does
/// `intake.json`: the drain runs intake again (OWL-245). So do `plan.json`
/// and `review.json`: the next dispatch designs anew (OWL-303). So does
/// `held.json` (OWL-403): a ticket at Ready holds nothing, and its next
/// dispatch writes what it holds afresh. `cost.json`
/// stays when it reads (OWL-378): the runs were paid for, and a ticket past
/// its budget still parks at its next run, until a person's `owlshift
/// continue` counts the budget afresh. The asks, their verdicts, what
/// the last answer check read and the decisions go; their comments stay on
/// the ticket. A record that cannot be read gives what still reads on its
/// own: the round of a `state.json` that parses, the refusals of a
/// `questions.json` that does, round 0 and none otherwise. A symbolic ref,
/// or one that points to anything but a commit, is deleted, its target
/// untouched. A git command that fails changes nothing.
pub fn forget(git: &Git, checkout: &Path, ticket: &TicketId) -> Result<Option<Forgot>, String> {
    let name = ticket_ref(ticket);
    let Some(listed) = listed(git, checkout, &name)? else {
        return Ok(None);
    };
    let (from, kept) = match read_listed(git, checkout, &name, &listed) {
        Ok(stored) => {
            let kept = kept(
                stored.record.state.round,
                Some(&stored.record.questions),
                stored.record.cost.clone(),
            )?;
            if kept == stored.record {
                return Ok(Some(Forgot::Unchanged {
                    commit: stored.commit,
                    kept,
                }));
            }
            (
                Previous::Read(Box::new(stored.record)),
                (kept, ProposalsFile::Keep),
            )
        }
        Err(ReadError::Git(error)) => return Err(error),
        Err(ReadError::Invalid(why)) if listed.symref.is_some() || listed.kind != "commit" => {
            // `--no-deref`: a symbolic ref goes, not the ref it points to.
            git.run(
                checkout,
                &[
                    "update-ref",
                    "-m",
                    "owlshift: forget",
                    "--no-deref",
                    "-d",
                    &name,
                    &listed.oid,
                ],
            )
            .map_err(|e| format!("deleting {name}: {e}"))?;
            return Ok(Some(Forgot::Deleted {
                previous: listed.oid,
                why,
            }));
        }
        Err(ReadError::Invalid(why)) => (
            Previous::Unreadable(why),
            salvage(git, checkout, &listed.oid)?,
        ),
    };
    let (kept, proposals) = kept;
    // Far below the cap: the refusals are bounded (64 choices of two
    // 512-byte texts, reasons of 2 KB), and `write` keeps any other entry of
    // the old tree as it is, `proposals.json` included unless it no longer
    // parses, so nothing is filed twice (OWL-245, OWL-253).
    let commit = write_proposals(git, checkout, ticket, &kept, proposals, Some(&listed.oid))?;
    Ok(Some(Forgot::Rewritten {
        previous: listed.oid,
        commit,
        from,
        kept,
    }))
}

/// The record [`forget`] leaves: Ready, `round` kept, every other counter
/// at zero, of `questions` only the refusals the next runs are told, and
/// `cost` as it was; no session, the ticket starting over (OWL-394). Ready
/// with any round is a state the machine can be in, and the one `owlshift
/// do` starts from.
fn kept(
    round: u32,
    questions: Option<&TicketQuestions>,
    cost: Option<TicketCost>,
) -> Result<TicketRecord, String> {
    let state = TicketState::restore(Status::Active(Stage::Ready), round, 0, 0, 0, 0)
        .map_err(|e| e.to_string())?;
    Ok(TicketRecord {
        state: PersistedState::from(&state),
        questions: TicketQuestions {
            build_refusal: questions.and_then(|q| q.build_refusal.clone()),
            resolver_refusal: questions.and_then(|q| q.resolver_refusal.clone()),
            ..TicketQuestions::new()
        },
        delivery: None,
        intake: None,
        plan: None,
        review: None,
        cost,
        session: None,
        held: None,
    })
}

/// What [`forget`] keeps of the unreadable record at `commit`: the round
/// of its `state.json`, the refusals of its `questions.json` and its
/// `cost.json`, each when it still reads and parses on its own, and its
/// `proposals.json` unless it no longer does.
fn salvage(
    git: &Git,
    checkout: &Path,
    commit: &str,
) -> Result<(TicketRecord, ProposalsFile<'static>), String> {
    let entries = tree_entries(git, checkout, commit)?;
    let has = |file: &str| entries.iter().any(|entry| entry.name == file);
    let wanted: Vec<&str> = [STATE_FILE, QUESTIONS_FILE, COST_FILE, PROPOSALS_FILE]
        .into_iter()
        .filter(|file| has(file) || ![COST_FILE, PROPOSALS_FILE].contains(file))
        .collect();
    // Each file stands alone: one that is refused only loses its own part.
    let mut texts = read_files(git, checkout, &entries, &wanted, true)?;
    let proposals_text = if has(PROPOSALS_FILE) {
        texts.pop()
    } else {
        None
    };
    let cost_text = if has(COST_FILE) {
        texts.pop().and_then(Result::ok)
    } else {
        None
    };
    let questions_text = texts.pop().and_then(Result::ok);
    let state_text = texts.pop().and_then(Result::ok);
    let round = state_text
        .and_then(|text| PersistedState::parse(&text).ok())
        .map_or(0, |state| state.round);
    let questions = questions_text.and_then(|text| TicketQuestions::parse(&text).ok());
    let proposals = if proposals_text
        .is_none_or(|text| text.is_ok_and(|text| TicketProposals::parse(&text).is_ok()))
    {
        ProposalsFile::Keep
    } else {
        ProposalsFile::Drop
    };
    let cost = cost_text.and_then(|text| TicketCost::parse(&text).ok());
    Ok((kept(round, questions.as_ref(), cost)?, proposals))
}

/// A ticket waiting for its decider's answers waits for the latest ask's
/// round: the answer check judges that ask.
fn check_consistent(state: &PersistedState, questions: &TicketQuestions) -> Result<(), String> {
    let core = TicketState::try_from(state).map_err(|e| e.to_string())?;
    let waits = matches!(
        core.status(),
        Status::NeedsInput { .. }
            | Status::Parked {
                awaiting_input: true,
                ..
            }
    );
    let latest = questions.latest().map(|ask| ask.round.get());
    if waits && latest != Some(core.round()) {
        return Err(format!(
            "waits for the answers of round {}, but its latest ask is {}",
            core.round(),
            latest.map_or_else(|| "none".to_owned(), |round| format!("of round {round}"))
        ));
    }
    Ok(())
}

/// One entry of a tree, as `ls-tree -l` lists it.
struct Entry {
    mode: String,
    kind: String,
    oid: String,
    /// `None` for an entry that is not a blob.
    size: Option<u64>,
    name: String,
}

fn tree_entries(git: &Git, checkout: &Path, commit: &str) -> Result<Vec<Entry>, String> {
    let listed = git
        .run(checkout, &["ls-tree", "-z", "-l", commit])
        .map_err(|e| e.to_string())?;
    listed
        .split(|&byte| byte == 0)
        .filter(|line| !line.is_empty())
        .map(|line| {
            let line = String::from_utf8_lossy(line);
            // `<mode> <type> <object> <size>\t<name>`, the size padded.
            let (meta, name) = line
                .split_once('\t')
                .ok_or_else(|| format!("unexpected tree entry {line:?}"))?;
            let mut fields = meta.split_whitespace();
            let (Some(mode), Some(kind), Some(oid), Some(size)) =
                (fields.next(), fields.next(), fields.next(), fields.next())
            else {
                return Err(format!("unexpected tree entry {line:?}"));
            };
            Ok(Entry {
                mode: mode.to_owned(),
                kind: kind.to_owned(),
                oid: oid.to_owned(),
                size: size.parse().ok(),
                name: name.to_owned(),
            })
        })
        .collect()
}

/// Writes `content` as a blob, with no filter, and returns its id.
fn blob(git: &Git, checkout: &Path, content: &[u8]) -> Result<String, String> {
    with_input(
        git,
        checkout,
        &["hash-object", "-w", "--no-filters", "--stdin"],
        content,
    )
}

/// Runs git with `input` on its standard input; returns its output, trimmed.
fn with_input(git: &Git, checkout: &Path, args: &[&str], input: &[u8]) -> Result<String, String> {
    let output = git
        .output(checkout, args, Some(input))
        .map_err(|e| e.to_string())?;
    if output.success() {
        Ok(text(output.stdout))
    } else {
        Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn text(bytes: Vec<u8>) -> String {
    String::from_utf8_lossy(&bytes).trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::parse_batch;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn several_blobs_come_out_whole_whatever_they_hold() {
        // As printed by git 2.43.0 (2026-10-15): a blob with newlines, an
        // empty one, and the same id asked twice.
        let out = format!("{A} blob 5\na\nb\n\n\n{B} blob 0\n\n{A} blob 5\na\nb\n\n\n");
        let blobs = parse_batch(out.as_bytes(), &[A, B, A]).unwrap();
        assert_eq!(
            blobs,
            [b"a\nb\n\n".to_vec(), Vec::new(), b"a\nb\n\n".to_vec()]
        );
    }

    #[test]
    fn a_missing_object_is_an_error() {
        let out = format!("{A} blob 1\nx\n{B} missing\n");
        assert_eq!(
            parse_batch(out.as_bytes(), &[A, B]),
            Err(format!("{B} is missing"))
        );
    }

    #[test]
    fn an_answer_that_is_not_a_whole_blob_is_an_error() {
        for out in [
            String::new(),
            format!("{A} tree 3\nabc\n"),
            format!("{B} blob 1\nx\n"),
            format!("{A} blob 4\nx\n"),
            format!("{A} blob 1\nxy\n"),
            format!("{A} blob\n"),
        ] {
            assert!(parse_batch(out.as_bytes(), &[A]).is_err(), "{out:?}");
        }
    }
}
