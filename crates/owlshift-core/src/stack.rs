//! Stack detection for `owlshift init` (P9, OWL-539): the gate, the setup it
//! needs and the caches of the home that setup reads confined (OWL-593), read
//! from the names and contents of the repository's root files; pnpm's
//! setup through an overlay store since OWL-598.
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
    /// What the person should know about the gate lines alone, written only
    /// where the detected gate is used, never above a gate given by hand
    /// (OWL-595).
    pub gate_notes: Vec<String>,
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

/// The other root files [`detect`] reads when the runner could, for what
/// they say of a manifest's setup: yarn's settings (OWL-593). One missing
/// or unread counts as empty.
pub const SETTINGS: [&str; 1] = [".yarnrc.yml"];

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
        gate_notes: vec![],
    }))
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
    /// What the person should know about the cache, when none is declared.
    note: Option<String>,
}

/// yarn 2 or later's global cache, `~/.yarn/berry` (OWL-593).
const YARN_BERRY: &str = ".yarn/berry";

/// pnpm's folder in the home on macOS, which holds its store (OWL-598).
const PNPM_HOME: &str = "Library/pnpm";

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

/// npm's install under the sandbox (OWL-593): the home is closed, so npm
/// reads its cache, `~/.npm`, as a cache the operator allows, read-only.
/// `--prefer-offline` takes what the cache holds without asking the
/// registry, and `--logs-dir` writes npm's log in the run's temporary
/// folder, its default being inside the read-only cache. A package missing
/// from the cache fails the install: npm cannot write it there.
pub const NPM_SETUP: &str =
    "npm ci --prefer-offline --cache \"$HOME/.npm\" --logs-dir \"$TMPDIR/npm-logs\"";

/// pnpm 10's install under the sandbox (OWL-598). pnpm cannot install from
/// its store read-only, since it registers each project in the store's
/// `projects` folder, and a closed home makes it put a store of its own in
/// the worktree, fetching every package. So the setup builds an overlay
/// store in `node_modules/.pnpm-overlay`, ignored wherever `node_modules`
/// is: its `files` and `index` link to the store in `~/Library/pnpm`,
/// opened read-only as a cache, and its `projects` is its own. Its path
/// does not change between runs, since pnpm records it in `node_modules`
/// and, finding another, wants to remove `node_modules`. `--offline`
/// installs from the store alone: a package missing from it fails the
/// install with `ERR_PNPM_NO_OFFLINE_TARBALL`. Without pnpm 10's store at
/// that path, on Linux or with pnpm 9, it runs pnpm's plain install.
pub const PNPM_SETUP: &str = "if [ -d \"$HOME/Library/pnpm/store/v10\" ]; then \
     mkdir -p node_modules/.pnpm-overlay/v10/projects && \
     ln -sfn \"$HOME/Library/pnpm/store/v10/files\" node_modules/.pnpm-overlay/v10/files && \
     ln -sfn \"$HOME/Library/pnpm/store/v10/index\" node_modules/.pnpm-overlay/v10/index && \
     pnpm install --offline --frozen-lockfile --store-dir node_modules/.pnpm-overlay; \
     else pnpm install --frozen-lockfile; fi";

/// Which manager the lock file names, its install, and the cache it needs
/// confined. Checked live (OWL-593): yarn 1 skips a cache it cannot write
/// and bun falls back to one of its own, both fetching from the registry,
/// so they need none; npm, and yarn 4 on its global cache, fail with
/// `EPERM` unless they read theirs ([`yarn_global_cache`]); pnpm 10 needs
/// an overlay over its store ([`PNPM_SETUP`], OWL-598), and pnpm 12 failed
/// confined whatever its store, so a `packageManager` naming a pnpm other
/// than 10 keeps the plain install.
fn node_manager(root: &RootFiles, package: &serde_json::Value) -> Option<NodeManager> {
    let plain = |name, lock, install: String| NodeManager {
        name,
        lock,
        install,
        cache: None,
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
                    "`{manager}`, the `packageManager` of package.json, is not pnpm 10: the \
                     setup is pnpm's plain install and no store was declared, only pnpm 10 \
                     having been found to install from a read-only store; `owlshift gate` \
                     checks it"
                )),
                ..pnpm
            },
            _ => NodeManager {
                install: PNPM_SETUP.into(),
                cache: Some((PNPM_HOME, "pnpm-lock.yaml, the lock file of pnpm".into())),
                note: Some(
                    "the setup installs through an overlay in `node_modules/.pnpm-overlay` over \
                     pnpm 10's store on macOS, `~/Library/pnpm/store/v10`; without that folder \
                     (pnpm 9, or Linux, whose store is `~/.local/share/pnpm`, not checked) it \
                     runs `pnpm install --frozen-lockfile`, which fails confined on macOS"
                        .to_owned(),
                ),
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
            cache: Some((".npm", "package-lock.json, the lock file of npm".into())),
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
        gate_notes: vec![],
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
            (&["pnpm-lock.yaml"], PNPM_SETUP, "pnpm", &[PNPM_HOME]),
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
            (&["package-lock.json"], NPM_SETUP, "npm", &[".npm"]),
        ];
        for (names, setup, manager, caches) in cases {
            let d = detect(&root(names, &[("package.json", pkg)]));
            assert_eq!(commands(d.setup()), [setup], "{names:?}");
            assert_eq!(commands(d.caches()), caches, "{names:?}");
            assert_eq!(
                commands(d.gate()),
                [
                    format!("{manager} run lint"),
                    format!("{manager} run test"),
                    format!("{manager} run build")
                ],
                "{names:?}"
            );
            // A manager reading a cache says what each machine must allow;
            // yarn 2 or later, its cache unknown, says what to declare;
            // pnpm also says where its overlay lies; the gate's own note is
            // a guess (OWL-595).
            let notes = &d.found[0].notes;
            let noted = !caches.is_empty() || names.contains(&".yarnrc.yml");
            let pnpm = usize::from(manager == "pnpm");
            assert_eq!(notes.len(), usize::from(noted) + pnpm, "{notes:?}");
            assert!(notes[pnpm..].iter().all(|n| n.contains("allow_caches")));
            let gate_notes = &d.found[0].gate_notes;
            assert_eq!(gate_notes.len(), 1, "{gate_notes:?}");
            assert!(gate_notes[0].contains("a guess"), "{gate_notes:?}");
            assert!(d.found[0].source.starts_with("package.json ("));
            assert!(
                d.setup()
                    .chain(d.caches())
                    .all(|l| l.read_from.contains(manager))
            );
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

    /// OWL-598: pnpm installs through the overlay over `~/Library/pnpm`,
    /// unless `packageManager` names a pnpm other than 10, which keeps the
    /// plain install, declares no store and says why.
    #[test]
    fn pnpms_store_is_declared_unless_package_manager_names_another_major() {
        let cases: [(&str, bool); 6] = [
            ("", true),
            ("pnpm@10.33.2", true),
            ("pnpm@10", true),
            ("pnpm@^10", true),
            ("pnpm@9.15.0", false),
            ("pnpm@12.21.0+sha512.abc", false),
        ];
        for (manager, overlay) in cases {
            let pkg = format!(r#"{{"packageManager":"{manager}"}}"#);
            let d = detect(&root(&["pnpm-lock.yaml"], &[("package.json", &pkg)]));
            let notes = &d.found[0].notes;
            if overlay {
                assert_eq!(commands(d.setup()), [PNPM_SETUP], "{manager}");
                assert_eq!(commands(d.caches()), [PNPM_HOME], "{manager}");
                assert_eq!(notes.len(), 2, "{manager} {notes:?}");
                assert!(notes[0].contains("`node_modules/.pnpm-overlay`"));
                assert!(notes[0].contains("`~/.local/share/pnpm`, not checked"));
            } else {
                assert_eq!(
                    commands(d.setup()),
                    ["pnpm install --frozen-lockfile"],
                    "{manager}"
                );
                assert!(d.caches().next().is_none(), "{manager}");
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
        assert_eq!(commands(d.setup()), [NPM_SETUP]);
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
