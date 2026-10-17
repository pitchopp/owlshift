//! The project's rules, injected into the brief (OWL-61, OWL-67, OWL-312,
//! build plan "The project's rules" and "Rules per role").
//!
//! The rules are the files the project names in `stack.rules`, by default
//! the `AGENTS.md` at the root of the repository, which every brief takes,
//! and the files `[roles.<role>]` names, which that role's brief takes after
//! them. All are read at the base commit in the dedicated checkout, never in
//! a worktree: the ticket's branch is written by agents, and the build role
//! takes `rules` as instructions. A rule is never cut: a file the runner
//! cannot take whole refuses the run.

use std::collections::BTreeMap;
use std::path::Path;

use owlshift_contracts::brief::Rule;
use owlshift_contracts::config::{ProjectConfig, RULE_ROLES};
use owlshift_core::vocab::Role;

use crate::executor::Git;
use crate::project::Base;

/// The most bytes the rules of one brief take, all its files together.
const MAX_RULE_BYTES: u64 = 64 * 1024;

/// What the rules are, in a refusal.
const RULES: &str = "the project's rules";

/// The rules a project gives its briefs: those of every brief, and those of
/// one role's alone (OWL-312); and its gate, `stack.gate`, from the same
/// project file, so that a run's rules and gate always come from one commit
/// (OWL-331).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectRules {
    /// The rules of `stack.rules`, in order.
    common: Vec<Rule>,
    /// The rules of `[roles.<role>]`, in order, for each role that has any.
    roles: BTreeMap<Role, Vec<Rule>>,
    /// The commands of `stack.gate`, in order.
    gate: Vec<String>,
}

impl ProjectRules {
    /// No rule and no gate: for the brief of a role told neither, such as
    /// the answer check (architecture section 9), whose run never runs a
    /// gate. Never for a Build or a rebase, where an empty gate passes.
    pub fn none() -> Self {
        Self {
            common: Vec::new(),
            roles: BTreeMap::new(),
            gate: Vec::new(),
        }
    }

    /// The project's gate: `stack.gate` of the project file the rules were
    /// listed from.
    pub fn gate(&self) -> &[String] {
        &self.gate
    }

    /// The rules of `stack.rules`, which every brief holds.
    pub fn common(&self) -> &[Rule] {
        &self.common
    }

    /// The rules of the brief of `role`: those of `stack.rules`, then the
    /// role's own.
    pub fn for_role(&self, role: Role) -> Vec<Rule> {
        let mut rules = self.common.clone();
        if let Some(own) = self.roles.get(&role) {
            rules.extend(own.iter().cloned());
        }
        rules
    }

    /// Every source read, each once: `stack.rules` first, then each role's, in
    /// the order of the roles.
    pub fn sources(&self) -> Vec<&str> {
        let mut sources: Vec<&str> = Vec::new();
        for rule in self.common.iter().chain(self.roles.values().flatten()) {
            if !sources.contains(&rule.source.as_str()) {
                sources.push(&rule.source);
            }
        }
        sources
    }
}

/// A rule file listed at the base commit, not yet read.
struct Listed<'a> {
    file: &'a str,
    entry: Entry,
}

/// The project's rules at `base`, as the dedicated `checkout` knows it after
/// a fetch: one rule for the whole repository (`applies_to` empty) per file
/// `stack.rules` names, in order, or, when it names none, per root
/// `AGENTS.md` the base commit has; and, for each role of
/// [`RULE_ROLES`], one per file `[roles.<role>]` names. Only the roles in `briefed` are read, so that a bad table of another role refuses nothing.
///
/// The rules are read at `base.commit`, which
/// [`sync_checkout`](crate::project::sync_checkout) resolved once through
/// the base's full name, so a local branch named like it
/// (`refs/heads/origin/main`) cannot stand in for it. A blank file gives no
/// rule. A named file the base commit lacks, an entry that is not a regular
/// file, files over 64 KiB together in one brief (`stack.rules` and the
/// role's), a file not in UTF-8, and any git failure are errors: a reason
/// that names the file. Every file is listed and measured before any is
/// read. The gate is `config`'s `stack.gate`: `config` is the project file
/// at `base`, or the person's own when `base` has none (OWL-331).
pub fn project_rules(
    git: &Git,
    checkout: &Path,
    base: &Base,
    config: &ProjectConfig,
    briefed: &[Role],
) -> Result<ProjectRules, String> {
    let files = config.stack.rule_files();
    let required = config.stack.rules.is_some();
    let common = list(git, checkout, base, files, "`stack.rules`", required)?;
    check_total(base, &common, &[], None)?;
    let mut own = Vec::new();
    for role in RULE_ROLES.into_iter().filter(|role| briefed.contains(role)) {
        let named: Vec<&str> = config
            .role_rule_files(role)
            .iter()
            .map(String::as_str)
            .collect();
        if named.is_empty() {
            continue;
        }
        let key = format!("`roles.{}.rules`", role.as_str());
        let listed = list(git, checkout, base, named, &key, true)?;
        check_total(base, &common, &listed, Some(role))?;
        own.push((role, listed));
    }
    let mut rules = ProjectRules {
        common: read_all(git, checkout, base, &common)?,
        roles: BTreeMap::new(),
        gate: config.stack.gate.clone(),
    };
    for (role, listed) in own {
        let read = read_all(git, checkout, base, &listed)?;
        if !read.is_empty() {
            rules.roles.insert(role, read);
        }
    }
    Ok(rules)
}

/// The entries of `files` at the base commit. `key` names the list in a
/// refusal; a file the commit lacks is one only when the list `required`
/// them (a default file is optional).
fn list<'a>(
    git: &Git,
    checkout: &Path,
    base: &Base,
    files: Vec<&'a str>,
    key: &str,
    required: bool,
) -> Result<Vec<Listed<'a>>, String> {
    let mut listed = Vec::with_capacity(files.len());
    for file in files {
        let at = format!("{file} on {}", base.remote_ref);
        match entry(git, checkout, base, file, &at, RULES)? {
            Some(entry) => listed.push(Listed { file, entry }),
            None if required => {
                return Err(format!(
                    "{at} does not exist: {key} in owlshift.toml names it, and a named rule \
                     file is never skipped"
                ));
            }
            None => {}
        }
    }
    Ok(listed)
}

/// Refuses the files of one brief (`common` then `own`, for `role`'s brief,
/// or `common` alone) when together they hold over 64 KiB, naming each file
/// and its size.
fn check_total(
    base: &Base,
    common: &[Listed<'_>],
    own: &[Listed<'_>],
    role: Option<Role>,
) -> Result<(), String> {
    let all: Vec<&Listed<'_>> = common.iter().chain(own).collect();
    let total: u64 = all.iter().map(|listed| listed.entry.size).sum();
    if total <= MAX_RULE_BYTES {
        return Ok(());
    }
    let at = &base.remote_ref;
    Err(match (&all[..], role) {
        ([one], None) => format!(
            "{} on {at} holds {total} bytes, over the {MAX_RULE_BYTES} Owlshift passes to an \
             agent as rules; a rule is never cut, so shorten the file",
            one.file
        ),
        _ => {
            let sizes: Vec<String> = all
                .iter()
                .map(|listed| format!("{} {}", listed.file, listed.entry.size))
                .collect();
            let brief = role.map_or(String::new(), |role| {
                format!(" of the `{}` brief", role.as_str())
            });
            format!(
                "the rule files{brief} on {at} hold {total} bytes together ({}), over the \
                 {MAX_RULE_BYTES} Owlshift passes to an agent as rules; a rule is never cut, so \
                 name fewer files or shorten them",
                sizes.join(", ")
            )
        }
    })
}

/// The rules of `listed`, read in order; a blank file gives none.
fn read_all(
    git: &Git,
    checkout: &Path,
    base: &Base,
    listed: &[Listed<'_>],
) -> Result<Vec<Rule>, String> {
    let mut rules = Vec::with_capacity(listed.len());
    for Listed { file, entry } in listed {
        let at = format!("{file} on {}", base.remote_ref);
        let text = read_blob(git, checkout, entry, &at, RULES)?;
        if text.trim().is_empty() {
            continue;
        }
        rules.push(Rule {
            applies_to: Vec::new(),
            source: (*file).to_owned(),
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
