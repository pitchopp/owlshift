//! The project's rules, injected into the brief (OWL-61, OWL-67, build plan
//! "The project's rules").
//!
//! The rules are the files the project names in `stack.rules`, by default
//! the `AGENTS.md` at the root of the repository, read at the base commit in
//! the dedicated checkout, never in a worktree: the ticket's branch is
//! written by agents, and the build role takes `rules` as instructions. A
//! rule is never cut: a file the runner cannot take whole refuses the run.

use std::path::Path;

use owlshift_contracts::brief::Rule;
use owlshift_contracts::config::Stack;

use crate::executor::Git;
use crate::project::Base;

/// The file the rules come from when the project names none.
const DEFAULT_RULE_FILE: &str = "AGENTS.md";

/// The most bytes the rules take, all files together.
const MAX_RULE_BYTES: u64 = 64 * 1024;

/// What the rules are, in a refusal.
const RULES: &str = "the project's rules";

/// The project's rules at `base`, as the dedicated `checkout` knows it after
/// a fetch: one rule for the whole repository (`applies_to` empty) per file
/// `stack.rules` names, in order, or, when it names none, per root
/// `AGENTS.md` the base commit has.
///
/// The rules are read at `base.commit`, which
/// [`sync_checkout`](crate::project::sync_checkout) resolved once through
/// the base's full name, so a local branch named like it
/// (`refs/heads/origin/main`) cannot stand in for it. A blank file gives no
/// rule. A named file the base commit lacks, an entry that is not a regular
/// file, files over 64 KiB together, a file not in UTF-8, and any git failure
/// are errors: a reason that names the file.
pub fn project_rules(
    git: &Git,
    checkout: &Path,
    base: &Base,
    stack: &Stack,
) -> Result<Vec<Rule>, String> {
    let (files, required) = match &stack.rules {
        Some(named) => (named.iter().map(String::as_str).collect(), true),
        None => (vec![DEFAULT_RULE_FILE], false),
    };
    // Every entry is listed and checked before any is read, so the limit
    // bounds what is read, not only what is kept.
    let mut entries = Vec::with_capacity(files.len());
    for file in files {
        let at = format!("{file} on {}", base.remote_ref);
        match entry(git, checkout, base, file, &at, RULES)? {
            Some(entry) => entries.push((file, entry)),
            None if required => {
                return Err(format!(
                    "{at} does not exist: `stack.rules` in owlshift.toml names it, and a named \
                     rule file is never skipped"
                ));
            }
            None => {}
        }
    }
    let total: u64 = entries.iter().map(|(_, entry)| entry.size).sum();
    if total > MAX_RULE_BYTES {
        let at = &base.remote_ref;
        return Err(match &entries[..] {
            [(file, _)] => format!(
                "{file} on {at} holds {total} bytes, over the {MAX_RULE_BYTES} Owlshift passes \
                 to an agent as rules; a rule is never cut, so shorten the file"
            ),
            _ => {
                let sizes: Vec<String> = entries
                    .iter()
                    .map(|(file, entry)| format!("{file} {}", entry.size))
                    .collect();
                format!(
                    "the rule files on {at} hold {total} bytes together ({}), over the \
                     {MAX_RULE_BYTES} Owlshift passes to an agent as rules; a rule is never \
                     cut, so name fewer files or shorten them",
                    sizes.join(", ")
                )
            }
        });
    }
    let mut rules = Vec::with_capacity(entries.len());
    for (file, entry) in entries {
        let at = format!("{file} on {}", base.remote_ref);
        let text = read_blob(git, checkout, &entry, &at, RULES)?;
        if text.trim().is_empty() {
            continue;
        }
        rules.push(Rule {
            applies_to: Vec::new(),
            source: file.to_owned(),
            text,
        });
    }
    Ok(rules)
}

/// The text of `file` at `base.commit`, read as the rules are, `None` when
/// the commit has no such file: a regular file of at most `limit` bytes, in
/// UTF-8, its byte-order mark dropped. Anything else is a refusal naming the
/// file and, for a file that is not regular or not text, `what` Owlshift
/// reads from it.
pub(crate) fn text_at_base(
    git: &Git,
    checkout: &Path,
    base: &Base,
    file: &str,
    limit: u64,
    what: &str,
) -> Result<Option<String>, String> {
    let at = format!("{file} on {}", base.remote_ref);
    let Some(entry) = entry(git, checkout, base, file, &at, what)? else {
        return Ok(None);
    };
    if entry.size > limit {
        return Err(format!(
            "{at} holds {} bytes, over the {limit} Owlshift reads",
            entry.size
        ));
    }
    read_blob(git, checkout, &entry, &at, what).map(Some)
}

/// The blob of `entry` as text: UTF-8, a leading byte-order mark dropped,
/// read with no filter, text conversion or hook.
fn read_blob(
    git: &Git,
    checkout: &Path,
    entry: &Entry,
    at: &str,
    what: &str,
) -> Result<String, String> {
    let bytes = git
        .run(checkout, &["cat-file", "blob", entry.oid.as_str()])
        .map_err(|e| format!("reading {at}: {}", e.detail))?;
    let text = String::from_utf8(bytes)
        .map_err(|_| format!("{at} is not UTF-8 text: Owlshift reads {what} as text only"))?;
    Ok(match text.strip_prefix('\u{feff}') {
        Some(rest) => rest.to_owned(),
        None => text,
    })
}

/// The base commit's entry for `file`, `None` when it has none, or a
/// refusal when it is not a regular file. One `ls-tree` per file: given
/// `docs docs/deep`, git lists `docs/deep` alone (build plan, OWL-67).
fn entry(
    git: &Git,
    checkout: &Path,
    base: &Base,
    file: &str,
    at: &str,
    what: &str,
) -> Result<Option<Entry>, String> {
    let listing = git
        .run(
            checkout,
            &[
                "--literal-pathspecs",
                "ls-tree",
                "-z",
                "-l",
                "--full-tree",
                base.commit.as_str(),
                "--",
                file,
            ],
        )
        .map_err(|e| format!("reading {at}: {}", e.detail))?;
    let Some(entry) =
        Entry::parse(&listing, file).map_err(|reason| format!("reading {at}: {reason}"))?
    else {
        return Ok(None);
    };
    if entry.kind != "blob" || !matches!(entry.mode.as_str(), "100644" | "100755") {
        return Err(format!(
            "{at} is not a regular file (mode {}, {}): Owlshift reads {what} from a regular file \
             only",
            entry.mode, entry.kind
        ));
    }
    Ok(Some(entry))
}

/// One entry of `git ls-tree -z -l`: `<mode> <type> <oid> <size>\t<path>\0`,
/// the size padded with spaces, and `-` for anything but a blob.
struct Entry {
    mode: String,
    kind: String,
    oid: String,
    /// The blob's size; 0 until the entry is known to be a blob.
    size: u64,
}

impl Entry {
    /// The listing's one entry for `file`, or `None` when it is empty.
    fn parse(listing: &[u8], file: &str) -> Result<Option<Self>, String> {
        let listing = std::str::from_utf8(listing).map_err(|_| "git listed a name not in UTF-8")?;
        let mut records = listing.split_terminator('\0');
        let Some(record) = records.next() else {
            return Ok(None);
        };
        let unexpected = || format!("git listed {record:?}, not one entry for {file}");
        if records.next().is_some() {
            return Err(unexpected());
        }
        let (meta, path) = record.split_once('\t').ok_or_else(unexpected)?;
        let fields: Vec<&str> = meta.split_whitespace().collect();
        let [mode, kind, oid, size] = fields[..] else {
            return Err(unexpected());
        };
        if path != file {
            return Err(unexpected());
        }
        let size = match kind {
            "blob" => size
                .parse()
                .map_err(|_| "git gave no size for it".to_owned())?,
            _ => 0,
        };
        Ok(Some(Self {
            mode: mode.to_owned(),
            kind: kind.to_owned(),
            oid: oid.to_owned(),
            size,
        }))
    }
}
