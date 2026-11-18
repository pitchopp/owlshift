//! The folders of the home a confined setup or gate command was refused,
//! said and offered (OWL-597; build plan, "P9 tasks & exit gate", "OWL-597";
//! runtime and operations, "Configuration").
//!
//! [`refused`] takes the folders `owlshift_core::closed_home::scan` reads in
//! a failing command's output and settles each against this machine:
//! Owlshift's own folders are left out unread; a folder that is or lies in a
//! credential path is named, never opened, and left out when the output only
//! said it was missing; a path the sandbox already opens read-only is a
//! write; any other folder is checked with one metadata call, the only file
//! system read here, which never reaches a credential path, Owlshift's own
//! folders or a folder that only gathers others. [`gate_note`] says it all
//! for `owlshift gate`, with the two lines that open a folder
//! ([`lines`]); [`reason_note`] says it in one line for a Build's reason,
//! naming no credential path. [`open`] writes both lines on the operator's
//! yes, [`allow`] the personal file's alone for `owlshift init`: each new
//! text checked by its file's loader before either file is written, the
//! personal file first, each file's comments and layout kept.

use std::fs;
use std::path::{Component, Path, PathBuf};

use owlshift_adapters::forge::Repo;
use owlshift_contracts::config::{PersonalConfig, ProjectConfig};
use owlshift_core::closed_home::{self, Kind};
use owlshift_platform::sandbox;
use toml_edit::{Array, DocumentMut, Item, Table, TableLike, Value};

use crate::agent_env::{AgentEnv, RunPaths};
use crate::config::{FileState, repository_key, write_file};
use crate::init::allow_caches_lines;
use crate::personal_edit;

/// What [`refused`] reads a command's output against.
pub struct Context<'a> {
    /// The home as the commands saw it, and its real path.
    homes: Vec<String>,
    home: PathBuf,
    /// The paths of the home the sandbox opens read-only, from the home.
    opened: Vec<String>,
    /// Owlshift's own folders and the person's checkout, from the home.
    own: Vec<String>,
    /// Whether a path is a folder on this machine: a metadata call alone.
    is_folder: &'a dyn Fn(&Path) -> bool,
}

impl<'a> Context<'a> {
    /// `home`, the home the commands saw; `opened`, the paths the sandbox
    /// opens read-only; `own`, the folders never named (Owlshift's data and
    /// configuration folders, the checkout); `is_folder`, the metadata check
    /// ([`host_is_folder`] outside the tests). Paths outside the home are
    /// left out.
    pub fn new(
        home: &Path,
        opened: &[PathBuf],
        own: &[PathBuf],
        is_folder: &'a dyn Fn(&Path) -> bool,
    ) -> Self {
        let real = sandbox::real(home);
        let roots: Vec<&Path> = if real == home {
            vec![home]
        } else {
            vec![home, real.as_path()]
        };
        let relative = |paths: &[PathBuf]| -> Vec<String> {
            paths
                .iter()
                .filter_map(|path| from_home(path, &roots))
                .collect()
        };
        Self {
            homes: roots
                .iter()
                .filter_map(|root| root.to_str())
                .map(|root| root.trim_end_matches('/').to_owned())
                .collect(),
            home: home.to_owned(),
            opened: relative(opened),
            own: relative(own),
            is_folder,
        }
    }

    /// The context of the commands `agent` runs: its `HOME`, and what its
    /// sandbox opens read-only in it (the allowed caches, the tool chains,
    /// git's configuration, the folders its `PATH` and declared variables
    /// name). `None` without an absolute `HOME`, where nothing is read.
    pub fn for_agent(
        agent: &AgentEnv,
        own: &[PathBuf],
        is_folder: &'a dyn Fn(&Path) -> bool,
    ) -> Option<Self> {
        let home = PathBuf::from(agent.var("HOME")?);
        if !home.is_absolute() {
            return None;
        }
        let opened = agent.policy(&RunPaths::default()).readable;
        Some(Self::new(&home, &opened, own, is_folder))
    }
}

/// Whether `path` is a folder on this machine, by its metadata alone.
pub fn host_is_folder(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|meta| meta.is_dir())
}

/// Owlshift's own folders, never named: its data directory and the folder
/// of its personal file.
pub fn own_folders() -> Vec<PathBuf> {
    let personal = owlshift_platform::paths::personal_config_file()
        .and_then(|file| file.parent().map(Path::to_owned));
    owlshift_platform::paths::data_dir()
        .into_iter()
        .chain(personal)
        .collect()
}

/// What becomes of a refused folder on this machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Closed, and a folder here: the two lines open it.
    Closed,
    /// No folder here: opening it would change nothing.
    Absent,
    /// The sandbox opens it read-only already: the command wrote there.
    Open,
    /// It is, lies in or holds a credential path: never opened.
    Credential,
    /// It only gathers others, or holds the person's own files.
    Wide,
}

/// A folder of the home the command was refused, from the home.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused {
    pub folder: String,
    pub state: State,
}

/// The folders of the home `output`, a failing command's, says the sandbox
/// refused, settled as the module documentation says, each once.
pub fn refused(output: &str, ctx: &Context<'_>) -> Vec<Refused> {
    let homes: Vec<&str> = ctx.homes.iter().map(String::as_str).collect();
    let mut out: Vec<Refused> = Vec::new();
    for found in closed_home::scan(output, &homes) {
        if ctx
            .own
            .iter()
            .any(|own| within(&found.folder, own) || within(own, &found.folder))
        {
            continue;
        }
        let state = match found.kind {
            Kind::Credential if found.missing_only => continue,
            Kind::Credential => State::Credential,
            _ if ctx.opened.iter().any(|opened| within(&found.path, opened)) => {
                if found.missing_only {
                    continue;
                }
                State::Open
            }
            // A folder that gathers others may hold a credential path: never
            // read, so a missing file in it says nothing.
            Kind::Wide if found.missing_only => continue,
            Kind::Wide => State::Wide,
            Kind::Offer => {
                match (
                    (ctx.is_folder)(&ctx.home.join(&found.folder)),
                    found.missing_only,
                ) {
                    (true, _) => State::Closed,
                    (false, true) => continue,
                    (false, false) => State::Absent,
                }
            }
        };
        out.push(Refused {
            folder: found.folder,
            state,
        });
    }
    out
}

/// The two lines that open `folders`, as each file would then hold them:
/// `caches` with the folders `declared` does not hold, and the personal
/// file's entry for `repo` with the folders `allowed` (every folder the
/// personal file allows the repository) does not hold, after those of its
/// own entry, `scoped`. `None` for a line not needed.
pub fn lines(
    folders: &[String],
    declared: &[String],
    scoped: &[String],
    allowed: &[&str],
    repo: &Repo,
) -> Lines {
    let added = |kept: &[String], wanted: &dyn Fn(&String) -> bool| -> Option<Vec<String>> {
        let new: Vec<String> = folders
            .iter()
            .filter(|folder| wanted(folder))
            .cloned()
            .collect();
        (!new.is_empty()).then(|| kept.iter().cloned().chain(new).collect())
    };
    Lines {
        caches: added(declared, &|folder| !declared.contains(folder)).map(|all| {
            let quoted: Vec<String> = all.iter().map(|folder| quote(folder)).collect();
            format!("caches = [{}]", quoted.join(", "))
        }),
        allow: added(scoped, &|folder| !allowed.contains(&folder.as_str()))
            .map(|all| allow_caches_lines(Some(repo), &all).trim_end().to_owned()),
    }
}

/// The lines [`lines`] gives.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Lines {
    /// `caches = [...]`, under `[stack]` in `owlshift.toml`.
    pub caches: Option<String>,
    /// The repository's table and its `allow_caches`, in the personal file.
    pub allow: Option<String>,
}

impl Lines {
    pub fn is_empty(&self) -> bool {
        self.caches.is_none() && self.allow.is_none()
    }
}

const WRITES: &str = "keep the tool from writing there (as `--prefer-offline` does for npm), \
                      or move its writable folder into $TMPDIR, the run's own";

const NPM: &str = "npm: use the setup `owlshift init` writes, `npm ci --prefer-offline --cache \
                   \"$HOME/.npm\" --logs-dir \"$TMPDIR/npm-logs\"`, and run `npm ci` once in your \
                   checkout so that ~/.npm holds every package of the lock file.";

/// What `owlshift gate` says after the gate's result: each folder refused,
/// the lines that open the closed ones (`project`, the project file;
/// `personal`, the personal file when known), and why `sudo` changes
/// nothing.
pub fn gate_note(
    refused: &[Refused],
    lines: &Lines,
    project: &Path,
    personal: Option<&Path>,
) -> String {
    let mut out = String::from(
        "The sandbox closes your home to every run and gate. The failing command was refused:\n",
    );
    for one in refused {
        let words = match one.state {
            State::Closed => String::new(),
            State::Absent => ": no folder on this machine, so opening it changes nothing: point \
                              the tool's writable folder into $TMPDIR, the run's own"
                .to_owned(),
            State::Open => format!(
                ": open already, read-only, and the tool wrote there: fill the folder outside \
                 the sandbox, {WRITES}"
            ),
            State::Credential => ": it holds credentials, so the sandbox never opens it, \
                                  whatever the configuration"
                .to_owned(),
            State::Wide => ": too wide to open as a cache: point the tool at a folder of its own, \
                            or into $TMPDIR"
                .to_owned(),
        };
        out.push_str(&format!("  ~/{}{words}\n", one.folder));
    }
    if !lines.is_empty() {
        out.push_str(
            "A folder of your home opens to runs, read-only, once the project declares it and \
             this machine allows it:\n",
        );
        if let Some(caches) = &lines.caches {
            out.push_str(&format!(
                "  in {}, under [stack]:\n    {caches}\n",
                project.display()
            ));
        }
        if let Some(allow) = &lines.allow {
            out.push_str(&format!(
                "  in {}:\n",
                personal.map_or_else(
                    || "your personal configuration file".to_owned(),
                    |path| path.display().to_string()
                )
            ));
            for line in allow.lines() {
                out.push_str(&format!("    {line}\n"));
            }
        }
        out.push_str(&format!(
            "Every run of this project can then read it: allow caches, never secrets. Read-only, \
             it still refuses writes: {WRITES}.\n"
        ));
    }
    let named = |states: &[State], npm: bool| {
        refused
            .iter()
            .any(|one| states.contains(&one.state) && (!npm || one.folder == ".npm"))
    };
    if named(&[State::Closed, State::Open], true) {
        out.push_str(NPM);
        out.push('\n');
    }
    if named(&[State::Closed, State::Open, State::Absent], false) {
        out.push_str("`sudo` changes nothing: the folder's owner is not the cause.\n");
    }
    out
}

/// What a Build's reason adds when its setup failed (OWL-597): the closed
/// folders and the two lines that open them for the repository
/// `<owner>/<name>` `repo`, the folders open already that the command
/// wrote, and those that are no folder here, in one line; `None` when there
/// are none. It never names a credential path nor a folder too wide.
pub fn reason_note(refused: &[Refused], repo: &str) -> Option<String> {
    let of = |state: State| -> Vec<&str> {
        refused
            .iter()
            .filter(|one| one.state == state)
            .map(|one| one.folder.as_str())
            .collect()
    };
    let shown = |folders: &[&str]| -> String {
        folders
            .iter()
            .map(|folder| format!("~/{folder}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut parts = Vec::new();
    let closed = of(State::Closed);
    if !closed.is_empty() {
        let quoted: Vec<String> = closed.iter().map(|folder| quote(folder)).collect();
        parts.push(format!(
            "the sandbox closes the home and refused {}: to open {} read-only, add {} to \
             stack.caches in owlshift.toml and to allow_caches under [repositories.{}] in the \
             personal file (`owlshift gate` offers to), and keep the tool from writing there or \
             move its writable folder into $TMPDIR",
            shown(&closed),
            if closed.len() == 1 { "it" } else { "them" },
            quoted.join(", "),
            quote(&repository_key(&repo))
        ));
    }
    let open = of(State::Open);
    if !open.is_empty() {
        parts.push(format!(
            "{} {} open already, read-only, and the tool wrote there: keep it from writing \
             there, fill it outside the sandbox or move its writable folder into $TMPDIR",
            shown(&open),
            if open.len() == 1 { "is" } else { "are" }
        ));
    }
    let absent = of(State::Absent);
    if !absent.is_empty() {
        parts.push(format!(
            "the sandbox refused {}, no folder on this machine: move the tool's writable folder \
             into $TMPDIR",
            shown(&absent)
        ));
    }
    if parts.is_empty() {
        return None;
    }
    parts.push("never sudo".to_owned());
    Some(parts.join("; "))
}

/// What [`open`] wrote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Written {
    pub personal: bool,
    pub project: bool,
}

/// Opens `folders` for the repository `repo` on the operator's yes: adds the
/// ones the personal file at `personal` does not allow the repository to its
/// entry, and the ones the project file at `project` does not declare to its
/// `stack.caches`. Both new texts are checked first, the personal file's by
/// its loader and the project file's by its loader and against the
/// allowance the personal file will then hold; then the personal file is
/// written, since a project file declaring a folder this machine does not
/// allow is refused at loading, and the project file second. Nothing is
/// written when either check fails.
pub fn open(
    project: &Path,
    personal: &Path,
    repo: &Repo,
    folders: &[String],
) -> Result<Written, String> {
    let key = repository_key(repo);
    let (personal_text, new_personal, allowed) = allowed_text(personal, &key, folders)?;
    let project_text = read_text(project)?;
    let mut document = parse(project, &project_text)?;
    let stack = document
        .as_table_mut()
        .entry("stack")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_like_mut()
        .ok_or_else(|| format!("{}: `stack` is not a table", project.display()))?;
    add_to_list(stack, "caches", folders).map_err(|why| format!("{}: {why}", project.display()))?;
    let new_project = document.to_string();
    let allowed: Vec<&str> = allowed.iter().map(String::as_str).collect();
    match FileState::from_text(project.to_owned(), &new_project, ProjectConfig::parse) {
        FileState::Loaded { config, .. } => config
            .check_caches(&allowed)
            .map_err(|error| format!("{}: {error}", project.display()))?,
        FileState::Invalid { error, .. } => return Err(format!("{}: {error}", project.display())),
        _ => return Err(format!("{}: it did not load", project.display())),
    }
    let written = Written {
        personal: new_personal != personal_text,
        project: new_project != project_text,
    };
    if written.personal {
        write(personal, &new_personal)?;
    }
    if written.project {
        write(project, &new_project)?;
    }
    Ok(written)
}

/// Allows `folders` to the repository `repo` in the personal file at
/// `personal`, as [`open`] does, for `owlshift init`, whose project file
/// declares them already: whether the file changed.
pub fn allow(personal: &Path, repo: &Repo, folders: &[String]) -> Result<bool, String> {
    let (text, new, _) = allowed_text(personal, &repository_key(repo), folders)?;
    if new == text {
        return Ok(false);
    }
    write(personal, &new)?;
    Ok(true)
}

/// The personal file's text, the text with the folders of `folders` it does
/// not allow the repository `key` added to its entry, and every folder the
/// new text allows it: the new text checked by the loader.
fn allowed_text(
    personal: &Path,
    key: &str,
    folders: &[String],
) -> Result<(String, String, Vec<String>), String> {
    let text = read_text(personal)?;
    let loaded =
        |text: &str| match FileState::from_text(personal.to_owned(), text, PersonalConfig::parse) {
            FileState::Loaded { config, .. } => Ok(config),
            FileState::Invalid { error, .. } => Err(format!("{}: {error}", personal.display())),
            _ => Err(format!("{}: it did not load", personal.display())),
        };
    let current = loaded(&text)?;
    let allowed = current.allow_caches_names(Some(key));
    let missing: Vec<String> = folders
        .iter()
        .filter(|folder| !allowed.contains(&folder.as_str()))
        .cloned()
        .collect();
    let mut document = parse(personal, &text)?;
    if !missing.is_empty() {
        let repositories = document
            .as_table_mut()
            .entry("repositories")
            .or_insert_with(|| {
                let mut made = Table::new();
                made.set_implicit(true);
                Item::Table(made)
            })
            .as_table_mut()
            .ok_or_else(|| {
                format!(
                    "{}: `repositories` is not a table this edit writes: add the lines by hand",
                    personal.display()
                )
            })?;
        // The key as the file writes it, in any letter case, as the loader
        // reads it.
        let name = repositories
            .iter()
            .map(|(name, _)| name)
            .find(|name| name.eq_ignore_ascii_case(key))
            .unwrap_or(key)
            .to_owned();
        let entry = repositories
            .entry(&name)
            .or_insert_with(|| Item::Table(Table::new()))
            .as_table_mut()
            .ok_or_else(|| {
                format!(
                    "{}: `repositories.\"{name}\"` is not a table this edit writes: add the lines \
                     by hand",
                    personal.display()
                )
            })?;
        add_to_list(entry, "allow_caches", &missing)
            .map_err(|why| format!("{}: {why}", personal.display()))?;
    }
    let new = document.to_string();
    let config = loaded(&new)?;
    let allowed = config
        .allow_caches_names(Some(key))
        .into_iter()
        .map(str::to_owned)
        .collect();
    Ok((text, new, allowed))
}

/// Adds to the list `key` of `table` the items of `items` it does not hold,
/// each laid out as the list's last item is, the items there and their
/// comments kept; the key is made when absent.
fn add_to_list(table: &mut dyn TableLike, key: &str, items: &[String]) -> Result<(), String> {
    let Some(item) = table.get_mut(key) else {
        let mut list = Array::new();
        for item in items {
            list.push(item.as_str());
        }
        table.insert(key, Item::Value(Value::Array(list)));
        return Ok(());
    };
    let list = item
        .as_array_mut()
        .ok_or_else(|| format!("`{key}` is not a list"))?;
    for wanted in items {
        if list.iter().any(|value| value.as_str() == Some(wanted)) {
            continue;
        }
        let Some(last) = list.iter().last() else {
            list.push(wanted.as_str());
            continue;
        };
        let last = last
            .decor()
            .prefix()
            .and_then(|raw| raw.as_str())
            .unwrap_or("");
        let mut prefix = match last.rfind('\n') {
            Some(at) => format!("\n{}", &last[at + 1..]),
            None => " ".to_owned(),
        };
        // A comment after the last item's comma stays on its line: the
        // list's trailing text holds it, which would now follow the item
        // added.
        let trailing = list.trailing().as_str().unwrap_or("").to_owned();
        if list.trailing_comma()
            && let Some(at) = trailing.find('\n')
            && at > 0
        {
            prefix = format!("{}{prefix}", &trailing[..at]);
            list.set_trailing(&trailing[at..]);
        }
        list.push(wanted.as_str());
        if let Some(added) = list.iter_mut().last() {
            added.decor_mut().set_prefix(prefix);
        }
    }
    Ok(())
}

/// The file at `path` as text, empty when it is absent.
fn read_text(path: &Path) -> Result<String, String> {
    let bytes =
        personal_edit::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    String::from_utf8(bytes.unwrap_or_default())
        .map_err(|error| format!("{}: {}", path.display(), error.utf8_error()))
}

fn parse(path: &Path, text: &str) -> Result<DocumentMut, String> {
    text.parse()
        .map_err(|error: toml_edit::TomlError| format!("{}: {error}", path.display()))
}

fn write(path: &Path, text: &str) -> Result<(), String> {
    write_file(path, text).map_err(|error| format!("{}: {error}", path.display()))
}

/// `text` as a TOML string.
fn quote(text: &str) -> String {
    toml::Value::String(text.to_owned()).to_string()
}

/// `path` from one of `roots`, `/` between its parts; `None` outside them,
/// for a root itself, or for a part that is not plain.
fn from_home(path: &Path, roots: &[&Path]) -> Option<String> {
    let rest = roots.iter().find_map(|root| path.strip_prefix(root).ok())?;
    let parts: Option<Vec<&str>> = rest
        .components()
        .map(|part| match part {
            Component::Normal(part) => part.to_str(),
            _ => None,
        })
        .collect();
    let parts = parts?;
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// Whether `path` is `folder` or lies in it, both from the home, in any
/// letter case.
fn within(path: &str, folder: &str) -> bool {
    path.len() >= folder.len()
        && path.is_char_boundary(folder.len())
        && path[..folder.len()].eq_ignore_ascii_case(folder)
        && (path.len() == folder.len() || path.as_bytes()[folder.len()] == b'/')
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    const PROJECT: &str = r#"requires = ">=0.0"
[tracker]
kind = "linear"
team = "OWL"
admit = { label = "agent" }
states = { ready = "Todo", working = "In Progress", needs_input = "Needs Input", review = "In Review" }

[stack]
gate = ["npm test"]
# The caches the setup reads.
caches = [
    # read from package-lock.json, the lock file of npm
    ".npm",
]

[pipeline]
default = "standard"
plan_approval = "on-fork"

[models]

[policy]
always_human = []
"#;

    fn repo() -> Repo {
        Repo::parse("acme/web").unwrap()
    }

    /// The guardrails of the one file system read (OWL-597, the maintainer's
    /// choice on Q2): only a candidate from a refusal is checked; never a
    /// credential path, under `ENOENT` neither checked nor named; never
    /// Owlshift's own folders, nor a folder that gathers others; a path the
    /// sandbox opens already is a write, never checked.
    #[test]
    fn only_a_plain_candidate_is_checked_and_named() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let checked = RefCell::new(Vec::new());
        let is_folder = |path: &Path| {
            checked.borrow_mut().push(path.to_owned());
            path.ends_with(".cache/demo") || path.ends_with(".cache/closed")
        };
        let ctx = Context::new(
            home,
            &[
                home.join(".npm"),
                home.join(".gitconfig"),
                PathBuf::from("/opt/x"),
            ],
            &[
                home.join("Library/Application Support/owlshift"),
                home.join("src/web"),
            ],
            &is_folder,
        );
        let output = [
            "mkdir: ~/.cache/demo/x: Operation not permitted",
            "mkdir: ~/.cache/new: Operation not permitted",
            "cat: ~/.cache/closed/f: No such file or directory",
            "cat: ~/.cache/gone/f: No such file or directory",
            "cat: ~/.ssh/id_ed25519: No such file or directory",
            "open '~/.aws/credentials': Operation not permitted",
            "EPERM: operation not permitted, open '~/.npm/_cacache/tmp/x'",
            "git: could not lock config file ~/.gitconfig: Operation not permitted",
            "EPERM ~/Library/Application Support/owlshift/projects/x",
            "ls: ~/.config: No such file or directory",
            "cat: ~/src/web/.env: Permission denied",
            "ls: ~/Documents: Operation not permitted",
        ]
        .join("\n")
        .replace('~', &home.display().to_string());
        let got: Vec<(String, State)> = refused(&output, &ctx)
            .into_iter()
            .map(|one| (one.folder, one.state))
            .collect();
        let want = [
            (".cache/demo", State::Closed),
            (".cache/new", State::Absent),
            (".cache/closed", State::Closed),
            (".aws", State::Credential),
            (".npm", State::Open),
            (".gitconfig", State::Open),
            ("Documents", State::Wide),
        ]
        .map(|(folder, state)| (folder.to_owned(), state));
        assert_eq!(got, want);
        assert_eq!(
            checked.into_inner(),
            [".cache/demo", ".cache/new", ".cache/closed", ".cache/gone"]
                .map(|folder| home.join(folder))
        );
    }

    #[test]
    fn the_notes_name_the_folders_and_the_lines() {
        let refused = [
            Refused {
                folder: ".cache/demo".into(),
                state: State::Closed,
            },
            Refused {
                folder: ".npm".into(),
                state: State::Open,
            },
            Refused {
                folder: ".aws".into(),
                state: State::Credential,
            },
            Refused {
                folder: ".cache".into(),
                state: State::Wide,
            },
        ];
        let lines = lines(
            &[".cache/demo".to_owned()],
            &[".npm".to_owned()],
            &[".npm".to_owned()],
            &[".npm"],
            &repo(),
        );
        assert_eq!(
            lines.caches.as_deref(),
            Some("caches = [\".npm\", \".cache/demo\"]")
        );
        assert_eq!(
            lines.allow.as_deref(),
            Some(
                "[repositories.\"github.com/acme/web\"]\nallow_caches = [\".npm\", \".cache/demo\"]"
            )
        );
        let note = gate_note(
            &refused,
            &lines,
            Path::new("/src/web/owlshift.toml"),
            Some(Path::new("/conf/config.toml")),
        );
        for said in [
            "The sandbox closes your home to every run and gate.",
            "  ~/.cache/demo\n",
            "  ~/.npm: open already, read-only",
            "  ~/.aws: it holds credentials, so the sandbox never opens it",
            "  ~/.cache: too wide",
            "  in /src/web/owlshift.toml, under [stack]:\n    caches = [\".npm\", \".cache/demo\"]\n",
            "  in /conf/config.toml:\n    [repositories.\"github.com/acme/web\"]\n    allow_caches",
            "$TMPDIR",
            "npm: use the setup `owlshift init` writes",
            "`sudo` changes nothing",
        ] {
            assert!(note.contains(said), "{said:?} not in:\n{note}");
        }
        assert!(!note.contains("sudo chown"), "{note}");

        let reason = reason_note(&refused, "acme/web").unwrap();
        assert!(
            !reason.contains('\n') && !reason.contains(".aws") && !reason.contains("~/.cache:")
        );
        for said in [
            "the sandbox closes the home and refused ~/.cache/demo: to open it read-only, add \
             \".cache/demo\" to stack.caches",
            "[repositories.\"github.com/acme/web\"]",
            "~/.npm is open already",
            "never sudo",
        ] {
            assert!(reason.contains(said), "{said:?} not in: {reason}");
        }
        assert_eq!(reason_note(&refused[2..], "acme/web"), None);
    }

    /// Both lines written, each file's comments kept, the repository's
    /// entry found in another letter case; a result its loader refuses
    /// writes neither file.
    #[test]
    fn the_offer_writes_both_files_and_keeps_their_comments() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("owlshift.toml");
        let personal = dir.path().join("config.toml");
        fs::write(&project, PROJECT).unwrap();
        let mine = "# my machine\nkeep_awake = true\n\n[repositories.\"GitHub.com/Acme/Web\"]\n\
                    allow_caches = [\n    \".npm\", # npm's\n]\n";
        fs::write(&personal, mine).unwrap();
        let folders = [".cache/demo".to_owned()];

        let written = open(&project, &personal, &repo(), &folders).unwrap();
        assert_eq!(
            written,
            Written {
                personal: true,
                project: true
            }
        );
        let personal_text = fs::read_to_string(&personal).unwrap();
        assert_eq!(
            personal_text,
            "# my machine\nkeep_awake = true\n\n[repositories.\"GitHub.com/Acme/Web\"]\n\
             allow_caches = [\n    \".npm\", # npm's\n    \".cache/demo\",\n]\n"
        );
        let project_text = fs::read_to_string(&project).unwrap();
        assert!(
            project_text.contains(
                "# The caches the setup reads.\ncaches = [\n    # read from package-lock.json, \
                 the lock file of npm\n    \".npm\",\n    \".cache/demo\",\n]\n"
            ),
            "{project_text}"
        );
        let loaded = ProjectConfig::parse(&project_text).unwrap();
        assert_eq!(loaded.stack.caches, [".npm", ".cache/demo"]);
        // Again: nothing to add, nothing written.
        assert_eq!(
            open(&project, &personal, &repo(), &folders).unwrap(),
            Written {
                personal: false,
                project: false
            }
        );

        // A personal file without the entry gets one; `init` writes it alone.
        fs::write(&personal, "keep_awake = true\n").unwrap();
        assert!(allow(&personal, &repo(), &folders).unwrap());
        assert_eq!(
            fs::read_to_string(&personal).unwrap(),
            "keep_awake = true\n\n[repositories.\"github.com/acme/web\"]\nallow_caches = \
             [\".cache/demo\"]\n"
        );

        // Over the 16 folders a project may declare: neither file changes.
        fs::write(&personal, "keep_awake = true\n").unwrap();
        let many: Vec<String> = (0..16).map(|n| format!(".cache/f{n}")).collect();
        let before = fs::read_to_string(&project).unwrap();
        let error = open(&project, &personal, &repo(), &many).unwrap_err();
        assert!(error.contains("owlshift.toml"), "{error}");
        assert_eq!(fs::read_to_string(&project).unwrap(), before);
        assert_eq!(
            fs::read_to_string(&personal).unwrap(),
            "keep_awake = true\n"
        );
    }
}
