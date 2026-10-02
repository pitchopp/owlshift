//! The ticket ref: what the runner keeps about a ticket between two
//! commands (P2, OWL-122). `refs/owlshift/tickets/<ticket>` points to a
//! commit whose tree holds the ticket's core state ([`STATE_FILE`]) and the
//! questions the runner asked on it ([`QUESTIONS_FILE`]), so `owlshift
//! resume` knows the round, its re-asks and failed runs, and which comments
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

use std::path::Path;

use owlshift_contracts::ids::TicketId;
use owlshift_contracts::refs::{
    PersistedState, QUESTIONS_FILE, STATE_FILE, TicketQuestions, ticket_ref,
};
use owlshift_core::state::{Status, TicketState};

use crate::executor::Git;

/// The most a file of the ticket ref may hold; the runner's own files are
/// far smaller.
const MAX_FILE: u64 = 1024 * 1024;

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
/// none. A ref that does not point to a commit, a tree without both files
/// as regular blobs, a file that does not parse, or asks that do not match
/// the state (a ticket waiting for answers to a round the asks do not end
/// with) is an error. Other entries of the tree are left alone.
pub fn read(git: &Git, checkout: &Path, ticket: &TicketId) -> Result<Option<Stored>, String> {
    let name = ticket_ref(ticket);
    // `for-each-ref` answers an absent ref with nothing; `show-ref --verify
    // --hash` exits 128 on one (checked 2026-10-02, git 2.54.0). The exact
    // name is kept: a pattern also lists the refs below it.
    let listed = git
        .run(
            checkout,
            &[
                "for-each-ref",
                "--format=%(objectname) %(objecttype) %(refname)",
                &name,
            ],
        )
        .map_err(|e| e.to_string())?;
    let listed = String::from_utf8_lossy(&listed);
    let Some((commit, kind)) = listed.lines().find_map(|line| {
        let mut fields = line.splitn(3, ' ');
        let (oid, kind, refname) = (fields.next()?, fields.next()?, fields.next()?);
        (refname == name).then(|| (oid.to_owned(), kind.to_owned()))
    }) else {
        return Ok(None);
    };
    let broken = |why: String| format!("the ticket ref {name} {why}");
    if kind != "commit" {
        return Err(broken(format!("points to a {kind}, not a commit")));
    }
    let entries = tree_entries(git, checkout, &commit)?;
    let file = |file: &str| -> Result<String, String> {
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
            .map_err(|e| e.to_string())?;
        String::from_utf8(bytes).map_err(|_| broken(format!("holds {file} not in UTF-8")))
    };
    let state = PersistedState::parse(&file(STATE_FILE)?)
        .map_err(|e| broken(format!("holds an invalid {STATE_FILE}: {e}")))?;
    let questions = TicketQuestions::parse(&file(QUESTIONS_FILE)?)
        .map_err(|e| broken(format!("holds an invalid {QUESTIONS_FILE}: {e}")))?;
    check_consistent(&state, &questions).map_err(broken)?;
    Ok(Some(Stored {
        record: TicketRecord { state, questions },
        commit,
    }))
}

/// Writes `record` as the ticket ref of `ticket` in `checkout` and returns
/// the new commit. `previous` is the commit the ref was read at, `None` when
/// it did not exist: the ref moves only from there, and the new commit has
/// it as its parent and keeps every other entry of its tree.
pub fn write(
    git: &Git,
    checkout: &Path,
    ticket: &TicketId,
    record: &TicketRecord,
    previous: Option<&str>,
) -> Result<String, String> {
    let name = ticket_ref(ticket);
    let state = blob(git, checkout, record.state.render().as_bytes())?;
    let questions = blob(git, checkout, record.questions.render().as_bytes())?;
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
