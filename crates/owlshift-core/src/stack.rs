//! Stack detection for `owlshift init` (P9, OWL-539): the gate, the setup it
//! needs and the caches of the home that setup reads confined (OWL-593), read
//! from the names and contents of the repository's root files.
//!
//! A pure function: the caller reads the root and passes in the file names and
//! the contents of the four manifests it recognises (`Cargo.toml`,
//! `package.json`, `pyproject.toml`, `Makefile`); lock files are read by name
//! only. Nothing detected here is ever run. The commands were checked with real
//! calls, recorded in `docs/design/runtime-and-operations.md`, "Stack detection
//! (P9)"; a tool that could not be checked is not detected.

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
    /// What the person should know about the lines above.
    pub notes: Vec<String>,
}

/// The result of [`detect`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Detection {
    /// In the fixed order `Cargo.toml`, `package.json`, `pyproject.toml`,
    /// `Makefile`.
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
}

/// The manifests [`detect`] reads, which the runner must read.
pub const MANIFESTS: [&str; 4] = ["Cargo.toml", "package.json", "pyproject.toml", "Makefile"];

/// Detect the stack of a root.
pub fn detect(root: &RootFiles) -> Detection {
    let mut out = Detection::default();
    for name in MANIFESTS {
        if !root.has(name) {
            continue;
        }
        let Some(text) = root.content(name) else {
            out.skipped.push(format!("{name}: it could not be read"));
            continue;
        };
        let result = match name {
            "Cargo.toml" => cargo(root, text),
            "package.json" => package_json(root, text),
            "pyproject.toml" => pyproject(root, text),
            _ => Ok(makefile(text)),
        };
        match result {
            Ok(Some(found)) => out.found.push(found),
            Ok(None) => {}
            Err(why) => out.skipped.push(format!("{name}: {why}")),
        }
    }
    out
}

fn line(command: impl Into<String>, read_from: impl Into<String>) -> Line {
    Line {
        command: command.into(),
        read_from: read_from.into(),
    }
}

fn cargo(root: &RootFiles, text: &str) -> Result<Option<Found>, String> {
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
        notes: vec![],
    }))
}

/// A Node package manager, as the lock file at the root names it.
struct NodeManager {
    name: &'static str,
    lock: &'static str,
    /// The setup's install command.
    install: String,
    /// The folder of the home the install must read, read-only, in a
    /// confined run: its cache, which it cannot do without (OWL-593).
    cache: Option<&'static str>,
}

/// npm's install under the sandbox (OWL-593): the home is closed, so npm
/// reads its cache, `~/.npm`, as a cache the operator allows, read-only.
/// `--prefer-offline` takes what the cache holds without asking the
/// registry, and `--logs-dir` writes npm's log in the run's temporary
/// folder, its default being inside the read-only cache. A package missing
/// from the cache fails the install: npm cannot write it there.
pub const NPM_SETUP: &str =
    "npm ci --prefer-offline --cache \"$HOME/.npm\" --logs-dir \"$TMPDIR/npm-logs\"";

/// Which manager the lock file names, its install, and the cache it needs
/// confined. Checked live (OWL-593): yarn 1 skips a cache it cannot write
/// and bun falls back to one of its own, both fetching from the registry,
/// so they need none; npm and yarn 2 or later (`.yarnrc.yml`) fail with
/// `EPERM` unless they read theirs; pnpm needs an overlay store, which
/// `init` does not write.
fn node_manager(root: &RootFiles) -> Option<NodeManager> {
    let plain = |name, lock, install: String| NodeManager {
        name,
        lock,
        install,
        cache: None,
    };
    if root.has("pnpm-lock.yaml") {
        Some(plain(
            "pnpm",
            "pnpm-lock.yaml",
            "pnpm install --frozen-lockfile".into(),
        ))
    } else if root.has("yarn.lock") {
        // `.yarnrc.yml` is yarn 2 or later, whose global cache is
        // `~/.yarn/berry` (the default since yarn 4).
        Some(if root.has(".yarnrc.yml") {
            NodeManager {
                cache: Some(".yarn/berry"),
                ..plain("yarn", "yarn.lock", "yarn install --immutable".into())
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
            cache: Some(".npm"),
            ..plain("npm", "package-lock.json", NPM_SETUP.into())
        })
    } else {
        None
    }
}

fn package_json(root: &RootFiles, text: &str) -> Result<Option<Found>, String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|_| "it is not valid JSON".to_string())?;
    let scripts = value.get("scripts").and_then(|s| s.as_object());
    let script = |key: &str| scripts.and_then(|s| s.get(key)).and_then(|v| v.as_str());
    let mut found = Found {
        source: "package.json".to_string(),
        gate: vec![],
        setup: vec![],
        caches: vec![],
        notes: vec![],
    };
    let manager = match node_manager(root) {
        Some(manager) => {
            let (name, lock) = (manager.name, manager.lock);
            let from = format!("{lock}, the lock file of {name}");
            found.source = format!("package.json ({lock})");
            found.setup.push(line(manager.install, from.clone()));
            if let Some(folder) = manager.cache {
                found.caches.push(line(folder, from.clone()));
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
    for key in ["lint", "test"] {
        let Some(body) = script(key) else { continue };
        if key == "test" && body.contains("no test specified") {
            found.notes.push(
                "the `test` script is npm's placeholder, which always fails: it was left out"
                    .to_string(),
            );
            continue;
        }
        found.gate.push(line(
            format!("{manager} run {key}"),
            format!("the `{key}` script of package.json"),
        ));
    }
    Ok(Some(found))
}

fn pyproject(root: &RootFiles, text: &str) -> Result<Option<Found>, String> {
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
    if tools.is_empty() {
        return Ok(None);
    }
    let mut found = Found {
        source: "pyproject.toml".to_string(),
        gate: vec![],
        setup: vec![],
        caches: vec![],
        notes: vec![],
    };
    // Only `uv run` was checked to install the locked environment itself; a
    // bare tool needs a setup, as does a manager not checked (poetry, pdm).
    let prefix = if root.has("uv.lock") {
        found.source = "pyproject.toml (uv.lock)".to_string();
        "uv run "
    } else {
        let manager = ["poetry", "pdm"]
            .into_iter()
            .find(|m| root.has(&format!("{m}.lock")));
        found.notes.push(match manager {
            Some(m) => format!(
                "pyproject.toml's tools are run bare and no setup was written, but {m}.lock \
                 names {m}: the gate needs `{m} run` in front of them and a setup that \
                 installs the project (not checked here)"
            ),
            None => "pyproject.toml's tools are run bare and no setup was written: the gate \
                     needs a setup that installs them"
                .to_string(),
        });
        ""
    };
    for (command, from) in tools {
        found
            .gate
            .push(line(format!("{prefix}{command}"), from.to_string()));
    }
    Ok(Some(found))
}

/// The `lint`, `test` and `check` targets a Makefile defines, in that order.
fn makefile(text: &str) -> Option<Found> {
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
        notes: vec![],
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
        let cases: [(&[&str], &str, &str, &[&str]); 6] = [
            (
                &["pnpm-lock.yaml"],
                "pnpm install --frozen-lockfile",
                "pnpm",
                &[],
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
                &[".yarn/berry"],
            ),
            (&["bun.lock"], "bun install --frozen-lockfile", "bun", &[]),
            (&["bun.lockb"], "bun install --frozen-lockfile", "bun", &[]),
            (&["package-lock.json"], NPM_SETUP, "npm", &[".npm"]),
        ];
        for (names, setup, manager, caches) in cases {
            let d = detect(&root(names, &[("package.json", pkg)]));
            assert_eq!(commands(d.setup()), [setup], "{names:?}");
            assert_eq!(commands(d.caches()), caches, "{names:?}");
            assert_eq!(
                commands(d.gate()),
                [format!("{manager} run lint"), format!("{manager} run test")],
                "{names:?}"
            );
            // A manager reading a cache says what each machine must allow.
            assert_eq!(d.found[0].notes.len(), usize::from(!caches.is_empty()));
            assert!(d.found[0].notes.iter().all(|n| n.contains("allow_caches")));
            assert!(d.found[0].source.starts_with("package.json ("));
            assert!(
                d.setup()
                    .chain(d.caches())
                    .all(|l| l.read_from.contains(manager))
            );
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
    }

    #[test]
    fn a_package_json_without_scripts_gives_the_setup_only() {
        let d = detect(&root(&["package-lock.json"], &[("package.json", "{}")]));
        assert_eq!(commands(d.setup()), [NPM_SETUP]);
        assert_eq!(commands(d.gate()), Vec::<&str>::new());
    }

    #[test]
    fn npms_placeholder_test_script_is_left_out() {
        let pkg = r#"{"scripts":{"test":"echo \"Error: no test specified\" && exit 1"}}"#;
        let d = detect(&root(&["package-lock.json"], &[("package.json", pkg)]));
        assert_eq!(commands(d.gate()), Vec::<&str>::new());
        assert!(d.found[0].notes.iter().any(|n| n.contains("placeholder")));
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
    }

    #[test]
    fn a_python_root_without_uv_runs_bare_and_says_a_setup_is_needed() {
        for names in [&[][..], &["poetry.lock"], &["pdm.lock"]] {
            let d = detect(&root(names, &[("pyproject.toml", PY)]));
            assert_eq!(commands(d.gate()), ["ruff check .", "pytest"], "{names:?}");
            assert_eq!(d.found[0].notes.len(), 1);
            assert!(d.found[0].notes[0].contains("setup"));
        }
    }

    #[test]
    fn a_poetry_or_pdm_root_names_the_manager_in_its_note() {
        for m in ["poetry", "pdm"] {
            let lock = format!("{m}.lock");
            let d = detect(&root(&[lock.as_str()], &[("pyproject.toml", PY)]));
            assert!(d.found[0].notes[0].contains(&format!("`{m} run`")), "{m}");
        }
    }

    #[test]
    fn a_pyproject_without_a_known_tool_gives_nothing() {
        let d = detect(&root(
            &["uv.lock"],
            &[(
                "pyproject.toml",
                "[project]\nname = \"x\"\n[tool.pytest]\nx = 1\n",
            )],
        ));
        assert!(d.found.is_empty() && d.skipped.is_empty());
    }

    #[test]
    fn a_root_holding_two_ecosystems_lists_both_in_the_fixed_order() {
        let d = detect(&root(
            &["pnpm-lock.yaml", "Cargo.lock"],
            &[
                ("package.json", r#"{"scripts":{"test":"vitest"}}"#),
                ("Cargo.toml", CARGO),
                ("Makefile", "lint:\n"),
            ],
        ));
        let sources: Vec<&str> = d.found.iter().map(|f| f.source.as_str()).collect();
        assert_eq!(
            sources,
            ["Cargo.toml", "package.json (pnpm-lock.yaml)", "Makefile"]
        );
        assert_eq!(
            commands(d.gate()),
            [
                "cargo fmt --all -- --check",
                "cargo clippy --workspace --all-targets --locked -- -D warnings",
                "cargo test --workspace --locked",
                "pnpm run test",
                "make lint"
            ]
        );
        assert!(d.gate().all(|l| !l.read_from.is_empty()));
    }
}
