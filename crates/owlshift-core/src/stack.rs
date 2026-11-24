//! Stack detection for `owlshift init` (P9, OWL-539): the gate, the setup it
//! needs and the caches of Owlshift's own its runs may write
//! (`stack.run_caches`, OWL-617), read from the names and contents of the
//! repository's root files, and of the folders below the root that hold a
//! project of their own (OWL-630, [`reads_folder`], [`CONTAINERS`]).
//!
//! A pure function: the caller reads the root and passes in the file names and
//! the contents of the four manifests it recognises (`Cargo.toml`,
//! `package.json`, `pyproject.toml`, `Makefile`); lock files, yarn's
//! `.yarnrc.yml`, and the files of Go, pip, bundler and Gradle (OWL-624), of
//! pdm, Maven, Composer and .NET (OWL-627), of Elixir, Dart, Swift and Deno
//! (OWL-628), are read by name only; asdf's `.tool-versions` is read for the
//! tools it names (OWL-631). Nothing detected here is ever run. The
//! commands were checked with real calls, recorded in
//! `docs/design/runtime-and-operations.md`, "Stack detection (P9)", and in the
//! records of OWL-617, OWL-624, OWL-627, OWL-628 and OWL-629; a tool that
//! could not be checked is not detected, and a run cache whose live check did
//! not show it needed is not written, only named in a note.

use serde::{Deserialize, Serialize};

/// The root of a repository, as the runner read it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootFiles {
    /// The names of the files at the root.
    pub names: Vec<String>,
    /// The contents of the manifests the runner read, by name. A manifest in
    /// `names` with no entry here could not be read.
    pub files: Vec<(String, String)>,
    /// The version managers of the machine whose installs lie in the home
    /// (OWL-631), as the runner found them: not of the root, but read with it
    /// so that detection stays a function of what the runner read.
    #[serde(default)]
    pub home_managers: Vec<HomeManager>,
    /// The folders below the root the runner read (OWL-630): each folder of
    /// the root that [`reads_folder`] allows, and each such folder of the
    /// [`CONTAINERS`], in name order, a container before its folders.
    #[serde(default)]
    pub folders: Vec<Folder>,
}

/// A folder below the root, as the runner read it (OWL-630): its names and
/// manifests, as [`RootFiles`] holds the root's.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Folder {
    /// Its path from the root, its parts joined with `/`: `backend`,
    /// `apps/web`.
    pub path: String,
    /// The names of the files in it.
    pub names: Vec<String>,
    /// The contents of its manifests the runner read, by name.
    pub files: Vec<(String, String)>,
    /// The runner could not list it, so nothing of it is known: reported as
    /// skipped, never taken for an empty folder.
    #[serde(default)]
    pub unlisted: bool,
}

/// The folders of the root whose own folders are read too (OWL-630): they
/// gather projects, one folder each.
pub const CONTAINERS: [&str; 2] = ["apps", "services"];

/// The folders below the root never read (OWL-630): what package managers
/// and builds write, not a project.
const NOT_READ: [&str; 5] = ["node_modules", "vendor", "target", "dist", "build"];

/// Whether the runner reads the folder `name` below the root (OWL-630): not
/// a hidden folder, not `node_modules`, `vendor`, `target`, `dist` or
/// `build`, not a name holding a control character, which no `cd` line
/// should carry.
pub fn reads_folder(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !name.chars().any(char::is_control)
        && !NOT_READ.contains(&name)
}

/// A version manager on the machine's `PATH` whose installs folder lies in
/// the home, which a confined run cannot read (OWL-631).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HomeManager {
    /// One of [`HOME_MANAGERS`].
    pub name: String,
    /// Its installs folder, relative to the home: `.pyenv`.
    pub folder: String,
}

/// The version managers the runner looks for (OWL-631): pyenv's Pythons,
/// rbenv's Rubies, asdf's tool chains, and the tools pipx installs.
pub const HOME_MANAGERS: [&str; 4] = ["pyenv", "rbenv", "asdf", "pipx"];

impl RootFiles {
    fn has(&self, name: &str) -> bool {
        self.names.iter().any(|n| n == name)
    }

    fn content(&self, name: &str) -> Option<&str> {
        self.files
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, c)| c.as_str())
    }
}

/// One command, with what it was read from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Line {
    pub command: String,
    pub read_from: String,
}

/// What one root file gave.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Found {
    /// The file, and the lock file that chose the manager when there is one:
    /// `package.json (pnpm-lock.yaml)`.
    pub source: String,
    pub gate: Vec<Line>,
    pub setup: Vec<Line>,
    /// The caches of Owlshift's own the runs may write, as
    /// `stack.run_caches` declares them (OWL-617).
    pub run_caches: Vec<RunCache>,
    /// What the person should know about the lines above.
    pub notes: Vec<String>,
    /// What the person should know about the gate lines alone, written only
    /// where the detected gate is used, never above a gate given by hand
    /// (OWL-595).
    pub gate_notes: Vec<String>,
}

/// One cache a run may write, with what it was read from (OWL-617).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunCache {
    /// The variable the package manager reads for its cache.
    pub variable: String,
    /// The name of the folder of Owlshift's own the runner sets it to.
    pub folder: String,
    pub read_from: String,
}

/// The result of [`detect`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Detection {
    /// In the fixed order `Cargo.toml`, `go.mod`, `package.json`, Python
    /// (`pyproject.toml`, `requirements.txt`), `Gemfile`, the tool chains in
    /// the home (OWL-631), Gradle, Maven,
    /// Composer, .NET, Elixir, Dart, Swift, Deno, `Makefile`; then each
    /// folder below the root in the order the runner read them, its sources
    /// in the same order (OWL-630).
    pub found: Vec<Found>,
    /// A manifest that could not be read or parsed, or a folder that could
    /// not be listed, with why. Never an error.
    pub skipped: Vec<String>,
}

impl Detection {
    /// The detected gate commands, in order.
    pub fn gate(&self) -> impl Iterator<Item = &Line> {
        self.found.iter().flat_map(|f| f.gate.iter())
    }

    /// The detected setup commands, in order.
    pub fn setup(&self) -> impl Iterator<Item = &Line> {
        self.found.iter().flat_map(|f| f.setup.iter())
    }

    /// The detected caches a run may write, in order, one per source: a
    /// variable two sources name comes twice.
    pub fn run_caches(&self) -> impl Iterator<Item = &RunCache> {
        self.found.iter().flat_map(|f| f.run_caches.iter())
    }

    /// The detected caches a run may write, one per variable (OWL-630): the
    /// project file names a variable once, and its folder serves every
    /// source that names it, as it serves every run of the project. A
    /// variable keeps its first folder, every source naming one variable
    /// with one folder, and says what each source read it from.
    pub fn merged_run_caches(&self) -> Vec<RunCache> {
        let mut merged: Vec<RunCache> = vec![];
        for cache in self.run_caches() {
            match merged.iter_mut().find(|m| m.variable == cache.variable) {
                Some(kept) => {
                    if !kept.read_from.split("; ").any(|r| r == cache.read_from) {
                        kept.read_from = format!("{}; {}", kept.read_from, cache.read_from);
                    }
                }
                None => merged.push(cache.clone()),
            }
        }
        merged
    }
}

/// The manifests [`detect`] reads, which the runner must read: asdf's
/// `.tool-versions` included, for the tools it names (OWL-631).
pub const MANIFESTS: [&str; 5] = [
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "Makefile",
    ".tool-versions",
];

/// One source of [`detect`]: what it found in the root, `None` when the root
/// holds nothing of it, or why its manifest was skipped.
type Source = fn(&RootFiles) -> Result<Option<Found>, String>;

/// The name the tool chains in the home are skipped under (OWL-631).
const TOOL_VERSIONS: &str = ".tool-versions";

/// The source the tool chains in the home are found under (OWL-631).
const HOME_TOOLCHAINS: &str = "tool chains in the home";

/// The sources of [`detect`], in their fixed order, each with the name it is
/// skipped under, which also names its kind, and the files that make a
/// folder a project of its own beside a root of the same kind (OWL-630). A
/// member of the root's workspace has none of them: the root's lock or
/// wrapper serves it, and the root's lines cover it. Only a source that reads
/// a manifest's text, of `MANIFESTS`, can be skipped; the others read names
/// and keep a name here for the list's sake. A Go module is always its own,
/// since `./...` stops at a nested `go.mod`; a .NET project or a Makefile
/// beside a root of its kind is the root's.
const SOURCES: [(&str, Source, &[&str]); 15] = [
    ("Cargo.toml", cargo, &["Cargo.lock"]),
    ("go.mod", go, &["go.mod"]),
    (
        "package.json",
        package_json,
        &[
            "pnpm-lock.yaml",
            "yarn.lock",
            "bun.lock",
            "bun.lockb",
            "package-lock.json",
        ],
    ),
    (
        "pyproject.toml",
        python,
        &["uv.lock", "poetry.lock", "pdm.lock", "requirements.txt"],
    ),
    ("Gemfile", ruby, &["Gemfile.lock"]),
    // Never the root's: see `fresh_toolchain_notes`.
    (TOOL_VERSIONS, home_toolchains, &[]),
    ("gradlew", gradle, &["gradlew"]),
    ("pom.xml", maven, &["mvnw"]),
    ("composer.json", composer, &["composer.lock"]),
    (".NET", dotnet, &[]),
    ("mix.exs", elixir, &["mix.lock"]),
    ("pubspec.yaml", dart, &["pubspec.lock"]),
    ("Package.swift", swift, &["Package.resolved"]),
    ("deno.json", deno, &["deno.lock"]),
    ("Makefile", makefile, &[]),
];

/// Detect the stack of a root, then of the folders below it (OWL-630): a
/// folder's sources are those of a root, its lines run in it, and a source
/// whose kind the root found, or the folder's container (`apps/web`'s
/// `apps`), is left to them unless the folder holds its own lock or
/// wrapper, which makes it a project of its own.
pub fn detect(root: &RootFiles) -> Detection {
    let mut out = Detection::default();
    let mut toolchain_notes = vec![];
    let root_kinds = run_sources(root, None, &[], &mut out, &mut toolchain_notes);
    for folder in &root.folders {
        if folder.unlisted {
            out.skipped
                .push(format!("{}/: it could not be listed", folder.path));
            continue;
        }
        let mut covered = root_kinds.clone();
        if let Some((container, _)) = folder.path.split_once('/')
            && let Some(parent) = root.folders.iter().find(|f| f.path == container)
        {
            covered.extend(kinds(&folder_files(parent, root)));
        }
        run_sources(
            &folder_files(folder, root),
            Some(&folder.path),
            &covered,
            &mut out,
            &mut toolchain_notes,
        );
    }
    out
}

/// The files of `folder`, read as a root's, with the machine's version
/// managers the runner found for the root.
fn folder_files(folder: &Folder, root: &RootFiles) -> RootFiles {
    RootFiles {
        names: folder.names.clone(),
        files: folder.files.clone(),
        home_managers: root.home_managers.clone(),
        folders: vec![],
    }
}

/// The kinds of [`SOURCES`] that found something in `files`.
fn kinds(files: &RootFiles) -> Vec<&'static str> {
    SOURCES
        .iter()
        .filter(|(_, source, _)| matches!(source(files), Ok(Some(_))))
        .map(|(name, _, _)| *name)
        .collect()
}

/// Runs every source on `files` and adds to `out` what it found: the root's
/// as it is (`at` is `None`), a folder's run in it, unless `covered` holds
/// its kind and the folder none of its own files. Returns the kinds found.
fn run_sources(
    files: &RootFiles,
    at: Option<&str>,
    covered: &[&str],
    out: &mut Detection,
    toolchain_notes: &mut Vec<String>,
) -> Vec<&'static str> {
    let mut found_kinds = vec![];
    for (name, source, own) in SOURCES {
        let result = source(files);
        if matches!(result, Ok(Some(_))) {
            found_kinds.push(name);
        }
        // A member of the root's workspace, served by the root's lines.
        let rootward = name != TOOL_VERSIONS
            && covered.contains(&name)
            && !own.iter().any(|file| files.has(file));
        if at.is_some() && rootward {
            continue;
        }
        match result {
            Ok(Some(found)) => {
                let found = if name == TOOL_VERSIONS {
                    match fresh_toolchain_notes(found, toolchain_notes) {
                        Some(found) => found,
                        None => continue,
                    }
                } else {
                    found
                };
                out.found.push(match at {
                    Some(path) => in_folder(found, path),
                    None => found,
                });
            }
            Ok(None) => {}
            Err(why) => out.skipped.push(match at {
                Some(path) => format!("{path}/{name}: {why}"),
                None => format!("{name}: {why}"),
            }),
        }
    }
    found_kinds
}

/// The tool chains in the home, less the notes already written (OWL-630):
/// they speak of the machine, so a root and its folders, or two folders,
/// would repeat them word for word. `None` when every note was.
fn fresh_toolchain_notes(mut found: Found, written: &mut Vec<String>) -> Option<Found> {
    found.notes.retain(|note| !written.contains(note));
    written.extend(found.notes.iter().cloned());
    (!found.notes.is_empty()).then_some(found)
}

/// What a folder's source found, made true from the root (OWL-630): its
/// lines run in the folder, and what they were read from, its notes and its
/// source name the folder. Every field is named, so a field added to
/// [`Found`] is placed here before it compiles.
fn in_folder(found: Found, path: &str) -> Found {
    let Found {
        source,
        gate,
        setup,
        run_caches,
        notes,
        gate_notes,
    } = found;
    let from = |read_from: String| format!("{read_from}, in {path}/");
    let lines = |lines: Vec<Line>| -> Vec<Line> {
        lines
            .into_iter()
            .map(|l| Line {
                command: run_in(path, &l.command),
                read_from: from(l.read_from),
            })
            .collect()
    };
    let note = |note: String| format!("in `{path}/`: {note}");
    Found {
        source: if source == HOME_TOOLCHAINS {
            format!("{source}, for {path}/")
        } else {
            format!("{path}/{source}")
        },
        gate: lines(gate),
        setup: lines(setup),
        run_caches: run_caches
            .into_iter()
            .map(|cache| RunCache {
                read_from: from(cache.read_from),
                ..cache
            })
            .collect(),
        notes: notes.into_iter().map(note).collect(),
        gate_notes: gate_notes.into_iter().map(note).collect(),
    }
}

/// `command` run in the folder `path` from the root, where every line of
/// the gate and the setup starts (OWL-630). `&&` binds tighter than `;`,
/// `||`, `&` and a line break, so a command holding one is put in braces,
/// lest the `cd` cover its first part alone; braces never change what a
/// command does, so a match inside quotes costs only the braces. A path
/// starting with `-` gets `./`, which `cd` would read as an option.
fn run_in(path: &str, command: &str) -> String {
    let dir = if path.starts_with('-') {
        shell_word(&format!("./{path}"))
    } else {
        shell_word(path)
    };
    let looser = command.contains(';')
        || command.contains("||")
        || command.contains('\n')
        || command.replace("&&", "").contains('&');
    if looser {
        format!("cd {dir} && {{ {command}; }}")
    } else {
        format!("cd {dir} && {command}")
    }
}

/// The text of the manifest `name`, `None` when the root has no such file; an
/// error when it has one the runner could not read.
fn manifest<'a>(root: &'a RootFiles, name: &str) -> Result<Option<&'a str>, String> {
    if !root.has(name) {
        return Ok(None);
    }
    root.content(name)
        .map(Some)
        .ok_or_else(|| "it could not be read".to_string())
}

/// What `source` gave, with no line yet.
fn found(source: impl Into<String>) -> Found {
    Found {
        source: source.into(),
        gate: vec![],
        setup: vec![],
        run_caches: vec![],
        notes: vec![],
        gate_notes: vec![],
    }
}

fn line(command: impl Into<String>, read_from: impl Into<String>) -> Line {
    Line {
        command: command.into(),
        read_from: read_from.into(),
    }
}

fn run_cache(variable: &str, folder: &str, read_from: impl Into<String>) -> RunCache {
    RunCache {
        variable: variable.to_owned(),
        folder: folder.to_owned(),
        read_from: read_from.into(),
    }
}

fn cargo(root: &RootFiles) -> Result<Option<Found>, String> {
    let Some(text) = manifest(root, "Cargo.toml")? else {
        return Ok(None);
    };
    let table: toml::Table = text
        .parse()
        .map_err(|_| "it is not valid TOML".to_string())?;
    if !table.contains_key("package") && !table.contains_key("workspace") {
        return Ok(None);
    }
    let locked = if root.has("Cargo.lock") {
        " --locked"
    } else {
        ""
    };
    let from = "Cargo.toml";
    Ok(Some(Found {
        source: from.to_string(),
        gate: vec![
            line("cargo fmt --all -- --check", from),
            line(
                format!("cargo clippy --workspace --all-targets{locked} -- -D warnings"),
                from,
            ),
            line(format!("cargo test --workspace{locked}"), from),
        ],
        setup: vec![],
        // Checked live (OWL-617): cargo with `CARGO_HOME` moved, rustup's
        // proxies and the tool chain left where they are.
        run_caches: vec![run_cache("CARGO_HOME", "cargo", from)],
        notes: vec![],
        gate_notes: vec![],
    }))
}

/// A Go module, from `go.mod`, read by name (OWL-624): its modules fetched
/// by the setup, then `gofmt`, `go vet` and `go test` over every package.
/// Checked live, confined: the setup fails creating `~/go/pkg/mod` unless
/// `GOMODCACHE` is moved, and `go vet` its build cache unless `GOCACHE` is;
/// with both moved the gate passes, and the `gofmt` line fails naming a file
/// it would change. A `go.mod` that pins a newer Go than the machine's makes
/// Go download that tool chain as a module, into `GOMODCACHE`.
fn go(root: &RootFiles) -> Result<Option<Found>, String> {
    if !root.has("go.mod") {
        return Ok(None);
    }
    let from = "go.mod";
    let mut found = found(from);
    found.setup.push(line("go mod download", from));
    found.gate = vec![
        // `gofmt -l` lists the files it would change and exits 0 whatever it
        // finds: the line fails when it lists one, and names it.
        line("! gofmt -l . | grep .", from),
        line("go vet ./...", from),
        line("go test ./...", from),
    ];
    found.run_caches = vec![
        run_cache("GOMODCACHE", "go-mod", from),
        run_cache("GOCACHE", "go-build", from),
    ];
    found.notes.push(
        "Go keeps the modules it downloads read-only: to empty the run caches, make their folder \
         writable (`chmod -R u+w`) before deleting it"
            .to_string(),
    );
    Ok(Some(found))
}

/// A Node package manager, as the lock file at the root names it.
struct NodeManager {
    name: &'static str,
    lock: &'static str,
    /// The setup's install command.
    install: &'static str,
    /// The caches of Owlshift's own the install writes (OWL-617, OWL-629):
    /// the variable the manager reads for each and the folder's name.
    run_caches: &'static [(&'static str, &'static str)],
    /// What the person should know about the run caches.
    note: Option<&'static str>,
}

/// corepack's own folder, where it installs the pnpm or yarn a
/// `packageManager` pins when the machine's `pnpm` or `yarn` is its shim
/// (OWL-629).
const COREPACK: (&str, &str) = ("COREPACK_HOME", "corepack");

/// pnpm's run caches (OWL-629): its home, which holds the store and the
/// versions pnpm 10 and later switch to when `packageManager` pins another;
/// the folder pnpm 12 takes the store's locks in; corepack's.
const PNPM: [(&str, &str); 3] = [
    ("PNPM_HOME", "pnpm"),
    ("XDG_RUNTIME_DIR", "pnpm-locks"),
    COREPACK,
];

/// pnpm's run caches when `packageManager` pins a pnpm below 12, which takes
/// no lock there.
const PNPM_BELOW_12: [(&str, &str); 2] = [PNPM[0], COREPACK];

/// yarn 2 or later's run caches (OWL-629): its global folder, which it makes
/// whatever its cache settings, and corepack's.
const YARN_BERRY: [(&str, &str); 2] = [("YARN_GLOBAL_FOLDER", "yarn"), COREPACK];

/// The `packageManager` of `package.json` when it names `name`, with its
/// major version: `("pnpm@10.33.2", 10)`. `None` for another manager, or a
/// version that does not start with a number.
fn package_manager<'a>(package: &'a serde_json::Value, name: &str) -> Option<(&'a str, u32)> {
    let manager = package.get("packageManager")?.as_str()?;
    let major = manager
        .strip_prefix(name)?
        .strip_prefix('@')?
        .split('.')
        .next()?
        .parse()
        .ok()?;
    Some((manager, major))
}

/// Which manager the lock file names, its install, and the caches of
/// Owlshift's own its runs write. Each was checked live, confined (OWL-593,
/// OWL-617, OWL-629): yarn 1 skips a cache it cannot write and bun falls
/// back to one of its own, both fetching from the registry, so they need
/// none; npm writes `npm_config_cache`. pnpm of every major keeps its store
/// in `PNPM_HOME`, and pnpm 10 and later download there the version
/// `packageManager` pins when it is not theirs; pnpm 12 takes the store's
/// locks in `XDG_RUNTIME_DIR`, otherwise in the shared `/tmp`, which the
/// sandbox closes, so only a pin below 12 goes without it. yarn 2 and
/// later, named by `.yarnrc.yml` or by a `packageManager` pinning it, make
/// their global folder whatever their cache settings. A pnpm or a yarn the
/// machine reaches through corepack is installed into `COREPACK_HOME`.
fn node_manager(root: &RootFiles, package: &serde_json::Value) -> Option<NodeManager> {
    let plain = |name, lock, install| NodeManager {
        name,
        lock,
        install,
        run_caches: &[],
        note: None,
    };
    if root.has("pnpm-lock.yaml") {
        let below_12 = package_manager(package, "pnpm").is_some_and(|(_, major)| major < 12);
        Some(NodeManager {
            run_caches: if below_12 { &PNPM_BELOW_12 } else { &PNPM },
            note: (!below_12).then_some(
                "`XDG_RUNTIME_DIR` is the folder pnpm 12 takes its store's locks in, the shared \
                 `/tmp` otherwise, which confined runs cannot write; pnpm 11 and earlier leave it \
                 empty",
            ),
            ..plain("pnpm", "pnpm-lock.yaml", "pnpm install --frozen-lockfile")
        })
    } else if root.has("yarn.lock") {
        let berry = root.has(".yarnrc.yml")
            || package_manager(package, "yarn").is_some_and(|(_, major)| major >= 2);
        Some(if berry {
            NodeManager {
                run_caches: &YARN_BERRY,
                ..plain("yarn", "yarn.lock", "yarn install --immutable")
            }
        } else {
            plain("yarn", "yarn.lock", "yarn install --frozen-lockfile")
        })
    } else if root.has("bun.lock") {
        Some(plain("bun", "bun.lock", "bun install --frozen-lockfile"))
    } else if root.has("bun.lockb") {
        Some(plain("bun", "bun.lockb", "bun install --frozen-lockfile"))
    } else if root.has("package-lock.json") {
        Some(NodeManager {
            run_caches: &[("npm_config_cache", "npm")],
            ..plain("npm", "package-lock.json", "npm ci")
        })
    } else {
        None
    }
}

fn package_json(root: &RootFiles) -> Result<Option<Found>, String> {
    let Some(text) = manifest(root, "package.json")? else {
        return Ok(None);
    };
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|_| "it is not valid JSON".to_string())?;
    let scripts = value.get("scripts").and_then(|s| s.as_object());
    let script = |key: &str| scripts.and_then(|s| s.get(key)).and_then(|v| v.as_str());
    let mut found = found("package.json");
    let manager = match node_manager(root, &value) {
        Some(manager) => {
            let (name, lock) = (manager.name, manager.lock);
            let from = format!("{lock}, the lock file of {name}");
            found.source = format!("package.json ({lock})");
            found.setup.push(line(manager.install, from.clone()));
            found.notes.extend(manager.note.map(str::to_owned));
            found.run_caches = manager
                .run_caches
                .iter()
                .map(|(variable, folder)| run_cache(variable, folder, from.clone()))
                .collect();
            name
        }
        None => {
            // An install here would write a lock file into the repository.
            found.notes.push(
                "package.json has no lock file, so no setup was written: the gate needs a \
                 setup that installs the dependencies"
                    .to_string(),
            );
            "npm"
        }
    };
    // The order a developer runs them in: lint, typecheck, test, build.
    let typecheck = ["typecheck", "type-check"]
        .into_iter()
        .find(|key| script(key).is_some());
    let mut read: Vec<&str> = vec![];
    let mut left_out: Vec<&str> = vec![];
    for key in ["lint", typecheck.unwrap_or("typecheck"), "test", "build"] {
        let Some(body) = script(key) else { continue };
        if key == "test" && body.contains("no test specified") {
            found.notes.push(
                "the `test` script is npm's placeholder, which always fails: it was left out"
                    .to_string(),
            );
            left_out.push(key);
            continue;
        }
        read.push(key);
        found.gate.push(line(
            format!("{manager} run {key}"),
            format!("the `{key}` script of package.json"),
        ));
    }
    if !found.gate.is_empty() {
        // Every other script is named, sorted, so the operator sees what the
        // guess does not cover.
        let others: Vec<&str> = scripts
            .into_iter()
            .flat_map(|s| s.keys().map(String::as_str))
            .filter(|key| !read.contains(key) && !left_out.contains(key))
            .collect();
        left_out.extend(others);
        if let Some(chain) = ["check", "ci"]
            .into_iter()
            .find(|key| script(key).is_some())
        {
            found.gate_notes.push(format!(
                "package.json has a `{chain}` script, which the project may run before any work \
                 is done: compare it with this gate"
            ));
        }
        let names = |keys: &[&str]| {
            let shown: Vec<String> = keys.iter().take(8).map(|k| format!("`{k}`")).collect();
            let more = keys.len().saturating_sub(8);
            let tail = if more > 0 {
                format!(" and {more} more")
            } else {
                String::new()
            };
            format!("{}{tail}", shown.join(", "))
        };
        let out = if left_out.is_empty() {
            "none".to_string()
        } else {
            names(&left_out)
        };
        found.gate_notes.push(format!(
            "this gate is a guess from package.json, to review: it read the scripts {}; \
             it left out {out}",
            names(&read)
        ));
    }
    Ok(Some(found))
}

/// A Python tool the gate runs: its command, and what it was read from.
type Tool = (&'static str, &'static str);

/// Python, from the tools `pyproject.toml` configures and the file that names
/// its manager, first found in the order `uv.lock`, `poetry.lock`,
/// `pdm.lock` (each beside `pyproject.toml`), then pip's `requirements.txt`
/// (OWL-624). A manager's lock gives its setup and run caches even when no
/// tool is configured, with a note that no gate was written.
fn python(root: &RootFiles) -> Result<Option<Found>, String> {
    let tools = match manifest(root, "pyproject.toml")? {
        Some(text) => python_tools(text)?,
        None => vec![],
    };
    if root.has("pyproject.toml")
        && let Some(manager) = PYTHON_MANAGERS.iter().find(|m| root.has(m.lock))
    {
        return Ok(Some(locked_manager(manager, &tools)));
    }
    if root.has("requirements.txt") {
        return Ok(Some(pip(root, &tools)));
    }
    if tools.is_empty() {
        return Ok(None);
    }
    // A bare tool needs a setup.
    let mut found = found("pyproject.toml");
    found.notes.push(
        "pyproject.toml's tools are run bare and no setup was written: the gate needs a setup \
         that installs them"
            .to_string(),
    );
    found.gate = tool_lines("", &tools);
    Ok(Some(found))
}

/// The gate lines of `tools`, each run through `prefix`.
fn tool_lines(prefix: &str, tools: &[Tool]) -> Vec<Line> {
    tools
        .iter()
        .map(|(command, from)| line(format!("{prefix}{command}"), *from))
        .collect()
}

/// The note of a manager whose `pyproject.toml` configures no tool this
/// detection knows.
fn no_tool_note(prefix: &str) -> String {
    format!(
        "pyproject.toml configures neither `[tool.ruff]` nor `[tool.pytest.ini_options]`, so no \
         gate was written: add the project's lint and test commands, each run through `{prefix}`"
    )
}

/// A Python manager a lock file beside `pyproject.toml` names: its setup,
/// the run caches it writes, and the tools run through it.
struct PythonManager {
    name: &'static str,
    lock: &'static str,
    setup: Option<&'static str>,
    /// The variable and folder of each run cache.
    run_caches: &'static [(&'static str, &'static str)],
    notes: &'static [&'static str],
}

/// The Python managers, in the order their lock files are looked for.
///
/// - uv: `uv run` installs the locked environment itself, so there is no
///   setup (OWL-539). Checked live (OWL-617): confined, uv needs both run
///   caches on macOS, where it fails reading the closed
///   `~/.local/share/uv/python` otherwise.
/// - poetry (OWL-624): `poetry install` installs the locked environment into
///   a virtualenv in poetry's cache, one per worktree, which
///   `POETRY_CACHE_DIR` moves into a run cache. Checked live, confined:
///   without it, poetry fails writing its closed cache
///   (`~/Library/Caches/pypoetry` on macOS).
/// - pdm (OWL-627): `pdm install` installs the locked environment into the
///   worktree's `.venv`, which ignores itself. Checked live, confined: pdm
///   fails writing its closed cache (`~/Library/Caches/pdm`) unless
///   `PDM_CACHE_DIR` moves it, and its closed log folder
///   (`~/Library/Logs/pdm`) unless `PDM_LOG_DIR` does.
const PYTHON_MANAGERS: [PythonManager; 3] = [
    PythonManager {
        name: "uv",
        lock: "uv.lock",
        setup: None,
        run_caches: &[
            ("UV_CACHE_DIR", "uv"),
            ("UV_PYTHON_INSTALL_DIR", "uv-python"),
        ],
        notes: &[],
    },
    PythonManager {
        name: "poetry",
        lock: "poetry.lock",
        setup: Some("poetry install"),
        run_caches: &[("POETRY_CACHE_DIR", "poetry")],
        notes: &[],
    },
    PythonManager {
        name: "pdm",
        lock: "pdm.lock",
        setup: Some("pdm install"),
        run_caches: &[("PDM_CACHE_DIR", "pdm"), ("PDM_LOG_DIR", "pdm-log")],
        notes: &[
            "pdm saves the Python it chose in `.pdm-python`, beside `pyproject.toml`: ignore it in \
                  `.gitignore`, as `pdm init` does, since a setup that leaves a file git sees \
                  fails the gate",
        ],
    },
];

/// What a Python manager's lock gives: its setup and run caches, and the
/// tools run through it, or a note when there is none.
fn locked_manager(manager: &PythonManager, tools: &[Tool]) -> Found {
    let PythonManager { name, lock, .. } = manager;
    let mut found = found(format!("pyproject.toml ({lock})"));
    let from = format!("{lock}, the lock file of {name}");
    found
        .setup
        .extend(manager.setup.map(|setup| line(setup, from.clone())));
    found.run_caches = manager
        .run_caches
        .iter()
        .map(|(variable, folder)| run_cache(variable, folder, from.clone()))
        .collect();
    found.gate = tool_lines(&format!("{name} run "), tools);
    found
        .notes
        .extend(manager.notes.iter().map(|n| n.to_string()));
    if tools.is_empty() {
        found.notes.push(no_tool_note(&format!("{name} run")));
    }
    found
}

/// The first line of pip's setup: a virtualenv in the worktree that ignores
/// itself, as Python 3.13 and later make it, so that the setup changes no
/// file git sees whatever the Python and the project's `.gitignore`.
const PIP_VENV: &str = "python3 -m venv .venv && echo '*' > .venv/.gitignore";

/// pip (OWL-624), from `requirements.txt` with no other lock, and
/// `requirements-dev.txt` beside it: a virtualenv in the worktree, the gate's
/// tools run from it. Only with a tool `pyproject.toml` configures, since a
/// `requirements.txt` at the root may serve something else, such as a
/// documentation build: otherwise a note says what to write. Checked live,
/// confined: pip turns off a cache it cannot write and installs anyway, so
/// `PIP_CACHE_DIR` is not declared, only named in a note.
fn pip(root: &RootFiles, tools: &[Tool]) -> Found {
    let (files, install) = if root.has("requirements-dev.txt") {
        (
            "requirements.txt and requirements-dev.txt",
            ".venv/bin/pip install -r requirements.txt -r requirements-dev.txt",
        )
    } else {
        (
            "requirements.txt",
            ".venv/bin/pip install -r requirements.txt",
        )
    };
    if tools.is_empty() {
        let mut found = found("requirements.txt");
        found.notes.push(format!(
            "pip's requirements are in {files}, but pyproject.toml configures neither \
             `[tool.ruff]` nor `[tool.pytest.ini_options]`, so nothing was written: a Python \
             project's setup is `{PIP_VENV}` then `{install}`, and its gate runs its tools from \
             `.venv/bin/`"
        ));
        return found;
    }
    let from = format!("{files}, the requirements of pip");
    let mut found = found("pyproject.toml (requirements.txt)");
    found.setup = vec![line(PIP_VENV, from.clone()), line(install, from)];
    found.gate = tool_lines(".venv/bin/", tools);
    found.notes.push(
        "no run cache was declared for pip, which turns off a cache it cannot write and installs \
         anyway: `run_caches.PIP_CACHE_DIR = \"pip\"` keeps its downloads for the next runs"
            .to_string(),
    );
    found
}

/// The tools `pyproject.toml` configures, in the order the gate runs them:
/// `ruff check .` for `[tool.ruff]`, `pytest` for `[tool.pytest.ini_options]`
/// (`[tool.pytest]` alone was not checked, OWL-539).
fn python_tools(text: &str) -> Result<Vec<Tool>, String> {
    let table: toml::Table = text
        .parse()
        .map_err(|_| "it is not valid TOML".to_string())?;
    let tool = table.get("tool").and_then(|t| t.as_table());
    let has = |name: &str| tool.is_some_and(|t| t.contains_key(name));
    let pytest = tool
        .and_then(|t| t.get("pytest"))
        .and_then(|p| p.as_table())
        .is_some_and(|p| p.contains_key("ini_options"));
    let mut tools = vec![];
    if has("ruff") {
        tools.push(("ruff check .", "[tool.ruff] in pyproject.toml"));
    }
    if pytest {
        tools.push(("pytest", "[tool.pytest.ini_options] in pyproject.toml"));
    }
    Ok(tools)
}

/// Ruby, from `Gemfile` (OWL-624), read by name with `Gemfile.lock` and
/// `.rspec`: bundler installs the locked gems into `BUNDLE_PATH`, a run cache,
/// and the gate is RSpec's when `.rspec` is there. The install is bundler's
/// plain one: a setup that rewrote `Gemfile.lock` fails the gate, which
/// allows it to change ignored files only, and bundler 1.17 given
/// `BUNDLE_FROZEN` writes it into `.bundle/config` (checked live). Checked
/// live, confined: without `BUNDLE_PATH`, bundler fails writing the gems where
/// the machine's Ruby keeps them.
fn ruby(root: &RootFiles) -> Result<Option<Found>, String> {
    if !root.has("Gemfile") {
        return Ok(None);
    }
    let mut found = found("Gemfile");
    if root.has("Gemfile.lock") {
        found.source = "Gemfile (Gemfile.lock)".to_string();
        let from = "Gemfile.lock, the lock file of bundler";
        found.setup.push(line("bundle install", from));
        found
            .run_caches
            .push(run_cache("BUNDLE_PATH", "bundle", from));
    } else {
        // An install here would write a lock file into the repository.
        found.notes.push(
            "Gemfile has no lock file, so no setup was written: the gate needs a setup that \
             installs the gems"
                .to_string(),
        );
    }
    if root.has(".rspec") {
        found
            .gate
            .push(line("bundle exec rspec", ".rspec, the settings of RSpec"));
    } else {
        found.notes.push(
            "no Ruby gate was written, only `.rspec` being read for one: add the project's tests, \
             such as `bundle exec rake` or `bin/rails test`"
                .to_string(),
        );
    }
    Ok(Some(found))
}

/// What declaring a folder of the home takes, as `owlshift gate` proposes it.
fn declare(folder: &str) -> String {
    format!(
        "declare `caches = [\"{folder}\"]` under `[stack]` and allow `{folder}` in your personal \
         `allow_caches`"
    )
}

/// The tool chains a version manager installs in the home (OWL-631), which a
/// confined run cannot read (architecture section 8, "Tool chains"): a note
/// each, nothing written, since the folder is the machine's, not the
/// project's. A manager the runner found on `PATH` with its installs in the
/// home gives the folder to declare, for the languages the root holds; the
/// files that pin a version (`.python-version`, `.ruby-version`,
/// `.tool-versions`) give a note naming the managers that install it, unless
/// a manager found reads that file. Only poetry was seen to fail on pyenv's
/// folder (OWL-624), so a manager's note says when it applies.
fn home_toolchains(root: &RootFiles) -> Result<Option<Found>, String> {
    let tools: Vec<&str> = match manifest(root, ".tool-versions")? {
        Some(text) => text
            .lines()
            .map(str::trim)
            .filter(|l| !l.starts_with('#'))
            .filter_map(|l| l.split_whitespace().next())
            .collect(),
        None => vec![],
    };
    let python = ["pyproject.toml", "requirements.txt", ".python-version"]
        .iter()
        .any(|n| root.has(n))
        || tools.contains(&"python");
    let ruby = ["Gemfile", ".ruby-version"].iter().any(|n| root.has(n)) || tools.contains(&"ruby");
    let python_manager =
        root.has("pyproject.toml") && (root.has("poetry.lock") || root.has("pdm.lock"));
    let found_manager = |name: &str| {
        root.home_managers
            .iter()
            .find(|m| m.name == name)
            .map(|m| m.folder.as_str())
    };
    let mut notes = vec![];
    if python && let Some(folder) = found_manager("pyenv") {
        notes.push(format!(
            "pyenv keeps its Pythons in `~/{folder}`, which a confined run cannot read: when the \
             gate's Python, or poetry looking for one, goes through pyenv, {}, or install the \
             project's Python outside the home",
            declare(folder)
        ));
    }
    if ruby && let Some(folder) = found_manager("rbenv") {
        notes.push(format!(
            "rbenv keeps its Rubies in `~/{folder}`, which a confined run cannot read: when the \
             gate's Ruby goes through rbenv, {}, or install the project's Ruby outside the home",
            declare(folder)
        ));
    }
    if !tools.is_empty()
        && let Some(folder) = found_manager("asdf")
    {
        notes.push(format!(
            "asdf keeps the tool chains `.tool-versions` names ({}) in `~/{folder}`, which a \
             confined run cannot read: when the gate runs them through asdf, {}, or install them \
             outside the home",
            tools.join(", "),
            declare(folder)
        ));
    }
    if python_manager && let Some(folder) = found_manager("pipx") {
        notes.push(format!(
            "pipx keeps the tools it installs in `~/{folder}`, which a confined run cannot read: \
             when the project's poetry or pdm is a pipx install, {}, or install it outside the \
             home",
            declare(folder)
        ));
    }
    let uv = root.has("pyproject.toml") && root.has("uv.lock");
    if root.has(".python-version") && !uv && found_manager("pyenv").is_none() {
        notes.push(
            "`.python-version` names a Python: installed by a version manager in your home (pyenv \
             in `~/.pyenv`, asdf in `~/.asdf`, mise in `~/.local/share/mise`), a confined run \
             cannot read it, so declare that folder in `stack.caches` and allow it in your \
             personal `allow_caches`, or install that Python outside the home"
                .to_string(),
        );
    }
    if root.has(".ruby-version") && found_manager("rbenv").is_none() {
        notes.push(
            "`.ruby-version` names a Ruby: installed by a version manager in your home (rbenv in \
             `~/.rbenv`, rvm in `~/.rvm`, chruby's `~/.rubies`, asdf in `~/.asdf`, mise in \
             `~/.local/share/mise`), a confined run cannot read it, so declare that folder in \
             `stack.caches` and allow it in your personal `allow_caches`, or install that Ruby \
             outside the home"
                .to_string(),
        );
    }
    if !tools.is_empty() && found_manager("asdf").is_none() {
        notes.push(format!(
            "`.tool-versions` names {}: installed by asdf in `~/.asdf` or mise in \
             `~/.local/share/mise`, a confined run cannot read them, so declare that folder in \
             `stack.caches` and allow it in your personal `allow_caches`, or install them outside \
             the home",
            tools.join(", ")
        ));
    }
    if notes.is_empty() {
        return Ok(None);
    }
    let mut found = found(HOME_TOOLCHAINS);
    found.notes = notes;
    Ok(Some(found))
}

/// The build files that make a root a Gradle project (OWL-624).
const GRADLE_BUILDS: [&str; 4] = [
    "build.gradle.kts",
    "build.gradle",
    "settings.gradle.kts",
    "settings.gradle",
];

/// Gradle (OWL-624), from its wrapper `gradlew` beside a build file, read by
/// name: `check` runs the tests and the checks the build configures, without
/// the daemon, which would outlive the command. `GRADLE_USER_HOME` holds the
/// wrapper's Gradle, the dependencies and the tool chains Gradle downloads.
/// Checked live, confined: without it, the wrapper fails writing
/// `~/.gradle`. With no wrapper, the machine's Gradle was not checked: a note
/// says what to write.
fn gradle(root: &RootFiles) -> Result<Option<Found>, String> {
    let Some(build) = GRADLE_BUILDS.into_iter().find(|name| root.has(name)) else {
        return Ok(None);
    };
    if !root.has("gradlew") {
        let mut found = found(build);
        found.notes.push(
            "no Gradle wrapper (`gradlew`) beside the build file, so nothing was written: the gate runs the \
             project's Gradle, such as `gradle check --no-daemon`, with \
             `run_caches.GRADLE_USER_HOME = \"gradle\"` for the caches it writes"
                .to_string(),
        );
        return Ok(Some(found));
    }
    let from = "gradlew, the Gradle wrapper";
    let mut found = found("gradlew");
    found.gate.push(line("./gradlew check --no-daemon", from));
    found
        .run_caches
        .push(run_cache("GRADLE_USER_HOME", "gradle", from));
    Ok(Some(found))
}

/// Maven (OWL-627), from `pom.xml`, read by name: `verify` runs the tests and
/// the checks the build binds. Maven moves its local repository with a
/// property only, so the line sets `maven.repo.local` inside the run cache
/// `MAVEN_USER_HOME`. Checked live, confined: without the property Maven
/// fails creating `~/.m2/repository`. With the wrapper `mvnw` at the root the
/// line runs it, and the Maven it pins goes to `$MAVEN_USER_HOME/wrapper`
/// (OWL-637, checked live on macOS, whose sandbox sends its `mktemp -d` to
/// the run's own temporary folder); without it, the machine's `mvn`.
fn maven(root: &RootFiles) -> Result<Option<Found>, String> {
    if !root.has("pom.xml") {
        return Ok(None);
    }
    let (program, from) = if root.has("mvnw") {
        ("./mvnw", "mvnw, the Maven wrapper")
    } else {
        ("mvn", "pom.xml")
    };
    let mut found = found("pom.xml");
    found.gate.push(line(
        format!("{program} -B -Dmaven.repo.local=\"$MAVEN_USER_HOME/repository\" verify"),
        from,
    ));
    found
        .run_caches
        .push(run_cache("MAVEN_USER_HOME", "maven", from));
    found.gate_notes.push(
        "the Maven line reads `$MAVEN_USER_HOME`, the run cache Owlshift sets: to run it by hand, \
         set it first, such as `MAVEN_USER_HOME=~/.m2`"
            .to_string(),
    );
    Ok(Some(found))
}

/// The settings files of PHPUnit, in the order PHPUnit reads them.
const PHPUNIT_SETTINGS: [&str; 3] = ["phpunit.xml", "phpunit.dist.xml", "phpunit.xml.dist"];

/// Composer (OWL-627), from `composer.json`, read by name with
/// `composer.lock` and PHPUnit's settings: Composer installs the locked
/// packages into `vendor/`, and the gate is PHPUnit's when its settings are
/// at the root. Checked live, confined: Composer turns off a cache it cannot
/// write and installs anyway, and reads no file of its home it needs, so
/// neither `COMPOSER_CACHE_DIR` nor `COMPOSER_HOME` is declared, the first
/// named in a note.
fn composer(root: &RootFiles) -> Result<Option<Found>, String> {
    if !root.has("composer.json") {
        return Ok(None);
    }
    let mut found = found("composer.json");
    if root.has("composer.lock") {
        found.source = "composer.json (composer.lock)".to_string();
        let from = "composer.lock, the lock file of Composer";
        found
            .setup
            .push(line("composer install --no-interaction", from));
        found.notes.push(
            "no run cache was declared for Composer, which turns off a cache it cannot write and \
             installs anyway: `run_caches.COMPOSER_CACHE_DIR = \"composer\"` keeps its downloads \
             for the next runs"
                .to_string(),
        );
    } else {
        // An install here would write a lock file into the repository.
        found.notes.push(
            "composer.json has no lock file, so no setup was written: the gate needs a setup \
             that installs the packages"
                .to_string(),
        );
    }
    match PHPUNIT_SETTINGS.into_iter().find(|name| root.has(name)) {
        // PHPUnit 9 and earlier look for `phpunit.xml` and
        // `phpunit.xml.dist` only, and run without settings otherwise.
        Some(settings) => found.gate.push(line(
            if settings == "phpunit.dist.xml" {
                "vendor/bin/phpunit --configuration phpunit.dist.xml"
            } else {
                "vendor/bin/phpunit"
            },
            format!("{settings}, the settings of PHPUnit"),
        )),
        None => found.notes.push(
            "no PHP gate was written, only PHPUnit's settings (`phpunit.xml`) being read for one: \
             add the project's tests, such as `composer test` or `vendor/bin/phpunit`"
                .to_string(),
        ),
    }
    Ok(Some(found))
}

/// The files `dotnet` builds: solutions first, then projects.
const DOTNET_SOLUTIONS: [&str; 2] = [".sln", ".slnx"];
const DOTNET_PROJECTS: [&str; 3] = [".csproj", ".fsproj", ".vbproj"];

/// `name` as one word of `sh`: as it is when it holds no character the
/// shell reads, single-quoted otherwise.
fn shell_word(name: &str) -> String {
    if name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+' | '/'))
    {
        name.to_owned()
    } else {
        format!("'{}'", name.replace('\'', r"'\''"))
    }
}

/// What every `dotnet` line runs with, checked live on macOS, confined
/// (OWL-627): MSBuild's worker nodes cannot reach each other in the sandbox,
/// so a build stalls five minutes and fails unless it stays in one process
/// (`-m:1`, `-nodeReuse:false`); and .NET's TLS on macOS goes through the
/// keychain, closed with the home, so NuGet cannot reach its feed unless it
/// uses Network.framework instead, a switch of .NET 10 and later that other
/// systems ignore.
const DOTNET: &str = "DOTNET_SYSTEM_NET_SECURITY_USENETWORKFRAMEWORK=1 dotnet";
const DOTNET_FLAGS: &str = "-m:1 -nodeReuse:false";

/// .NET (OWL-627), from the one solution at the root, or, with none, the one
/// project there, read by name: `dotnet restore` fetches its packages, then
/// `dotnet test` builds it and runs its tests, each as [`DOTNET`] says.
/// Several solutions, or several projects and no solution, leave `dotnet` to
/// be told which, and a `global.json` with neither says the projects lie below
/// the root: each gets a note. Checked live, confined: `DOTNET_CLI_HOME`
/// holds the SDK's first-run files and NuGet's home, its packages included,
/// and without it `dotnet` fails at once; `NUGET_PACKAGES` is not needed.
/// `NUGET_SCRATCH` is the folder NuGet takes its extraction lock in, under
/// each run's own `TMPDIR` otherwise: two restores at once into one
/// `DOTNET_CLI_HOME` did not see each other's lock and failed in most pairs,
/// and never with the folder shared, a lock folder the project's runs share
/// on purpose (OWL-632, OWL-641 on Linux, OWL-644 on macOS).
fn dotnet(root: &RootFiles) -> Result<Option<Found>, String> {
    let lines = |file: &str| {
        format!(
            "the setup is `{DOTNET} restore {DOTNET_FLAGS} {file}` and the gate `{DOTNET} test \
             {DOTNET_FLAGS} {file}`, with `run_caches.DOTNET_CLI_HOME = \"dotnet\"` and \
             `run_caches.NUGET_SCRATCH = \"nuget-scratch\"`"
        )
    };
    let with = |extensions: &[&str]| -> Vec<&str> {
        root.names
            .iter()
            .map(String::as_str)
            .filter(|n| {
                extensions
                    .iter()
                    .any(|e| n.len() > e.len() && n.ends_with(e))
            })
            .collect()
    };
    let solutions = with(&DOTNET_SOLUTIONS);
    let files = if solutions.is_empty() {
        with(&DOTNET_PROJECTS)
    } else {
        solutions
    };
    let file = match files[..] {
        [file] => file,
        [] if root.has("global.json") => {
            let mut found = found("global.json");
            found.notes.push(format!(
                "global.json names a .NET SDK, but no solution or project is beside it, so \
                 nothing was written: {}",
                lines("<solution>")
            ));
            return Ok(Some(found));
        }
        [] => return Ok(None),
        _ => {
            let mut found = found(files.join(", "));
            found.notes.push(format!(
                "{} are side by side, so nothing was written: for the one that builds the rest, \
                 {}",
                files.join(", "),
                lines("<file>")
            ));
            return Ok(Some(found));
        }
    };
    let word = shell_word(file);
    let from = format!("{file}, the file dotnet builds");
    let mut found = found(file);
    found.setup.push(line(
        format!("{DOTNET} restore {DOTNET_FLAGS} {word}"),
        from.clone(),
    ));
    found.gate.push(line(
        format!("{DOTNET} test {DOTNET_FLAGS} {word}"),
        from.clone(),
    ));
    found
        .run_caches
        .push(run_cache("DOTNET_CLI_HOME", "dotnet", from.clone()));
    found
        .run_caches
        .push(run_cache("NUGET_SCRATCH", "nuget-scratch", from));
    found.notes.push(
        "the dotnet lines build in one process and, on macOS, reach NuGet through \
         Network.framework, since the sandbox closes MSBuild's worker nodes and the keychain .NET's \
         TLS reads otherwise"
            .to_string(),
    );
    found.notes.push(
        "`NUGET_SCRATCH` is the folder NuGet takes its extraction lock in, each run's own \
         temporary folder otherwise, so that restores running at once share one lock (checked on \
         Linux and macOS)"
            .to_string(),
    );
    if root.has("global.json") {
        found.notes.push(
            "global.json pins the .NET SDK: the Network.framework switch needs .NET 10 or later, \
             and with an earlier SDK the setup fails on macOS reaching NuGet"
                .to_string(),
        );
    }
    Ok(Some(found))
}

/// Mix's install of Hex and rebar into its home, when they are missing, with
/// the certificates of `/etc/ssl/cert.pem` where that file is, macOS's bundle
/// of the system's roots (checked live on macOS, confined, OWL-628). Elsewhere
/// Mix reads the system's certificates itself.
const MIX_INSTALL: &str = "[ ! -f /etc/ssl/cert.pem ] || export HEX_CACERTS_PATH=/etc/ssl/cert.pem; \
                           mix local.hex --force --if-missing && mix local.rebar --force --if-missing";

/// Elixir (OWL-628), from `mix.exs`, read by name with `mix.lock` and
/// `.formatter.exs`: the setup installs Hex and rebar into Mix's home when
/// they are missing, then fetches the locked dependencies, refusing a lock
/// that would change; the gate checks the formatting when the formatter has
/// its settings, then runs the tests. Checked live, confined: Mix fails
/// writing its home and Hex its cache unless `MIX_HOME` and `HEX_HOME` are
/// moved; and on macOS Mix reads the system's certificates through the
/// keychain, closed to a confined run, so its download of Hex and rebar
/// fails unless `HEX_CACERTS_PATH` names macOS's own bundle,
/// `/etc/ssl/cert.pem` ([`MIX_INSTALL`]).
fn elixir(root: &RootFiles) -> Result<Option<Found>, String> {
    if !root.has("mix.exs") {
        return Ok(None);
    }
    let mut found = found("mix.exs");
    if root.has("mix.lock") {
        found.source = "mix.exs (mix.lock)".to_string();
        let from = "mix.lock, the lock file of Mix";
        found.setup = vec![
            line(MIX_INSTALL, from),
            line("mix deps.get --check-locked", from),
        ];
        found.run_caches = vec![
            run_cache("MIX_HOME", "mix", from),
            run_cache("HEX_HOME", "hex", from),
        ];
        found.notes.push(
            "the Mix setup gives Mix the certificates of `/etc/ssl/cert.pem` where that file is, \
             since on macOS Mix reads them through the keychain, which the sandbox closes"
                .to_string(),
        );
    } else {
        // An install here would write a lock file into the repository.
        found.notes.push(
            "mix.exs has no lock file, so no setup was written: the gate needs a setup that \
             fetches the dependencies, such as `mix deps.get`, with `run_caches.MIX_HOME = \"mix\"` \
             and `run_caches.HEX_HOME = \"hex\"`"
                .to_string(),
        );
    }
    if root.has(".formatter.exs") {
        found.gate.push(line(
            "mix format --check-formatted",
            ".formatter.exs, the settings of mix format",
        ));
    } else {
        found.notes.push(
            "no `.formatter.exs` beside `mix.exs`, so the formatting is not checked: with one, the gate \
             can run `mix format --check-formatted`"
                .to_string(),
        );
    }
    found.gate.push(line("mix test", "mix.exs"));
    Ok(Some(found))
}

/// Dart and Flutter (OWL-628), from `pubspec.yaml`, read by name: a note
/// only. Neither was checked confined, the Dart SDK being too large to fetch
/// at the network's rate the day of the check and Flutter's larger still, so
/// the note gives the lines and the variable to check by hand.
fn dart(root: &RootFiles) -> Result<Option<Found>, String> {
    if !root.has("pubspec.yaml") {
        return Ok(None);
    }
    let mut found = found("pubspec.yaml");
    found.notes.push(
        "Dart and Flutter were not checked confined, so nothing was written: for Dart, the setup \
         `dart pub get --enforce-lockfile` and the gate `dart analyze` and `dart test`; for \
         Flutter, the same with `flutter` in place of `dart`; each with \
         `run_caches.PUB_CACHE = \"pub\"` for the packages pub downloads"
            .to_string(),
    );
    Ok(Some(found))
}

/// Swift (OWL-628, OWL-638), from `Package.swift`, read by name: a note only.
/// Checked live, confined, on macOS (OWL-638, Swift 6.4 of the Command Line
/// Tools): SwiftPM compiles the manifest inside a `sandbox-exec` of its own,
/// which cannot run inside Owlshift's (`--disable-sandbox` turns it off);
/// swiftc then needs a writable clang module cache, which
/// `CLANG_MODULE_CACHE_PATH` moves into a run cache, and with it the setup
/// and `swift build` pass; `swift test` of a swift-testing target does not,
/// the `TestingMacros` plugin not being found. Linux is unchecked.
fn swift(root: &RootFiles) -> Result<Option<Found>, String> {
    if !root.has("Package.swift") {
        return Ok(None);
    }
    let mut found = found("Package.swift");
    found.notes.push(
        "Swift was not written, its gate not having passed confined (macOS; Linux unchecked): \
         the setup `swift package --disable-sandbox resolve` and the gate `swift build \
         --disable-sandbox` and `swift test --disable-sandbox` (SwiftPM's own sandbox cannot run \
         inside Owlshift's) need, on macOS, `run_caches.CLANG_MODULE_CACHE_PATH = \"clang-modules\"` \
         for the module cache swiftc writes; with it the setup and the build passed"
            .to_string(),
    );
    found.notes.push(
        "on macOS, `swift test` of a swift-testing target failed confined even so, its \
         `TestingMacros` plugin not being found"
            .to_string(),
    );
    Ok(Some(found))
}

/// The settings files of Deno, in the order Deno reads them.
const DENO_SETTINGS: [&str; 2] = ["deno.json", "deno.jsonc"];

/// Deno (OWL-628), from `deno.json` or `deno.jsonc`, read by name with
/// `deno.lock`: the setup installs the locked dependencies, the gate checks
/// the formatting and the lint, then runs the tests, a root without any
/// passing (`--permit-no-files`). Checked live, confined: without `DENO_DIR`
/// moved, the tests fail caching the modules they import. `deno install` of
/// the settings' imports and `--frozen` need Deno 2.
fn deno(root: &RootFiles) -> Result<Option<Found>, String> {
    let Some(settings) = DENO_SETTINGS.into_iter().find(|name| root.has(name)) else {
        return Ok(None);
    };
    let mut found = found(settings);
    let from = format!("{settings}, the settings of Deno");
    let lock = if root.has("deno.lock") {
        found.source = format!("{settings} (deno.lock)");
        found.setup.push(line(
            "deno install --frozen",
            "deno.lock, the lock file of Deno",
        ));
        "--frozen"
    } else {
        // Deno writes a `deno.lock` beside its settings otherwise, a file
        // the gate refuses to see changed.
        found.notes.push(format!(
            "{settings} has no lock file, so no setup was written and the tests run with \
             `--no-lock`: Deno fetches the imports as they run"
        ));
        "--no-lock"
    };
    found.gate = vec![
        line("deno fmt --check", from.clone()),
        line("deno lint", from.clone()),
        line(format!("deno test {lock} --permit-no-files"), from.clone()),
    ];
    found.run_caches.push(run_cache("DENO_DIR", "deno", from));
    Ok(Some(found))
}

/// The `lint`, `test` and `check` targets a Makefile defines, in that order.
fn makefile(root: &RootFiles) -> Result<Option<Found>, String> {
    Ok(manifest(root, "Makefile")?.and_then(makefile_targets))
}

/// The Found of a Makefile's text.
fn makefile_targets(text: &str) -> Option<Found> {
    let mut defined = [false; 3];
    for raw in text.lines() {
        if raw.starts_with([' ', '\t', '#', '.']) {
            continue;
        }
        let Some((before, after)) = raw.split_once(':') else {
            continue;
        };
        // `x := 1` and `x ::= 1` assign a variable, they do not define a rule.
        if after.starts_with('=') || after.starts_with(":=") || before.contains('=') {
            continue;
        }
        for target in before.split_whitespace() {
            for (i, name) in ["lint", "test", "check"].iter().enumerate() {
                if target == *name {
                    defined[i] = true;
                }
            }
        }
    }
    let gate: Vec<Line> = ["lint", "test", "check"]
        .iter()
        .zip(defined)
        .filter(|(_, d)| *d)
        .map(|(name, _)| {
            line(
                format!("make {name}"),
                format!("the `{name}` target of Makefile"),
            )
        })
        .collect();
    if gate.is_empty() {
        return None;
    }
    Some(Found {
        source: "Makefile".to_string(),
        gate,
        setup: vec![],
        run_caches: vec![],
        notes: vec![],
        gate_notes: vec![],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(names: &[&str], files: &[(&str, &str)]) -> RootFiles {
        let mut all: Vec<String> = names.iter().map(|n| n.to_string()).collect();
        for (n, _) in files {
            if !all.iter().any(|a| a == n) {
                all.push(n.to_string());
            }
        }
        RootFiles {
            names: all,
            files: files
                .iter()
                .map(|(n, c)| (n.to_string(), c.to_string()))
                .collect(),
            ..RootFiles::default()
        }
    }

    /// The folder `path` below a root, read as [`root`] reads one.
    fn folder(path: &str, names: &[&str], files: &[(&str, &str)]) -> Folder {
        let RootFiles { names, files, .. } = root(names, files);
        Folder {
            path: path.to_owned(),
            names,
            files,
            unlisted: false,
        }
    }

    /// `root` with `folders` below it.
    fn with_folders(mut root: RootFiles, folders: Vec<Folder>) -> RootFiles {
        root.folders = folders;
        root
    }

    fn commands<'a>(lines: impl Iterator<Item = &'a Line>) -> Vec<&'a str> {
        lines.map(|l| l.command.as_str()).collect()
    }

    fn run_caches<'a>(caches: impl Iterator<Item = &'a RunCache>) -> Vec<(&'a str, &'a str)> {
        caches
            .map(|c| (c.variable.as_str(), c.folder.as_str()))
            .collect()
    }

    const CARGO: &str = "[package]\nname = \"x\"\n";

    #[test]
    fn a_cargo_root_gives_the_three_commands_locked_only_with_a_lock_file() {
        let with = detect(&root(&["Cargo.lock"], &[("Cargo.toml", CARGO)]));
        assert_eq!(
            commands(with.gate()),
            [
                "cargo fmt --all -- --check",
                "cargo clippy --workspace --all-targets --locked -- -D warnings",
                "cargo test --workspace --locked"
            ]
        );
        assert_eq!(with.found[0].source, "Cargo.toml");
        assert!(with.gate().all(|l| l.read_from == "Cargo.toml"));
        // OWL-617: cargo's registry goes to a cache its runs write.
        assert_eq!(run_caches(with.run_caches()), [("CARGO_HOME", "cargo")]);
        assert_eq!(with.found[0].run_caches[0].read_from, "Cargo.toml");
        let without = detect(&root(&[], &[("Cargo.toml", "[workspace]\nmembers = []\n")]));
        assert_eq!(
            commands(without.gate()),
            [
                "cargo fmt --all -- --check",
                "cargo clippy --workspace --all-targets -- -D warnings",
                "cargo test --workspace"
            ]
        );
    }

    #[test]
    fn a_cargo_toml_without_package_or_workspace_gives_nothing() {
        let d = detect(&root(&[], &[("Cargo.toml", "[dependencies]\n")]));
        assert!(d.found.is_empty() && d.skipped.is_empty());
    }

    #[test]
    fn an_unparsable_or_unread_manifest_is_skipped_never_an_error() {
        let d = detect(&root(
            &["package.json"],
            &[("Cargo.toml", "[package\n"), ("Makefile", "lint:\n")],
        ));
        assert_eq!(d.skipped.len(), 2, "{:?}", d.skipped);
        assert!(d.skipped[0].starts_with("Cargo.toml"));
        assert!(d.skipped[1].starts_with("package.json"));
        assert_eq!(commands(d.gate()), ["make lint"]);
        let bad = detect(&root(&[], &[("package.json", "{")]));
        assert!(bad.found.is_empty());
        assert!(bad.skipped[0].starts_with("package.json"));
        let bad = detect(&root(&[], &[("pyproject.toml", "[tool")]));
        assert!(bad.skipped[0].starts_with("pyproject.toml"));
    }

    #[test]
    fn an_empty_root_detects_nothing() {
        assert_eq!(detect(&RootFiles::default()), Detection::default());
        let d = detect(&root(&["README.md", "src"], &[]));
        assert!(d.found.is_empty() && d.skipped.is_empty());
    }

    #[test]
    fn a_makefile_gives_its_real_lint_test_and_check_targets() {
        let text = "\
.PHONY: lint test check
lint := x
CHECK = y
\ttest:
  check:
test lint: deps
\t@echo hi
check::
\tmake it
";
        let d = detect(&root(&[], &[("Makefile", text)]));
        assert_eq!(commands(d.gate()), ["make lint", "make test", "make check"]);
        let none = detect(&root(
            &[],
            &[("Makefile", ".PHONY: lint\nlint := x\n\tlint:\nall:\n")],
        ));
        assert!(none.found.is_empty());
    }

    #[test]
    fn a_makefile_target_line_keeps_the_fixed_order() {
        let d = detect(&root(&[], &[("Makefile", "check:\ntest:\nlint:\n")]));
        assert_eq!(commands(d.gate()), ["make lint", "make test", "make check"]);
    }

    #[test]
    fn each_node_manager_gives_its_install_and_its_scripts() {
        let pkg = r#"{"scripts":{"test":"jest","lint":"eslint .","build":"tsc"}}"#;
        type Case<'a> = (&'a [&'a str], &'a str, &'a str, &'a [(&'a str, &'a str)]);
        let cases: [Case; 6] = [
            (
                &["pnpm-lock.yaml"],
                "pnpm install --frozen-lockfile",
                "pnpm",
                &PNPM,
            ),
            (
                &["yarn.lock"],
                "yarn install --frozen-lockfile",
                "yarn",
                &[],
            ),
            (
                &["yarn.lock", ".yarnrc.yml"],
                "yarn install --immutable",
                "yarn",
                &YARN_BERRY,
            ),
            (&["bun.lock"], "bun install --frozen-lockfile", "bun", &[]),
            (&["bun.lockb"], "bun install --frozen-lockfile", "bun", &[]),
            (
                &["package-lock.json"],
                "npm ci",
                "npm",
                &[("npm_config_cache", "npm")],
            ),
        ];
        for (names, setup, manager, written) in cases {
            let d = detect(&root(names, &[("package.json", pkg)]));
            assert_eq!(commands(d.setup()), [setup], "{names:?}");
            // OWL-617, OWL-629: each manager installs online into caches of
            // Owlshift's own.
            assert_eq!(run_caches(d.run_caches()), written, "{names:?}");
            assert_eq!(
                commands(d.gate()),
                [
                    format!("{manager} run lint"),
                    format!("{manager} run test"),
                    format!("{manager} run build")
                ],
                "{names:?}"
            );
            // pnpm says why it writes `XDG_RUNTIME_DIR`; the gate's own note
            // is a guess (OWL-595).
            let notes = &d.found[0].notes;
            assert_eq!(notes.len(), usize::from(manager == "pnpm"), "{notes:?}");
            assert!(notes.iter().all(|n| n.contains("`XDG_RUNTIME_DIR`")));
            let gate_notes = &d.found[0].gate_notes;
            assert_eq!(gate_notes.len(), 1, "{gate_notes:?}");
            assert!(gate_notes[0].contains("a guess"), "{gate_notes:?}");
            assert!(d.found[0].source.starts_with("package.json ("));
            assert!(d.setup().all(|l| l.read_from.contains(manager)));
            assert!(d.run_caches().all(|c| c.read_from.contains(manager)));
        }
    }

    /// OWL-629: pnpm's home and corepack's are run caches whatever the pin;
    /// the folder pnpm 12 takes its locks in is one unless `packageManager`
    /// pins a pnpm below 12. A pin that names another manager or a version
    /// that does not start with a number counts as no pin.
    #[test]
    fn pnpm_writes_its_lock_folder_unless_pinned_below_12() {
        let cases: [(&str, bool); 6] = [
            ("", true),
            ("yarn@4.19.0", true),
            ("pnpm@9.15.9+sha512.abc", false),
            ("pnpm@11.28.13", false),
            ("pnpm@12.0.0-rc.1", true),
            ("pnpm@latest", true),
        ];
        for (manager, locks) in cases {
            let pkg = format!(r#"{{"packageManager":"{manager}"}}"#);
            let d = detect(&root(&["pnpm-lock.yaml"], &[("package.json", &pkg)]));
            assert_eq!(
                commands(d.setup()),
                ["pnpm install --frozen-lockfile"],
                "{manager}"
            );
            let written = if locks { &PNPM[..] } else { &PNPM_BELOW_12[..] };
            assert_eq!(run_caches(d.run_caches()), written, "{manager}");
            assert_eq!(d.found[0].notes.len(), usize::from(locks), "{manager}");
        }
    }

    /// OWL-629: yarn 2 or later, named by `.yarnrc.yml` or by a
    /// `packageManager` pinning it, installs `--immutable` into its global
    /// folder, which a `globalFolder` setting does not move: the variable
    /// wins. yarn 1 keeps its plain install and no run cache.
    #[test]
    fn yarn_2_or_later_writes_its_global_folder() {
        let cases: [(&str, Option<&str>, bool); 5] = [
            ("", Some("globalFolder: /opt/yarn\n"), true),
            ("yarn@4.19.0", None, true),
            ("yarn@2.4.2", None, true),
            ("yarn@1.22.22", None, false),
            ("", None, false),
        ];
        for (manager, rc, berry) in cases {
            let pkg = format!(r#"{{"packageManager":"{manager}"}}"#);
            let mut files = vec![("package.json", pkg.as_str())];
            files.extend(rc.map(|rc| (".yarnrc.yml", rc)));
            let d = detect(&root(&["yarn.lock"], &files));
            let (setup, written) = if berry {
                ("yarn install --immutable", &YARN_BERRY[..])
            } else {
                ("yarn install --frozen-lockfile", &[][..])
            };
            assert_eq!(commands(d.setup()), [setup], "{manager} {rc:?}");
            assert_eq!(run_caches(d.run_caches()), written, "{manager} {rc:?}");
            assert!(d.found[0].notes.is_empty(), "{manager} {rc:?}");
        }
    }

    /// OWL-629: `XDG_RUNTIME_DIR` may name a run cache because a run never
    /// gets the person's: it locates the Secret Service, so no agent inherits
    /// it, and the run cache sets it to an empty folder of Owlshift's own.
    #[test]
    fn the_lock_folder_is_a_run_cache_and_the_real_one_is_never_inherited() {
        assert!(
            !crate::agent_env::INHERITED
                .iter()
                .any(|name| name.eq_ignore_ascii_case("XDG_RUNTIME_DIR"))
        );
        crate::agent_env::check_run_caches(&[PNPM[1]]).unwrap();
    }

    #[test]
    fn a_node_root_without_a_lock_file_has_no_setup_and_says_so() {
        let d = detect(&root(
            &[],
            &[("package.json", r#"{"scripts":{"test":"jest"}}"#)],
        ));
        assert_eq!(commands(d.setup()), Vec::<&str>::new());
        assert_eq!(commands(d.gate()), ["npm run test"]);
        assert_eq!(d.found[0].source, "package.json");
        assert_eq!(d.found[0].notes.len(), 1);
        assert!(d.found[0].notes[0].contains("no lock file"));
        assert!(d.found[0].gate_notes[0].contains("a guess"));
    }

    /// OWL-595: the project's own scripts, in the order a developer runs them.
    #[test]
    fn a_package_json_offers_lint_typecheck_test_and_build_in_order() {
        let pkg = r#"{"scripts":{"build":"next build","check":"npm run lint && npm run build",
            "test":"vitest","smoke":"node smoke.js","typecheck":"tsc","lint":"eslint ."}}"#;
        let d = detect(&root(&["package-lock.json"], &[("package.json", pkg)]));
        assert_eq!(
            commands(d.gate()),
            [
                "npm run lint",
                "npm run typecheck",
                "npm run test",
                "npm run build"
            ]
        );
        let notes = &d.found[0].gate_notes;
        let check = notes.iter().find(|n| n.contains("`check`")).unwrap();
        assert!(check.contains("compare"), "{check}");
        let guess = notes.last().unwrap();
        assert!(
            guess.contains("a guess") && guess.contains("review"),
            "{guess}"
        );
        assert!(
            guess.contains("read the scripts `lint`, `typecheck`, `test`, `build`"),
            "{guess}"
        );
        assert!(guess.contains("left out `check`, `smoke`"), "{guess}");
    }

    #[test]
    fn type_check_is_used_only_when_typecheck_is_absent() {
        let both = r#"{"scripts":{"typecheck":"a","type-check":"b","ci":"c"}}"#;
        let d = detect(&root(&["yarn.lock"], &[("package.json", both)]));
        assert_eq!(commands(d.gate()), ["yarn run typecheck"]);
        assert!(d.found[0].gate_notes.iter().any(|n| n.contains("`ci`")));
        let dashed = r#"{"scripts":{"type-check":"b"}}"#;
        let d = detect(&root(&["yarn.lock"], &[("package.json", dashed)]));
        assert_eq!(commands(d.gate()), ["yarn run type-check"]);
        assert!(!d.found[0].gate_notes.iter().any(|n| n.contains("compare")));
    }

    #[test]
    fn a_package_json_without_scripts_gives_the_setup_only() {
        let d = detect(&root(&["package-lock.json"], &[("package.json", "{}")]));
        assert_eq!(commands(d.setup()), ["npm ci"]);
        assert_eq!(commands(d.gate()), Vec::<&str>::new());
    }

    #[test]
    fn npms_placeholder_test_script_is_left_out() {
        let pkg = r#"{"scripts":{"test":"echo \"Error: no test specified\" && exit 1"}}"#;
        let d = detect(&root(&["package-lock.json"], &[("package.json", pkg)]));
        assert_eq!(commands(d.gate()), Vec::<&str>::new());
        assert!(d.found[0].notes.iter().any(|n| n.contains("placeholder")));
        // No gate, so no guess to review.
        assert!(d.found[0].gate_notes.is_empty());
    }

    #[test]
    fn the_placeholder_test_is_named_among_the_scripts_left_out() {
        let pkg =
            r#"{"scripts":{"lint":"x","test":"echo \"Error: no test specified\" && exit 1"}}"#;
        let d = detect(&root(&["package-lock.json"], &[("package.json", pkg)]));
        let guess = d.found[0].gate_notes.last().unwrap();
        assert!(guess.ends_with("left out `test`"), "{guess}");
    }

    const PY: &str =
        "[tool.ruff]\nline-length = 100\n[tool.pytest.ini_options]\ntestpaths = [\"tests\"]\n";

    #[test]
    fn a_python_root_with_uv_runs_its_tools_through_uv() {
        let d = detect(&root(&["uv.lock"], &[("pyproject.toml", PY)]));
        assert_eq!(commands(d.gate()), ["uv run ruff check .", "uv run pytest"]);
        assert_eq!(d.found[0].source, "pyproject.toml (uv.lock)");
        assert!(d.found[0].notes.is_empty());
        assert!(d.setup().next().is_none());
        // OWL-617: uv's cache and the Python it installs go to run caches.
        assert_eq!(
            run_caches(d.run_caches()),
            [
                ("UV_CACHE_DIR", "uv"),
                ("UV_PYTHON_INSTALL_DIR", "uv-python")
            ]
        );
    }

    #[test]
    fn a_python_root_without_a_manager_runs_bare_and_says_a_setup_is_needed() {
        let d = detect(&root(&[], &[("pyproject.toml", PY)]));
        assert_eq!(commands(d.gate()), ["ruff check .", "pytest"]);
        assert_eq!(d.found[0].notes.len(), 1);
        assert!(d.found[0].notes[0].contains("setup"));
        assert!(d.setup().next().is_none() && d.run_caches().next().is_none());
    }

    /// No tool `[tool.pytest]` alone names; a manager's lock still gives its
    /// setup and run caches, with a note that no gate was written (OWL-624).
    #[test]
    fn a_pyproject_without_a_known_tool_gives_its_managers_lines_only() {
        let text = "[project]\nname = \"x\"\n[tool.pytest]\nx = 1\n";
        let bare = detect(&root(&[], &[("pyproject.toml", text)]));
        assert!(bare.found.is_empty() && bare.skipped.is_empty());
        for (lock, prefix, written) in [
            (
                "uv.lock",
                "uv run",
                &[
                    ("UV_CACHE_DIR", "uv"),
                    ("UV_PYTHON_INSTALL_DIR", "uv-python"),
                ][..],
            ),
            (
                "poetry.lock",
                "poetry run",
                &[("POETRY_CACHE_DIR", "poetry")],
            ),
            (
                "pdm.lock",
                "pdm run",
                &[("PDM_CACHE_DIR", "pdm"), ("PDM_LOG_DIR", "pdm-log")],
            ),
        ] {
            let d = detect(&root(&[lock], &[("pyproject.toml", text)]));
            assert!(d.gate().next().is_none(), "{lock}");
            assert_eq!(run_caches(d.run_caches()), written, "{lock}");
            let notes = &d.found[0].notes;
            assert!(
                notes
                    .last()
                    .unwrap()
                    .contains(&format!("run through `{prefix}`")),
                "{notes:?}"
            );
        }
    }

    /// OWL-624, OWL-627: poetry and pdm install the lock, each writing its
    /// run caches, and run the tools through `poetry run` and `pdm run`; pdm
    /// says to ignore the interpreter it saves.
    #[test]
    fn a_poetry_or_pdm_root_installs_its_lock_and_runs_through_it() {
        let (poetry, pdm): (&[_], &[_]) = (
            &[("POETRY_CACHE_DIR", "poetry")],
            &[("PDM_CACHE_DIR", "pdm"), ("PDM_LOG_DIR", "pdm-log")],
        );
        for (name, lock, caches) in [("poetry", "poetry.lock", poetry), ("pdm", "pdm.lock", pdm)] {
            let d = detect(&root(&[lock], &[("pyproject.toml", PY)]));
            assert_eq!(d.found[0].source, format!("pyproject.toml ({lock})"));
            assert_eq!(commands(d.setup()), [format!("{name} install")]);
            assert_eq!(
                commands(d.gate()),
                [
                    format!("{name} run ruff check ."),
                    format!("{name} run pytest")
                ]
            );
            assert_eq!(run_caches(d.run_caches()), caches);
            let notes = &d.found[0].notes;
            assert_eq!(notes.len(), usize::from(name == "pdm"), "{notes:?}");
            assert!(notes.iter().all(|n| n.contains("`.pdm-python`")));
            assert!(d.setup().all(|l| l.read_from.contains(lock)));
        }
    }

    /// OWL-624: pip runs the tools from a virtualenv that ignores itself; its
    /// cache, which it turns off confined and installs without, is named in a
    /// note, never declared (criterion: an unchecked run cache is not written).
    #[test]
    fn pip_installs_into_a_self_ignoring_venv_and_only_names_its_cache() {
        let d = detect(&root(&["requirements.txt"], &[("pyproject.toml", PY)]));
        assert_eq!(d.found[0].source, "pyproject.toml (requirements.txt)");
        assert_eq!(
            commands(d.setup()),
            [PIP_VENV, ".venv/bin/pip install -r requirements.txt"]
        );
        assert!(PIP_VENV.ends_with("echo '*' > .venv/.gitignore"));
        assert_eq!(
            commands(d.gate()),
            [".venv/bin/ruff check .", ".venv/bin/pytest"]
        );
        assert!(d.run_caches().next().is_none());
        let notes = &d.found[0].notes;
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("`run_caches.PIP_CACHE_DIR = \"pip\"`"));
        let dev = detect(&root(
            &["requirements.txt", "requirements-dev.txt"],
            &[("pyproject.toml", PY)],
        ));
        assert_eq!(
            commands(dev.setup()).last().unwrap(),
            &".venv/bin/pip install -r requirements.txt -r requirements-dev.txt"
        );
        assert!(
            dev.setup()
                .all(|l| l.read_from.contains("requirements-dev.txt"))
        );
        // Without a configured tool, a root `requirements.txt` may serve
        // something else: only a note.
        for files in [&[][..], &[("pyproject.toml", "[project]\n")]] {
            let d = detect(&root(&["requirements.txt"], files));
            assert_eq!(d.found[0].source, "requirements.txt");
            assert!(d.gate().next().is_none() && d.setup().next().is_none());
            assert!(d.found[0].notes[0].contains(PIP_VENV));
        }
    }

    #[test]
    fn the_first_python_lock_found_chooses_the_manager() {
        let cases: [(&[&str], &str); 4] = [
            (
                &["uv.lock", "poetry.lock", "requirements.txt"],
                "uv run pytest",
            ),
            (
                &["poetry.lock", "pdm.lock", "requirements.txt"],
                "poetry run pytest",
            ),
            (&["pdm.lock", "requirements.txt"], "pdm run pytest"),
            (&["requirements.txt"], ".venv/bin/pytest"),
        ];
        let py = "[tool.pytest.ini_options]\n";
        for (names, gate) in cases {
            let d = detect(&root(names, &[("pyproject.toml", py)]));
            assert_eq!(commands(d.gate()), [gate], "{names:?}");
        }
        // A lock without `pyproject.toml` names no manager.
        let d = detect(&root(&["poetry.lock", "requirements.txt"], &[]));
        assert_eq!(d.found[0].source, "requirements.txt");
    }

    /// OWL-624: a Go module's modules and build cache go to run caches.
    #[test]
    fn a_go_module_gives_its_setup_gate_and_run_caches() {
        let d = detect(&root(&["go.mod", "go.sum"], &[]));
        assert_eq!(d.found[0].source, "go.mod");
        assert_eq!(commands(d.setup()), ["go mod download"]);
        assert_eq!(
            commands(d.gate()),
            ["! gofmt -l . | grep .", "go vet ./...", "go test ./..."]
        );
        assert_eq!(
            run_caches(d.run_caches()),
            [("GOMODCACHE", "go-mod"), ("GOCACHE", "go-build")]
        );
        assert!(d.found[0].notes[0].contains("chmod -R u+w"));
        assert!(d.gate().chain(d.setup()).all(|l| l.read_from == "go.mod"));
    }

    /// OWL-624: bundler installs the locked gems, frozen, into a run cache;
    /// RSpec is the gate when `.rspec` is there.
    #[test]
    fn a_ruby_root_installs_its_locked_gems_into_a_run_cache() {
        let d = detect(&root(&["Gemfile", "Gemfile.lock", ".rspec"], &[]));
        assert_eq!(d.found[0].source, "Gemfile (Gemfile.lock)");
        assert_eq!(commands(d.setup()), ["bundle install"]);
        assert_eq!(commands(d.gate()), ["bundle exec rspec"]);
        assert_eq!(run_caches(d.run_caches()), [("BUNDLE_PATH", "bundle")]);
        assert!(d.found[0].notes.is_empty());
        // No lock: no setup and no run cache; no `.rspec`: no gate. Each
        // says so.
        let d = detect(&root(&["Gemfile"], &[]));
        assert!(d.setup().next().is_none() && d.gate().next().is_none());
        assert!(d.run_caches().next().is_none());
        let notes = &d.found[0].notes;
        assert!(notes[0].contains("no lock file") && notes[1].contains("bin/rails test"));
        assert!(detect(&root(&["Gemfile.lock"], &[])).found.is_empty());
    }

    /// OWL-624: Gradle through its wrapper, its user home a run cache; a
    /// build with no wrapper gets a note naming both.
    #[test]
    fn a_gradle_build_is_run_through_its_wrapper() {
        for build in GRADLE_BUILDS {
            let d = detect(&root(&["gradlew", build], &[]));
            assert_eq!(d.found[0].source, "gradlew", "{build}");
            assert_eq!(commands(d.gate()), ["./gradlew check --no-daemon"]);
            assert!(d.setup().next().is_none());
            assert_eq!(run_caches(d.run_caches()), [("GRADLE_USER_HOME", "gradle")]);
        }
        let d = detect(&root(&["build.gradle"], &[]));
        assert_eq!(d.found[0].source, "build.gradle");
        assert!(d.gate().next().is_none() && d.run_caches().next().is_none());
        assert!(d.found[0].notes[0].contains("run_caches.GRADLE_USER_HOME"));
        assert!(detect(&root(&["gradlew"], &[])).found.is_empty());
    }

    /// OWL-627, OWL-637: the wrapper `mvnw` when it is there, else the
    /// machine's `mvn`, the local repository inside the run cache either way.
    #[test]
    fn a_maven_build_runs_verify_with_its_repository_in_a_run_cache() {
        for (names, program) in [(&["pom.xml", "mvnw"][..], "./mvnw"), (&["pom.xml"], "mvn")] {
            let d = detect(&root(names, &[]));
            assert_eq!(d.found[0].source, "pom.xml");
            assert_eq!(
                commands(d.gate()),
                [format!(
                    "{program} -B -Dmaven.repo.local=\"$MAVEN_USER_HOME/repository\" verify"
                )]
            );
            assert!(d.setup().next().is_none());
            assert_eq!(run_caches(d.run_caches()), [("MAVEN_USER_HOME", "maven")]);
            assert!(d.found[0].gate_notes[0].contains("MAVEN_USER_HOME=~/.m2"));
            assert!(d.found[0].notes.is_empty(), "{names:?}");
        }
        assert!(detect(&root(&["mvnw"], &[])).found.is_empty());
    }

    /// OWL-627: Composer installs the lock; PHPUnit is the gate when its
    /// settings are at the root. A missing lock or settings file says so.
    #[test]
    fn a_composer_root_installs_its_lock_and_runs_phpunit() {
        let d = detect(&root(
            &["composer.json", "composer.lock", "phpunit.xml.dist"],
            &[],
        ));
        assert_eq!(d.found[0].source, "composer.json (composer.lock)");
        assert_eq!(commands(d.setup()), ["composer install --no-interaction"]);
        assert_eq!(commands(d.gate()), ["vendor/bin/phpunit"]);
        assert_eq!(
            d.gate().next().unwrap().read_from,
            "phpunit.xml.dist, the settings of PHPUnit"
        );
        // Checked live: Composer installs without a cache it cannot write.
        assert!(d.run_caches().next().is_none());
        let notes = &d.found[0].notes;
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("`run_caches.COMPOSER_CACHE_DIR = \"composer\"`"));
        // PHPUnit 9 does not find `phpunit.dist.xml` itself.
        let dist = detect(&root(
            &["composer.json", "composer.lock", "phpunit.dist.xml"],
            &[],
        ));
        assert_eq!(
            commands(dist.gate()),
            ["vendor/bin/phpunit --configuration phpunit.dist.xml"]
        );
        let d = detect(&root(&["composer.json"], &[]));
        assert!(d.setup().next().is_none() && d.gate().next().is_none());
        assert!(d.run_caches().next().is_none());
        let notes = &d.found[0].notes;
        assert!(notes[0].contains("no lock file") && notes[1].contains("composer test"));
    }

    /// OWL-627: .NET builds the one solution at the root, or the one project
    /// when there is no solution, in one process and through Network.framework
    /// on macOS; otherwise a note says what to write. A `global.json` adds the
    /// switch's minimum SDK. OWL-641: restores share NuGet's lock folder.
    #[test]
    fn a_dotnet_root_restores_and_tests_its_one_solution_or_project() {
        let cases: [(&[&str], &str); 4] = [
            (&["App.sln", "App.csproj", "global.json"], "App.sln"),
            (&["App.slnx"], "App.slnx"),
            (&["Lib.fsproj", "README.md"], "Lib.fsproj"),
            (&["My App.csproj"], "'My App.csproj'"),
        ];
        for (names, word) in cases {
            let d = detect(&root(names, &[]));
            let dotnet = "DOTNET_SYSTEM_NET_SECURITY_USENETWORKFRAMEWORK=1 dotnet";
            assert_eq!(
                commands(d.setup()),
                [format!("{dotnet} restore -m:1 -nodeReuse:false {word}")]
            );
            assert_eq!(
                commands(d.gate()),
                [format!("{dotnet} test -m:1 -nodeReuse:false {word}")]
            );
            assert_eq!(
                run_caches(d.run_caches()),
                [
                    ("DOTNET_CLI_HOME", "dotnet"),
                    ("NUGET_SCRATCH", "nuget-scratch")
                ]
            );
            let notes = &d.found[0].notes;
            assert!(notes[0].contains("Network.framework"), "{notes:?}");
            assert!(notes[1].contains("`NUGET_SCRATCH`"), "{notes:?}");
            let pinned = names.contains(&"global.json");
            assert_eq!(notes.len(), 2 + usize::from(pinned), "{notes:?}");
            assert!(notes.iter().skip(2).all(|n| n.contains(".NET 10")));
        }
        for (names, source) in [
            (&["A.sln", "B.sln", "A.csproj"][..], "A.sln, B.sln"),
            (&["A.csproj", "B.fsproj"], "A.csproj, B.fsproj"),
            (&["global.json"], "global.json"),
        ] {
            let d = detect(&root(names, &[]));
            assert_eq!(d.found[0].source, source);
            assert!(d.setup().next().is_none() && d.gate().next().is_none());
            assert!(d.run_caches().next().is_none());
            let note = &d.found[0].notes[0];
            assert!(
                note.contains("run_caches.DOTNET_CLI_HOME")
                    && note.contains("run_caches.NUGET_SCRATCH"),
                "{names:?}"
            );
        }
        assert!(detect(&root(&[".sln", "csproj"], &[])).found.is_empty());
    }

    /// OWL-628: Mix installs Hex and rebar, then the locked dependencies,
    /// into its run caches; the formatting is checked only with its
    /// settings. A missing lock or settings file says so.
    #[test]
    fn an_elixir_root_fetches_its_lock_and_runs_its_tests() {
        let d = detect(&root(&["mix.exs", "mix.lock", ".formatter.exs"], &[]));
        assert_eq!(d.found[0].source, "mix.exs (mix.lock)");
        assert_eq!(
            commands(d.setup()),
            [
                "[ ! -f /etc/ssl/cert.pem ] || export HEX_CACERTS_PATH=/etc/ssl/cert.pem; mix \
                 local.hex --force --if-missing && mix local.rebar --force --if-missing",
                "mix deps.get --check-locked"
            ]
        );
        assert_eq!(
            commands(d.gate()),
            ["mix format --check-formatted", "mix test"]
        );
        assert_eq!(
            run_caches(d.run_caches()),
            [("MIX_HOME", "mix"), ("HEX_HOME", "hex")]
        );
        let notes = &d.found[0].notes;
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("keychain"));
        let d = detect(&root(&["mix.exs"], &[]));
        assert!(d.setup().next().is_none() && d.run_caches().next().is_none());
        assert_eq!(commands(d.gate()), ["mix test"]);
        let notes = &d.found[0].notes;
        assert!(notes[0].contains("run_caches.HEX_HOME") && notes[1].contains(".formatter.exs"));
        assert!(detect(&root(&["mix.lock"], &[])).found.is_empty());
    }

    /// OWL-628: Dart, Flutter and Swift were not shown to pass confined: a
    /// note each, naming the lines, and nothing written.
    #[test]
    fn dart_and_swift_roots_get_a_note_only() {
        for (name, says) in [
            ("pubspec.yaml", "run_caches.PUB_CACHE"),
            ("Package.swift", "CLANG_MODULE_CACHE_PATH"),
        ] {
            let d = detect(&root(&[name, "pubspec.lock", "Package.resolved"], &[]));
            assert_eq!(d.found.len(), 1, "{name}");
            assert_eq!(d.found[0].source, name);
            assert!(d.gate().next().is_none() && d.setup().next().is_none());
            assert!(d.run_caches().next().is_none());
            assert!(d.found[0].notes[0].contains(says), "{name}");
        }
        // OWL-638: Swift's note keeps its three commands and its two caveats.
        let d = detect(&root(&["Package.swift"], &[]));
        let notes = d.found[0].notes.join(" ");
        for part in [
            "swift package --disable-sandbox resolve",
            "swift build --disable-sandbox",
            "swift test --disable-sandbox",
            "TestingMacros",
            "Linux unchecked",
        ] {
            assert!(notes.contains(part), "{part}: {notes}");
        }
    }

    /// The notes of the tool chains in the home, `None` when there is none.
    fn toolchain_notes(root: &RootFiles) -> Option<Vec<String>> {
        let d = detect(root);
        let found = d
            .found
            .iter()
            .find(|f| f.source == "tool chains in the home")?;
        assert!(found.gate.is_empty() && found.setup.is_empty() && found.run_caches.is_empty());
        Some(found.notes.clone())
    }

    fn with_managers(mut root: RootFiles, managers: &[(&str, &str)]) -> RootFiles {
        root.home_managers = managers
            .iter()
            .map(|(name, folder)| HomeManager {
                name: name.to_string(),
                folder: folder.to_string(),
            })
            .collect();
        root
    }

    /// OWL-631: each file that pins a tool chain gives a note naming the
    /// managers that install it in the home; uv installs a pinned Python
    /// into its own run cache, and a `.tool-versions` naming nothing says
    /// nothing.
    #[test]
    fn a_pinned_tool_chain_names_the_managers_that_install_it_in_the_home() {
        let notes = toolchain_notes(&root(&[".python-version"], &[])).unwrap();
        assert_eq!(notes.len(), 1);
        assert!(
            notes[0].starts_with("`.python-version` names a Python"),
            "{notes:?}"
        );
        assert!(notes[0].contains("`~/.pyenv`") && notes[0].contains("`allow_caches`"));

        let uv = root(&[".python-version", "uv.lock"], &[("pyproject.toml", "")]);
        assert_eq!(toolchain_notes(&uv), None);

        let notes = toolchain_notes(&root(&[".ruby-version", "Gemfile"], &[])).unwrap();
        assert_eq!(notes.len(), 1);
        assert!(
            notes[0].starts_with("`.ruby-version` names a Ruby"),
            "{notes:?}"
        );
        assert!(notes[0].contains("`~/.rbenv`"));

        let versions = "# pinned\npython 3.12.4\n\nnodejs 22.1.0 system\n";
        let notes = toolchain_notes(&root(&[], &[(".tool-versions", versions)])).unwrap();
        assert_eq!(notes.len(), 1);
        assert!(
            notes[0].starts_with("`.tool-versions` names python, nodejs:"),
            "{notes:?}"
        );
        assert!(notes[0].contains("`~/.asdf`"));

        let empty = root(&[], &[(".tool-versions", "# none yet\n\n")]);
        assert_eq!(toolchain_notes(&empty), None);
    }

    /// OWL-631: an unreadable `.tool-versions` is skipped under its name.
    #[test]
    fn an_unread_tool_versions_is_skipped() {
        let d = detect(&root(&[".tool-versions"], &[]));
        assert_eq!(d.skipped, [".tool-versions: it could not be read"]);
    }

    /// OWL-631: a manager found with its installs in the home names the
    /// folder to declare, for a root of its language only, and replaces the
    /// note of the file it reads.
    #[test]
    fn a_manager_in_the_home_names_its_folder_for_its_language_only() {
        let python = with_managers(
            root(&[".python-version", "requirements.txt"], &[]),
            &[("pyenv", ".pyenv"), ("rbenv", ".rbenv")],
        );
        let notes = toolchain_notes(&python).unwrap();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].starts_with("pyenv keeps its Pythons in `~/.pyenv`"));
        assert!(notes[0].contains("declare `caches = [\".pyenv\"]` under `[stack]`"));

        let rust = with_managers(
            root(&["Cargo.lock"], &[("Cargo.toml", CARGO)]),
            &[("pyenv", ".pyenv"), ("rbenv", ".rbenv"), ("asdf", ".asdf")],
        );
        assert_eq!(toolchain_notes(&rust), None);

        let ruby = with_managers(
            root(&["Gemfile", ".ruby-version"], &[]),
            &[("rbenv", "rubies/rbenv")],
        );
        let notes = toolchain_notes(&ruby).unwrap();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("`~/rubies/rbenv`") && notes[0].contains("\"rubies/rbenv\""));

        // asdf only with a `.tool-versions`, whose note it replaces.
        let asdf = &[("asdf", ".asdf")];
        assert_eq!(
            toolchain_notes(&with_managers(root(&["Gemfile"], &[]), asdf)),
            None
        );
        let pinned = root(&[], &[(".tool-versions", "ruby 3.3.0\n")]);
        let notes = toolchain_notes(&with_managers(pinned, asdf)).unwrap();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].starts_with("asdf keeps the tool chains `.tool-versions` names (ruby)"));

        // pipx only beside the lock of poetry or pdm.
        let pipx = &[("pipx", ".local/pipx")];
        let pip = root(&["requirements.txt"], &[("pyproject.toml", "")]);
        assert_eq!(toolchain_notes(&with_managers(pip, pipx)), None);
        let poetry = root(&["poetry.lock"], &[("pyproject.toml", "")]);
        let notes = toolchain_notes(&with_managers(poetry, pipx)).unwrap();
        assert!(notes[0].starts_with("pipx keeps the tools it installs in `~/.local/pipx`"));
    }

    /// OWL-628: Deno installs its lock, checks, lints and tests with its
    /// cache in a run cache; without the lock the tests write none.
    #[test]
    fn a_deno_root_installs_its_lock_and_runs_fmt_lint_and_test() {
        let d = detect(&root(&["deno.json", "deno.lock"], &[]));
        assert_eq!(d.found[0].source, "deno.json (deno.lock)");
        assert_eq!(commands(d.setup()), ["deno install --frozen"]);
        assert_eq!(
            commands(d.gate()),
            [
                "deno fmt --check",
                "deno lint",
                "deno test --frozen --permit-no-files"
            ]
        );
        assert_eq!(run_caches(d.run_caches()), [("DENO_DIR", "deno")]);
        assert!(d.found[0].notes.is_empty());
        let d = detect(&root(&["deno.jsonc"], &[]));
        assert_eq!(d.found[0].source, "deno.jsonc");
        assert!(d.setup().next().is_none());
        assert_eq!(
            d.gate().last().unwrap().command,
            "deno test --no-lock --permit-no-files"
        );
        assert_eq!(run_caches(d.run_caches()), [("DENO_DIR", "deno")]);
        assert!(d.found[0].notes[0].contains("no lock file"));
        assert!(detect(&root(&["deno.lock"], &[])).found.is_empty());
    }

    /// Every run cache `init` can write passes the check the project file's
    /// loader makes (OWL-624, criterion 4), together with the others a root
    /// can hold beside it.
    #[test]
    fn every_run_cache_detected_passes_the_names_check() {
        let all = &[
            "Cargo.lock",
            "go.mod",
            "pnpm-lock.yaml",
            "poetry.lock",
            "Gemfile",
            "Gemfile.lock",
            "gradlew",
            "build.gradle.kts",
        ][..];
        let roots = [
            root(
                all,
                &[
                    ("Cargo.toml", CARGO),
                    ("package.json", "{}"),
                    ("pyproject.toml", PY),
                ],
            ),
            root(
                &["package-lock.json", "uv.lock"],
                &[("package.json", "{}"), ("pyproject.toml", PY)],
            ),
            root(
                &[
                    "pdm.lock",
                    "pom.xml",
                    "composer.json",
                    "composer.lock",
                    "App.sln",
                ],
                &[("pyproject.toml", PY)],
            ),
            root(&["mix.exs", "mix.lock", "deno.json"], &[]),
            root(&["yarn.lock", ".yarnrc.yml"], &[("package.json", "{}")]),
        ];
        let mut seen = 0;
        let mut pairs = vec![];
        for r in &roots {
            let d = detect(r);
            let entries = run_caches(d.run_caches());
            seen += entries.len();
            crate::agent_env::check_run_caches(&entries).unwrap();
            pairs.extend(
                entries
                    .iter()
                    .map(|(v, f)| (v.to_ascii_lowercase(), f.to_ascii_lowercase())),
            );
        }
        assert_eq!(seen, 22);
        // OWL-630: one variable is one folder whatever its source, and one
        // folder one variable, ignoring case as the loader does, so the
        // project file can name a variable once for every folder below the
        // root that needs it.
        for (variable, folder) in &pairs {
            assert!(
                pairs.iter().all(|(v, f)| (v == variable) == (f == folder)),
                "{variable} = {folder}"
            );
        }
    }

    #[test]
    fn a_root_holding_several_ecosystems_lists_them_in_the_fixed_order() {
        let d = detect(&root(
            &[
                "pnpm-lock.yaml",
                "Cargo.lock",
                "gradlew",
                "settings.gradle",
                "Gemfile",
                "go.mod",
                "requirements.txt",
                "App.csproj",
                "composer.json",
                "pom.xml",
                "deno.json",
                "Package.swift",
                "pubspec.yaml",
                "mix.exs",
                ".ruby-version",
            ],
            &[
                ("package.json", r#"{"scripts":{"test":"vitest"}}"#),
                ("Cargo.toml", CARGO),
                ("Makefile", "lint:\n"),
                ("pyproject.toml", PY),
            ],
        ));
        let sources: Vec<&str> = d.found.iter().map(|f| f.source.as_str()).collect();
        assert_eq!(
            sources,
            [
                "Cargo.toml",
                "go.mod",
                "package.json (pnpm-lock.yaml)",
                "pyproject.toml (requirements.txt)",
                "Gemfile",
                "tool chains in the home",
                "gradlew",
                "pom.xml",
                "composer.json",
                "App.csproj",
                "mix.exs",
                "pubspec.yaml",
                "Package.swift",
                "deno.json",
                "Makefile"
            ]
        );
        assert_eq!(
            commands(d.gate()),
            [
                "cargo fmt --all -- --check",
                "cargo clippy --workspace --all-targets --locked -- -D warnings",
                "cargo test --workspace --locked",
                "! gofmt -l . | grep .",
                "go vet ./...",
                "go test ./...",
                "pnpm run test",
                ".venv/bin/ruff check .",
                ".venv/bin/pytest",
                "./gradlew check --no-daemon",
                "mvn -B -Dmaven.repo.local=\"$MAVEN_USER_HOME/repository\" verify",
                "DOTNET_SYSTEM_NET_SECURITY_USENETWORKFRAMEWORK=1 dotnet test -m:1 -nodeReuse:false \
                 App.csproj",
                "mix test",
                "deno fmt --check",
                "deno lint",
                "deno test --no-lock --permit-no-files",
                "make lint"
            ]
        );
        assert!(d.gate().all(|l| !l.read_from.is_empty()));
        assert!(d.setup().all(|l| !l.read_from.is_empty()));
    }

    const SCRIPTS: &str = r#"{"scripts":{"lint":"eslint .","test":"jest"}}"#;

    fn sources(d: &Detection) -> Vec<&str> {
        d.found.iter().map(|f| f.source.as_str()).collect()
    }

    /// OWL-630: Locary's shape. A root Makefile delegates to `backend/` (uv)
    /// and `frontend/` (pnpm), each with a Makefile of its own, which the
    /// root's covers; the folders' lines run in them and their run caches
    /// are named once.
    #[test]
    fn a_repository_of_folders_is_detected_one_folder_down() {
        let d = detect(&with_folders(
            root(&[], &[("Makefile", "lint:\ntest:\n")]),
            vec![
                folder(
                    "backend",
                    &["uv.lock"],
                    &[
                        ("pyproject.toml", "[tool.ruff]\n[tool.pytest.ini_options]\n"),
                        ("Makefile", "lint:\n"),
                    ],
                ),
                folder(
                    "frontend",
                    &["pnpm-lock.yaml"],
                    &[("package.json", SCRIPTS), ("Makefile", "test:\n")],
                ),
            ],
        ));
        assert_eq!(
            sources(&d),
            [
                "Makefile",
                "backend/pyproject.toml (uv.lock)",
                "frontend/package.json (pnpm-lock.yaml)"
            ]
        );
        assert_eq!(
            commands(d.gate()),
            [
                "make lint",
                "make test",
                "cd backend && uv run ruff check .",
                "cd backend && uv run pytest",
                "cd frontend && pnpm run lint",
                "cd frontend && pnpm run test"
            ]
        );
        assert_eq!(
            commands(d.setup()),
            ["cd frontend && pnpm install --frozen-lockfile"]
        );
        assert_eq!(
            d.setup().next().unwrap().read_from,
            "pnpm-lock.yaml, the lock file of pnpm, in frontend/"
        );
        assert_eq!(
            run_caches(d.merged_run_caches().iter()),
            [
                ("UV_CACHE_DIR", "uv"),
                ("UV_PYTHON_INSTALL_DIR", "uv-python"),
                ("PNPM_HOME", "pnpm"),
                ("XDG_RUNTIME_DIR", "pnpm-locks"),
                ("COREPACK_HOME", "corepack")
            ]
        );
        let frontend = &d.found[2];
        assert!(!frontend.notes.is_empty() && !frontend.gate_notes.is_empty());
        assert!(
            frontend
                .notes
                .iter()
                .chain(&frontend.gate_notes)
                .all(|n| n.starts_with("in `frontend/`: ")),
            "{frontend:?}"
        );
        assert!(d.skipped.is_empty(), "{:?}", d.skipped);
    }

    /// OWL-630: a folder of a kind the root found is a member of the root's
    /// workspace, which the root's lines cover, unless it holds a lock of
    /// its own. A Go module is its own wherever it lies; a Makefile below a
    /// root Makefile is the root's.
    #[test]
    fn the_roots_workspace_covers_its_members_unless_they_hold_their_own_lock() {
        let pytest = "[tool.pytest.ini_options]\n";
        let cases = [
            (
                root(
                    &["Cargo.lock"],
                    &[("Cargo.toml", "[workspace]\nmembers = [\"member\"]\n")],
                ),
                ("Cargo.toml", CARGO),
                "Cargo.lock",
                "member/Cargo.toml",
            ),
            (
                root(
                    &["pnpm-lock.yaml", "pnpm-workspace.yaml"],
                    &[("package.json", SCRIPTS)],
                ),
                ("package.json", SCRIPTS),
                "pnpm-lock.yaml",
                "member/package.json (pnpm-lock.yaml)",
            ),
            (
                root(&["uv.lock"], &[("pyproject.toml", pytest)]),
                ("pyproject.toml", pytest),
                "uv.lock",
                "member/pyproject.toml (uv.lock)",
            ),
        ];
        for (top, manifest, lock, source) in cases {
            let member = detect(&with_folders(
                top.clone(),
                vec![
                    folder("member", &[], &[manifest]),
                    // A member's manifest is the root's to read.
                    folder("broken", &[], &[(manifest.0, "{[")]),
                ],
            ));
            assert_eq!(member.found.len(), 1, "{member:?}");
            assert!(member.skipped.is_empty(), "{:?}", member.skipped);
            let own = detect(&with_folders(
                top,
                vec![folder("member", &[lock], &[manifest])],
            ));
            assert_eq!(sources(&own)[1..], [source]);
        }
        let d = detect(&with_folders(
            root(&["go.mod"], &[("Makefile", "test:\n")]),
            vec![folder("tools", &["go.mod"], &[("Makefile", "lint:\n")])],
        ));
        assert_eq!(sources(&d), ["go.mod", "Makefile", "tools/go.mod"]);
    }

    /// OWL-630: a variable two folders need is written once, one folder
    /// serving both, its comment naming each source.
    #[test]
    fn a_variable_two_folders_need_is_written_once() {
        let d = detect(&with_folders(
            RootFiles::default(),
            vec![
                folder("admin", &["pnpm-lock.yaml"], &[("package.json", SCRIPTS)]),
                folder("web", &["pnpm-lock.yaml"], &[("package.json", SCRIPTS)]),
            ],
        ));
        assert_eq!(d.run_caches().count(), 6);
        let merged = d.merged_run_caches();
        let entries = run_caches(merged.iter());
        assert_eq!(
            entries,
            [
                ("PNPM_HOME", "pnpm"),
                ("XDG_RUNTIME_DIR", "pnpm-locks"),
                ("COREPACK_HOME", "corepack")
            ]
        );
        assert_eq!(
            merged[0].read_from,
            "pnpm-lock.yaml, the lock file of pnpm, in admin/; pnpm-lock.yaml, the lock file of \
             pnpm, in web/"
        );
        crate::agent_env::check_run_caches(&entries).unwrap();
    }

    /// OWL-630: whatever its name and its command, a folder's line runs in
    /// it; what could not be read or listed is named by its folder.
    #[test]
    fn a_folders_lines_run_in_it_whatever_its_name_or_its_command() {
        let d = detect(&with_folders(
            RootFiles::default(),
            vec![
                folder("-web", &["go.mod"], &[]),
                folder("my app", &["go.mod"], &[]),
                folder("services/api", &["mix.exs", "mix.lock"], &[]),
                folder("site", &[], &[("package.json", "{")]),
                Folder {
                    path: "locked".to_owned(),
                    unlisted: true,
                    ..Folder::default()
                },
            ],
        ));
        let setup = commands(d.setup());
        assert_eq!(
            setup[..2],
            [
                "cd ./-web && go mod download",
                "cd 'my app' && go mod download"
            ]
        );
        // Mix's install holds `;` and `||`, which bind looser than `&&`.
        assert_eq!(setup[2], format!("cd services/api && {{ {MIX_INSTALL}; }}"));
        assert_eq!(setup[3], "cd services/api && mix deps.get --check-locked");
        assert!(commands(d.gate()).contains(&"cd ./-web && ! gofmt -l . | grep ."));
        assert_eq!(
            d.found[0].notes,
            [
                "in `-web/`: Go keeps the modules it downloads read-only: to empty the run caches, \
              make their folder writable (`chmod -R u+w`) before deleting it"
            ]
        );
        assert_eq!(
            d.skipped,
            [
                "site/package.json: it is not valid JSON",
                "locked/: it could not be listed"
            ]
        );
    }

    /// OWL-630: the folders of `apps/` and `services/` are projects too,
    /// unless the container's own workspace covers them.
    #[test]
    fn a_containers_folders_are_projects_unless_its_workspace_covers_them() {
        let d = detect(&with_folders(
            RootFiles::default(),
            vec![
                folder("apps", &["pnpm-lock.yaml"], &[("package.json", SCRIPTS)]),
                folder("apps/web", &[], &[("package.json", SCRIPTS)]),
                folder("services/api", &["go.mod"], &[]),
            ],
        ));
        assert_eq!(
            sources(&d),
            ["apps/package.json (pnpm-lock.yaml)", "services/api/go.mod"]
        );
    }

    #[test]
    fn the_folders_below_the_root_that_are_read() {
        for (name, read) in [
            ("backend", true),
            ("apps", true),
            ("my app", true),
            ("-web", true),
            (".github", false),
            (".venv", false),
            ("node_modules", false),
            ("vendor", false),
            ("target", false),
            ("dist", false),
            ("build", false),
            ("a\nb", false),
            ("", false),
        ] {
            assert_eq!(reads_folder(name), read, "{name:?}");
        }
    }

    /// OWL-630: the tool chains in the home are the machine's, so a note the
    /// root or a folder wrote is not written again.
    #[test]
    fn a_tool_chain_note_is_written_once_for_the_root_and_its_folders() {
        let d = detect(&with_folders(
            root(&[".python-version"], &[]),
            vec![
                folder("backend", &[".python-version", ".ruby-version"], &[]),
                folder("worker", &[".python-version"], &[]),
            ],
        ));
        assert_eq!(
            sources(&d),
            [
                "tool chains in the home",
                "tool chains in the home, for backend/"
            ]
        );
        assert_eq!(d.found[1].notes.len(), 1);
        assert!(
            d.found[1].notes[0].starts_with("in `backend/`: `.ruby-version` names a Ruby"),
            "{:?}",
            d.found[1].notes
        );
    }
}
