//! The ticket ref: what the runner keeps about a ticket between two
//! commands (P2, OWL-122). `refs/owlshift/tickets/<ticket>` points to a
//! commit whose tree holds the ticket's core state ([`STATE_FILE`]) and the
//! questions the runner asked on it ([`QUESTIONS_FILE`]), so `owlshift
//! continue` knows the round, its re-asks and failed runs, and which comments
//! are the runner's asks.
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

use owlshift_contracts::ids::TicketId;
use owlshift_contracts::refs::{
    PersistedState, QUESTIONS_FILE, STATE_FILE, TICKETS_PREFIX, TicketQuestions, ticket_ref,
};
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
}

/// A ticket record and the commit it was read from, or last written as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stored {
    pub record: TicketRecord,
    pub commit: String,
}

/// Reads the ticket ref of `ticket` in `checkout`: `None` when there is
/// none. A symbolic ref (Owlshift never writes one), a ref that does not
/// point to a commit, a tree without both files as regular blobs, a file
/// that does not parse, or asks that do not match the state (a ticket
/// waiting for answers to a round the asks do not end with) is an error.
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
    let state = PersistedState::parse(&read_file(git, checkout, name, &entries, STATE_FILE)?)
        .map_err(|e| broken(format!("holds an invalid {STATE_FILE}: {e}")))?;
    let questions =
        TicketQuestions::parse(&read_file(git, checkout, name, &entries, QUESTIONS_FILE)?)
            .map_err(|e| broken(format!("holds an invalid {QUESTIONS_FILE}: {e}")))?;
    check_consistent(&state, &questions).map_err(broken)?;
    Ok(Stored {
        record: TicketRecord { state, questions },
        commit: listed.oid.clone(),
    })
}

/// The text of `file` among `entries`: a regular blob of at most
/// [`MAX_FILE`] bytes, in UTF-8.
fn read_file(
    git: &Git,
    checkout: &Path,
    name: &str,
    entries: &[Entry],
    file: &str,
) -> Result<String, ReadError> {
    let broken = |why: String| ReadError::Invalid(format!("the ticket ref {name} {why}"));
    let entry = entries
        .iter()
        .find(|entry| entry.name == file)
        .ok_or_else(|| broken(format!("has no {file}")))?;
    if entry.mode != "100644" || entry.kind != "blob" {
        return Err(broken(format!(
            "holds {file} as {} {}, not a regular file",
            entry.mode, entry.kind
        )));
    }
    if entry.size.is_none_or(|size| size > MAX_FILE) {
        return Err(broken(format!("holds {file} past {MAX_FILE} bytes")));
    }
    let bytes = git
        .run(checkout, &["cat-file", "blob", &entry.oid])
        .map_err(|e| ReadError::Git(e.to_string()))?;
    String::from_utf8(bytes).map_err(|_| broken(format!("holds {file} not in UTF-8")))
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

/// Writes `record` as the ticket ref of `ticket` in `checkout` and returns
/// the new commit. `previous` is the commit the ref was read at, `None` when
/// it did not exist: the ref moves only from there, and the new commit has
/// it as its parent and keeps every other entry of its tree.
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
    let name = ticket_ref(ticket);
    let state = record.state.render();
    let questions = record.questions.render();
    for (file, content) in [(STATE_FILE, &state), (QUESTIONS_FILE, &questions)] {
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
    let state = blob(git, checkout, state.as_bytes())?;
    let questions = blob(git, checkout, questions.as_bytes())?;
    let mut tree = Vec::new();
    if let Some(previous) = previous {
        for entry in tree_entries(git, checkout, previous)? {
            if entry.name != STATE_FILE && entry.name != QUESTIONS_FILE {
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
    for (file, oid) in [(STATE_FILE, &state), (QUESTIONS_FILE, &questions)] {
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
/// refusals, which the next runs are told. The asks, their verdicts, what
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
            let kept = kept(stored.record.state.round, Some(&stored.record.questions))?;
            if kept == stored.record {
                return Ok(Some(Forgot::Unchanged {
                    commit: stored.commit,
                    kept,
                }));
            }
            (Previous::Read(Box::new(stored.record)), kept)
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
        Err(ReadError::Invalid(why)) => {
            let kept = salvage(git, checkout, &name, &listed.oid)?;
            (Previous::Unreadable(why), kept)
        }
    };
    // Far below the cap: the refusals are bounded (64 choices of two
    // 512-byte texts, reasons of 2 KB), and `write` keeps any other entry of
    // the old tree as it is.
    let commit = write(git, checkout, ticket, &kept, Some(&listed.oid))?;
    Ok(Some(Forgot::Rewritten {
        previous: listed.oid,
        commit,
        from,
        kept,
    }))
}

/// The record [`forget`] leaves: Ready, `round` kept, every other counter
/// at zero, and of `questions` only the refusals the next runs are told.
/// Ready with any round is a state the machine can be in, and the one
/// `owlshift do` starts from.
fn kept(round: u32, questions: Option<&TicketQuestions>) -> Result<TicketRecord, String> {
    let state = TicketState::restore(Status::Active(Stage::Ready), round, 0, 0)
        .map_err(|e| e.to_string())?;
    Ok(TicketRecord {
        state: PersistedState::from(&state),
        questions: TicketQuestions {
            build_refusal: questions.and_then(|q| q.build_refusal.clone()),
            resolver_refusal: questions.and_then(|q| q.resolver_refusal.clone()),
            ..TicketQuestions::new()
        },
    })
}

/// What [`forget`] keeps of the unreadable record at `commit`: the round
/// of its `state.json` and the refusals of its `questions.json`, each when
/// it still reads and parses on its own.
fn salvage(git: &Git, checkout: &Path, name: &str, commit: &str) -> Result<TicketRecord, String> {
    let entries = tree_entries(git, checkout, commit)?;
    let text = |file: &str| match read_file(git, checkout, name, &entries, file) {
        Ok(text) => Ok(Some(text)),
        Err(ReadError::Invalid(_)) => Ok(None),
        Err(ReadError::Git(error)) => Err(error),
    };
    let round = text(STATE_FILE)?
        .and_then(|text| PersistedState::parse(&text).ok())
        .map_or(0, |state| state.round);
    let questions = text(QUESTIONS_FILE)?.and_then(|text| TicketQuestions::parse(&text).ok());
    kept(round, questions.as_ref())
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
