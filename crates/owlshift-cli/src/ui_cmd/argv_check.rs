//! Live check U4 of the build plan, "P8 · Local web UI" (OWL-495): whether a
//! confined run reads the arguments of the user's other processes, and so
//! whether the login link, its one-time code included, may be one
//! ([`super::opener_for`]).
//!
//! The argument reader, [`scan`], lists every process and reads each one's
//! arguments as `owlshift-platform`'s `test_proc::command` does
//! (`/proc/<pid>/cmdline` on Linux, `sysctl` `KERN_PROCARGS2` on macOS). A
//! confined run of it is this unit-test binary run again, its ignored test
//! [`reader`] alone, inside the agent's sandbox: the sandbox builds the
//! agent's own environment, so the reader takes its request from a file of
//! its working directory, [`REQUEST`], and writes what it saw to [`OUTPUT`].
//!
//! Three checks. The confinement test,
//! `a_confined_run_reads_other_processes_arguments_on_macos_only`, runs with
//! every test and on CI's unix legs, where `OWLSHIFT_REQUIRE_CONFINEMENT`
//! makes it fail rather than skip. Two live checks run by hand:
//!
//! ```sh
//! # From inside a run already confined, such as a Build: its own scan.
//! OWLSHIFT_LIVE_UI_ARGV_SELF=1 cargo test -p owlshift-cli --bin owlshift -- --ignored --nocapture ui_cmd::argv_check::a_run_already_confined
//! # On macOS, outside any sandbox, once with the default browser open and
//! # once with it quit: it opens a tab on a throwaway listener.
//! OWLSHIFT_LIVE_UI_ARGV=1 cargo test -p owlshift-cli --bin owlshift -- --ignored --nocapture ui_cmd::argv_check::the_login_link
//! ```
#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use owlshift_runner::agent_env::{AgentEnv, RunPaths};

// Its `state` and `is_alive` are not used here.
#[allow(dead_code)]
#[path = "../../../owlshift-platform/src/test_proc.rs"]
mod test_proc;

/// The reader's request, in its working directory: one needle per line,
/// then, optionally, `watch_ms=<n>`, how long it scans again and again.
pub const REQUEST: &str = "argv-probe.request";

/// What the reader saw, beside its request, as `key=value` lines:
/// `processes`, `read`, `own` and one `found_<i>` per needle, `1` or `0`.
pub const OUTPUT: &str = "argv-probe.out";

/// How often a watching reader scans.
const EVERY: Duration = Duration::from_millis(20);

/// What one scan, or the scans of one watch, saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scan {
    /// The processes listed, this one left out.
    pub processes: usize,
    /// Those whose arguments came back, not empty: a process of another
    /// user, a gone one or one the sandbox hides from its reader counts in
    /// `processes` alone.
    pub read: usize,
    /// For each needle, whether some process's arguments held it.
    pub found: Vec<bool>,
}

/// Reads the arguments of every other process once, and says which of
/// `needles` they hold.
pub fn scan(needles: &[&str]) -> Scan {
    let me = std::process::id();
    let mut seen = Scan {
        processes: 0,
        read: 0,
        found: vec![false; needles.len()],
    };
    for pid in pids().into_iter().filter(|&pid| pid != me) {
        seen.processes += 1;
        let command = test_proc::command(pid);
        if command.is_empty() {
            continue;
        }
        seen.read += 1;
        for (found, needle) in seen.found.iter_mut().zip(needles) {
            *found |= command.contains(needle);
        }
    }
    seen
}

/// Every process id this process can list.
#[cfg(target_os = "linux")]
fn pids() -> Vec<u32> {
    std::fs::read_dir("/proc")
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Every process id this process can list.
#[cfg(target_os = "macos")]
fn pids() -> Vec<u32> {
    // SAFETY: a null buffer of no length asks for the count alone.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if count <= 0 {
        return Vec::new();
    }
    // Room for processes started since the count.
    let mut buffer = vec![0 as libc::pid_t; count as usize + 256];
    let bytes = (buffer.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
    // SAFETY: `buffer` holds `bytes` bytes and outlives the call.
    let listed = unsafe { libc::proc_listallpids(buffer.as_mut_ptr().cast(), bytes) };
    buffer.truncate(listed.max(0) as usize);
    buffer
        .into_iter()
        .filter_map(|pid| u32::try_from(pid).ok().filter(|&pid| pid > 0))
        .collect()
}

/// A needle no process holds by chance: `owl495-` and 16 random bytes in
/// hex, read as `ui_token_live.rs`'s `random_hex` reads them.
pub fn needle() -> String {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut bytes))
        .expect("/dev/urandom");
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("owl495-{hex}")
}

/// A process holding `needle` among its arguments until it is dropped.
pub struct Holder(Child);

impl Drop for Holder {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Starts `/bin/sh -c '<script>' <needle>`, the needle its `$0`. The script
/// is never one simple command, which a shell may exec in its own place,
/// arguments and all.
pub fn hold_for(script: &str, needle: &str) -> Holder {
    Holder(
        Command::new("/bin/sh")
            .args(["-c", script, needle])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("/bin/sh"),
    )
}

/// [`hold_for`] until dropped: a loop, so killing the shell leaves at most
/// a one-second `sleep` behind.
pub fn hold(needle: &str) -> Holder {
    hold_for("while :; do sleep 1; done", needle)
}

/// The needles and the watch of a request.
fn parse_request(text: &str) -> (Vec<String>, Duration) {
    let mut needles = Vec::new();
    let mut watch = Duration::ZERO;
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        match line.strip_prefix("watch_ms=") {
            Some(ms) => watch = Duration::from_millis(ms.parse().expect("watch_ms")),
            None => needles.push(line.to_owned()),
        }
    }
    (needles, watch)
}

/// A request for `needles`, watched for `watch_ms` milliseconds after the
/// first scan.
fn request(needles: &[&str], watch_ms: u64) -> String {
    let mut text: String = needles.iter().map(|needle| format!("{needle}\n")).collect();
    text.push_str(&format!("watch_ms={watch_ms}\n"));
    text
}

/// The `key=value` lines of an output.
fn parse_output(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

/// Runs the request of `dir`, when there is one: holds a needle of its own
/// as a control, scans once or until the watch ends, every needle counted
/// as found when any scan found it, then writes the output beside it.
fn answer(dir: &Path) {
    let Ok(text) = std::fs::read_to_string(dir.join(REQUEST)) else {
        return;
    };
    let (mut needles, watch) = parse_request(&text);
    let own = needle();
    let holder = hold(&own);
    needles.push(own);
    let needles: Vec<&str> = needles.iter().map(String::as_str).collect();
    let end = Instant::now() + watch;
    let mut seen = scan(&needles);
    while Instant::now() < end {
        std::thread::sleep(EVERY);
        let next = scan(&needles);
        seen.processes = seen.processes.max(next.processes);
        seen.read = seen.read.max(next.read);
        for (found, now) in seen.found.iter_mut().zip(next.found) {
            *found |= now;
        }
    }
    drop(holder);
    let flag = |found: bool| if found { "1" } else { "0" };
    let (own, asked) = seen.found.split_last().expect("the own needle");
    let mut output = format!(
        "processes={}\nread={}\nown={}\n",
        seen.processes,
        seen.read,
        flag(*own)
    );
    for (index, found) in asked.iter().enumerate() {
        output.push_str(&format!("found_{index}={}\n", flag(*found)));
    }
    std::fs::write(dir.join(OUTPUT), output).expect("the reader's output");
}

/// The reader, run by this module's tests as this binary's one test, in a
/// sandbox or not: it answers the request of its working directory, and
/// does nothing where there is none.
#[test]
#[ignore = "the reader argv_check's tests run, with a request in its working directory"]
fn reader() {
    answer(&std::env::current_dir().expect("a working directory"));
}

#[test]
fn the_scan_finds_the_argument_of_a_process_and_no_needle_nobody_holds() {
    let (held, nobody) = (needle(), needle());
    let _holder = hold(&held);
    let seen = scan(&[&held, &nobody]);
    assert_eq!(seen.found, [true, false], "{seen:?}");
    assert!(seen.read >= 1 && seen.processes >= seen.read, "{seen:?}");
}

/// A run of [`reader`], confined by `agent` or bare, in a scratch folder
/// laid out as a run's: its worktree, which holds the request, a hidden run
/// folder and its own temporary folder. This binary's folder is opened,
/// read-only: it lies in the checkout, under the home the policy closes.
pub struct Reader {
    child: Child,
    workdir: PathBuf,
    _scratch: tempfile::TempDir,
}

impl Reader {
    /// Starts the reader on `needles`, watched for `watch_ms`.
    pub fn start(agent: Option<&AgentEnv>, needles: &[&str], watch_ms: u64) -> Self {
        let scratch = tempfile::tempdir().unwrap();
        let workdir = scratch.path().join("wt");
        let run_dir = scratch.path().join("run");
        let temp = scratch.path().join("temp");
        for dir in [&workdir, &run_dir, &temp] {
            std::fs::create_dir(dir).unwrap();
        }
        std::fs::write(workdir.join(REQUEST), request(needles, watch_ms)).unwrap();
        let binary = std::env::current_exe().unwrap();
        let mut inner = Command::new(&binary);
        inner.args([
            "ui_cmd::argv_check::reader",
            "--exact",
            "--ignored",
            "--nocapture",
        ]);
        let run = RunPaths {
            workdir: workdir.clone(),
            git_dir: None,
            worktree_git_dir: None,
            readable: vec![binary.parent().unwrap().to_owned()],
            writable: Vec::new(),
            hidden: vec![run_dir],
            temp: Some(temp),
            read_only: false,
        };
        let mut command = match agent {
            Some(agent) => agent.confine(inner, &run).unwrap(),
            None => {
                inner.current_dir(&workdir);
                inner
            }
        };
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        Reader {
            child,
            workdir,
            _scratch: scratch,
        }
    }

    /// Waits for the reader's end, and gives what it saw; panics, with its
    /// output, when it saw nothing.
    pub fn finish(self) -> BTreeMap<String, String> {
        let output = self.child.wait_with_output().unwrap();
        match std::fs::read_to_string(self.workdir.join(OUTPUT)) {
            Ok(text) if output.status.success() => parse_output(&text),
            _ => panic!("the reader failed: {}", shown(&output)),
        }
    }
}

fn shown(output: &Output) -> String {
    format!(
        "{}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Whether a confined run can be made here, as `owlshift-testkit`'s
/// `tests/confinement.rs` decides: a skip, but a failure when
/// `OWLSHIFT_REQUIRE_CONFINEMENT` is set, as on CI's unix legs.
fn sandbox_or_skip(agent: &AgentEnv) -> bool {
    match agent.sandbox_ready() {
        Ok(()) => true,
        Err(error) => {
            assert!(
                std::env::var_os("OWLSHIFT_REQUIRE_CONFINEMENT").is_none(),
                "confinement is required here, but: {error}"
            );
            eprintln!("skipped: agent runs cannot be confined here: {error}");
            false
        }
    }
}

/// The value of `key` in a reader's output.
fn value<'a>(seen: &'a BTreeMap<String, String>, key: &str) -> &'a str {
    seen.get(key).map_or("missing", String::as_str)
}

/// Checks what a reader saw: its own child, so its scan works where it ran,
/// and the needle of another process when `expected`.
fn assert_saw(seen: &BTreeMap<String, String>, case: &str, expected: bool) {
    let counts = format!("{case} on {}: {seen:?}", std::env::consts::OS);
    assert_eq!(value(seen, "own"), "1", "{counts}");
    assert!(
        value(seen, "read").parse::<usize>().unwrap() >= 1,
        "{counts}"
    );
    assert_eq!(
        value(seen, "found_0"),
        if expected { "1" } else { "0" },
        "{counts}"
    );
}

/// Live check U4's confinement test (build plan, "P8 · Local web UI"): a
/// run confined by the executor's policy reads the arguments of a process
/// outside its sandbox on macOS, whose Seatbelt profile lets `sysctl`
/// `KERN_PROCARGS2` through, and none on Linux, where bwrap gives it its own
/// process namespace and `/proc`. The reader run bare is the control.
#[test]
fn a_confined_run_reads_other_processes_arguments_on_macos_only() {
    let other = needle();
    let _holder = hold(&other);
    // First and everywhere: the reader works, and finds the needle bare.
    assert_saw(&Reader::start(None, &[&other], 0).finish(), "bare", true);

    // The data directory a scratch one, so its hidden token folder, which
    // the sandbox makes, is not made in the real one.
    let data = tempfile::tempdir().unwrap();
    let agent = AgentEnv::from_runner(&[], &[], &[])
        .unwrap()
        .with_data_dir(data.path());
    if !sandbox_or_skip(&agent) {
        return;
    }
    let seen = Reader::start(Some(&agent), &[&other], 0).finish();
    println!("confined on {}: {seen:?}", std::env::consts::OS);
    assert_saw(&seen, "confined", cfg!(target_os = "macos"));
}

/// Live check U4 from a run already confined, where no sandbox nests, such
/// as a Build's: the scan of its own process, which prints what it read of
/// the machine's other processes.
#[test]
#[ignore = "live: run from a confined run; set OWLSHIFT_LIVE_UI_ARGV_SELF=1"]
fn a_run_already_confined_reads_other_processes_arguments() {
    assert_eq!(
        std::env::var("OWLSHIFT_LIVE_UI_ARGV_SELF").as_deref(),
        Ok("1"),
        "a live check: set OWLSHIFT_LIVE_UI_ARGV_SELF=1 to run it"
    );
    let held = needle();
    let _holder = hold(&held);
    let seen = scan(&[&held]);
    println!(
        "U4 self scan on {} {}: processes={} read={} found={}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        seen.processes,
        seen.read,
        u8::from(seen.found[0])
    );
    assert!(
        seen.found[0],
        "the scan did not find its own child: {seen:?}"
    );
}

/// A throwaway listener on `127.0.0.1`, which answers every request with a
/// small page and counts them.
#[cfg(target_os = "macos")]
fn listen() -> (u16, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::io::Write;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let count = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&count);
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") && head.len() < 16 * 1024 {
                match stream.read(&mut byte) {
                    Ok(1) => head.push(byte[0]),
                    _ => break,
                }
            }
            counted.fetch_add(1, Ordering::SeqCst);
            let page =
                "<!doctype html><title>U4</title><p>OWL-495's live check U4: close this tab.";
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{page}",
                page.len()
            );
        }
    });
    (port, count)
}

/// Live check U4's browser check, on macOS, outside any sandbox: the login
/// link opened by `owlshift ui`'s own opener reaches the browser, and while
/// it does, a confined run watching every process's arguments never reads
/// its code, though it reads a control needle held as an argument meanwhile.
/// The link points at a throwaway listener and its code is a random needle,
/// so nothing can be redeemed. Run it with the default browser open, then
/// with it quit.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "live: opens a browser tab; set OWLSHIFT_LIVE_UI_ARGV=1"]
fn the_login_link_reaches_the_browser_in_no_argument_a_confined_run_reads() {
    use std::sync::atomic::Ordering;

    assert_eq!(
        std::env::var("OWLSHIFT_LIVE_UI_ARGV").as_deref(),
        Ok("1"),
        "a live check that opens a browser tab: set OWLSHIFT_LIVE_UI_ARGV=1 to run it"
    );
    let data = tempfile::tempdir().unwrap();
    let agent = AgentEnv::from_runner(&[], &[], &[])
        .unwrap()
        .with_data_dir(data.path());
    agent.sandbox_ready().expect("a working sandbox");
    let (port, requests) = listen();
    let (code, control) = (needle(), needle());
    let reader = Reader::start(Some(&agent), &[&code, &control], 20_000);
    std::thread::sleep(Duration::from_secs(1));

    let opened = super::open_with(super::opener(&super::login_link(port, &code))).unwrap();
    assert!(opened.success(), "the opener failed: {opened}");
    // Held for 3 seconds only: a watch that missed it would also miss a
    // short-lived opener.
    let _control = hold_for("sleep 3; :", &control);
    let end = Instant::now() + Duration::from_secs(15);
    while requests.load(Ordering::SeqCst) == 0 && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(100));
    }
    let received = requests.load(Ordering::SeqCst);
    let seen = reader.finish();
    println!(
        "U4 browser check on macOS: requests={received} processes={} read={} own={} code={} \
         control={}",
        value(&seen, "processes"),
        value(&seen, "read"),
        value(&seen, "own"),
        value(&seen, "found_0"),
        value(&seen, "found_1")
    );
    assert!(received >= 1, "the browser never requested the link");
    assert_eq!(value(&seen, "own"), "1", "{seen:?}");
    assert_eq!(
        value(&seen, "found_1"),
        "1",
        "the control was missed: {seen:?}"
    );
    assert_eq!(
        value(&seen, "found_0"),
        "0",
        "a confined run read the code: {seen:?}"
    );
}
