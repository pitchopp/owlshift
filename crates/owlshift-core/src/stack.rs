//! Stack detection for `owlshift init` (P9, OWL-539): the gate, the setup it
//! needs, the caches of the home that setup reads confined (OWL-593) and the
//! caches of Owlshift's own its runs may write (`stack.run_caches`, OWL-617),
//! read from the names and contents of the repository's root files.
//!
//! A pure function: the caller reads the root and passes in the file names and
//! the contents of the four manifests it recognises (`Cargo.toml`,
//! `package.json`, `pyproject.toml`, `Makefile`); lock files, and the files of
//! Go, pip, bundler and Gradle (OWL-624), of pdm, Maven, Composer and .NET
//! (OWL-627), are read by name only. Nothing detected here is ever run. The
//! commands were checked with real calls, recorded in
//! `docs/design/runtime-and-operations.md`, "Stack detection (P9)", and in the
//! records of OWL-617, OWL-624 and OWL-627; a tool that could not be
//! checked is not detected, and a run cache whose live check did not show it
//! needed is not written, only named in a note.

use serde::{Deserialize, Serialize};

/// The root of a repository, as the runner read it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootFiles {
    /// The names of the files at the root.
    pub names: Vec<String>,
    /// The contents of the manifests the runner read, by name. A manifest in
    /// `names` with no entry here could not be read.
    pub files: Vec<(String, String)>,
}

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
    /// The folders of the home the setup reads, as `stack.caches` names
    /// them: each line's `command` is the folder (OWL-593).
    pub caches: Vec<Line>,
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
    /// (`pyproject.toml`, `requirements.txt`), `Gemfile`, Gradle, Maven,
    /// Composer, .NET, `Makefile`.
    pub found: Vec<Found>,
    /// A manifest that could not be read or parsed, with why. Never an error.
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

    /// The detected cache folders, in order; a line's `command` is the folder.
    pub fn caches(&self) -> impl Iterator<Item = &Line> {
        self.found.iter().flat_map(|f| f.caches.iter())
    }

    /// The detected caches a run may write, in order.
    pub fn run_caches(&self) -> impl Iterator<Item = &RunCache> {
        self.found.iter().flat_map(|f| f.run_caches.iter())
    }
}

/// The manifests [`detect`] reads, which the runner must read.
pub const MANIFESTS: [&str; 4] = ["Cargo.toml", "package.json", "pyproject.toml", "Makefile"];

/// The other root files [`detect`] reads when the runner could, for what
/// they say of a manifest's setup: yarn's settings (OWL-593). One missing
/// or unread counts as empty.
pub const SETTINGS: [&str; 1] = [".yarnrc.yml"];

/// One source of [`detect`]: what it found in the root, `None` when the root
/// holds nothing of it, or why its manifest was skipped.
type Source = fn(&RootFiles) -> Result<Option<Found>, String>;

/// Detect the stack of a root.
pub fn detect(root: &RootFiles) -> Detection {
    // Each with the name it is skipped under. Only a source that reads a
    // manifest's text, of `MANIFESTS`, can be skipped; the others read names
    // and keep a name here for the list's sake.
    let sources: [(&str, Source); 10] = [
        ("Cargo.toml", cargo),
        ("go.mod", go),
        ("package.json", package_json),
        ("pyproject.toml", python),
        ("Gemfile", ruby),
        ("gradlew", gradle),
        ("pom.xml", maven),
        ("composer.json", composer),
        (".NET", dotnet),
        ("Makefile", makefile),
    ];
    let mut out = Detection::default();
    for (name, source) in sources {
        match source(root) {
            Ok(Some(found)) => out.found.push(found),
            Ok(None) => {}
            Err(why) => out.skipped.push(format!("{name}: {why}")),
        }
    }
    out
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
        caches: vec![],
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
        caches: vec![],
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
    install: String,
    /// The folder of the home the install must read, read-only, in a
    /// confined run: its cache, which it cannot do without (OWL-593), with
    /// what it was read from.
    cache: Option<(&'static str, String)>,
    /// The cache of Owlshift's own the install writes instead (OWL-617): the
    /// variable the manager reads for it and the folder's name.
    run_cache: Option<(&'static str, &'static str)>,
    /// What the person should know about the cache, when none is declared.
    note: Option<String>,
}

/// yarn 2 or later's global cache, `~/.yarn/berry` (OWL-593).
const YARN_BERRY: &str = ".yarn/berry";

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

/// Whether a yarn 2 or later project installs from yarn's global cache in
/// the home (OWL-593): only when that is established, since a project file
/// declaring a cache no machine needs is refused until each operator allows
/// it. It is when the root's `.yarnrc.yml` sets `enableGlobalCache: true`,
/// or when `package.json`'s `packageManager` names yarn 4 or later, whose
/// default it is; and in neither case when `.yarnrc.yml` sets
/// `enableGlobalCache: false`, a `cacheFolder` or a `globalFolder`, which
/// move it. Only the top-level keys of `.yarnrc.yml` are read, line by line.
fn yarn_global_cache(root: &RootFiles, package: &serde_json::Value) -> Option<String> {
    let rc = root.content(".yarnrc.yml").unwrap_or_default();
    let setting = |key: &str| {
        rc.lines().find_map(|raw| {
            let value = raw.strip_prefix(key)?.strip_prefix(':')?;
            let value = value.split('#').next().unwrap_or_default().trim();
            Some(value.trim_matches(['"', '\'']).to_owned())
        })
    };
    if setting("cacheFolder").is_some() || setting("globalFolder").is_some() {
        return None;
    }
    match setting("enableGlobalCache").as_deref() {
        Some("false") => return None,
        Some("true") => return Some("`enableGlobalCache: true` in .yarnrc.yml".to_owned()),
        _ => {}
    }
    let (manager, major) = package_manager(package, "yarn")?;
    (major >= 4).then(|| format!("`{manager}`, the `packageManager` of package.json"))
}

/// Which manager the lock file names, its install, and the cache it needs
/// confined. Checked live (OWL-593): yarn 1 skips a cache it cannot write
/// and bun falls back to one of its own, both fetching from the registry,
/// so they need none; yarn 4 on its global cache fails with `EPERM` unless
/// it reads it ([`yarn_global_cache`]). npm and pnpm 10 install online into
/// a cache of Owlshift's own their runs write, `npm_config_cache` and
/// `npm_config_store_dir` moving it (OWL-617, checked live on macOS and
/// Linux); pnpm 12 failed confined whatever its store (OWL-598), so a
/// `packageManager` naming a pnpm other than 10 keeps the plain install and
/// gets no run cache.
fn node_manager(root: &RootFiles, package: &serde_json::Value) -> Option<NodeManager> {
    let plain = |name, lock, install: String| NodeManager {
        name,
        lock,
        install,
        cache: None,
        run_cache: None,
        note: None,
    };
    if root.has("pnpm-lock.yaml") {
        let pnpm = plain(
            "pnpm",
            "pnpm-lock.yaml",
            "pnpm install --frozen-lockfile".into(),
        );
        Some(match package_manager(package, "pnpm") {
            Some((manager, major)) if major != 10 => NodeManager {
                note: Some(format!(
                    "`{manager}`, the `packageManager` of package.json, is not pnpm 10: no run \
                     cache was declared for its store, only pnpm 10 having been checked to \
                     install into one; `owlshift gate` checks the setup"
                )),
                ..pnpm
            },
            _ => NodeManager {
                run_cache: Some(("npm_config_store_dir", "pnpm-store")),
                ..pnpm
            },
        })
    } else if root.has("yarn.lock") {
        // `.yarnrc.yml` is yarn 2 or later.
        Some(if root.has(".yarnrc.yml") {
            let berry = plain("yarn", "yarn.lock", "yarn install --immutable".into());
            match yarn_global_cache(root, package) {
                Some(from) => NodeManager {
                    cache: Some((YARN_BERRY, from)),
                    ..berry
                },
                None => NodeManager {
                    note: Some(format!(
                        "no yarn cache was declared, yarn's global cache not being established \
                         here: when the setup installs from `~/{YARN_BERRY}`, add \
                         `caches = [\"{YARN_BERRY}\"]`, which each machine allows in \
                         `allow_caches`"
                    )),
                    ..berry
                },
            }
        } else {
            plain("yarn", "yarn.lock", "yarn install --frozen-lockfile".into())
        })
    } else if root.has("bun.lock") {
        Some(plain(
            "bun",
            "bun.lock",
            "bun install --frozen-lockfile".into(),
        ))
    } else if root.has("bun.lockb") {
        Some(plain(
            "bun",
            "bun.lockb",
            "bun install --frozen-lockfile".into(),
        ))
    } else if root.has("package-lock.json") {
        Some(NodeManager {
            run_cache: Some(("npm_config_cache", "npm")),
            ..plain("npm", "package-lock.json", "npm ci".into())
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
    let mut found = Found {
        source: "package.json".to_string(),
        gate: vec![],
        setup: vec![],
        caches: vec![],
        run_caches: vec![],
        notes: vec![],
        gate_notes: vec![],
    };
    let manager = match node_manager(root, &value) {
        Some(manager) => {
            let (name, lock) = (manager.name, manager.lock);
            found.source = format!("package.json ({lock})");
            found.setup.push(line(
                manager.install,
                format!("{lock}, the lock file of {name}"),
            ));
            found.notes.extend(manager.note);
            if let Some((variable, folder)) = manager.run_cache {
                found.run_caches.push(run_cache(
                    variable,
                    folder,
                    format!("{lock}, the lock file of {name}"),
                ));
            }
            if let Some((folder, from)) = manager.cache {
                found.caches.push(line(folder, from));
                found.notes.push(format!(
                    "the setup installs from {name}'s cache, `~/{folder}`, read-only: each \
                     machine allows `{folder}` in `allow_caches` of its personal configuration, \
                     and its cache must hold the project's packages"
                ));
            }
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
            "pdm saves the Python it chose in `.pdm-python` at the root: ignore it in \
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
            "no Gradle wrapper (`gradlew`) at the root, so nothing was written: the gate runs the \
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

/// Maven (OWL-627), from `pom.xml`, read by name, run by the machine's `mvn`:
/// `verify` runs the tests and the checks the build binds. Maven moves its
/// local repository with a property only, so the line sets `maven.repo.local`
/// inside the run cache `MAVEN_USER_HOME`. Checked live, confined: without the
/// property Maven fails creating `~/.m2/repository`. The wrapper `mvnw` is not
/// used, a note says why: it makes its download folder with `mktemp -d`,
/// which on macOS writes to the user's temporary folder, closed to a confined
/// run whatever its `TMPDIR` (checked live).
fn maven(root: &RootFiles) -> Result<Option<Found>, String> {
    if !root.has("pom.xml") {
        return Ok(None);
    }
    let from = "pom.xml";
    let mut found = found(from);
    found.gate.push(line(
        "mvn -B -Dmaven.repo.local=\"$MAVEN_USER_HOME/repository\" verify",
        from,
    ));
    if root.has("mvnw") {
        found.notes.push(
            "the gate runs the machine's `mvn`, not the wrapper `mvnw`, which fails confined on \
             macOS making its download folder in the user's temporary folder: Maven must be \
             installed on each machine, and it runs instead of the version the wrapper pins"
                .to_string(),
        );
    }
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
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
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
fn dotnet(root: &RootFiles) -> Result<Option<Found>, String> {
    let lines = |file: &str| {
        format!(
            "the setup is `{DOTNET} restore {DOTNET_FLAGS} {file}` and the gate `{DOTNET} test \
             {DOTNET_FLAGS} {file}`, with `run_caches.DOTNET_CLI_HOME = \"dotnet\"`"
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
                "global.json names a .NET SDK, but no solution or project is at the root, so \
                 nothing was written: {}",
                lines("<solution>")
            ));
            return Ok(Some(found));
        }
        [] => return Ok(None),
        _ => {
            let mut found = found(files.join(", "));
            found.notes.push(format!(
                "{} are at the root, so nothing was written: for the one that builds the rest, \
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
        .push(run_cache("DOTNET_CLI_HOME", "dotnet", from));
    found.notes.push(
        "the dotnet lines build in one process and, on macOS, reach NuGet through \
         Network.framework, since the sandbox closes MSBuild's worker nodes and the keychain .NET's \
         TLS reads otherwise"
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
        caches: vec![],
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
        }
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
                &[("npm_config_store_dir", "pnpm-store")],
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
                &[],
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
            // OWL-617: npm and pnpm install online into a run cache, and
            // read none of the home's.
            assert_eq!(commands(d.caches()), Vec::<&str>::new(), "{names:?}");
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
            // yarn 2 or later, its cache unknown, says what to declare; the
            // gate's own note is a guess (OWL-595).
            let notes = &d.found[0].notes;
            let noted = names.contains(&".yarnrc.yml");
            assert_eq!(notes.len(), usize::from(noted), "{notes:?}");
            assert!(notes.iter().all(|n| n.contains("allow_caches")));
            let gate_notes = &d.found[0].gate_notes;
            assert_eq!(gate_notes.len(), 1, "{gate_notes:?}");
            assert!(gate_notes[0].contains("a guess"), "{gate_notes:?}");
            assert!(d.found[0].source.starts_with("package.json ("));
            assert!(d.setup().all(|l| l.read_from.contains(manager)));
            assert!(d.run_caches().all(|c| c.read_from.contains(manager)));
        }
    }

    /// OWL-593: yarn 2 or later declares `~/.yarn/berry` only when its
    /// global cache is established: yarn 4 by `packageManager`, or
    /// `enableGlobalCache: true`, unless `.yarnrc.yml` turns it off or moves
    /// it; otherwise a note says what to declare.
    #[test]
    fn yarns_global_cache_is_declared_only_when_established() {
        let cases: [(&str, &str, bool); 7] = [
            ("yarn@4.5.0", "nodeLinker: node-modules\n", true),
            ("", "enableGlobalCache: true # on\n", true),
            ("yarn@3.6.4", "nodeLinker: node-modules\n", false),
            ("", "nodeLinker: node-modules\n", false),
            ("yarn@4.5.0", "enableGlobalCache: false\n", false),
            ("yarn@4.5.0", "cacheFolder: ./.yarn/cache\n", false),
            ("yarn@4.5.0", "globalFolder: /opt/yarn\n", false),
        ];
        for (manager, rc, global) in cases {
            let pkg = if manager.is_empty() {
                "{}".to_owned()
            } else {
                format!(r#"{{"packageManager":"{manager}"}}"#)
            };
            let d = detect(&root(
                &["yarn.lock"],
                &[("package.json", pkg.as_str()), (".yarnrc.yml", rc)],
            ));
            let caches = if global { &[YARN_BERRY][..] } else { &[] };
            assert_eq!(commands(d.caches()), caches, "{manager} {rc:?}");
            assert_eq!(d.found[0].notes.len(), 1, "{manager} {rc:?}");
            assert_eq!(
                d.found[0].notes[0].starts_with("no yarn cache"),
                !global,
                "{manager} {rc:?}"
            );
        }
    }

    /// OWL-617: pnpm's store goes to a run cache, unless `packageManager`
    /// names a pnpm other than 10, which declares none and says why; the
    /// install is pnpm's plain one either way.
    #[test]
    fn pnpms_store_is_a_run_cache_unless_package_manager_names_another_major() {
        let cases: [(&str, bool); 6] = [
            ("", true),
            ("pnpm@10.33.2", true),
            ("pnpm@10", true),
            ("pnpm@^10", true),
            ("pnpm@9.15.0", false),
            ("pnpm@12.21.0+sha512.abc", false),
        ];
        for (manager, ten) in cases {
            let pkg = format!(r#"{{"packageManager":"{manager}"}}"#);
            let d = detect(&root(&["pnpm-lock.yaml"], &[("package.json", &pkg)]));
            let notes = &d.found[0].notes;
            assert_eq!(
                commands(d.setup()),
                ["pnpm install --frozen-lockfile"],
                "{manager}"
            );
            assert!(d.caches().next().is_none(), "{manager}");
            if ten {
                assert_eq!(
                    run_caches(d.run_caches()),
                    [("npm_config_store_dir", "pnpm-store")],
                    "{manager}"
                );
                assert!(notes.is_empty(), "{manager} {notes:?}");
            } else {
                assert!(d.run_caches().next().is_none(), "{manager}");
                assert_eq!(notes.len(), 1, "{manager} {notes:?}");
                assert!(notes[0].starts_with(&format!("`{manager}`, the `packageManager`")));
                assert!(notes[0].contains("is not pnpm 10"));
            }
        }
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

    /// OWL-627: the machine's `mvn`, its local repository inside the run
    /// cache; a wrapper `mvnw` is named in a note, never run.
    #[test]
    fn a_maven_build_runs_verify_with_its_repository_in_a_run_cache() {
        for names in [&["pom.xml", "mvnw"][..], &["pom.xml"]] {
            let d = detect(&root(names, &[]));
            assert_eq!(d.found[0].source, "pom.xml");
            assert_eq!(
                commands(d.gate()),
                ["mvn -B -Dmaven.repo.local=\"$MAVEN_USER_HOME/repository\" verify"]
            );
            assert!(d.setup().next().is_none());
            assert_eq!(run_caches(d.run_caches()), [("MAVEN_USER_HOME", "maven")]);
            assert!(d.found[0].gate_notes[0].contains("MAVEN_USER_HOME=~/.m2"));
            let notes = &d.found[0].notes;
            assert_eq!(notes.len(), usize::from(names.contains(&"mvnw")));
            assert!(notes.iter().all(|n| n.contains("must be installed")));
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
    /// switch's minimum SDK.
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
            assert_eq!(run_caches(d.run_caches()), [("DOTNET_CLI_HOME", "dotnet")]);
            let notes = &d.found[0].notes;
            assert!(notes[0].contains("Network.framework"), "{notes:?}");
            let pinned = names.contains(&"global.json");
            assert_eq!(notes.len(), 1 + usize::from(pinned), "{notes:?}");
            assert!(notes.iter().skip(1).all(|n| n.contains(".NET 10")));
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
            assert!(
                d.found[0].notes[0].contains("run_caches.DOTNET_CLI_HOME"),
                "{names:?}"
            );
        }
        assert!(detect(&root(&[".sln", "csproj"], &[])).found.is_empty());
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
        ];
        let mut seen = 0;
        for r in &roots {
            let d = detect(r);
            let entries = run_caches(d.run_caches());
            seen += entries.len();
            crate::agent_env::check_run_caches(&entries).unwrap();
        }
        assert_eq!(seen, 14);
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
                "gradlew",
                "pom.xml",
                "composer.json",
                "App.csproj",
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
                "make lint"
            ]
        );
        assert!(d.gate().all(|l| !l.read_from.is_empty()));
        assert!(d.setup().all(|l| !l.read_from.is_empty()));
    }
}
