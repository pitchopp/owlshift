//! Live check U1 of the build plan, "P8 · Local web UI" (OWL-466): the local
//! web UI's token stays out of an agent's reach, while the agent still
//! reaches a listener on `127.0.0.1`.
//!
//! A confined command is spawned as the executor spawns a run's: the agent
//! environment built from your own (`AgentEnv::from_runner`), the run's paths
//! as `Executor::run_paths` builds them (a worktree, its repository's common
//! git folder and its own entry there, the run folder hidden, the run's own
//! temporary folder), with `<data>/service/ui` added to the hidden list as
//! part 2 of P8 adds it. The data directory is a scratch folder, placed under
//! the home where the platform keeps Owlshift's data, then outside the home
//! where `OWLSHIFT_DATA_DIR` can put it (`/Users/Shared` on macOS, `/srv` on
//! Linux, or `OWLSHIFT_LIVE_OUTSIDE`): never a temporary folder, which the
//! policy closes anyway. Each placement runs again without the hidden entry,
//! as a control. The listener is the test's own throwaway one, not the UI.
//!
//! It prints yes or no and status codes, never the token. It reads no
//! credential and spends nothing, but needs a working sandbox, so it never
//! runs in CI:
//!
//! ```sh
//! OWLSHIFT_LIVE_UI_TOKEN=1 cargo test -p owlshift-runner --test ui_token_live -- --ignored --nocapture
//! ```
//!
//! On Linux, `docs/design/live-checks/owl-466-linux.sh` runs it under bwrap in
//! a container.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use owlshift_platform::paths::{self, DataDirSource};
use owlshift_runner::agent_env::{AgentEnv, RunPaths};

/// What the listener saw of a request's session header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Seen {
    Absent,
    Invalid,
    Valid,
}

const HEADER: &str = "x-owlshift-session";

/// A throwaway listener on `127.0.0.1`: 200 for a request carrying `secret`
/// in [`HEADER`], 401 otherwise, each request recorded.
struct Listener {
    port: u16,
    seen: Arc<Mutex<Vec<Seen>>>,
}

fn listen(secret: String) -> Listener {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&seen);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let found = answer(stream, &secret);
            record.lock().unwrap().push(found);
        }
    });
    Listener { port, seen }
}

fn answer(mut stream: TcpStream, secret: &str) -> Seen {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") && head.len() < 16 * 1024 {
        match stream.read(&mut byte) {
            Ok(1) => head.push(byte[0]),
            _ => break,
        }
    }
    let head = String::from_utf8_lossy(&head);
    let found = head
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case(HEADER))
        .map_or(Seen::Absent, |(_, value)| {
            if value.trim() == secret {
                Seen::Valid
            } else {
                Seen::Invalid
            }
        });
    let status = if found == Seen::Valid {
        "200 OK"
    } else {
        "401 Unauthorized"
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    found
}

/// The status code of a request from this process, unconfined.
fn status_of(port: u16, secret: Option<&str>) -> u16 {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let header = secret.map_or(String::new(), |s| format!("X-Owlshift-Session: {s}\r\n"));
    write!(
        stream,
        "GET /api/probe HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{header}Connection: close\r\n\r\n"
    )
    .unwrap();
    let mut reply = String::new();
    stream.read_to_string(&mut reply).unwrap();
    reply.split(' ').nth(1).unwrap().parse().unwrap()
}

fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    std::fs::File::open("/dev/urandom")
        .unwrap()
        .read_exact(&mut buffer)
        .unwrap();
    buffer.iter().map(|b| format!("{b:02x}")).collect()
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args([
            "-c",
            "user.name=u1",
            "-c",
            "user.email=u1@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?} failed");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// A data directory in `parent`: `service/ui` 0700 holding `token` 0600, 32
/// random bytes as hex, and an `events.jsonl` beside `service`.
fn data_dir_in(parent: &Path) -> tempfile::TempDir {
    std::fs::create_dir_all(parent).unwrap();
    let data = tempfile::Builder::new()
        .prefix("owlshift-u1-")
        .tempdir_in(parent)
        .unwrap();
    let ui = data.path().join("service").join("ui");
    std::fs::create_dir_all(&ui).unwrap();
    std::fs::set_permissions(&ui, std::fs::Permissions::from_mode(0o700)).unwrap();
    let token = ui.join("token");
    std::fs::write(&token, random_hex(32)).unwrap();
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(data.path().join("events.jsonl"), "{}\n").unwrap();
    data
}

/// The run's paths, as the executor's: a worktree of a scratch repository,
/// the common git folder, the worktree's own entry, the run folder hidden,
/// the run's temporary folder; `ui` hidden too when given.
struct Run {
    paths: RunPaths,
    _scratch: tempfile::TempDir,
}

fn run_paths(ui: Option<&Path>) -> Run {
    let scratch = tempfile::tempdir().unwrap();
    let main = scratch.path().join("repo");
    std::fs::create_dir(&main).unwrap();
    git(&main, &["init", "-q"]);
    git(&main, &["commit", "-q", "--allow-empty", "-m", "start"]);
    let worktree = scratch.path().join("wt");
    git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "u1",
            worktree.to_str().unwrap(),
        ],
    );
    let git_dir = PathBuf::from(git(
        &worktree,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ));
    let own = PathBuf::from(git(&worktree, &["rev-parse", "--absolute-git-dir"]));
    let run_dir = scratch.path().join("run");
    let temp = scratch.path().join("temp");
    std::fs::create_dir(&run_dir).unwrap();
    std::fs::create_dir(&temp).unwrap();
    let mut hidden = vec![run_dir];
    hidden.extend(ui.map(Path::to_owned));
    Run {
        paths: RunPaths {
            workdir: worktree,
            git_dir: Some(git_dir),
            worktree_git_dir: Some(own),
            readable: Vec::new(),
            writable: Vec::new(),
            hidden,
            temp: Some(temp),
            read_only: false,
        },
        _scratch: scratch,
    }
}

fn quoted(path: &Path) -> String {
    let text = path.to_str().unwrap();
    assert!(!text.contains('\''), "a path with a quote: {text}");
    format!("'{text}'")
}

/// The probe a confined run makes: exit codes and counts only.
fn probe_line(data: &Path, workdir: &Path, port: u16) -> String {
    let service = data.join("service");
    let ui = service.join("ui");
    let token = ui.join("token");
    format!(
        r#"T={token}; D={ui}; S={service}; W={workdir}; E={events}; P={port}
r() {{ if "$@" >/dev/null 2>&1; then echo yes; else echo no; fi; }}
echo "exists=$(r test -e "$T")"
echo "read=$(r cat "$T")"
echo "list=$(r ls "$D")"
echo "listed=$(ls -A "$D" 2>/dev/null | wc -l | tr -d ' ')"
echo "found=$(find "$S" 2>/dev/null | grep -c '/token$')"
ln -s "$T" "$W/u1-link" 2>/dev/null; echo "via_symlink=$(r cat "$W/u1-link")"
echo "hardlink=$(r ln "$T" "$W/u1-hard")"
echo "copy=$(r cp "$T" "$W/u1-copy")"
echo "rename=$(r mv "$D" "$S/ui-moved")"
if [ -d /System/Volumes/Data ]; then echo "via_firmlink=$(r cat "/System/Volumes/Data$T")"; else echo "via_firmlink=n/a"; fi
if [ -d /proc/self/root ]; then echo "via_proc=$(r cat "/proc/self/root$T")"; else echo "via_proc=n/a"; fi
echo "events=$(r cat "$E")"
echo "curl=$(r command -v curl)"
echo "no_secret=$(curl --noproxy '*' -s -o /dev/null -m 5 -w '%{{http_code}}' "http://127.0.0.1:$P/api/probe")"
echo "made_up=$(curl --noproxy '*' -s -o /dev/null -m 5 -w '%{{http_code}}' -H 'X-Owlshift-Session: made-up' "http://127.0.0.1:$P/api/probe")"
"#,
        token = quoted(&token),
        ui = quoted(&ui),
        service = quoted(&service),
        workdir = quoted(workdir),
        events = quoted(&data.join("events.jsonl")),
    )
}

fn related(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// One confined probe: its `key=value` lines and what the listener saw.
fn probe(
    agent: &AgentEnv,
    data: &Path,
    hide: bool,
) -> (BTreeMap<String, String>, Vec<Seen>, &'static str) {
    let ui = data.join("service").join("ui");
    let run = run_paths(hide.then_some(ui.as_path()));
    let policy = agent.policy(&run.paths);
    // A placement inside or above anything the run opens would test that
    // opening rather than the hidden entry.
    let real_data = owlshift_platform::sandbox::real(data);
    for opened in policy
        .readable
        .iter()
        .chain(&policy.writable)
        .chain(&policy.temp)
        .chain(&policy.protected)
        .chain(&policy.reopened)
        .chain([&policy.workdir])
    {
        let opened = owlshift_platform::sandbox::real(opened);
        assert!(
            !related(&opened, &real_data),
            "{} and the opened {} overlap: the check would be vacuous",
            real_data.display(),
            opened.display()
        );
    }
    let listener = listen(random_hex(32));
    let line = probe_line(data, &run.paths.workdir, listener.port);
    let output = agent
        .confine_shell(&line, &run.paths)
        .unwrap()
        .stdin(Stdio::null())
        .output()
        .unwrap();
    // Every probe line runs; its own exit status is its last command's.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let values = stdout
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    std::thread::sleep(Duration::from_millis(200));
    let seen = listener.seen.lock().unwrap().clone();
    let closed_by = if policy.home.as_ref().is_some_and(|home| {
        owlshift_platform::sandbox::real(data).starts_with(owlshift_platform::sandbox::real(home))
    }) {
        "the home"
    } else if policy
        .closed
        .iter()
        .any(|dir| real_data.starts_with(owlshift_platform::sandbox::real(dir)))
    {
        "a closed folder"
    } else {
        "nothing but the hidden list"
    };
    (values, seen, closed_by)
}

#[test]
#[ignore = "live: needs a working sandbox (sandbox-exec or bwrap); set OWLSHIFT_LIVE_UI_TOKEN=1"]
fn the_ui_token_stays_out_of_a_confined_run_which_still_reaches_the_listener() {
    assert_eq!(
        std::env::var("OWLSHIFT_LIVE_UI_TOKEN").as_deref(),
        Ok("1"),
        "a live check of the sandbox: set OWLSHIFT_LIVE_UI_TOKEN=1 to run it"
    );
    let agent = AgentEnv::from_runner(&[], &[], &[]).unwrap();
    agent.sandbox_ready().expect("a working sandbox");

    // The listener answers 200 with the secret only, so a 401 below is the
    // listener's, reached.
    let control = listen("the-secret".to_owned());
    assert_eq!(status_of(control.port, Some("the-secret")), 200);
    assert_eq!(status_of(control.port, None), 401);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        *control.seen.lock().unwrap(),
        [Seen::Valid, Seen::Absent],
        "the listener records what it saw"
    );

    let home = paths::home_dir().expect("a home");
    let under_home = match paths::data_dir_and_source() {
        Some((dir, DataDirSource::Platform)) => dir.parent().unwrap().to_owned(),
        _ => panic!("unset OWLSHIFT_DATA_DIR: the first placement is the platform's own"),
    };
    let outside = std::env::var_os("OWLSHIFT_LIVE_OUTSIDE").map_or_else(
        || {
            PathBuf::from(if cfg!(target_os = "macos") {
                "/Users/Shared"
            } else {
                "/srv"
            })
        },
        PathBuf::from,
    );
    assert!(
        !outside.starts_with(&home),
        "the second placement is outside the home"
    );
    println!(
        "U1 on {} {}, home {}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        if cfg!(target_os = "macos") {
            "closed by sandbox-exec"
        } else {
            "a tmpfs under bwrap"
        }
    );

    let mut failures = Vec::new();
    for (placement, parent) in [
        ("under the home", &under_home),
        ("outside the home", &outside),
    ] {
        let data = data_dir_in(parent);
        for hide in [true, false] {
            let (values, seen, closed_by) = probe(&agent, data.path(), hide);
            let get = |key: &str| values.get(key).map_or("missing", String::as_str);
            let case = format!(
                "{placement}, {}",
                if hide {
                    "ui folder hidden"
                } else {
                    "control, not hidden"
                }
            );
            println!(
                "{case} (else closed by {}): {}; listener saw {seen:?}",
                closed_by,
                values
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            assert_eq!(get("curl"), "yes", "curl inside the sandbox");
            // Reached, both times, whatever the placement.
            if (get("no_secret"), get("made_up")) != ("401", "401")
                || seen != [Seen::Absent, Seen::Invalid]
            {
                failures.push(format!("{case}: the listener was not reached as expected"));
            }
            if hide {
                for key in [
                    "exists",
                    "read",
                    "via_symlink",
                    "hardlink",
                    "copy",
                    "rename",
                ] {
                    if get(key) != "no" {
                        failures.push(format!("{case}: {key}={}", get(key)));
                    }
                }
                for key in ["via_firmlink", "via_proc"] {
                    if !matches!(get(key), "no" | "n/a") {
                        failures.push(format!("{case}: {key}={}", get(key)));
                    }
                }
                for key in ["listed", "found"] {
                    if get(key) != "0" {
                        failures.push(format!("{case}: {key}={}", get(key)));
                    }
                }
            } else if placement == "outside the home" && get("read") != "yes" {
                // The probe sees a read where nothing else closes the token.
                failures.push(format!("{case}: the control could not read the token"));
            }
        }
    }
    assert!(failures.is_empty(), "U1 failed:\n{}", failures.join("\n"));
}
