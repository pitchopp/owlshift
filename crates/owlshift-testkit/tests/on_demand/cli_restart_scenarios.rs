//! OWL-522: the web UI's Restart child, end to end through the command
//! line's own code. `owlshift-cli-bench` is `owlshift_cli::main_with`, the
//! whole of the shipped `owlshift` binary, with a keychain held in memory;
//! the child is built by `restart_command`, as the UI's server builds it.
//! Its twin, the same command without `--from-ui`, is the typed restart.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt as _;

use super::to_watch_scenarios::strip_times;
use super::*;
use owlshift_runner::ui::server::restart_command;
use owlshift_testkit::step_child::{
    self, ChildProject, REPO, TICKET, record_of, served_parked_at_build,
};

const BIN: &str = env!("CARGO_BIN_EXE_owlshift-cli-bench");
const FAKE_HARNESS: &str = env!("CARGO_BIN_EXE_owlshift-fake-harness");

/// `text` with each compact time, as in a run's folder or a comment's file
/// (`20261111T014649Z`), and the suffix a second name in the same second
/// takes (`-2`, `.2`), read `<time>`.
fn untimed(text: &str) -> String {
    let shape = |window: &[u8]| {
        window.len() == 16
            && window[..8].iter().all(u8::is_ascii_digit)
            && window[8] == b'T'
            && window[9..15].iter().all(u8::is_ascii_digit)
            && window[15] == b'Z'
    };
    let bytes = text.as_bytes();
    let mut timeless = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if bytes.get(at..at + 16).is_some_and(shape) {
            timeless.extend_from_slice(b"<time>");
            at += 16;
            let digits = bytes[(at + 1).min(bytes.len())..]
                .iter()
                .take_while(|b| b.is_ascii_digit())
                .count();
            if matches!(bytes.get(at), Some(b'-' | b'.')) && digits > 0 {
                at += 1 + digits;
            }
        } else {
            timeless.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8(timeless).unwrap()
}

/// A served bench under `root`, DEMO-1 parked at Build, and what its
/// child prints, then what it left.
struct Restarted {
    root: PathBuf,
    project: ChildProject,
    stdout: String,
    stderr: String,
    success: bool,
    /// The events the child appended.
    events: Vec<Event>,
}

/// `command`, the restart's child, run on a served bench under `root`, with
/// no route to the network: git reaches the bare remote through the bench's
/// configuration, and anything else goes to a proxy that answers nothing.
fn restart_on(root: &Path, from_ui: bool) -> Restarted {
    let project = served_parked_at_build(root, PathBuf::from(FAKE_HARNESS));
    let before = step_child::events(&project.data).len();
    // `claude` only has to be found: a restart runs nothing.
    let bin = root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let claude = bin.join("claude");
    fs::write(&claude, "#!/bin/sh\nexit 1\n").unwrap();
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).unwrap();
    let config = root.join("config");
    fs::create_dir_all(&config).unwrap();

    let mut command = restart_command(
        Path::new(BIN),
        &Repo::parse(REPO).unwrap(),
        &TicketId::new(TICKET).unwrap(),
    );
    if !from_ui {
        // The typed restart: the UI's command without its flag.
        let args: Vec<_> = command
            .get_args()
            .filter(|arg| *arg != "--from-ui")
            .map(ToOwned::to_owned)
            .collect();
        let mut typed = Command::new(BIN);
        typed
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        command = typed;
    }
    project.env().apply(&mut command);
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![bin];
    paths.extend(std::env::split_paths(&path));
    let dead = "http://127.0.0.1:9";
    command
        .current_dir(root)
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env("OWLSHIFT_DATA_DIR", &project.data)
        .env("OWLSHIFT_CONFIG_DIR", &config)
        .env_remove("NO_PROXY")
        .env_remove("no_proxy");
    for name in [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        command.env(name, dead);
    }
    let output = command.output().unwrap();
    let events = step_child::events(&project.data).split_off(before);
    Restarted {
        root: root.to_owned(),
        project,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        success: output.status.success(),
        events,
    }
}

impl Restarted {
    /// `text` with this bench's root, commit ids and times made the same on
    /// every bench.
    fn plain(&self, text: &str) -> String {
        let root = self.root.to_string_lossy();
        let text = text.replace(root.as_ref(), "<root>");
        // A commit id: 40 hexadecimal digits.
        let mut plain = String::new();
        let mut run = String::new();
        for c in text.chars().chain(std::iter::once(' ')) {
            if c.is_ascii_hexdigit() {
                run.push(c);
                continue;
            }
            if run.len() >= 40 {
                plain.push_str("<sha>");
            } else {
                plain.push_str(&run);
            }
            run.clear();
            plain.push(c);
        }
        plain.pop();
        untimed(&plain)
    }

    /// `value` made plain, its times dropped.
    fn plain_value(&self, mut value: Value) -> Value {
        strip_times(&mut value);
        serde_json::from_str(&self.plain(&value.to_string())).unwrap()
    }

    /// The child's events: kind and data, made plain.
    fn told(&self) -> Vec<(EventKind, Value)> {
        self.events
            .iter()
            .map(|event| {
                let data = Value::Object(event.data.clone());
                (event.kind, self.plain_value(data))
            })
            .collect()
    }

    /// Every file under `dir`, by its path from it: its text made plain, or
    /// its size when not text. Locks, the event log (compared as events) and
    /// git's own folders are left out. A name that holds a time, a run's
    /// folder or a comment's file, is named by its rank among its folder's
    /// names of the same shape, oldest first: two runs in one second differ
    /// by a suffix, in two seconds by their time, and either way are runs 1
    /// and 2.
    fn files(&self, dir: &Path) -> BTreeMap<String, String> {
        let mut files = BTreeMap::new();
        let mut stack = vec![(dir.to_owned(), String::new())];
        while let Some((at, shown)) = stack.pop() {
            let mut names: Vec<String> = fs::read_dir(&at)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            let mut ranks: BTreeMap<String, usize> = BTreeMap::new();
            for name in names {
                let path = at.join(&name);
                if name == ".git" || name.ends_with(".git") && path.is_dir() {
                    continue;
                }
                let shape = untimed(&name);
                let named = if shape == name {
                    self.plain(&name)
                } else {
                    let rank = ranks.entry(shape.clone()).or_default();
                    *rank += 1;
                    shape.replacen("<time>", &format!("<time #{rank}>"), 1)
                };
                let relative = format!("{shown}{named}");
                if path.is_dir() {
                    stack.push((path, format!("{relative}/")));
                    continue;
                }
                if name.ends_with(".lock") || name == "events.jsonl" {
                    files.insert(relative, String::new());
                    continue;
                }
                let bytes = fs::read(&path).unwrap();
                let text = match String::from_utf8(bytes) {
                    Ok(text) => match serde_json::from_str::<Value>(&text) {
                        Ok(value) => self.plain_value(value).to_string(),
                        Err(_) => self.plain(&text),
                    },
                    Err(error) => format!("{} bytes", error.as_bytes().len()),
                };
                files.insert(relative, text);
            }
        }
        files
    }

    /// The ticket's ref: its state, and its spend with its times dropped.
    fn record(&self) -> (Value, Value) {
        let record = record_of(&self.project, TICKET).expect("the ticket's ref");
        (
            self.plain_value(serde_json::to_value(&record.state).unwrap()),
            self.plain_value(serde_json::to_value(&record.cost).unwrap()),
        )
    }
}

/// OWL-522's acceptance: the binary's `continue DEMO-1 --to-watch --served
/// demo/project --from-ui`, the web UI's Restart, run on a served bench
/// where DEMO-1 is parked at Build, ends as the same command typed on its
/// twin: the same outcome, the same files in the data directory and the
/// tracker, the same ticket ref and the same events, but for the restart's
/// `decision` event, which names `by` = `ui` on the UI's side only. Nothing
/// reaches the network, and agent runs stay confined as `owlshift` confines
/// them: skipped where they cannot be, unless `OWLSHIFT_REQUIRE_CONFINEMENT`
/// is set.
#[test]
fn the_uis_restart_child_is_the_typed_restart_with_by_ui() {
    if let Err(error) = AgentEnv::new(std::env::vars_os()).unwrap().sandbox_ready() {
        assert!(
            std::env::var_os("OWLSHIFT_REQUIRE_CONFINEMENT").is_none(),
            "confinement is required here, but: {error}"
        );
        eprintln!("skipped: agent runs cannot be confined here: {error}");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let typed = restart_on(&tmp.path().join("typed"), false);
    let from_ui = restart_on(&tmp.path().join("ui"), true);
    let said = |r: &Restarted| format!("{}\n{}", r.stdout, r.stderr);

    for r in [&typed, &from_ui] {
        assert!(r.success, "{}", said(r));
        assert!(
            r.stdout
                .contains("DEMO-1 is restarted at Build, and nothing runs now"),
            "{}",
            said(r)
        );
        // No forge, tracker or network trouble was swallowed on the way.
        assert!(
            r.events
                .iter()
                .all(|event| event.kind != EventKind::Warning),
            "{:?}",
            r.events
        );
    }
    // The outcome, printed after the events, which are compared below.
    let outcome = |r: &Restarted| {
        r.plain(
            r.stdout
                .rsplit_once("\n\n")
                .map_or("", |(_, outcome)| outcome),
        )
    };
    assert_eq!(outcome(&from_ui), outcome(&typed));
    assert_eq!(from_ui.plain(&from_ui.stderr), typed.plain(&typed.stderr));
    assert_eq!(
        MarkdownTracker::new(&from_ui.project.checkout)
            .stage(&ticket())
            .unwrap(),
        "In Progress"
    );
    assert_eq!(
        from_ui.files(&from_ui.project.checkout.join("tickets")),
        typed.files(&typed.project.checkout.join("tickets"))
    );
    let (ui_files, typed_files) = (
        from_ui.files(&from_ui.project.data),
        typed.files(&typed.project.data),
    );
    // The park's two Build runs, each with its brief, on each side.
    let runs = |files: &BTreeMap<String, String>| {
        files
            .keys()
            .filter(|path| path.contains("/runs/demo-1/") && path.ends_with("/brief.json"))
            .count()
    };
    assert_eq!(runs(&typed_files), 2, "{typed_files:#?}");
    assert_eq!(ui_files, typed_files);
    assert_eq!(from_ui.record(), typed.record());

    let mut ui_events = from_ui.told();
    let typed_events = typed.told();
    let restart = ui_events
        .iter_mut()
        .find(|(kind, data)| *kind == EventKind::Decision && data.get("restarted").is_some())
        .expect("the restart's decision");
    assert_eq!(restart.1["by"], json!("ui"));
    restart.1.as_object_mut().unwrap().remove("by");
    assert_eq!(ui_events, typed_events);
    assert!(
        typed_events
            .iter()
            .filter(|(kind, _)| *kind == EventKind::Decision)
            .all(|(_, data)| data.get("by").is_none())
    );
}
