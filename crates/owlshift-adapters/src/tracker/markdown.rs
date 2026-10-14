//! The Markdown tracker: tickets kept as files in the repository, the test
//! tracker of P0 and the public Markdown tracker of P9.
//!
//! One folder per ticket, `tickets/<ID>/`, holds `ticket.md` (a TOML front
//! matter between `+++` lines, then the description) and `comments/`, one
//! file per comment named `<YYYYMMDDTHHMMSSZ>[.<n>]-<author>.md`, `.<n>`
//! marking the n-th comment of a second. The format is settled in
//! `docs/design/build-plan.md`, "The test tracker".
//!
//! A reply opens with a TOML front matter naming the comment it answers,
//! `+++\nreply_to = "<id>"\n+++`, the id being that comment's file name
//! without `.md` (OWL-146); any other comment file is its body alone.
//!
//! A comment's time is its second plus a microsecond for each comment of
//! that second before it by n (OWL-136): the times then order the comments
//! as the tracker lists them, as Linear's do, which the runner's comparisons
//! need. It is not the instant the comment was written, only its place.

use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};

use jiff::civil::DateTime;
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};
use serde::Deserialize;

use owlshift_contracts::Priority;
use owlshift_contracts::config::Admit;
use owlshift_contracts::ids::TicketId;

use crate::tracker::{self as shared, Capability, ErrorKind};

/// A ticket as its `ticket.md` gives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ticket {
    pub id: TicketId,
    pub title: String,
    pub author: String,
    /// The visible stage: a tracker state name, as mapped by the project's
    /// `[tracker].states`.
    pub stage: String,
    pub priority: Priority,
    /// The decider, when the ticket has one.
    pub assignee: Option<String>,
    pub labels: Vec<String>,
    pub blocked_by: Vec<TicketId>,
    pub description: String,
}

/// A comment on a ticket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Comment {
    /// The file name without `.md`: unique on its ticket.
    pub id: String,
    /// Its second, plus a microsecond per comment of that second before it
    /// (see the module).
    pub at: Timestamp,
    pub author: String,
    pub body: String,
    /// The comment it replies to, from its front matter.
    pub reply_to: Option<String>,
}

/// A reply's front matter.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplyFront {
    reply_to: String,
}

/// A tracker file that could not be read or written, named by its path.
#[derive(Debug)]
pub struct TrackerError {
    pub path: PathBuf,
    pub reason: String,
}

impl fmt::Display for TrackerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.reason)
    }
}

impl std::error::Error for TrackerError {}

/// The tickets under a repository's `tickets/` folder.
#[derive(Clone, Debug)]
pub struct MarkdownTracker {
    tickets: PathBuf,
}

impl MarkdownTracker {
    /// The tracker of the repository at `root`.
    pub fn new(root: &Path) -> Self {
        Self {
            tickets: root.join("tickets"),
        }
    }

    /// Reads a ticket.
    pub fn ticket(&self, id: &TicketId) -> Result<Ticket, TrackerError> {
        let path = self.ticket_file(id);
        let input = read(&path)?;
        let (front, description) = parse(&input).map_err(|reason| error(&path, reason))?;
        Ok(Ticket {
            id: id.clone(),
            title: front.title,
            author: front.author,
            stage: front.stage,
            priority: front.priority,
            assignee: front.assignee,
            labels: front.labels,
            blocked_by: front.blocked_by,
            description: description.to_owned(),
        })
    }

    /// A ticket's comments, oldest first. A file in `comments/` whose name is
    /// not a comment's is an error.
    pub fn comments(&self, id: &TicketId) -> Result<Vec<Comment>, TrackerError> {
        let dir = self.comments_dir(id);
        comment_files(&dir)?
            .into_iter()
            .map(|file| {
                let path = dir.join(&file.name);
                let (reply_to, body) = reply_front(&read(&path)?).map_err(|e| error(&path, e))?;
                Ok(Comment {
                    id: file.name.strip_suffix(".md").unwrap_or_default().to_owned(),
                    at: time(file.second, file.nth).map_err(|e| error(&dir, e))?,
                    author: file.author,
                    body,
                    reply_to,
                })
            })
            .collect()
    }

    /// Adds a comment after the others of its second, whoever wrote them,
    /// and returns it as [`MarkdownTracker::comments`] reads it. The file
    /// keeps `at` to the second; a second holds a million comments at most.
    /// A file is never overwritten, so a name already taken (by a
    /// concurrent poster) is an error.
    pub fn post_comment(
        &self,
        id: &TicketId,
        author: &str,
        at: Timestamp,
        body: &str,
    ) -> Result<Comment, TrackerError> {
        self.post(id, author, at, None, body)
    }

    /// Adds a reply to the comment `reply_to`, as
    /// [`MarkdownTracker::post_comment`] adds a comment (OWL-146). Only a
    /// person replies: the runner never does.
    pub fn post_reply(
        &self,
        id: &TicketId,
        author: &str,
        at: Timestamp,
        reply_to: &str,
        body: &str,
    ) -> Result<Comment, TrackerError> {
        self.post(id, author, at, Some(reply_to), body)
    }

    fn post(
        &self,
        id: &TicketId,
        author: &str,
        at: Timestamp,
        reply_to: Option<&str>,
        body: &str,
    ) -> Result<Comment, TrackerError> {
        let ticket = self.ticket_file(id);
        if !ticket.is_file() {
            return Err(error(&ticket, "no such ticket"));
        }
        let dir = self.comments_dir(id);
        if !valid_author(author) {
            return Err(error(
                &dir,
                format!("invalid comment author {author:?}: expected {AUTHOR_PATTERN}"),
            ));
        }
        let second = Timestamp::from_second(at.as_second()).map_err(|e| error(&dir, e))?;
        let nth = match comment_files(&dir)?
            .iter()
            .filter(|file| file.second == second)
            .map(|file| file.nth)
            .max()
        {
            None => 1,
            Some(last) if last < MAX_PER_SECOND => last + 1,
            Some(_) => return Err(error(&dir, "too many comments in one second")),
        };
        let at = time(second, nth).map_err(|e| error(&dir, e))?;
        fs::create_dir_all(&dir).map_err(|e| error(&dir, e))?;
        let id = stem(second, nth, author);
        let path = dir.join(format!("{id}.md"));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| error(&path, e))?;
        let text = match reply_to {
            Some(parent) => format!(
                "+++\nreply_to = {}\n+++\n\n{body}",
                toml::Value::String(parent.to_owned())
            ),
            None => body.to_owned(),
        };
        file.write_all(text.as_bytes())
            .map_err(|e| error(&path, e))?;
        Ok(Comment {
            id,
            at,
            author: author.to_owned(),
            body: body.to_owned(),
            reply_to: reply_to.map(str::to_owned),
        })
    }

    /// Sets the visible stage by rewriting the `stage` line of the front
    /// matter, and only that line: the front matter must set it on exactly
    /// one line written `stage = …`, and every other value must read the same
    /// afterwards.
    pub fn set_stage(&self, id: &TicketId, stage: &str) -> Result<(), TrackerError> {
        let path = self.ticket_file(id);
        let input = read(&path)?;
        let (before, _) = parse(&input).map_err(|reason| error(&path, reason))?;
        let (front, _) = split(&input).map_err(|reason| error(&path, reason))?;

        let mut offset = front.start;
        let mut stage_lines = Vec::new();
        for line in input[front].split_inclusive('\n') {
            let rest = line.trim_start();
            if rest
                .strip_prefix("stage")
                .is_some_and(|r| r.trim_start().starts_with('='))
            {
                stage_lines.push((offset, line));
            }
            offset += line.len();
        }
        let [(start, line)] = stage_lines[..] else {
            return Err(error(
                &path,
                format!(
                    "expected one `stage = …` line in the front matter, found {}",
                    stage_lines.len()
                ),
            ));
        };
        let ending = &line[line.trim_end_matches(['\r', '\n']).len()..];
        let output = format!(
            "{}stage = {}{ending}{}",
            &input[..start],
            toml::Value::String(stage.to_owned()),
            &input[start + line.len()..]
        );

        let (after, _) = parse(&output).map_err(|reason| error(&path, reason))?;
        let expected = FrontMatter {
            stage: stage.to_owned(),
            ..before
        };
        if after != expected {
            return Err(error(
                &path,
                "the stage line cannot be rewritten on its own",
            ));
        }
        fs::write(&path, output).map_err(|e| error(&path, e))
    }

    fn ticket_file(&self, id: &TicketId) -> PathBuf {
        self.tickets.join(id.as_str()).join("ticket.md")
    }

    fn comments_dir(&self, id: &TicketId) -> PathBuf {
        self.tickets.join(id.as_str()).join("comments")
    }
}

/// The runner's side of the Markdown tracker. The inherent methods above
/// serve the test bench, which sets comment authors and times itself; the
/// runner posts as [`MarkdownTracker::AGENT`] at the current time.
///
/// The inherent types carry more than the shared ones: the trait reads the
/// stage on its own ([`shared::Tracker::stage`]) and blocked-by through the
/// listing ([`shared::Tracker::admitted`]); the conversion lives only here.
impl shared::Tracker for MarkdownTracker {
    fn capabilities(&self) -> &'static [Capability] {
        Self::CAPABILITIES
    }

    fn ticket(&self, id: &TicketId) -> Result<shared::Ticket, shared::Error> {
        self.require_ticket(id)?;
        MarkdownTracker::ticket(self, id)
            .map(shared_ticket)
            .map_err(other)
    }

    fn stage(&self, id: &TicketId) -> Result<String, shared::Error> {
        self.require_ticket(id)?;
        Ok(MarkdownTracker::ticket(self, id).map_err(other)?.stage)
    }

    fn comments(&self, id: &TicketId) -> Result<Vec<shared::Comment>, shared::Error> {
        self.require_ticket(id)?;
        let comments = MarkdownTracker::comments(self, id).map_err(other)?;
        Ok(comments.into_iter().map(shared_comment).collect())
    }

    fn post_comment(&self, id: &TicketId, body: &str) -> Result<shared::Comment, shared::Error> {
        self.require_ticket(id)?;
        MarkdownTracker::post_comment(self, id, Self::AGENT, Timestamp::now(), body)
            .map(shared_comment)
            .map_err(other)
    }

    fn set_stage(&self, id: &TicketId, state: &str) -> Result<(), shared::Error> {
        self.require_ticket(id)?;
        MarkdownTracker::set_stage(self, id, state).map_err(other)
    }

    /// The tickets of `tickets/` at the stage `column.ready`, with the label
    /// for a label gesture, in folder order. Their blockers are
    /// `blocked_by`: one at the stage `Done` is completed; any other stage
    /// has no type, and neither has a ticket the folder lacks, so both keep
    /// the ticket waiting. A folder whose name is not a ticket identifier,
    /// or a ticket that cannot be read, fails the listing with its path.
    fn admitted(&self, column: &shared::Column<'_>) -> Result<Vec<shared::Listed>, shared::Error> {
        let label = match column.admit {
            Admit::Label(label) => Some(label),
            Admit::State(_) => None,
            Admit::Delegation => return Err(shared::no_delegation()),
        };
        let mut listed = Vec::new();
        for id in self.ticket_ids().map_err(other)? {
            let ticket = MarkdownTracker::ticket(self, &id).map_err(other)?;
            if ticket.stage != column.ready || label.is_some_and(|l| !ticket.labels.contains(l)) {
                continue;
            }
            let mut blockers: Vec<shared::Blocker> = Vec::new();
            for blocker in &ticket.blocked_by {
                if blockers.iter().any(|known| &known.id == blocker) {
                    continue;
                }
                let state = if self.ticket_file(blocker).is_file() {
                    let stage = MarkdownTracker::ticket(self, blocker).map_err(other)?.stage;
                    (stage == Self::DONE).then_some(shared::StateType::Completed)
                } else {
                    None
                };
                blockers.push(shared::Blocker {
                    id: blocker.clone(),
                    state,
                });
            }
            listed.push(shared::Listed {
                ticket: shared_ticket(ticket),
                created_at: None,
                blockers,
            });
        }
        Ok(listed)
    }

    /// `Completed` at the stage `Done`, no type at any other; a ticket the
    /// folder lacks is not found (OWL-250).
    fn state_type(&self, id: &TicketId) -> Result<Option<shared::StateType>, shared::Error> {
        self.require_ticket(id)?;
        let stage = MarkdownTracker::ticket(self, id).map_err(other)?.stage;
        Ok((stage == Self::DONE).then_some(shared::StateType::Completed))
    }

    /// The ticket's `author`: its files keep no history, so whoever wrote
    /// the ticket made the gesture, label or state alike, as trusted as the
    /// tree it is read from. No time.
    fn gesture(&self, id: &TicketId, admit: &Admit) -> Result<shared::Gesture, shared::Error> {
        if *admit == Admit::Delegation {
            return Err(shared::no_delegation());
        }
        self.require_ticket(id)?;
        let ticket = MarkdownTracker::ticket(self, id).map_err(other)?;
        Ok(shared::Gesture {
            account: account(ticket.author),
            at: None,
        })
    }
}

impl MarkdownTracker {
    /// What the Markdown tracker implements through [`shared::Tracker`].
    pub const CAPABILITIES: &'static [Capability] = &[
        Capability::ReadTicket,
        Capability::ListAdmitted,
        Capability::Comments,
        Capability::VisibleStage,
        Capability::BlockedBy,
    ];

    /// The author of the comments the runner posts.
    pub const AGENT: &'static str = "owlshift";

    /// The stage at which a blocker is done (build plan, "P4 · The drain",
    /// part B).
    pub const DONE: &'static str = "Done";

    /// The identifiers of the ticket folders, in name order. Anything in
    /// `tickets/` but a folder is not a ticket and is skipped; a folder
    /// whose name is not an identifier is an error. No `tickets/` folder is
    /// no ticket.
    fn ticket_ids(&self) -> Result<Vec<TicketId>, TrackerError> {
        let entries = match fs::read_dir(&self.tickets) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(error(&self.tickets, e)),
        };
        let mut ids = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| error(&self.tickets, e))?;
            let path = entry.path();
            if !entry.file_type().map_err(|e| error(&path, e))?.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let id = name
                .to_str()
                .and_then(|name| TicketId::new(name).ok())
                .ok_or_else(|| error(&path, "not a ticket folder: expected a ticket identifier"))?;
            ids.push(id);
        }
        ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        Ok(ids)
    }

    fn require_ticket(&self, id: &TicketId) -> Result<(), shared::Error> {
        let path = self.ticket_file(id);
        if path.is_file() {
            Ok(())
        } else {
            Err(shared::Error::new(
                ErrorKind::NotFound,
                format!("{}: no such ticket", path.display()),
            ))
        }
    }
}

/// A ticket file as a shared ticket: its author is an account.
fn shared_ticket(ticket: Ticket) -> shared::Ticket {
    shared::Ticket {
        id: ticket.id,
        title: ticket.title,
        description: ticket.description,
        priority: ticket.priority,
        assignee: ticket.assignee.map(|name| shared::Person {
            id: name.clone(),
            name,
        }),
        labels: ticket.labels,
        author: account(ticket.author),
    }
}

/// A comment file as a shared comment. Comment files are never edited (build
/// plan, "The test tracker"), so the last edit is the creation time.
fn shared_comment(comment: Comment) -> shared::Comment {
    shared::Comment {
        id: comment.id,
        author: account(comment.author),
        created_at: comment.at,
        edited_at: None,
        body: comment.body,
        parent: comment.reply_to,
    }
}

/// A name in a ticket folder is an account: the file format has no bots, and
/// the name is both the account's identifier and what it shows.
fn account(name: String) -> shared::Author {
    shared::Author::Account(shared::Person {
        id: name.clone(),
        name,
    })
}

fn other(error: TrackerError) -> shared::Error {
    shared::Error::new(ErrorKind::Other, error.to_string())
}

/// The time part of a comment's file name, in UTC.
const TIME_FORMAT: &str = "%Y%m%dT%H%M%SZ";

/// Who may sign a comment: a name that is safe in a file name.
const AUTHOR_PATTERN: &str = "1 to 64 ASCII letters, digits, `_` or `-`";

/// The most comments one second holds: the n-th reads n − 1 microseconds
/// after its second ([`time`]), which keeps it within that second.
const MAX_PER_SECOND: u32 = 1_000_000;

/// The front matter of `ticket.md`.
#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct FrontMatter {
    title: String,
    author: String,
    stage: String,
    #[serde(default = "no_priority")]
    priority: Priority,
    #[serde(default)]
    assignee: Option<String>,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    blocked_by: Vec<TicketId>,
}

fn no_priority() -> Priority {
    Priority::Unset
}

/// Parses `ticket.md` into its front matter and its description.
fn parse(input: &str) -> Result<(FrontMatter, &str), String> {
    let (front, description) = split(input)?;
    let front = toml::from_str(&input[front]).map_err(|e| format!("invalid front matter: {e}"))?;
    Ok((front, description))
}

/// A comment file's text as the comment it replies to, from its front
/// matter, and its body. A file that does not open with a `+++` line is a
/// body alone. Test tracker only: a body of the runner's own that opened
/// with `+++` would read as a reply, and none does.
fn reply_front(text: &str) -> Result<(Option<String>, String), String> {
    if text
        .split_inclusive('\n')
        .next()
        .unwrap_or_default()
        .trim_end()
        != "+++"
    {
        return Ok((None, text.to_owned()));
    }
    let (front, body) = split(text)?;
    let front: ReplyFront =
        toml::from_str(&text[front]).map_err(|e| format!("invalid reply front matter: {e}"))?;
    Ok((Some(front.reply_to), body.to_owned()))
}

/// Where the front matter's text sits, and the description after it, without
/// its leading blank lines.
fn split(input: &str) -> Result<(Range<usize>, &str), String> {
    let mut lines = input.split_inclusive('\n');
    let first = lines.next().unwrap_or_default();
    if first.trim_end() != "+++" {
        return Err("the front matter does not open with a `+++` line".to_owned());
    }
    let mut offset = first.len();
    for line in lines {
        if line.trim_end() == "+++" {
            let rest = &input[offset + line.len()..];
            return Ok((first.len()..offset, rest.trim_start_matches(['\r', '\n'])));
        }
        offset += line.len();
    }
    Err("the front matter is not closed by a `+++` line".to_owned())
}

/// A file of a ticket's `comments/` folder, as its name gives it.
struct CommentFile {
    name: String,
    /// The second its name holds; the comment reads at [`time`].
    second: Timestamp,
    /// Its place among the comments of its second, from 1.
    nth: u32,
    author: String,
}

/// When the `nth` comment of `second` reads: n − 1 microseconds after it, so
/// the comments of one second read in the order they were posted.
fn time(second: Timestamp, nth: u32) -> Result<Timestamp, jiff::Error> {
    second.checked_add(SignedDuration::from_micros(i64::from(nth) - 1))
}

/// The comment files in `dir`, in reading order: second, then place in the
/// second, then name, which keeps today's order for files of one second
/// without a suffix. No folder is no comment; a file whose name is not a
/// comment's is an error.
fn comment_files(dir: &Path) -> Result<Vec<CommentFile>, TrackerError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(error(dir, e)),
    };
    let mut files = Vec::new();
    for entry in entries {
        let path = entry.map_err(|e| error(dir, e))?.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        let Some((second, nth, author)) = comment_name(name) else {
            return Err(error(
                &path,
                "not a comment file: expected <YYYYMMDDTHHMMSSZ>[.<n>]-<author>.md",
            ));
        };
        files.push(CommentFile {
            name: name.to_owned(),
            second,
            nth,
            author,
        });
    }
    files.sort_by(|a, b| (a.second, a.nth, &a.name).cmp(&(b.second, b.nth, &b.name)));
    Ok(files)
}

/// A comment's file name without `.md`, which [`comment_name`] reads back.
fn stem(second: Timestamp, nth: u32, author: &str) -> String {
    let time = second.strftime(TIME_FORMAT);
    if nth == 1 {
        format!("{time}-{author}")
    } else {
        format!("{time}.{nth}-{author}")
    }
}

/// The second, place in it and author of a comment file name, if it is one
/// written as [`stem`] writes it.
fn comment_name(name: &str) -> Option<(Timestamp, u32, String)> {
    let stem = name.strip_suffix(".md")?;
    let time = stem.get(..16)?;
    let rest = stem.get(16..)?;
    let (nth, author) = match rest.strip_prefix('.') {
        None => (1, rest.strip_prefix('-')?),
        Some(rest) => {
            let (digits, author) = rest.split_once('-')?;
            let nth: u32 = digits.parse().ok()?;
            // One name per place: no `.1`, `.0`, `.02` or `.+2`.
            ((2..=MAX_PER_SECOND).contains(&nth) && nth.to_string() == digits)
                .then_some((nth, author))?
        }
    };
    let at = DateTime::strptime(TIME_FORMAT, time)
        .ok()?
        .to_zoned(TimeZone::UTC)
        .ok()?
        .timestamp();
    let canonical = at.strftime(TIME_FORMAT).to_string() == time;
    (canonical && valid_author(author)).then(|| (at, nth, author.to_owned()))
}

fn valid_author(author: &str) -> bool {
    (1..=64).contains(&author.len())
        && author
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn read(path: &Path) -> Result<String, TrackerError> {
    fs::read_to_string(path).map_err(|e| error(path, e))
}

fn error(path: &Path, reason: impl fmt::Display) -> TrackerError {
    TrackerError {
        path: path.to_owned(),
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TICKET: &str = "+++\r\n\
        title = \"Add a greeting\"\r\n\
        author = \"reporter\"\r\n\
        stage = \"Todo\"\r\n\
        priority = \"high\"\r\n\
        assignee = \"maintainer\"\r\n\
        labels = [\"docs\"]\r\n\
        blocked_by = [\"DEMO-0\"]\r\n\
        +++\r\n\
        \r\n\
        Say hello in the README.\r\n";

    fn id() -> TicketId {
        TicketId::new("DEMO-1").unwrap()
    }

    /// A repository holding one ticket with this `ticket.md`.
    fn repository(ticket: &str) -> (tempfile::TempDir, MarkdownTracker, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("tickets").join("DEMO-1");
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("ticket.md");
        fs::write(&file, ticket).unwrap();
        let tracker = MarkdownTracker::new(root.path());
        (root, tracker, file)
    }

    fn at(time: &str) -> Timestamp {
        time.parse().unwrap()
    }

    #[test]
    fn reads_a_ticket_written_with_crlf() {
        let (_root, tracker, _) = repository(TICKET);
        let ticket = tracker.ticket(&id()).unwrap();
        assert_eq!(
            ticket,
            Ticket {
                id: id(),
                title: "Add a greeting".to_owned(),
                author: "reporter".to_owned(),
                stage: "Todo".to_owned(),
                priority: Priority::High,
                assignee: Some("maintainer".to_owned()),
                labels: vec!["docs".to_owned()],
                blocked_by: vec![TicketId::new("DEMO-0").unwrap()],
                description: "Say hello in the README.\r\n".to_owned(),
            }
        );
    }

    #[test]
    fn a_malformed_ticket_is_refused_with_its_path() {
        let unclosed = "+++\ntitle = \"x\"\nauthor = \"a\"\nstage = \"Todo\"\n";
        let unknown = "+++\ntitle = \"x\"\nauthor = \"a\"\nstage = \"Todo\"\nsize = 3\n+++\n";
        for (input, reason) in [(unclosed, "not closed"), (unknown, "size")] {
            let (_root, tracker, file) = repository(input);
            let error = tracker.ticket(&id()).unwrap_err();
            assert_eq!(error.path, file);
            assert!(error.to_string().contains(reason), "{error}");
        }
    }

    /// A reply names the comment it answers in its front matter (OWL-146),
    /// which its body leaves out; a comment without one is its body alone.
    #[test]
    fn a_reply_names_the_comment_it_answers() {
        let (_root, tracker, _) = repository(TICKET);
        let decision = tracker
            .post_comment(&id(), "owlshift", at("2026-09-28T10:00:00Z"), "Decided.\n")
            .unwrap();
        let reply = tracker
            .post_reply(
                &id(),
                "maintainer",
                at("2026-09-28T10:05:00Z"),
                &decision.id,
                "No: in docs/.\n",
            )
            .unwrap();
        let read = tracker.comments(&id()).unwrap();
        assert_eq!(read, [decision.clone(), reply]);
        assert_eq!(read[1].reply_to.as_deref(), Some(decision.id.as_str()));
        assert_eq!(read[1].body, "No: in docs/.\n");
        assert_eq!(read[0].reply_to, None);
    }

    #[test]
    fn comments_come_back_in_time_order() {
        let (_root, tracker, _) = repository(TICKET);
        assert_eq!(tracker.comments(&id()).unwrap(), []);
        tracker
            .post_comment(&id(), "maintainer", at("2026-09-28T10:05:00Z"), "Second.\n")
            .unwrap();
        let first = tracker
            .post_comment(&id(), "owlshift", at("2026-09-28T10:01:00Z"), "First.\n")
            .unwrap();
        let comments = tracker.comments(&id()).unwrap();
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0], first);
        assert_eq!(comments[1].author, "maintainer");
        assert_eq!(comments[1].body, "Second.\n");

        for author in ["", "a b", "../x", "a:b"] {
            let refused = tracker.post_comment(&id(), author, at("2026-09-28T11:00:00Z"), "x");
            assert!(refused.is_err(), "{author:?}");
        }

        let stray = tracker
            .tickets
            .join("DEMO-1")
            .join("comments")
            .join("notes.md");
        fs::write(&stray, "?").unwrap();
        assert_eq!(tracker.comments(&id()).unwrap_err().path, stray);
    }

    /// Comments posted back to back in one second are all kept, in posting
    /// order, whoever wrote them, and read at times in that order (OWL-136);
    /// files of one second written before the suffix keep their names, their
    /// ids and their order, and share the second's time.
    #[test]
    fn comments_of_one_second_are_kept_in_posting_order() {
        let (_root, tracker, _) = repository(TICKET);
        let comments_dir = tracker.comments_dir(&id());
        fs::create_dir_all(&comments_dir).unwrap();
        // Two unsuffixed files of one second read in file-name order (`-`
        // sorts before `.`), and the next comment of that second is its 2nd.
        for author in ["a-b", "a"] {
            fs::write(
                comments_dir.join(format!("20260928T100000Z-{author}.md")),
                author,
            )
            .unwrap();
        }
        let second = at("2026-09-28T10:00:00.700Z");
        let mut posted = Vec::new();
        for n in 2..=10 {
            let author = if n == 4 { "maintainer" } else { "owlshift" };
            let comment = tracker
                .post_comment(&id(), author, second, &n.to_string())
                .unwrap();
            assert_eq!(comment.id, format!("20260928T100000Z.{n}-{author}"));
            assert_eq!(
                comment.at,
                at(&format!("2026-09-28T10:00:00.{:06}Z", n - 1))
            );
            posted.push(comment);
        }
        // A new second starts again without a suffix.
        tracker
            .post_comment(&id(), "owlshift", at("2026-09-28T10:00:01Z"), "next")
            .unwrap();

        let comments = tracker.comments(&id()).unwrap();
        let bodies: Vec<_> = comments.iter().map(|comment| &comment.body).collect();
        assert_eq!(
            bodies,
            [
                "a-b", "a", "2", "3", "4", "5", "6", "7", "8", "9", "10", "next"
            ]
        );
        // Each comment reads back as it was posted.
        assert_eq!(comments[2..11], posted[..]);
        let shared = shared::Tracker::comments(&tracker, &id()).unwrap();
        assert_eq!(shared[0].id, "20260928T100000Z-a-b");
        assert_eq!(shared[2].id, "20260928T100000Z.2-owlshift");
        assert_eq!(shared[11].id, "20260928T100001Z-owlshift");
        let times: Vec<_> = shared.iter().map(|comment| comment.created_at).collect();
        assert_eq!(times[0], times[1]);
        assert!(times[1..].is_sorted_by(|a, b| a < b), "{times:?}");

        // A second holds a million comments at most.
        fs::write(comments_dir.join("20260928T100002Z.1000000-a.md"), "last").unwrap();
        let full = tracker.post_comment(&id(), "a", at("2026-09-28T10:00:02Z"), "x");
        assert!(
            full.unwrap_err()
                .reason
                .contains("too many comments in one second"),
        );
        assert_eq!(
            tracker.comments(&id()).unwrap().last().unwrap().at,
            at("2026-09-28T10:00:02.999999Z")
        );
    }

    #[test]
    fn a_comment_name_is_read_in_its_one_written_form() {
        for (name, read) in [
            ("20260928T100000Z-2-bob.md", Some((1, "2-bob"))),
            ("20260928T100000Z.2-2-bob.md", Some((2, "2-bob"))),
            ("20260928T100000Z.12-bob.md", Some((12, "bob"))),
            ("20260928T100000Z.1-bob.md", None),
            ("20260928T100000Z.0-bob.md", None),
            ("20260928T100000Z.02-bob.md", None),
            ("20260928T100000Z.+2-bob.md", None),
            ("20260928T100000Z.-bob.md", None),
            ("20260928T100000Z.2bob.md", None),
            ("20260928T100000Z.2-.md", None),
            ("20260928T100000Z.1000000-bob.md", Some((1_000_000, "bob"))),
            ("20260928T100000Z.1000001-bob.md", None),
            ("20260928T100000Z.4294967296-bob.md", None),
        ] {
            let parsed = comment_name(name).map(|(at, nth, author)| {
                assert_eq!(stem(at, nth, &author) + ".md", name);
                (nth, author)
            });
            assert_eq!(
                parsed,
                read.map(|(nth, author)| (nth, author.to_owned())),
                "{name}"
            );
        }
    }

    /// A `ticket.md` at `stage`, with these labels and blockers.
    fn ticket_md(author: &str, stage: &str, labels: &[&str], blocked_by: &[&str]) -> String {
        format!(
            "+++\ntitle = \"t\"\nauthor = \"{author}\"\nstage = \"{stage}\"\n\
             labels = {labels:?}\nblocked_by = {blocked_by:?}\n+++\n"
        )
    }

    /// The ready column (OWL-246): the stage, the label for a label gesture,
    /// and the blockers' states, `Done` alone completed.
    #[test]
    fn the_ready_column_lists_admitted_tickets_with_their_blockers() {
        use shared::{Blocker, Column, StateType, Tracker as _};
        let (root, tracker, _) = repository(&ticket_md("reporter", "Todo", &["agent"], &[]));
        let tickets = root.path().join("tickets");
        for (id, text) in [
            (
                "DEMO-2",
                ticket_md(
                    "maintainer",
                    "Todo",
                    &[],
                    &["DEMO-3", "DEMO-4", "DEMO-9", "DEMO-3"],
                ),
            ),
            ("DEMO-3", ticket_md("a", "Done", &[], &[])),
            ("DEMO-4", ticket_md("a", "Canceled", &["agent"], &[])),
        ] {
            fs::create_dir_all(tickets.join(id)).unwrap();
            fs::write(tickets.join(id).join("ticket.md"), text).unwrap();
        }
        fs::write(tickets.join("README.md"), "not a ticket").unwrap();

        let label = Admit::Label("agent".to_owned());
        let column = |admit| Column {
            team: None,
            ready: "Todo",
            admit,
        };
        let listed = tracker.admitted(&column(&label)).unwrap();
        let ids: Vec<&str> = listed.iter().map(|l| l.ticket.id.as_str()).collect();
        assert_eq!(ids, ["DEMO-1"]);
        assert_eq!(
            listed[0].ticket,
            shared_ticket(tracker.ticket(&id()).unwrap())
        );
        assert_eq!(listed[0].created_at, None);

        let state = Admit::State("Todo".to_owned());
        let listed = tracker.admitted(&column(&state)).unwrap();
        let ids: Vec<&str> = listed.iter().map(|l| l.ticket.id.as_str()).collect();
        assert_eq!(ids, ["DEMO-1", "DEMO-2"]);
        let blocker = |id: &str, state| Blocker {
            id: TicketId::new(id).unwrap(),
            state,
        };
        assert_eq!(
            listed[1].blockers,
            [
                blocker("DEMO-3", Some(StateType::Completed)),
                blocker("DEMO-4", None),
                blocker("DEMO-9", None),
            ]
        );

        let error = tracker.admitted(&column(&Admit::Delegation)).unwrap_err();
        assert!(error.message.contains("not built"), "{error}");
        fs::create_dir_all(tickets.join("drafts")).unwrap();
        let error = tracker.admitted(&column(&state)).unwrap_err();
        assert!(error.message.contains("drafts"), "{error}");
    }

    /// The gesture is the ticket's author, label or state alike, with no
    /// time; a missing ticket is not found.
    #[test]
    fn the_gesture_is_the_tickets_author() {
        use shared::Tracker as _;
        let (_root, tracker, _) = repository(TICKET);
        let gesture = tracker
            .gesture(&id(), &Admit::State("Todo".to_owned()))
            .unwrap();
        assert_eq!(gesture.account, account("reporter".to_owned()));
        assert_eq!(gesture.at, None);
        let missing = TicketId::new("DEMO-9").unwrap();
        let error = tracker
            .gesture(&missing, &Admit::Label("agent".to_owned()))
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::NotFound);
        assert!(tracker.gesture(&id(), &Admit::Delegation).is_err());
    }

    /// A blocker of the text convention (OWL-250): `Done` is completed, any
    /// other stage has no type, a missing folder is not found.
    #[test]
    fn a_state_type_is_completed_at_done_and_a_missing_ticket_not_found() {
        use shared::{StateType, Tracker as _};
        let (root, tracker, _) = repository(&ticket_md("a", "Todo", &[], &[]));
        assert_eq!(tracker.state_type(&id()), Ok(None));
        let done = root.path().join("tickets").join("DEMO-3");
        fs::create_dir_all(&done).unwrap();
        fs::write(done.join("ticket.md"), ticket_md("a", "Done", &[], &[])).unwrap();
        let done = TicketId::new("DEMO-3").unwrap();
        assert_eq!(tracker.state_type(&done), Ok(Some(StateType::Completed)));
        let missing = TicketId::new("UTF-8").unwrap();
        assert_eq!(
            tracker.state_type(&missing).unwrap_err().kind,
            ErrorKind::NotFound
        );
    }

    #[test]
    fn setting_the_stage_changes_one_line_only() {
        let (_root, tracker, file) = repository(TICKET);
        tracker.set_stage(&id(), "In Progress").unwrap();
        let after = fs::read_to_string(&file).unwrap();
        let changed: Vec<_> = TICKET
            .split_inclusive('\n')
            .zip(after.split_inclusive('\n'))
            .filter(|(before, after)| before != after)
            .collect();
        assert_eq!(
            changed,
            [("stage = \"Todo\"\r\n", "stage = \"In Progress\"\r\n")]
        );
        assert_eq!(
            after.len(),
            TICKET.len() + "In Progress".len() - "Todo".len()
        );
        assert_eq!(tracker.ticket(&id()).unwrap().stage, "In Progress");

        // A second line that looks like the stage makes the edit ambiguous.
        let ambiguous =
            "+++\ntitle = \"\"\"\nstage = \"x\"\n\"\"\"\nauthor = \"a\"\nstage = \"Todo\"\n+++\n";
        let (_root, tracker, file) = repository(ambiguous);
        assert!(tracker.set_stage(&id(), "Done").is_err());
        assert_eq!(fs::read_to_string(&file).unwrap(), ambiguous);
    }
}
