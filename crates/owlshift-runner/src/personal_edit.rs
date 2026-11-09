//! The personal file edited from the web UI's Config screen (OWL-473,
//! architecture section 5, "The local web UI", "Configuration"): one key at
//! a time, the file's comments and order kept (`toml_edit`), checked as the
//! loader checks it at loading before anything is written.
//!
//! - *The table* ([`KEYS`]): every key the file takes, with its kind, which
//!   gives the page its control and the server the TOML type it writes, and
//!   when an edit of it applies. A key applies at the *next pass* when no
//!   process a running service keeps (the daemon, a served watch, the UI)
//!   holds it from its start: each step of a served watch runs in a child
//!   that loads the personal file again (`owlshift continue --step
//!   --served`, through `served::effective`), so the next step reads it. It
//!   applies at the *next start* of the service when one of them holds it,
//!   even when a step child reads it again too, since its whole effect waits
//!   for that start. Each entry names its readers; a test holds the table
//!   complete against the personal file's schema, and another pins each
//!   label.
//! - *The edit* ([`edit`]): one key set to a value of its kind, or removed,
//!   a missing table made as a standard one; nothing else of the text moves.
//! - *Its application* ([`apply`]): the file read, refused when it changed
//!   since the page read it (its SHA-256, [`rev_of`]), edited, checked with
//!   the loader (`FileState::from_text` with `PersonalConfig::parse`), then
//!   written whole into the folder of its real path, a link kept a link and
//!   the file's permissions kept. A refused edit writes nothing.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

use owlshift_contracts::config::PersonalConfig;
use serde_json::Value as Json;
use sha2::{Digest, Sha256};
use toml_edit::{Array, DocumentMut, Item, Key, Table, TableLike, Value};

use crate::config::FileState;

/// What a key holds, which gives the page its control and the server the
/// TOML type it writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `true` or `false`; removed, the default applies.
    Bool,
    /// A whole number.
    Integer,
    /// A number, written with a decimal point.
    Float,
    /// A text.
    Text,
    /// A list of texts.
    Texts,
    /// A harness's name, `claude` or `codex`.
    Harness,
}

impl Kind {
    /// Its name in the Config answer.
    pub fn key(self) -> &'static str {
        match self {
            Self::Bool => "bool",
            Self::Integer => "integer",
            Self::Float => "float",
            Self::Text => "text",
            Self::Texts => "texts",
            Self::Harness => "harness",
        }
    }

    /// What a value of it is, in words.
    fn expects(self) -> &'static str {
        match self {
            Self::Bool => "true or false",
            Self::Integer => "a whole number",
            Self::Float => "a number",
            Self::Text => "a text",
            Self::Texts => "a list of texts",
            Self::Harness => "a harness's name",
        }
    }
}

/// When an edit of a key applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applies {
    /// At the next pass: the next step reads it.
    NextPass,
    /// At the next start of the service.
    NextStart,
}

impl Applies {
    /// Its name in the Config answer and the edit's event.
    pub fn key(self) -> &'static str {
        match self {
            Self::NextPass => "next_pass",
            Self::NextStart => "next_start",
        }
    }

    /// When it applies, in words.
    pub fn words(self) -> &'static str {
        match self {
            Self::NextPass => "at the next pass",
            Self::NextStart => "at the next start of the service",
        }
    }
}

/// A key of the personal file the Config screen edits.
#[derive(Debug)]
pub struct KeyInfo {
    /// Its path, [`REPOSITORY`] standing for a repository's key.
    pub path: &'static [&'static str],
    pub kind: Kind,
    pub applies: Applies,
    /// Who reads it, in a sentence.
    pub about: &'static str,
}

impl KeyInfo {
    /// Its path written as the file writes it, [`REPOSITORY`] as is.
    pub fn dotted(&self) -> String {
        self.path.join(".")
    }
}

/// The segment of a [`KeyInfo::path`] that stands for a repository's key,
/// `github.com/<owner>/<name>`.
pub const REPOSITORY: &str = "*";

const STEP_READS: &str = "Each step reads it as it starts, in the child of its served watch that \
                          loads the personal file again.";

const NO_READER: &str = "No process of the running service reads it: the next command that does \
                         reads the new value.";

/// Every key of the personal file, with when an edit of it applies. The
/// readers named are those of OWL-473's build: `do_cmd.rs` is
/// `crates/owlshift-cli/src/do_cmd.rs`, whose `run_with` a served watch runs
/// once at its start and each step's child runs again; `service_cmd.rs` is
/// the daemon's.
pub const KEYS: &[KeyInfo] = &[
    // `check_requires` at every load (`FileState::from_text`), a step
    // child's included.
    KeyInfo {
        path: &["requires"],
        kind: Kind::Text,
        applies: Applies::NextPass,
        about: "Checked at every load of the file: the next step's included.",
    },
    // No reader in a running process: `owlshift init` and the commands a
    // person types.
    KeyInfo {
        path: &["identity", "tracker"],
        kind: Kind::Text,
        applies: Applies::NextPass,
        about: NO_READER,
    },
    KeyInfo {
        path: &["identity", "forge"],
        kind: Kind::Text,
        applies: Applies::NextPass,
        about: NO_READER,
    },
    // do_cmd.rs:563, `keep_awake_enabled` in a step child.
    KeyInfo {
        path: &["keep_awake"],
        kind: Kind::Bool,
        applies: Applies::NextPass,
        about: STEP_READS,
    },
    // do_cmd.rs:317-320 in a step child, and `served::effective`'s check of
    // the kept project file (do_cmd.rs:196), which a step child runs.
    KeyInfo {
        path: &["allow_gate_env"],
        kind: Kind::Texts,
        applies: Applies::NextPass,
        about: "Each step reads it as it starts, and checks its project's kept file against it: \
                a project file it no longer allows stops the project's next step.",
    },
    KeyInfo {
        path: &["allow_caches"],
        kind: Kind::Texts,
        applies: Applies::NextPass,
        about: "Each step reads it as it starts, and checks its project's kept file against it: \
                a project file it no longer allows stops the project's next step.",
    },
    KeyInfo {
        path: &["repositories", REPOSITORY, "allow_gate_env"],
        kind: Kind::Texts,
        applies: Applies::NextPass,
        about: "Each step of this repository reads it as it starts, as the machine-wide list.",
    },
    KeyInfo {
        path: &["repositories", REPOSITORY, "allow_caches"],
        kind: Kind::Texts,
        applies: Applies::NextPass,
        about: "Each step of this repository reads it as it starts, as the machine-wide list.",
    },
    // do_cmd.rs:545, the step child's `OnDemand`.
    KeyInfo {
        path: &["web_tools"],
        kind: Kind::Bool,
        applies: Applies::NextPass,
        about: STEP_READS,
    },
    // do_cmd.rs:405-412, the step child's Claude Code harness.
    KeyInfo {
        path: &["harnesses", "claude", "budget_usd"],
        kind: Kind::Float,
        applies: Applies::NextPass,
        about: "Each step passes it to the Claude Code runs it starts.",
    },
    // do_cmd.rs:503-508, the step child's review fallback.
    KeyInfo {
        path: &["harnesses", "claude", "fallback"],
        kind: Kind::Harness,
        applies: Applies::NextPass,
        about: STEP_READS,
    },
    // No reader: Codex takes no budget, and only Claude Code's fallback is
    // read.
    KeyInfo {
        path: &["harnesses", "codex", "budget_usd"],
        kind: Kind::Float,
        applies: Applies::NextPass,
        about: NO_READER,
    },
    KeyInfo {
        path: &["harnesses", "codex", "fallback"],
        kind: Kind::Harness,
        applies: Applies::NextPass,
        about: NO_READER,
    },
    // The daemon (service_cmd.rs:85) and each served watch (do_cmd.rs:185,
    // :511) at their start; each step child for its own reports.
    KeyInfo {
        path: &["notifications", "desktop"],
        kind: Kind::Bool,
        applies: Applies::NextStart,
        about: "The daemon and each served watch read it at their start; a step started after \
                the edit already reads the new value for its own notifications.",
    },
    // The daemon's digest, service_cmd.rs:102.
    KeyInfo {
        path: &["service", "digest"],
        kind: Kind::Bool,
        applies: Applies::NextStart,
        about: "The daemon reads it at its start.",
    },
    KeyInfo {
        path: &["service", "digest_hour"],
        kind: Kind::Integer,
        applies: Applies::NextStart,
        about: "The daemon reads it at its start.",
    },
    // The daemon binds the UI's port, service_cmd.rs:104.
    KeyInfo {
        path: &["ui", "port"],
        kind: Kind::Integer,
        applies: Applies::NextStart,
        about: "The daemon binds the web UI's port at its start.",
    },
    // The watch's dispatch count, do_cmd.rs:274-277 with the drain's
    // children, and each step child's slot take.
    KeyInfo {
        path: &["concurrent_runs"],
        kind: Kind::Integer,
        applies: Applies::NextStart,
        about: "Each served watch reads it at its start, for how many steps it starts; a step \
                started after the edit already takes its run slot with the new value.",
    },
    // The watch's usage caps, do_cmd.rs:517-518, and each step child's.
    KeyInfo {
        path: &["harnesses", "claude", "usage_cap_percent"],
        kind: Kind::Integer,
        applies: Applies::NextStart,
        about: "Each served watch reads it at its start; a step started after the edit already \
                holds its runs at the new cap.",
    },
    KeyInfo {
        path: &["harnesses", "codex", "usage_cap_percent"],
        kind: Kind::Integer,
        applies: Applies::NextStart,
        about: "Each served watch reads it at its start; a step started after the edit already \
                holds its runs at the new cap.",
    },
    // The watch's forge token refresh, do_cmd.rs:360-366 with forge.rs, and
    // each step child's.
    KeyInfo {
        path: &["github_app_client_id"],
        kind: Kind::Text,
        applies: Applies::NextStart,
        about: "Each served watch reads it at its start, for its forge token; a step started \
                after the edit already reads the new value.",
    },
    KeyInfo {
        path: &["github_app_slug"],
        kind: Kind::Text,
        applies: Applies::NextStart,
        about: "Each served watch reads it at its start, for its forge token; a step started \
                after the edit already reads the new value.",
    },
];

/// The entry of [`KEYS`] whose path is `path`, [`REPOSITORY`] standing for
/// any segment but an empty one.
pub fn lookup(path: &[String]) -> Option<&'static KeyInfo> {
    KEYS.iter().find(|info| {
        info.path.len() == path.len()
            && info
                .path
                .iter()
                .zip(path)
                .all(|(want, got)| *want == got || (*want == REPOSITORY && !got.is_empty()))
    })
}

/// `path` as the file writes it, each segment bare when it can be and
/// quoted otherwise: `repositories."github.com/o/n".allow_caches`.
pub fn dotted(path: &[String]) -> String {
    path.iter()
        .map(|segment| Key::new(segment.as_str()).display_repr().into_owned())
        .collect::<Vec<_>>()
        .join(".")
}

/// Why [`edit`] makes nothing.
#[derive(Debug, PartialEq, Eq)]
pub enum EditError {
    /// The value, or the key, is not one the table takes.
    BadValue(String),
    /// The text is not TOML.
    NotToml(String),
}

/// What [`edit`] made.
#[derive(Debug)]
pub struct Edited {
    /// The whole new text.
    pub text: String,
    /// The value written, as the file writes it; `None` for a key removed.
    pub value: Option<String>,
    /// The key's entry in [`KEYS`].
    pub info: &'static KeyInfo,
}

/// `text` with the key at `path` set to `value`, written as the TOML type
/// of its kind in [`KEYS`], or removed when `value` is null; a missing table
/// is made as a standard table, the tables over it implicit. Comments,
/// order and layout of everything else are kept as written, and a value
/// replaced keeps the spacing and comment around it.
pub fn edit(text: &str, path: &[String], value: &Json) -> Result<Edited, EditError> {
    let info = lookup(path).ok_or_else(|| {
        EditError::BadValue(format!(
            "{} is not a key of the personal file the web UI edits",
            dotted(path)
        ))
    })?;
    let new = match value {
        Json::Null => None,
        value => Some(toml_value(info, path, value)?),
    };
    let mut document: DocumentMut = text
        .parse()
        .map_err(|error: toml_edit::TomlError| EditError::NotToml(error.to_string()))?;
    let (last, parents) = path.split_last().expect("every key has a name");
    let mut table: &mut dyn TableLike = document.as_table_mut();
    for (at, segment) in parents.iter().enumerate() {
        if new.is_none() && table.get(segment).is_none() {
            // Nothing to remove.
            return Ok(Edited {
                text: text.to_owned(),
                value: None,
                info,
            });
        }
        let implicit = at + 1 < parents.len();
        table = table
            .entry(segment)
            .or_insert_with(|| {
                let mut made = Table::new();
                made.set_implicit(implicit);
                Item::Table(made)
            })
            .as_table_like_mut()
            .ok_or_else(|| {
                EditError::BadValue(format!(
                    "{} is not a table in the file",
                    dotted(&path[..=at])
                ))
            })?;
    }
    let written = match new {
        None => {
            table.remove(last);
            None
        }
        Some(mut value) => {
            let shown = value.clone().decorated("", "").to_string();
            match table.get_mut(last) {
                Some(Item::Value(old)) => {
                    *value.decor_mut() = old.decor().clone();
                    *old = value;
                }
                Some(other) => *other = Item::Value(value),
                None => {
                    table.insert(last, Item::Value(value));
                }
            }
            Some(shown)
        }
    };
    Ok(Edited {
        text: document.to_string(),
        value: written,
        info,
    })
}

/// `value` as the TOML value of `info`'s kind: any number for a float, a
/// whole one for an integer, a list of texts and nothing deeper for a list.
fn toml_value(info: &KeyInfo, path: &[String], value: &Json) -> Result<Value, EditError> {
    let bad = || EditError::BadValue(format!("{} takes {}", dotted(path), info.kind.expects()));
    match (info.kind, value) {
        (Kind::Bool, Json::Bool(on)) => Ok(Value::from(*on)),
        (Kind::Integer, Json::Number(number)) => number.as_i64().map(Value::from).ok_or_else(bad),
        (Kind::Float, Json::Number(number)) => number.as_f64().map(Value::from).ok_or_else(bad),
        (Kind::Text | Kind::Harness, Json::String(text)) => Ok(Value::from(text.as_str())),
        (Kind::Texts, Json::Array(items)) => {
            let mut array = Array::new();
            for item in items {
                array.push(item.as_str().ok_or_else(bad)?);
            }
            Ok(Value::Array(array))
        }
        _ => Err(bad()),
    }
}

/// The SHA-256 of a file's bytes in lowercase hexadecimal, `None` for a
/// file that is absent: what the page read, which an edit must name.
pub fn rev_of(bytes: Option<&[u8]>) -> Option<String> {
    bytes.map(|bytes| crate::ui::auth::hex(&Sha256::digest(bytes)))
}

/// The bytes of the file at `file`, `None` when it is absent.
pub fn read(file: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(file) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// An edit [`apply`] wrote.
#[derive(Debug)]
pub struct Applied {
    /// The file's new text.
    pub text: String,
    /// Its [`rev_of`], which the page's next edit names.
    pub rev: String,
    /// The key, as the file writes it.
    pub key: String,
    /// The value written, as the file writes it; `None` for a key removed.
    pub value: Option<String>,
    pub applies: Applies,
}

/// Why [`apply`] wrote nothing.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The system has no configuration folder.
    NotApplicable(String),
    /// The file changed since the page read it.
    Changed,
    /// The loader refuses the file the edit would make, or the file as it is
    /// when it is not TOML at all: its error.
    Invalid(String),
    /// The key is not in [`KEYS`], or the value not of its kind.
    BadValue(String),
    /// The file could not be read or written.
    Io(String),
}

impl Refusal {
    /// The HTTP status that answers it.
    pub fn status(&self) -> u16 {
        match self {
            Self::NotApplicable(_) | Self::Changed => 409,
            Self::Invalid(_) => 422,
            Self::BadValue(_) => 400,
            Self::Io(_) => 500,
        }
    }

    /// Why, in words.
    pub fn message(&self) -> String {
        match self {
            Self::NotApplicable(why) => format!("the personal file cannot be edited: {why}"),
            Self::Changed => "the personal file changed since this page read it: reload it, then \
                              edit again"
                .to_owned(),
            Self::Invalid(error) | Self::BadValue(error) | Self::Io(error) => error.clone(),
        }
    }
}

/// Sets the key at `path` of the personal file at `file` to `value`, or
/// removes it when `value` is null, as [`edit`] does; `rev` is the
/// [`rev_of`] the page read, `None` for a file it read absent, and the edit
/// is refused when the file is not that one any more. `file` is `None` on a
/// system with no configuration folder. The key is looked up in [`KEYS`]
/// before anything is read. The new text is checked as the loader checks
/// the file and written only when it loads; a file that is not TOML at all
/// is refused with the loader's error on it as it stands. Nothing is
/// written when the edit is refused, and an edit that changes nothing
/// writes nothing.
pub fn apply(
    file: Option<&Path>,
    rev: Option<&str>,
    path: &[String],
    value: &Json,
) -> Result<Applied, Refusal> {
    let Some(file) = file else {
        return Err(Refusal::NotApplicable(
            "this system has no configuration directory".to_owned(),
        ));
    };
    if lookup(path).is_none() {
        return Err(Refusal::BadValue(format!(
            "{} is not a key of the personal file the web UI edits",
            dotted(path)
        )));
    }
    let current =
        read(file).map_err(|error| Refusal::Io(format!("{}: {error}", file.display())))?;
    if rev_of(current.as_deref()).as_deref() != rev {
        return Err(Refusal::Changed);
    }
    let existed = current.is_some();
    let text = match current {
        Some(bytes) => String::from_utf8(bytes)
            .map_err(|error| Refusal::Invalid(format!("cannot read it: {}", error.utf8_error())))?,
        None => String::new(),
    };
    let edited = match edit(&text, path, value) {
        Ok(edited) => edited,
        Err(EditError::BadValue(why)) => return Err(Refusal::BadValue(why)),
        Err(EditError::NotToml(why)) => {
            return Err(Refusal::Invalid(loader_error(file, &text).unwrap_or(why)));
        }
    };
    if let Some(error) = loader_error(file, &edited.text) {
        return Err(Refusal::Invalid(error));
    }
    if !existed || edited.text != text {
        write(file, &edited.text)
            .map_err(|error| Refusal::Io(format!("{}: {error}", file.display())))?;
    }
    Ok(Applied {
        rev: rev_of(Some(edited.text.as_bytes())).unwrap_or_default(),
        key: dotted(path),
        value: edited.value,
        applies: edited.info.applies,
        text: edited.text,
    })
}

/// The loader's error on `text` read from `file`, `None` when it loads.
fn loader_error(file: &Path, text: &str) -> Option<String> {
    match FileState::from_text(file.to_owned(), text, PersonalConfig::parse) {
        FileState::Invalid { error, .. } => Some(error),
        _ => None,
    }
}

/// Writes `text` as the file at `file`, whole or not at all: into the
/// folder of its real path, so a link stays a link and its target is
/// written, its permissions kept; a file not there yet is made, with its
/// folder, readable by its owner alone.
fn write(file: &Path, text: &str) -> io::Result<()> {
    let target = match fs::symlink_metadata(file) {
        Ok(_) => fs::canonicalize(file)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => file.to_owned(),
        Err(error) => return Err(error),
    };
    let dir = target
        .parent()
        .ok_or_else(|| io::Error::other("the file has no folder"))?;
    fs::create_dir_all(dir)?;
    let permissions = fs::metadata(&target).ok().map(|meta| meta.permissions());
    let mut partial = tempfile::NamedTempFile::new_in(dir)?;
    partial.write_all(text.as_bytes())?;
    partial.as_file().sync_all()?;
    if let Some(permissions) = permissions {
        partial.as_file().set_permissions(permissions)?;
    }
    partial.persist(&target).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    use serde_json::json;

    use super::*;

    fn path(dotted: &str) -> Vec<String> {
        dotted.split('.').map(str::to_owned).collect()
    }

    /// The leaf keys of the schema at `schema`, with what each holds, `*`
    /// for a map's key.
    fn leaves(schema: &Json, defs: &Json, prefix: &[String], out: &mut BTreeSet<(String, String)>) {
        let mut schema = schema;
        loop {
            if let Some(name) = schema
                .get("$ref")
                .and_then(Json::as_str)
                .and_then(|reference| reference.strip_prefix("#/$defs/"))
            {
                schema = &defs[name];
            } else if let Some(branches) = schema.get("anyOf").and_then(Json::as_array) {
                schema = branches
                    .iter()
                    .find(|branch| branch.get("type") != Some(&json!("null")))
                    .expect("a branch that is not null");
            } else {
                break;
            }
        }
        if let Some(properties) = schema.get("properties").and_then(Json::as_object) {
            for (name, inner) in properties {
                let mut deeper = prefix.to_vec();
                deeper.push(name.clone());
                leaves(inner, defs, &deeper, out);
            }
            return;
        }
        if let Some(extra) = schema.get("additionalProperties").filter(|v| v.is_object()) {
            let mut deeper = prefix.to_vec();
            deeper.push(REPOSITORY.to_owned());
            leaves(extra, defs, &deeper, out);
            return;
        }
        let types: Vec<&str> = match &schema["type"] {
            Json::String(one) => vec![one.as_str()],
            Json::Array(many) => many.iter().filter_map(Json::as_str).collect(),
            _ => Vec::new(),
        };
        let kind = if schema.get("enum").is_some() {
            Kind::Harness
        } else if types.contains(&"boolean") {
            Kind::Bool
        } else if types.contains(&"integer") {
            Kind::Integer
        } else if types.contains(&"number") {
            Kind::Float
        } else if types.contains(&"array") {
            assert_eq!(schema["items"]["type"], json!("string"), "{prefix:?}");
            Kind::Texts
        } else {
            assert!(types.contains(&"string"), "{prefix:?}: {schema}");
            Kind::Text
        };
        out.insert((prefix.join("."), kind.key().to_owned()));
    }

    /// The table holds every key of the personal file's schema, each with
    /// the kind the schema gives it, and nothing else.
    #[test]
    fn the_table_covers_every_key_of_the_schema() {
        let (_, schema) = owlshift_contracts::schema::all()
            .into_iter()
            .find(|(name, _)| *name == "personal-config")
            .expect("the personal file's schema");
        let schema: Json = serde_json::from_str(&owlshift_contracts::schema::render(&schema))
            .expect("a schema reads as JSON");
        let mut found = BTreeSet::new();
        leaves(&schema, &schema["$defs"], &[], &mut found);
        let table: BTreeSet<(String, String)> = KEYS
            .iter()
            .map(|info| (info.dotted(), info.kind.key().to_owned()))
            .collect();
        assert_eq!(table, found);
        assert_eq!(table.len(), KEYS.len(), "a key listed twice");
    }

    /// Each key's label, as its readers read it (see [`KEYS`]): changing a
    /// label without its readers fails here.
    #[test]
    fn each_key_applies_as_its_readers_read_it() {
        let next_pass = [
            "requires",
            "identity.tracker",
            "identity.forge",
            "keep_awake",
            "allow_gate_env",
            "allow_caches",
            "repositories.*.allow_gate_env",
            "repositories.*.allow_caches",
            "web_tools",
            "harnesses.claude.budget_usd",
            "harnesses.claude.fallback",
            "harnesses.codex.budget_usd",
            "harnesses.codex.fallback",
        ];
        let next_start = [
            "notifications.desktop",
            "service.digest",
            "service.digest_hour",
            "ui.port",
            "concurrent_runs",
            "harnesses.claude.usage_cap_percent",
            "harnesses.codex.usage_cap_percent",
            "github_app_client_id",
            "github_app_slug",
        ];
        assert_eq!(next_pass.len() + next_start.len(), KEYS.len());
        for info in KEYS {
            let dotted = info.dotted();
            let expected = if next_pass.contains(&dotted.as_str()) {
                Applies::NextPass
            } else {
                assert!(next_start.contains(&dotted.as_str()), "{dotted}");
                Applies::NextStart
            };
            assert_eq!(info.applies, expected, "{dotted}");
            assert!(!info.about.is_empty(), "{dotted}");
        }
    }

    const COMMENTED: &str = "\
# My machine.
concurrent_runs = 2 # two at once
keep_awake = true

# The harnesses.
[harnesses.claude]
budget_usd = 3.0
";

    /// One key changes, its line only: comments, order and the rest kept.
    #[test]
    fn an_edit_changes_its_line_only() {
        let edited = edit(COMMENTED, &path("concurrent_runs"), &json!(4)).unwrap();
        assert_eq!(
            edited.text,
            "\
# My machine.
concurrent_runs = 4 # two at once
keep_awake = true

# The harnesses.
[harnesses.claude]
budget_usd = 3.0
"
        );
        assert_eq!(edited.value.as_deref(), Some("4"));
        assert_eq!(edited.info.applies, Applies::NextStart);
    }

    /// A missing table is made as a standard one, a repository's key quoted
    /// and the tables over it implicit; a float key takes a whole number as
    /// a float.
    #[test]
    fn a_missing_table_is_made() {
        let edited = edit("", &path("ui.port"), &json!(18500)).unwrap();
        assert_eq!(edited.text, "[ui]\nport = 18500\n");
        let edited = edit(COMMENTED, &path("harnesses.codex.budget_usd"), &json!(5)).unwrap();
        assert_eq!(edited.value.as_deref(), Some("5.0"));
        assert!(
            edited.text.ends_with(
                "[harnesses.claude]\nbudget_usd = 3.0\n\n[harnesses.codex]\nbudget_usd = 5.0\n"
            ),
            "{}",
            edited.text
        );
        PersonalConfig::parse(&edited.text).unwrap();
        let repository = vec![
            "repositories".to_owned(),
            "github.com/a/b".to_owned(),
            "allow_gate_env".to_owned(),
        ];
        let edited = edit("", &repository, &json!(["DATABASE_URL"])).unwrap();
        assert_eq!(
            edited.text,
            "[repositories.\"github.com/a/b\"]\nallow_gate_env = [\"DATABASE_URL\"]\n"
        );
        assert_eq!(
            dotted(&repository),
            "repositories.\"github.com/a/b\".allow_gate_env"
        );
        let config = PersonalConfig::parse(&edited.text).unwrap();
        assert_eq!(
            config.allow_gate_env_names(Some("github.com/a/b")),
            ["DATABASE_URL"]
        );
    }

    /// A null removes the key, its table and comments kept; removing a key
    /// that is not there changes nothing.
    #[test]
    fn null_removes_the_key_and_keeps_its_table() {
        let edited = edit(COMMENTED, &path("harnesses.claude.budget_usd"), &Json::Null).unwrap();
        assert_eq!(
            edited.text,
            "# My machine.\nconcurrent_runs = 2 # two at once\nkeep_awake = true\n\n# The \
             harnesses.\n[harnesses.claude]\n"
        );
        assert_eq!(edited.value, None);
        let unchanged = edit(COMMENTED, &path("service.digest"), &Json::Null).unwrap();
        assert_eq!(unchanged.text, COMMENTED);
    }

    /// A key outside the table, or a value not of the key's kind, is
    /// refused.
    #[test]
    fn a_key_outside_the_table_or_a_wrong_value_is_refused() {
        for (key, value) in [
            ("foo", json!(1)),
            ("ui.nope", json!(1)),
            ("repositories..allow_caches", json!([])),
            ("concurrent_runs", json!(2.5)),
            ("concurrent_runs", json!("2")),
            ("keep_awake", json!("yes")),
            ("allow_caches", json!([["a"]])),
            ("allow_caches", json!({"a": 1})),
            ("budget_usd", json!(1)),
        ] {
            assert!(
                matches!(edit("", &path(key), &value), Err(EditError::BadValue(_))),
                "{key} {value}"
            );
        }
    }

    /// The personal file in a fresh folder, holding `text` when given.
    fn personal_file(text: Option<&str>) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("owlshift").join("config.toml");
        if let Some(text) = text {
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(&file, text).unwrap();
        }
        (dir, file)
    }

    fn rev(file: &Path) -> Option<String> {
        rev_of(read(file).unwrap().as_deref())
    }

    /// An edit the loader accepts is written; the next edit names the new
    /// rev, and an edit naming the old one is refused, the file unchanged.
    #[test]
    fn an_accepted_edit_is_written_and_a_stale_one_refused() {
        let (_dir, file) = personal_file(Some(COMMENTED));
        let before = rev(&file);
        let applied = apply(
            Some(&file),
            before.as_deref(),
            &path("web_tools"),
            &json!(false),
        )
        .unwrap();
        assert_eq!(applied.key, "web_tools");
        assert_eq!(applied.value.as_deref(), Some("false"));
        assert_eq!(applied.applies, Applies::NextPass);
        assert_eq!(fs::read_to_string(&file).unwrap(), applied.text);
        // A key of the file's root goes after the root's last key.
        assert_eq!(
            applied.text,
            COMMENTED.replace(
                "keep_awake = true\n",
                "keep_awake = true\nweb_tools = false\n"
            )
        );
        assert_eq!(rev(&file).as_deref(), Some(applied.rev.as_str()));
        let stale = apply(
            Some(&file),
            before.as_deref(),
            &path("keep_awake"),
            &json!(false),
        );
        assert_eq!(stale.unwrap_err(), Refusal::Changed);
        assert_eq!(fs::read_to_string(&file).unwrap(), applied.text);
        // A page that read the file absent names no rev.
        assert_eq!(
            apply(Some(&file), None, &path("keep_awake"), &json!(false)).unwrap_err(),
            Refusal::Changed
        );
    }

    /// An edit the loader refuses answers its error and leaves the file as
    /// it was, its bytes and time.
    #[test]
    fn an_edit_the_loader_refuses_writes_nothing() {
        let (_dir, file) = personal_file(Some(COMMENTED));
        let modified = fs::metadata(&file).unwrap().modified().unwrap();
        let refused = apply(
            Some(&file),
            rev(&file).as_deref(),
            &path("concurrent_runs"),
            &json!(40),
        )
        .unwrap_err();
        assert_eq!(refused.status(), 422);
        assert!(
            refused
                .message()
                .contains("concurrent_runs must be a whole number from 1 to 16"),
            "{refused:?}"
        );
        assert_eq!(fs::read_to_string(&file).unwrap(), COMMENTED);
        assert_eq!(fs::metadata(&file).unwrap().modified().unwrap(), modified);
        for key in ["foo", "ui.nope"] {
            let refused = apply(Some(&file), rev(&file).as_deref(), &path(key), &json!(1));
            assert_eq!(refused.as_ref().unwrap_err().status(), 400, "{refused:?}");
        }
        assert_eq!(fs::read_to_string(&file).unwrap(), COMMENTED);
    }

    /// A file that is not there is made, with its folder; one that is not
    /// TOML is refused with the loader's error on it, unchanged; a system
    /// with no configuration folder edits nothing.
    #[test]
    fn an_absent_file_is_made_and_a_broken_one_refused() {
        let (_dir, file) = personal_file(None);
        let applied = apply(Some(&file), None, &path("ui.port"), &json!(18500)).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "[ui]\nport = 18500\n");
        assert_eq!(rev(&file).as_deref(), Some(applied.rev.as_str()));

        let broken = "concurrent_runs = = 2\n";
        let (_dir, file) = personal_file(Some(broken));
        let refused = apply(
            Some(&file),
            rev(&file).as_deref(),
            &path("keep_awake"),
            &json!(true),
        )
        .unwrap_err();
        let loader = loader_error(&file, broken).unwrap();
        assert_eq!(refused, Refusal::Invalid(loader));
        assert_eq!(fs::read_to_string(&file).unwrap(), broken);

        let refused = apply(None, None, &path("keep_awake"), &json!(true)).unwrap_err();
        assert_eq!(refused.status(), 409);
        assert!(matches!(refused, Refusal::NotApplicable(_)));
    }

    /// A personal file reached through a link stays a link, its target
    /// edited; a file readable by its owner alone stays so.
    #[cfg(unix)]
    #[test]
    fn a_link_stays_a_link_and_permissions_are_kept() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, target) = personal_file(Some(COMMENTED));
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        let link = dir.path().join("config.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        apply(
            Some(&link),
            rev(&link).as_deref(),
            &path("keep_awake"),
            &json!(false),
        )
        .unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(
            fs::read_to_string(&target)
                .unwrap()
                .contains("keep_awake = false")
        );
        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
