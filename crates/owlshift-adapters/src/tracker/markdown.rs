//! The Markdown tracker: tickets kept as files in the repository, the test
//! tracker of P0 and the public Markdown tracker of P9.
//!
//! One folder per ticket, `tickets/<ID>/`, holds `ticket.md` (a TOML front
//! matter between `+++` lines, then the description) and `comments/`, one
//! file per comment named `<YYYYMMDDTHHMMSSZ>[.<n>]-<author>.md`, `.<n>`
//! marking the n-th comment of a second. The format is settled in
//! `docs/design/build-plan.md`, "The test tracker".

use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use jiff::civil::DateTime;
use jiff::tz::TimeZone;
use serde::Deserialize;

use owlshift_contracts::Priority;
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
    pub at: Timestamp,
    pub author: String,
    pub body: String,
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
                Ok(Comment {
                    id: file.name.strip_suffix(".md").unwrap_or_default().to_owned(),
                    at: file.at,
                    author: file.author,
                    body: read(&dir.join(&file.name))?,
                })
            })
            .collect()
    }

    /// Adds a comment after the others of its second, whoever wrote them.
    /// The time is kept to the second; a file is never overwritten, so a
    /// name already taken (by a concurrent poster) is an error.
    pub fn post_comment(
        &self,
        id: &TicketId,
        author: &str,
        at: Timestamp,
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
        let at = Timestamp::from_second(at.as_second()).map_err(|e| error(&dir, e))?;
        let nth = match comment_files(&dir)?
            .iter()
            .filter(|file| file.at == at)
            .map(|file| file.nth)
            .max()
        {
            None => 1,
            Some(last) => last
                .checked_add(1)
                .ok_or_else(|| error(&dir, "too many comments in one second"))?,
        };
        fs::create_dir_all(&dir).map_err(|e| error(&dir, e))?;
        let id = stem(at, nth, author);
        let path = dir.join(format!("{id}.md"));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| error(&path, e))?;
        file.write_all(body.as_bytes())
            .map_err(|e| error(&path, e))?;
        Ok(Comment {
            id,
            at,
            author: author.to_owned(),
            body: body.to_owned(),
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
/// The inherent types carry more than the shared ones (the stage,
/// blocked-by). The trait writes the visible stage but does not read it;
/// they converge when it reads both and blocked-by relations; until then the
/// conversion lives only here.
impl shared::Tracker for MarkdownTracker {
    fn capabilities(&self) -> &'static [Capability] {
        Self::CAPABILITIES
    }

    fn ticket(&self, id: &TicketId) -> Result<shared::Ticket, shared::Error> {
        self.require_ticket(id)?;
        let ticket = MarkdownTracker::ticket(self, id).map_err(other)?;
        Ok(shared::Ticket {
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
        })
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
}

impl MarkdownTracker {
    /// What the Markdown tracker implements through [`shared::Tracker`].
    pub const CAPABILITIES: &'static [Capability] = &[
        Capability::ReadTicket,
        Capability::Comments,
        Capability::VisibleStage,
    ];

    /// The author of the comments the runner posts.
    pub const AGENT: &'static str = "owlshift";

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

/// A comment file as a shared comment. Comment files are never edited (build
/// plan, "The test tracker"), so the last edit is the creation time.
fn shared_comment(comment: Comment) -> shared::Comment {
    shared::Comment {
        id: comment.id,
        author: account(comment.author),
        created_at: comment.at,
        edited_at: None,
        body: comment.body,
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
    at: Timestamp,
    /// Its place among the comments of its second, from 1.
    nth: u32,
    author: String,
}

/// The comment files in `dir`, in reading order: time, then place in the
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
        let Some((at, nth, author)) = comment_name(name) else {
            return Err(error(
                &path,
                "not a comment file: expected <YYYYMMDDTHHMMSSZ>[.<n>]-<author>.md",
            ));
        };
        files.push(CommentFile {
            name: name.to_owned(),
            at,
            nth,
            author,
        });
    }
    files.sort_by(|a, b| (a.at, a.nth, &a.name).cmp(&(b.at, b.nth, &b.name)));
    Ok(files)
}

/// A comment's file name without `.md`, which [`comment_name`] reads back.
fn stem(at: Timestamp, nth: u32, author: &str) -> String {
    let time = at.strftime(TIME_FORMAT);
    if nth == 1 {
        format!("{time}-{author}")
    } else {
        format!("{time}.{nth}-{author}")
    }
}

/// The time, place in its second and author of a comment file name, if it
/// is one written as [`stem`] writes it.
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
            (nth >= 2 && nth.to_string() == digits).then_some((nth, author))?
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
    /// order, whoever wrote them; files of one second written before the
    /// suffix keep their names, their ids and their order.
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
        for n in 2..=10 {
            let author = if n == 4 { "maintainer" } else { "owlshift" };
            let posted = tracker
                .post_comment(&id(), author, second, &n.to_string())
                .unwrap();
            assert_eq!(posted.id, format!("20260928T100000Z.{n}-{author}"));
            assert_eq!(posted.at, at("2026-09-28T10:00:00Z"));
        }
        // A new second starts again without a suffix.
        tracker
            .post_comment(&id(), "owlshift", at("2026-09-28T10:00:01Z"), "next")
            .unwrap();

        let bodies: Vec<_> = tracker
            .comments(&id())
            .unwrap()
            .into_iter()
            .map(|comment| comment.body)
            .collect();
        assert_eq!(
            bodies,
            [
                "a-b", "a", "2", "3", "4", "5", "6", "7", "8", "9", "10", "next"
            ]
        );
        let ids: Vec<_> = shared::Tracker::comments(&tracker, &id())
            .unwrap()
            .into_iter()
            .map(|comment| comment.id)
            .collect();
        assert_eq!(ids[0], "20260928T100000Z-a-b");
        assert_eq!(ids[2], "20260928T100000Z.2-owlshift");
        assert_eq!(ids[11], "20260928T100001Z-owlshift");
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
