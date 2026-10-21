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
//!
//! The proposals inbox (OWL-267) is the stage [`MarkdownTracker::TRIAGE`]:
//! a follow-up is filed as a new ticket folder after the parent's key's
//! highest number, authored [`MarkdownTracker::AGENT`], `related` to its
//! parent and, when it needs the parent's code, `blocked_by` it.
//!
//! A ticket's documents (OWL-350) are files `documents/<n>.md` in its
//! folder, `n` from 1, each opening with its title as a `# ` line.

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
    /// The tickets it relates to: a proposal's parent (OWL-267).
    pub related: Vec<TicketId>,
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
            related: front.related,
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
        self.rewrite_key(id, "stage", false, |before| {
            let value = toml::Value::String(stage.to_owned());
            let expected = FrontMatter {
                stage: stage.to_owned(),
                ..before.clone()
            };
            Some((value, expected))
        })
    }

    /// Adds `blocker` to the ticket's `blocked_by` (OWL-249), rewriting
    /// that key's line of the front matter, and only that line, or adding
    /// it before the closing `+++` when the front matter has none. One
    /// already listed changes nothing. The key must be written on one line,
    /// `blocked_by = [...]`, and every other value must read the same
    /// afterwards; a ticket is never its own blocker.
    pub fn add_blocked_by(&self, id: &TicketId, blocker: &TicketId) -> Result<(), TrackerError> {
        if blocker == id {
            return Err(error(&self.ticket_file(id), "a ticket never blocks itself"));
        }
        self.rewrite_key(id, "blocked_by", true, |before| {
            if before.blocked_by.contains(blocker) {
                return None;
            }
            let mut blocked_by = before.blocked_by.clone();
            blocked_by.push(blocker.clone());
            let value = ids_value(&blocked_by);
            let expected = FrontMatter {
                blocked_by,
                ..before.clone()
            };
            Some((value, expected))
        })
    }

    /// Files a follow-up of `parent` in the proposals inbox (OWL-267) and
    /// returns its identifier: a new folder `<KEY>-<n>`, `KEY` the parent's
    /// key and `n` one past the highest number among that key's folders,
    /// holding a `ticket.md` at the stage [`Self::TRIAGE`], authored
    /// [`Self::AGENT`], `related` to the parent and, with
    /// `blocked_by_parent`, blocked by it; no priority, assignee or label.
    /// The description follows one blank line, as given but for leading
    /// line breaks, which the format never reads.
    ///
    /// The folder is claimed by creating it, which fails when it exists, so
    /// a ticket is never overwritten: a number taken meanwhile (by another
    /// filing, or by a folder of that name in another case on a file system
    /// that ignores case) moves on to the next one, at most 64 times. A
    /// `ticket.md` that cannot be written leaves no folder behind. A parent that is not there, whose identifier ends in no number, or a
    /// title blank or on several lines is refused before anything is
    /// written.
    pub fn propose(
        &self,
        parent: &TicketId,
        title: &str,
        description: &str,
        blocked_by_parent: bool,
    ) -> Result<TicketId, TrackerError> {
        let parent_file = self.ticket_file(parent);
        if !parent_file.is_file() {
            return Err(error(&parent_file, "no such ticket"));
        }
        if title.trim().is_empty() || title.contains(['\n', '\r']) {
            return Err(error(
                &parent_file,
                "a proposal's title must be one line, not blank",
            ));
        }
        let Some((key, _)) = numbered(parent) else {
            return Err(error(
                &parent_file,
                format!("{parent} ends in no number to file a proposal after"),
            ));
        };
        let front = FrontMatter {
            title: title.to_owned(),
            author: Self::AGENT.to_owned(),
            stage: Self::TRIAGE.to_owned(),
            priority: Priority::Unset,
            assignee: None,
            labels: Vec::new(),
            blocked_by: if blocked_by_parent {
                vec![parent.clone()]
            } else {
                Vec::new()
            },
            related: vec![parent.clone()],
        };
        let description = description.trim_start_matches(['\r', '\n']);
        let text = format!(
            "+++\n{}+++\n\n{description}",
            render_front(&front).map_err(|e| error(&parent_file, e))?
        );
        if !matches!(parse(&text), Ok((read, rest)) if read == front && rest == description) {
            return Err(error(
                &parent_file,
                "the proposal does not read back as written",
            ));
        }
        let highest = self
            .ticket_ids()?
            .iter()
            .filter_map(numbered)
            .filter(|(k, _)| *k == key)
            .map(|(_, n)| n)
            .max()
            .unwrap_or(0);
        let (id, dir) = self.claim(key, highest, CLAIM_TRIES)?;
        let path = dir.join("ticket.md");
        let written = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .and_then(|mut file| file.write_all(text.as_bytes()));
        if let Err(e) = written {
            // The folder is this filing's own, just made: nothing else is
            // in it.
            let _ = fs::remove_dir_all(&dir);
            return Err(error(&path, e));
        }
        Ok(id)
    }

    /// Writes a document of ticket `id` (OWL-350): with `document` `None`, a
    /// new file `documents/<n>.md` in its folder, `n` one past the highest
    /// there, claimed by creating it, as a proposal's folder is; otherwise
    /// the file of that number, which must be there, written over. Either
    /// way the file is `title` on a `# ` line, flattened to one, a blank
    /// line, then `content`. Returns the document's number. A ticket that is
    /// not there, or a number that is not one, is refused before anything
    /// is written.
    pub fn publish_document(
        &self,
        id: &TicketId,
        document: Option<&str>,
        title: &str,
        content: &str,
    ) -> Result<String, TrackerError> {
        let ticket = self.ticket_file(id);
        if !ticket.is_file() {
            return Err(error(&ticket, "no such ticket"));
        }
        let dir = self.documents_dir(id);
        let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
        let text = format!("# {title}\n\n{content}");
        if let Some(document) = document {
            let path = dir.join(format!("{document}.md"));
            if document_number(document).is_none() || !path.is_file() {
                return Err(error(&path, "no such document"));
            }
            fs::write(&path, text).map_err(|e| error(&path, e))?;
            return Ok(document.to_owned());
        }
        fs::create_dir_all(&dir).map_err(|e| error(&dir, e))?;
        let highest = fs::read_dir(&dir)
            .map_err(|e| error(&dir, e))?
            .filter_map(|entry| {
                let name = entry.ok()?.file_name().into_string().ok()?;
                document_number(name.strip_suffix(".md")?)
            })
            .max()
            .unwrap_or(0);
        for n in (highest + 1..).take(CLAIM_TRIES as usize) {
            let path = dir.join(format!("{n}.md"));
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    file.write_all(text.as_bytes())
                        .map_err(|e| error(&path, e))?;
                    return Ok(n.to_string());
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(error(&path, e)),
            }
        }
        Err(error(
            &dir,
            format!("no free document number among the {CLAIM_TRIES} after {highest}"),
        ))
    }

    /// Where a document of ticket `id` is, from the repository's root.
    pub fn document_path(id: &TicketId, document: &str) -> String {
        format!("tickets/{id}/documents/{document}.md")
    }

    /// Creates the first free folder `<key>-<n>` for `n` past `after`,
    /// trying at most `tries` numbers, and returns its identifier and path.
    fn claim(
        &self,
        key: &str,
        after: u64,
        tries: u32,
    ) -> Result<(TicketId, PathBuf), TrackerError> {
        let mut n = after;
        for _ in 0..tries {
            n = n
                .checked_add(1)
                .ok_or_else(|| error(&self.tickets, format!("no number left after {key}-{n}")))?;
            let id = TicketId::new(format!("{key}-{n}")).map_err(|e| error(&self.tickets, e))?;
            let dir = self.tickets.join(id.as_str());
            match fs::create_dir(&dir) {
                Ok(()) => return Ok((id, dir)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(error(&dir, e)),
            }
        }
        Err(error(
            &self.tickets,
            format!("no free identifier among the {tries} after {key}-{after}"),
        ))
    }

    /// Rewrites the line of the front matter that sets `key`, the one line
    /// written `key = …`, to the value `change` gives, or, when `insert` is
    /// set and no line sets it, adds one before the closing `+++`. `change`
    /// returns the value and the front matter expected once it is written,
    /// or `None` when nothing is to change; the file is written only when
    /// it reads back exactly as expected, so no other value moves.
    fn rewrite_key(
        &self,
        id: &TicketId,
        key: &str,
        insert: bool,
        change: impl FnOnce(&FrontMatter) -> Option<(toml::Value, FrontMatter)>,
    ) -> Result<(), TrackerError> {
        let path = self.ticket_file(id);
        let input = read(&path)?;
        let (before, _) = parse(&input).map_err(|reason| error(&path, reason))?;
        let Some((value, expected)) = change(&before) else {
            return Ok(());
        };
        let (front, _) = split(&input).map_err(|reason| error(&path, reason))?;

        let mut offset = front.start;
        let mut key_lines = Vec::new();
        for line in input[front.clone()].split_inclusive('\n') {
            let rest = line.trim_start();
            if rest
                .strip_prefix(key)
                .is_some_and(|r| r.trim_start().starts_with('='))
            {
                key_lines.push((offset, line));
            }
            offset += line.len();
        }
        let output = match key_lines[..] {
            [(start, line)] => {
                let ending = &line[line.trim_end_matches(['\r', '\n']).len()..];
                format!(
                    "{}{key} = {value}{ending}{}",
                    &input[..start],
                    &input[start + line.len()..]
                )
            }
            [] if insert => {
                let opening = &input[..front.start];
                let ending = &opening[opening.trim_end_matches(['\r', '\n']).len()..];
                format!(
                    "{}{key} = {value}{ending}{}",
                    &input[..front.end],
                    &input[front.end..]
                )
            }
            _ => {
                return Err(error(
                    &path,
                    format!(
                        "expected one `{key} = …` line in the front matter, found {}",
                        key_lines.len()
                    ),
                ));
            }
        };

        let (after, _) = parse(&output).map_err(|reason| error(&path, reason))?;
        if after != expected {
            return Err(error(
                &path,
                format!("the {key} line cannot be rewritten on its own"),
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

    fn documents_dir(&self, id: &TicketId) -> PathBuf {
        self.tickets.join(id.as_str()).join("documents")
    }
}

/// The number a document's name gives: ASCII digits without a leading zero.
fn document_number(name: &str) -> Option<u64> {
    if name.is_empty() || name.starts_with('0') || !name.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    name.parse().ok()
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
            let blockers = self.blockers(&ticket)?;
            listed.push(shared::Listed {
                ticket: shared_ticket(ticket),
                created_at: None,
                blockers,
            });
        }
        Ok(listed)
    }

    /// Every ticket of the folder at the stage `state` (OWL-410); the team
    /// is ignored.
    fn working(
        &self,
        _team: Option<&str>,
        state: &str,
    ) -> Result<Vec<shared::Ticket>, shared::Error> {
        let mut working = Vec::new();
        for id in self.ticket_ids().map_err(other)? {
            let ticket = MarkdownTracker::ticket(self, &id).map_err(other)?;
            if ticket.stage == state {
                working.push(shared_ticket(ticket));
            }
        }
        Ok(working)
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

    /// The ticket's stage `Done` as completed, any other stage without a
    /// type, its description and its `blocked_by`, read as the listing
    /// reads them. Only a ticket file that is not there is
    /// [`ErrorKind::NotFound`]: one that cannot be looked at, or a path
    /// that is not a file, is another error, never an unknown ticket.
    fn relations(&self, id: &TicketId) -> Result<shared::Relations, shared::Error> {
        self.require_ticket_file(id)?;
        let ticket = MarkdownTracker::ticket(self, id).map_err(other)?;
        Ok(shared::Relations {
            id: id.clone(),
            state: (ticket.stage == Self::DONE).then_some(shared::StateType::Completed),
            blockers: self.blockers(&ticket)?,
            description: ticket.description,
        })
    }

    /// The ticket's `blocked_by` and `related`, then, from one scan of the
    /// folder, the tickets whose `blocked_by` names it, which it blocks, and
    /// those whose `related` names it, related to it too (OWL-416). A
    /// `blocked_by` or `related` entry the folder lacks has no title and no
    /// state; one of its own that cannot be read fails the read, as for
    /// [`relations`](shared::Tracker::relations), while another ticket of
    /// the scan that cannot be read is left out, since what it says about
    /// this one is unknown and only informs a role.
    fn links(&self, id: &TicketId) -> Result<shared::Links, shared::Error> {
        self.require_ticket_file(id)?;
        let ticket = MarkdownTracker::ticket(self, id).map_err(other)?;
        let mut links = shared::Links::default();
        for (list, ids) in [
            (&mut links.blocked_by, &ticket.blocked_by),
            (&mut links.related, &ticket.related),
        ] {
            for named in ids {
                let linked = if self.ticket_file(named).is_file() {
                    linked(MarkdownTracker::ticket(self, named).map_err(other)?)
                } else {
                    shared::Linked {
                        id: named.clone(),
                        title: None,
                        state: None,
                    }
                };
                add_linked(list, linked);
            }
        }
        for scanned in self.ticket_ids().map_err(other)? {
            if scanned == *id {
                continue;
            }
            let Ok(found) = MarkdownTracker::ticket(self, &scanned) else {
                continue;
            };
            let (blocks, relates) = (found.blocked_by.contains(id), found.related.contains(id));
            if blocks {
                add_linked(&mut links.blocks, linked(found.clone()));
            }
            if relates {
                add_linked(&mut links.related, linked(found));
            }
        }
        Ok(links)
    }

    /// Adds `blocker` to `blocked`'s `blocked_by`
    /// ([`MarkdownTracker::add_blocked_by`]).
    fn block(&self, blocker: &TicketId, blocked: &TicketId) -> Result<(), shared::Error> {
        self.require_ticket_file(blocked)?;
        MarkdownTracker::add_blocked_by(self, blocked, blocker).map_err(other)
    }

    /// Files the proposal at the stage [`MarkdownTracker::TRIAGE`]
    /// ([`MarkdownTracker::propose`]), refusing first a ready state of that
    /// name, which would put it in the queue without a person. A parent
    /// that is not there is [`ErrorKind::NotFound`]. One file is written,
    /// so a filing that succeeds has no problem to report.
    fn propose(&self, proposal: &shared::Proposal<'_>) -> Result<shared::Filed, shared::Error> {
        if proposal.ready == Self::TRIAGE {
            return Err(shared::Error::new(
                ErrorKind::Other,
                format!(
                    "the proposals inbox, the stage {:?}, is the ready state: a proposal \
                     there would enter the queue without a person",
                    Self::TRIAGE
                ),
            ));
        }
        self.require_ticket(proposal.parent)?;
        let id = MarkdownTracker::propose(
            self,
            proposal.parent,
            proposal.title,
            proposal.description,
            proposal.blocked_by_parent,
        )
        .map_err(other)?;
        Ok(shared::Filed {
            id,
            state: Self::TRIAGE.to_owned(),
            problems: Vec::new(),
        })
    }

    /// The file `documents/<n>.md` of the ticket's folder
    /// ([`MarkdownTracker::publish_document`]), its path from the
    /// repository's root as its link. A document that is not there is
    /// [`ErrorKind::NotFound`], as is a ticket that is not.
    fn publish_document(
        &self,
        id: &TicketId,
        document: Option<&str>,
        title: &str,
        content: &str,
    ) -> Result<shared::Document, shared::Error> {
        self.require_ticket(id)?;
        if let Some(document) = document
            && (document_number(document).is_none()
                || !self
                    .documents_dir(id)
                    .join(format!("{document}.md"))
                    .is_file())
        {
            return Err(shared::Error::new(
                ErrorKind::NotFound,
                format!("{id} has no document {document:?}"),
            ));
        }
        if content.chars().count() > shared::MAX_DOCUMENT_CHARS {
            return Err(shared::Error::new(
                ErrorKind::Other,
                format!(
                    "a document of {id} holds at most {} characters",
                    shared::MAX_DOCUMENT_CHARS
                ),
            ));
        }
        let number =
            MarkdownTracker::publish_document(self, id, document, title, content).map_err(other)?;
        Ok(shared::Document {
            url: Some(Self::document_path(id, &number)),
            id: number,
        })
    }

    /// Every ticket of `tickets/` not at the stage `Done` whose title is
    /// `title` once both are lowercased, or whose description contains
    /// `marker`, in folder order. Unicode's lowercasing, wider than the
    /// ASCII of the fingerprint's normalization, is as close as a file
    /// comes to Linear's `eqIgnoreCase`. A declined proposal is at another
    /// stage, so it still stops a duplicate. `team` is ignored, as in the
    /// listing; a ticket that cannot be read fails the search.
    fn proposed(
        &self,
        _team: Option<&str>,
        title: &str,
        marker: &str,
    ) -> Result<Vec<shared::Found>, shared::Error> {
        if title.trim().is_empty() || marker.trim().is_empty() {
            return Err(shared::Error::new(
                ErrorKind::Other,
                "a proposal is searched by a title and a marker that are not blank",
            ));
        }
        let title = title.to_lowercase();
        let mut found = Vec::new();
        for id in self.ticket_ids().map_err(other)? {
            let ticket = MarkdownTracker::ticket(self, &id).map_err(other)?;
            if ticket.stage == Self::DONE {
                continue;
            }
            if ticket.title.to_lowercase() == title || ticket.description.contains(marker) {
                found.push(shared::Found {
                    id,
                    description: ticket.description,
                });
            }
        }
        Ok(found)
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
        Capability::ProposalsInbox,
        Capability::Documents,
    ];

    /// The author of the comments the runner posts, and of the proposals it
    /// files. A Markdown ticket's admission gesture being its author's, a
    /// proposal is admitted only once a person has made it theirs (OWL-267).
    pub const AGENT: &'static str = "owlshift";

    /// The stage a proposal is filed at: the proposals inbox, a name fixed
    /// as [`Self::DONE`] is (OWL-267).
    pub const TRIAGE: &'static str = "Triage";

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

    /// The tickets `blocked_by` names, each once: one at the stage `Done` is
    /// completed; any other stage has no type, and neither has a ticket the
    /// folder lacks, so both keep the ticket waiting.
    fn blockers(&self, ticket: &Ticket) -> Result<Vec<shared::Blocker>, shared::Error> {
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
        Ok(blockers)
    }

    /// As [`Self::require_ticket`], but [`ErrorKind::NotFound`] only when
    /// the file system answers that the ticket file is not there.
    fn require_ticket_file(&self, id: &TicketId) -> Result<(), shared::Error> {
        let path = self.ticket_file(id);
        match fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => Ok(()),
            Ok(_) => Err(other(error(&path, "not a ticket file"))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Err(shared::Error::new(
                ErrorKind::NotFound,
                format!("{}: no such ticket", path.display()),
            )),
            Err(e) => Err(other(error(&path, e))),
        }
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

/// A ticket file as a linked ticket (OWL-416): its title, and its stage,
/// typed completed at `Done`, as [`MarkdownTracker::state_type`] has it.
fn linked(ticket: Ticket) -> shared::Linked {
    let kind = (ticket.stage == MarkdownTracker::DONE).then_some(shared::StateType::Completed);
    shared::Linked {
        id: ticket.id,
        title: Some(ticket.title),
        state: Some(shared::LinkedState {
            name: ticket.stage,
            kind,
        }),
    }
}

/// Adds `linked` to `list` unless it lists that ticket already.
fn add_linked(list: &mut Vec<shared::Linked>, linked: shared::Linked) {
    if !list.iter().any(|known| known.id == linked.id) {
        list.push(linked);
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
/// plan, "The test tracker"), so the last edit is the creation time; the
/// file format has no reactions (OWL-371).
fn shared_comment(comment: Comment) -> shared::Comment {
    shared::Comment {
        id: comment.id,
        author: account(comment.author),
        created_at: comment.at,
        edited_at: None,
        body: comment.body,
        parent: comment.reply_to,
        reactions: Vec::new(),
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
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
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
    #[serde(default)]
    related: Vec<TicketId>,
}

fn no_priority() -> Priority {
    Priority::Unset
}

/// How many numbers a filing tries past the highest one before giving up
/// ([`MarkdownTracker::propose`]).
const CLAIM_TRIES: u32 = 64;

/// A list of ticket identifiers as a TOML value.
fn ids_value(ids: &[TicketId]) -> toml::Value {
    toml::Value::Array(
        ids.iter()
            .map(|id| toml::Value::String(id.as_str().to_owned()))
            .collect(),
    )
}

/// A front matter's text, one `key = value` line per key in the format's
/// order, a key at its default left out.
fn render_front(front: &FrontMatter) -> Result<String, String> {
    let string = |s: &str| toml::Value::String(s.to_owned());
    let mut lines = vec![
        ("title", string(&front.title)),
        ("author", string(&front.author)),
        ("stage", string(&front.stage)),
    ];
    if front.priority != Priority::Unset {
        let priority = toml::Value::try_from(front.priority).map_err(|e| e.to_string())?;
        lines.push(("priority", priority));
    }
    if let Some(assignee) = &front.assignee {
        lines.push(("assignee", string(assignee)));
    }
    if !front.labels.is_empty() {
        let labels = front.labels.iter().map(|label| string(label)).collect();
        lines.push(("labels", toml::Value::Array(labels)));
    }
    if !front.blocked_by.is_empty() {
        lines.push(("blocked_by", ids_value(&front.blocked_by)));
    }
    if !front.related.is_empty() {
        lines.push(("related", ids_value(&front.related)));
    }
    Ok(lines
        .into_iter()
        .map(|(key, value)| format!("{key} = {value}\n"))
        .collect())
}

/// The key and the number of an identifier written `<key>-<digits>`; `None`
/// for any other, and for a number past `u64`. Leading zeros read as the
/// number: `DEMO-007` is 7.
fn numbered(id: &TicketId) -> Option<(&str, u64)> {
    let (key, digits) = id.as_str().rsplit_once('-')?;
    if key.is_empty() || digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((key, digits.parse().ok()?))
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
                related: Vec::new(),
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

    /// Intake's links (OWL-416): `blocked_by` and `related` from the front
    /// matter, a missing ticket without title or state; the tickets whose
    /// `blocked_by` names it as `blocks`, whose `related` names it as
    /// `related`, each once; a ticket of the scan that cannot be read left
    /// out.
    #[test]
    fn links_read_the_front_matter_and_the_other_tickets() {
        use shared::{Linked, LinkedState, StateType, Tracker as _};
        let related = |stage: &str, blocked_by: &str, related: &str| {
            format!(
                "+++\ntitle = \"t\"\nauthor = \"a\"\nstage = \"{stage}\"\n\
                 blocked_by = [{blocked_by}]\nrelated = [{related}]\n+++\n"
            )
        };
        let (root, tracker, _) =
            repository(&related("Todo", "\"DEMO-2\", \"DEMO-9\"", "\"DEMO-3\""));
        let tickets = root.path().join("tickets");
        for (id, text) in [
            ("DEMO-2", ticket_md("a", "Done", &[], &[])),
            ("DEMO-3", ticket_md("a", "Todo", &[], &[])),
            ("DEMO-4", ticket_md("a", "Todo", &[], &["DEMO-1"])),
            ("DEMO-5", related("Doing", "", "\"DEMO-1\", \"DEMO-3\"")),
            ("DEMO-6", "not a ticket".to_owned()),
        ] {
            fs::create_dir_all(tickets.join(id)).unwrap();
            fs::write(tickets.join(id).join("ticket.md"), text).unwrap();
        }

        let linked = |id: &str, stage: &str, kind| Linked {
            id: TicketId::new(id).unwrap(),
            title: Some("t".to_owned()),
            state: Some(LinkedState {
                name: stage.to_owned(),
                kind,
            }),
        };
        let links = tracker.links(&id()).unwrap();
        assert_eq!(
            links.blocked_by,
            [
                linked("DEMO-2", "Done", Some(StateType::Completed)),
                Linked {
                    id: TicketId::new("DEMO-9").unwrap(),
                    title: None,
                    state: None,
                },
            ]
        );
        assert_eq!(links.blocks, [linked("DEMO-4", "Todo", None)]);
        assert_eq!(
            links.related,
            [
                linked("DEMO-3", "Todo", None),
                linked("DEMO-5", "Doing", None)
            ]
        );
        assert_eq!(
            tracker
                .links(&TicketId::new("DEMO-7").unwrap())
                .unwrap_err()
                .kind,
            ErrorKind::NotFound
        );
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
        // The tickets of a stage, the working state's (OWL-410).
        let canceled = tracker.working(None, "Canceled").unwrap();
        let ids: Vec<&str> = canceled.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["DEMO-4"]);
        assert_eq!(canceled[0].labels, ["agent"]);
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

    /// A relation (OWL-249) is one more `blocked_by` entry: that line alone
    /// changes, line ending kept, or a line is added before the closing
    /// `+++`; an entry already there changes nothing; a key written over
    /// several lines, and the ticket itself, are refused, the file left as
    /// it was.
    #[test]
    fn a_blocker_is_added_to_blocked_by_and_nothing_else() {
        let (_root, tracker, file) = repository(TICKET);
        let demo = |n: u32| TicketId::new(format!("DEMO-{n}")).unwrap();
        tracker.add_blocked_by(&id(), &demo(2)).unwrap();
        let after = fs::read_to_string(&file).unwrap();
        let changed: Vec<_> = TICKET
            .split_inclusive('\n')
            .zip(after.split_inclusive('\n'))
            .filter(|(before, after)| before != after)
            .collect();
        assert_eq!(
            changed,
            [(
                "blocked_by = [\"DEMO-0\"]\r\n",
                "blocked_by = [\"DEMO-0\", \"DEMO-2\"]\r\n"
            )]
        );
        tracker.add_blocked_by(&id(), &demo(2)).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), after);
        assert!(tracker.add_blocked_by(&id(), &id()).is_err());

        let none = "+++\ntitle = \"t\"\nauthor = \"a\"\nstage = \"Todo\"\n+++\n\nText.\n";
        let (_root, tracker, file) = repository(none);
        tracker.add_blocked_by(&id(), &demo(2)).unwrap();
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "+++\ntitle = \"t\"\nauthor = \"a\"\nstage = \"Todo\"\nblocked_by = [\"DEMO-2\"]\n\
             +++\n\nText.\n"
        );

        let multiline = "+++\ntitle = \"t\"\nauthor = \"a\"\nstage = \"Todo\"\n\
                         blocked_by = [\n  \"DEMO-0\",\n]\n+++\n";
        let (_root, tracker, file) = repository(multiline);
        assert!(tracker.add_blocked_by(&id(), &demo(2)).is_err());
        assert_eq!(fs::read_to_string(&file).unwrap(), multiline);
    }

    /// The dependency check's read (OWL-249): `Done` alone is completed,
    /// the blockers come with their states; only a missing ticket file is
    /// not found, a path that is not a file is another error.
    #[test]
    fn a_tickets_relations_tell_a_missing_ticket_from_an_unreadable_one() {
        use crate::tracker::{StateType, Tracker as _};

        let (root, tracker, _file) = repository(&ticket_md("a", "Done", &[], &["DEMO-2"]));
        let relations = tracker.relations(&id()).unwrap();
        assert_eq!(relations.state, Some(StateType::Completed));
        assert_eq!(relations.blockers.len(), 1);
        assert_eq!(relations.blockers[0].state, None);

        let missing = TicketId::new("DEMO-9").unwrap();
        assert_eq!(
            tracker.relations(&missing).unwrap_err().kind,
            ErrorKind::NotFound
        );
        let odd = TicketId::new("DEMO-8").unwrap();
        fs::create_dir_all(root.path().join("tickets/DEMO-8/ticket.md")).unwrap();
        assert_eq!(tracker.relations(&odd).unwrap_err().kind, ErrorKind::Other);
        assert_eq!(
            tracker.block(&id(), &missing).unwrap_err().kind,
            ErrorKind::NotFound
        );
    }

    /// A proposal as the runner hands it over.
    fn proposal<'a>(
        parent: &'a TicketId,
        title: &'a str,
        description: &'a str,
        blocked_by_parent: bool,
        ready: &'a str,
    ) -> shared::Proposal<'a> {
        shared::Proposal {
            parent,
            title,
            description,
            blocked_by_parent,
            ready,
        }
    }

    const FOOTER: &str = "[owlshift] PROPOSAL · DEMO-1 · fingerprint 0123456789ab\n";

    /// A proposal (OWL-267) is a new folder one past the parent's key's
    /// highest number, other keys and unnumbered folders aside, written at
    /// `Triage` by `owlshift`, related to its parent and blocked by it only
    /// when asked; the two searches find it until it is done.
    #[test]
    fn a_proposal_is_filed_after_the_highest_number_and_found_again() {
        use shared::Tracker as _;
        let (root, tracker, _) = repository(TICKET);
        let tickets = root.path().join("tickets");
        for (id, stage) in [
            ("DEMO-07", "Todo"),
            ("OTHER-40", "Todo"),
            ("DEMO-x", "Done"),
        ] {
            fs::create_dir_all(tickets.join(id)).unwrap();
            fs::write(
                tickets.join(id).join("ticket.md"),
                ticket_md("a", stage, &[], &[]),
            )
            .unwrap();
        }
        let description = format!("**Why:** the docs drift.\n\n{FOOTER}");
        let filed = shared::Tracker::propose(
            &tracker,
            &proposal(&id(), "Lint the docs", &description, false, "Todo"),
        )
        .unwrap();
        let first = TicketId::new("DEMO-8").unwrap();
        assert_eq!(
            filed,
            shared::Filed {
                id: first.clone(),
                state: "Triage".to_owned(),
                problems: Vec::new(),
            }
        );
        assert_eq!(
            fs::read_to_string(tickets.join("DEMO-8/ticket.md")).unwrap(),
            format!(
                "+++\ntitle = \"Lint the docs\"\nauthor = \"owlshift\"\nstage = \"Triage\"\n\
                 related = [\"DEMO-1\"]\n+++\n\n{description}"
            )
        );

        let blocked = shared::Tracker::propose(
            &tracker,
            &proposal(&id(), "Ship \"it\"", "\n\nText.\n", true, "Todo"),
        )
        .unwrap();
        assert_eq!(blocked.id.as_str(), "DEMO-9");
        let read = tracker.ticket(&blocked.id).unwrap();
        assert_eq!(read.title, "Ship \"it\"");
        assert_eq!(
            (read.author.as_str(), read.stage.as_str()),
            ("owlshift", "Triage")
        );
        assert_eq!((read.blocked_by, read.related), (vec![id()], vec![id()]));
        assert_eq!(read.description, "Text.\n");
        assert_eq!((read.priority, read.assignee), (Priority::Unset, None));
        assert!(read.labels.is_empty());

        let ids = |found: Vec<shared::Found>| -> Vec<String> {
            found
                .into_iter()
                .map(|f| f.id.as_str().to_owned())
                .collect()
        };
        let marker = "fingerprint 0123456789ab";
        let both = tracker.proposed(None, "LINT THE DOCS", marker).unwrap();
        assert_eq!(both[0].description, description);
        assert_eq!(ids(both), ["DEMO-8"]);
        assert_eq!(
            ids(tracker.proposed(None, "ship \"IT\"", "none").unwrap()),
            ["DEMO-9"]
        );
        assert_eq!(
            ids(tracker.proposed(Some("X"), "Other", marker).unwrap()),
            ["DEMO-8"]
        );
        tracker.set_stage(&first, "Canceled").unwrap();
        assert_eq!(
            ids(tracker.proposed(None, "Other", marker).unwrap()),
            ["DEMO-8"]
        );
        tracker.set_stage(&first, "Done").unwrap();
        assert!(
            tracker
                .proposed(None, "Lint the docs", marker)
                .unwrap()
                .is_empty()
        );
        for (title, marker) in [("", marker), ("Lint the docs", " ")] {
            assert!(tracker.proposed(None, title, marker).is_err());
        }
        fs::write(tickets.join("DEMO-07/ticket.md"), "no front matter").unwrap();
        let error = tracker.proposed(None, "Lint the docs", marker).unwrap_err();
        assert!(error.message.contains("DEMO-07"), "{error}");
    }

    /// Each refusal comes before anything is written: a ready state named
    /// as the inbox, a parent that is not there, a title blank or on
    /// several lines, a parent whose identifier ends in no number.
    #[test]
    fn a_refused_proposal_writes_nothing() {
        let (root, tracker, _) = repository(TICKET);
        let tickets = root.path().join("tickets");
        fs::create_dir_all(tickets.join("DEMO")).unwrap();
        fs::write(
            tickets.join("DEMO/ticket.md"),
            ticket_md("a", "Todo", &[], &[]),
        )
        .unwrap();
        let listing = || {
            let mut names: Vec<_> = fs::read_dir(&tickets)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            names.sort();
            names
        };
        let before = listing();
        let missing = TicketId::new("DEMO-9").unwrap();
        let unnumbered = TicketId::new("DEMO").unwrap();
        for (parent, title, ready, kind) in [
            (&id(), "Lint", "Triage", ErrorKind::Other),
            (&missing, "Lint", "Todo", ErrorKind::NotFound),
            (&id(), " ", "Todo", ErrorKind::Other),
            (&id(), "Lint\nthe docs", "Todo", ErrorKind::Other),
            (&unnumbered, "Lint", "Todo", ErrorKind::Other),
        ] {
            let error =
                shared::Tracker::propose(&tracker, &proposal(parent, title, FOOTER, false, ready))
                    .unwrap_err();
            assert_eq!(error.kind, kind, "{title:?}: {error}");
        }
        assert_eq!(listing(), before);
    }

    /// A folder taken meanwhile is passed over, never written into; past
    /// the tries allowed, the filing fails.
    #[test]
    fn a_taken_number_is_passed_over_within_the_tries() {
        let (root, tracker, _) = repository(TICKET);
        let taken = root.path().join("tickets/DEMO-2");
        fs::create_dir_all(&taken).unwrap();
        fs::write(taken.join("keep.md"), "mine").unwrap();
        let (id, dir) = tracker.claim("DEMO", 1, 2).unwrap();
        assert_eq!((id.as_str(), dir.is_dir()), ("DEMO-3", true));
        assert_eq!(fs::read_to_string(taken.join("keep.md")).unwrap(), "mine");
        let error = tracker.claim("DEMO", 1, 2).unwrap_err();
        assert!(error.reason.contains("no free identifier"), "{error}");
    }

    #[test]
    fn a_number_is_read_after_the_last_dash() {
        for (id, read) in [
            ("DEMO-12", Some(("DEMO", 12))),
            ("DEMO-007", Some(("DEMO", 7))),
            ("A-B-3", Some(("A-B", 3))),
            ("DEMO", None),
            ("DEMO-", None),
            ("DEMO-1a", None),
            ("DEMO-99999999999999999999", None),
        ] {
            assert_eq!(numbered(&TicketId::new(id).unwrap()), read, "{id}");
        }
    }

    /// A ticket's first document is `documents/1.md`, its title on a first
    /// line; an update writes that file over and a second document is
    /// `2.md`. The comments ignore the folder. An unknown document, a number
    /// that is not one, and a ticket that is not there are not found
    /// (OWL-350).
    #[test]
    fn a_document_is_a_file_of_the_ticket_written_over_in_place() {
        let (root, tracker, _) = repository(TICKET);
        let publish = |id: &TicketId, document, title, content| {
            shared::Tracker::publish_document(&tracker, id, document, title, content)
        };
        let made = publish(&id(), None, "Plan ·\nDEMO-1", "v1").unwrap();
        assert_eq!(made.id, "1");
        assert_eq!(made.url.as_deref(), Some("tickets/DEMO-1/documents/1.md"));
        let file = root.path().join(made.url.as_deref().unwrap());
        assert_eq!(fs::read_to_string(&file).unwrap(), "# Plan · DEMO-1\n\nv1");
        let updated = publish(&id(), Some("1"), "Plan · DEMO-1", "v2").unwrap();
        assert_eq!(updated, made);
        assert_eq!(fs::read_to_string(&file).unwrap(), "# Plan · DEMO-1\n\nv2");
        let second = publish(&id(), None, "Notes", "n").unwrap();
        assert_eq!(second.id, "2");
        assert!(
            shared::Tracker::comments(&tracker, &id())
                .unwrap()
                .is_empty()
        );
        for document in ["3", "../1", "01", ""] {
            let error = publish(&id(), Some(document), "t", "x").unwrap_err();
            assert_eq!(error.kind, ErrorKind::NotFound, "{document:?}");
        }
        let error = publish(&TicketId::new("DEMO-9").unwrap(), None, "t", "x").unwrap_err();
        assert_eq!(error.kind, ErrorKind::NotFound);
    }
}
