//! The Markdown tracker's format page (`docs/markdown-tracker.md`) is read by
//! the tracker itself, so that it cannot drift from the code (OWL-34).
//!
//! An example of the page is a fenced block whose info string is
//! `markdown <kind> <path>`: its kind says what it shows, its path where it
//! sits in a repository. A block of any other language is prose and is left
//! alone; a `markdown` block of an unknown kind fails the test, so a block
//! the test does not read cannot slip in.

use std::fs;
use std::path::Path;

use owlshift_adapters::tracker::markdown::MarkdownTracker;
use owlshift_contracts::Priority;
use owlshift_contracts::ids::TicketId;

const PAGE: &str = include_str!("../../../docs/markdown-tracker.md");

struct Example {
    kind: String,
    path: String,
    text: String,
}

fn id(id: &str) -> TicketId {
    TicketId::new(id).unwrap()
}

/// The page's `markdown` blocks, in order.
fn examples() -> Vec<Example> {
    let mut found = Vec::new();
    let mut open: Option<Option<Example>> = None;
    for line in PAGE.lines() {
        match (&mut open, line.strip_prefix("```")) {
            (None, Some(info)) => {
                let mut words = info.split_whitespace();
                open = Some(match words.next() {
                    Some("markdown") => {
                        let kind = words.next().expect("a markdown block names its kind");
                        let path = words.next().expect("a markdown block names its path");
                        assert!(words.next().is_none(), "extra words after {path}");
                        Some(Example {
                            kind: kind.to_owned(),
                            path: path.to_owned(),
                            text: String::new(),
                        })
                    }
                    _ => None,
                });
            }
            (Some(_), Some("")) => {
                if let Some(Some(example)) = open.take() {
                    found.push(example);
                }
            }
            (Some(Some(example)), _) => {
                example.text.push_str(line);
                example.text.push('\n');
            }
            _ => {}
        }
    }
    assert!(open.is_none(), "a fenced block is not closed");
    for example in &found {
        assert!(
            ["ticket", "comment", "reply", "document", "proposal"].contains(&example.kind.as_str()),
            "unknown example kind {:?} at {}",
            example.kind,
            example.path
        );
    }
    found
}

fn example<'a>(all: &'a [Example], kind: &str) -> Vec<&'a Example> {
    all.iter().filter(|e| e.kind == kind).collect()
}

fn lay_out(root: &Path, example: &Example) {
    let path = root.join(&example.path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, &example.text).unwrap();
}

/// A repository holding the page's ticket, comment and reply examples.
fn repository(all: &[Example]) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    for e in all
        .iter()
        .filter(|e| ["ticket", "comment", "reply"].contains(&e.kind.as_str()))
    {
        lay_out(root.path(), e);
    }
    root
}

#[test]
fn the_page_has_an_example_of_each_kind() {
    let all = examples();
    for kind in ["ticket", "comment", "reply", "document", "proposal"] {
        assert!(!example(&all, kind).is_empty(), "no {kind} example");
    }
}

#[test]
fn the_ticket_example_reads_as_the_page_says() {
    let all = examples();
    let root = repository(&all);
    let ticket = MarkdownTracker::new(root.path())
        .ticket(&id("DEMO-1"))
        .unwrap();
    assert_eq!(ticket.title, "Add a greeting");
    assert_eq!(ticket.author, "reporter");
    assert_eq!(ticket.stage, "Todo");
    assert_eq!(ticket.priority, Priority::High);
    assert_eq!(ticket.assignee.as_deref(), Some("maintainer"));
    assert_eq!(ticket.labels, ["agent", "docs"]);
    assert_eq!(ticket.blocked_by, [id("DEMO-0")]);
    assert_eq!(ticket.related, [id("DEMO-0")]);
    assert_eq!(
        ticket.description,
        "Print `hello` when the program starts.\n"
    );
}

#[test]
fn the_comment_and_reply_examples_read_as_the_page_says() {
    let all = examples();
    let root = repository(&all);
    let comments = MarkdownTracker::new(root.path())
        .comments(&id("DEMO-1"))
        .unwrap();
    assert_eq!(comments.len(), 2);
    let (first, second) = (&comments[0], &comments[1]);
    assert_eq!(first.id, "20261003T101500Z-owlshift");
    assert_eq!(first.author, "owlshift");
    assert_eq!(first.reply_to, None);
    assert_eq!(
        first.body,
        "Which greeting do you want: `hello` or `hello, world`?\n"
    );
    // The n-th comment of a second reads n - 1 microseconds after it.
    assert_eq!(second.id, "20261003T101500Z.2-maintainer");
    assert_eq!(second.author, "maintainer");
    assert_eq!(second.reply_to.as_deref(), Some(first.id.as_str()));
    assert_eq!(second.body, "`hello, world`, please.\n");
    assert_eq!(second.at.as_microsecond() - first.at.as_microsecond(), 1);
    assert_eq!(first.at.to_string(), "2026-10-03T10:15:00Z");
}

#[test]
fn the_document_and_proposal_examples_are_what_the_tracker_writes() {
    let all = examples();
    let root = repository(&all);
    let tracker = MarkdownTracker::new(root.path());

    let document = example(&all, "document")[0];
    let number = tracker
        .publish_document(
            &id("DEMO-1"),
            None,
            "Plan",
            "1. Add the greeting.\n2. Test it.\n",
        )
        .unwrap();
    assert_eq!(number, "1");
    assert_eq!(
        MarkdownTracker::document_path(&id("DEMO-1"), &number),
        document.path
    );
    assert_eq!(
        fs::read_to_string(root.path().join(&document.path)).unwrap(),
        document.text
    );

    let proposal = example(&all, "proposal")[0];
    let filed = tracker
        .propose(
            &id("DEMO-1"),
            "Greet by name",
            "The greeting could take a name.\n",
            true,
        )
        .unwrap();
    assert_eq!(filed.as_str(), "DEMO-2");
    assert_eq!(proposal.path, "tickets/DEMO-2/ticket.md");
    assert_eq!(
        fs::read_to_string(root.path().join(&proposal.path)).unwrap(),
        proposal.text
    );
    let read = tracker.ticket(&filed).unwrap();
    assert_eq!(read.stage, MarkdownTracker::TRIAGE);
    assert_eq!(read.author, MarkdownTracker::AGENT);
}

#[test]
fn an_unknown_key_is_refused_as_the_page_says() {
    assert!(PAGE.contains("An unknown key is refused"));
    let all = examples();
    let root = repository(&all);
    let path = root.path().join("tickets/DEMO-1/ticket.md");
    let text = fs::read_to_string(&path).unwrap();
    fs::write(
        &path,
        text.replacen("+++\n\n", "colour = \"red\"\n+++\n\n", 1),
    )
    .unwrap();
    let error = MarkdownTracker::new(root.path())
        .ticket(&id("DEMO-1"))
        .unwrap_err();
    assert!(error.to_string().contains("colour"), "{error}");
}

/// The page's "Limits" section, which opens it.
fn limits() -> String {
    let start = PAGE.find("\n## Limits\n").expect("a Limits section");
    let rest = &PAGE[start + 1..];
    let end = rest[3..].find("\n## ").map_or(rest.len(), |at| at + 3);
    rest[..end].to_lowercase()
}

#[test]
fn the_limits_are_written_and_come_first() {
    let limits = limits();
    for phrase in [
        "one machine",
        "`owlshift do ticket`",
        "`owlshift continue ticket`",
        "never commits",
        "not served by the daemon",
        "`owlshift start` refuses",
    ] {
        assert!(limits.contains(phrase), "the Limits section lacks {phrase}");
    }
    let first = PAGE.find("\n## ").expect("a first section");
    assert!(PAGE[first..].starts_with("\n## Limits\n"));
}
