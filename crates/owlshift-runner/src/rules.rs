//! The project's rules, injected into the brief (OWL-61, build plan "The
//! project's rules").
//!
//! The rules are the `AGENTS.md` at the root of the repository, read at the
//! base commit in the dedicated checkout, never in a worktree: the ticket's
//! branch is written by agents, and the build role takes `rules` as
//! instructions. A rule is never cut: a file the runner cannot take whole
//! refuses the run.

use std::path::Path;

use owlshift_contracts::brief::Rule;

use crate::executor::Git;
use crate::project::Base;

/// The file the rules come from, relative to the repository's root.
const RULE_FILE: &str = "AGENTS.md";

/// The largest rule file taken, in bytes.
const MAX_RULE_BYTES: u64 = 64 * 1024;

/// The project's rules at `base`, as the dedicated `checkout` knows it after
/// a fetch: one rule for the whole repository (`applies_to` empty) when the
/// base commit has a root `AGENTS.md`, none when it has not.
///
/// The base is resolved once, through its full name, so a local branch named
/// like it (`refs/heads/origin/main`) cannot stand in for it. An entry that
/// is not a regular file, a file over 64 KiB or not UTF-8, and any git
/// failure are errors: a reason that names the file.
pub fn project_rules(git: &Git, checkout: &Path, base: &Base) -> Result<Vec<Rule>, String> {
    let at = format!("{RULE_FILE} on {}", base.remote_ref);
    let full = format!("refs/remotes/{}^{{commit}}", base.remote_ref);
    let commit = git
        .run(
            checkout,
            &["rev-parse", "--verify", "--quiet", full.as_str()],
        )
        .map_err(|e| format!("reading {at}: the base does not resolve ({})", e.detail))?;
    let commit = String::from_utf8_lossy(&commit).trim().to_owned();
    let listing = git
        .run(
            checkout,
            &[
                "--literal-pathspecs",
                "ls-tree",
                "-z",
                "-l",
                "--full-tree",
                commit.as_str(),
                "--",
                RULE_FILE,
            ],
        )
        .map_err(|e| format!("reading {at}: {}", e.detail))?;
    let Some(entry) = Entry::parse(&listing).map_err(|reason| format!("reading {at}: {reason}"))?
    else {
        return Ok(Vec::new());
    };
    if entry.kind != "blob" || !matches!(entry.mode.as_str(), "100644" | "100755") {
        return Err(format!(
            "{at} is not a regular file (mode {}, {}): Owlshift reads the project's rules \
             from a regular file only",
            entry.mode, entry.kind
        ));
    }
    let size: u64 = entry
        .size
        .parse()
        .map_err(|_| format!("reading {at}: git gave no size for it"))?;
    if size > MAX_RULE_BYTES {
        return Err(format!(
            "{at} holds {size} bytes, over the {MAX_RULE_BYTES} Owlshift passes to an agent \
             as rules; a rule is never cut, so shorten the file"
        ));
    }
    let bytes = git
        .run(checkout, &["cat-file", "blob", entry.oid.as_str()])
        .map_err(|e| format!("reading {at}: {}", e.detail))?;
    let text = String::from_utf8(bytes)
        .map_err(|_| format!("{at} is not UTF-8 text: Owlshift passes rules as text only"))?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    Ok(vec![Rule {
        applies_to: Vec::new(),
        source: RULE_FILE.to_owned(),
        text: text.to_owned(),
    }])
}

/// One entry of `git ls-tree -z -l`: `<mode> <type> <oid> <size>\t<path>\0`,
/// the size padded with spaces, and `-` for anything but a blob.
struct Entry {
    mode: String,
    kind: String,
    oid: String,
    size: String,
}

impl Entry {
    /// The listing's one entry for [`RULE_FILE`], or `None` when it is empty.
    fn parse(listing: &[u8]) -> Result<Option<Self>, String> {
        let listing = std::str::from_utf8(listing).map_err(|_| "git listed a name not in UTF-8")?;
        let mut records = listing.split_terminator('\0');
        let Some(record) = records.next() else {
            return Ok(None);
        };
        let unexpected = || format!("git listed {record:?}, not one entry for {RULE_FILE}");
        if records.next().is_some() {
            return Err(unexpected());
        }
        let (meta, path) = record.split_once('\t').ok_or_else(unexpected)?;
        let fields: Vec<&str> = meta.split_whitespace().collect();
        let [mode, kind, oid, size] = fields[..] else {
            return Err(unexpected());
        };
        if path != RULE_FILE {
            return Err(unexpected());
        }
        Ok(Some(Self {
            mode: mode.to_owned(),
            kind: kind.to_owned(),
            oid: oid.to_owned(),
            size: size.to_owned(),
        }))
    }
}
