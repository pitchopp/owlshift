//! The daemon's loop: a pass every [`Timing::pass`](super::Timing) reads the
//! served projects, and a look every tick polls the watches and the web UI,
//! the stop asked and the starts due.
//!
//! A project the daemon paused waits for you (OWL-650): its pause is said
//! once and notified once, and its watch is tried again quietly at each
//! pass, its lines held, so daemon.log grows only when something changes: a
//! new reason, another end, or the pause lifted. The pause holds no watch
//! (OWL-651): a watch so tried dispatches at its first pass, and the pause
//! is lifted once it is past its start.

use std::io::{self, Write};
use std::net::TcpListener;
use std::thread;
use std::time::{Duration, Instant};

use jiff::{Timestamp, Zoned};
use owlshift_adapters::forge::Repo;
use owlshift_contracts::event::EventKind;
use owlshift_contracts::service::{BY_DAEMON, Request};
use owlshift_core::restart::{Restart, Restarts};
use owlshift_platform::process::{self, AtCheckedStop, Degree};
use serde_json::json;

use super::child::{Ended as ChildEnded, Supervised};
use super::digest::Digests;
use super::upgrade::{self, End, Probed, Stamp, supersedes};
use super::{Daemon, Ended, Found, UNSERVABLE_EXIT, UPGRADE_EXIT, UiLaunch, requests};
use crate::events::{Data, EventLog, EventSink, data, printable};
use crate::notify;
use crate::project::ProjectDirs;
use crate::served::{self, Registry, ServedLock};
use crate::ui::auth::RunSecret;
use crate::ui::{record, token};
use crate::watch;

/// One served project, from the pass that listed it until its watch, if
/// any, has ended once it is no longer listed.
struct Slot {
    repo: Repo,
    dirs: ProjectDirs,
    /// Its served lock, taken before its first watch, kept across restarts.
    lock: Option<ServedLock>,
    /// Its watch under way.
    child: Option<Supervised>,
    restarts: Restarts,
    /// When its watch is to start; `None` while one runs, or while it waits
    /// for the next pass.
    due: Option<Instant>,
    /// Why the daemon paused it, while it stands paused.
    paused: Option<String>,
    /// While a watch started as it stood paused runs: when that watch counts
    /// as serving at the latest, its record saying so first (OWL-651).
    lift_at: Option<Instant>,
    /// Whether the last pass listed it.
    listed: bool,
    /// Whether the daemon wrote its watch's stop file.
    stop_written: bool,
    /// Whether the operator was told a terminal's watch holds it.
    told_held: bool,
}

impl Slot {
    /// A slot due now. A daemon's own pause that stands on the project,
    /// from a daemon before this one, is taken up as it stands, already said
    /// and notified (OWL-651): its watch is tried quietly and lifts it once
    /// it serves.
    fn new(repo: Repo, data: &std::path::Path, now: Instant) -> Self {
        let dirs = ProjectDirs::new(data, &repo);
        let paused = match requests::pause(&dirs) {
            Some(Some(request)) if request.by == BY_DAEMON => Some(request.why),
            _ => None,
        };
        Self {
            dirs,
            repo,
            lock: None,
            child: None,
            restarts: Restarts::default(),
            due: Some(now),
            paused,
            lift_at: None,
            listed: true,
            stop_written: false,
            told_held: false,
        }
    }
}

/// The web UI the daemon runs (OWL-467): what it keeps across the UI's
/// ends, for as long as it runs.
#[derive(Default)]
struct UiChild {
    /// The run secret, made once per daemon run, handed to each UI.
    run: Option<RunSecret>,
    /// The listening socket on `127.0.0.1:<[ui] port>`, once bound: kept, so
    /// the port is never free while the daemon runs.
    listener: Option<TcpListener>,
    /// The UI under way.
    child: Option<Supervised>,
    restarts: Restarts,
    /// When it is to start; `None` while one runs.
    due: Option<Instant>,
    /// Whether it was stopped, the watches having ended at a stop.
    killed: bool,
}

/// What the loop keeps between two looks.
struct State {
    slots: Vec<Slot>,
    ui: UiChild,
    /// Once a stop is asked: nothing starts any more.
    stopping: bool,
    /// The drain to write as the stop file of each watch, for a drain; none
    /// for a checked stop, whose watches the platform told.
    relay: Option<Request>,
    /// Whether the stop is an upgrade's drain, no stop being asked when it
    /// was seen (OWL-436).
    upgrading: bool,
    next_pass: Instant,
    digests: Digests,
    /// The look at the binary at the daemon's path under way (OWL-616): the
    /// stamp it is for and the thread asking its `--version`.
    probe: Option<(Stamp, thread::JoinHandle<Result<Probed, String>>)>,
    /// The newer binary found there, whose drain the daemon asked.
    found: Option<Found>,
}

/// How one run of the daemon's loop ended (OWL-616).
pub(super) enum Turn {
    Ended(Ended),
    /// Its upgrade's drain is done, for the newer binary it found at its
    /// path, and the service did not start it: it hands its lock over.
    HandOver(Found),
}

impl Daemon<'_> {
    /// Serves the listed projects until a stop asked of the daemon is done,
    /// saying what it does on `out`.
    pub(super) fn run(&self, out: &mut dyn Write) -> Turn {
        if let Some(request) = requests::stop_asked(self.data) {
            say(
                out,
                &format!(
                    "A stop was asked before this daemon started ({}, at {}: {}): it serves \
                     nothing and ends.",
                    request.by, request.at, request.why
                ),
            );
            self.remove_stop_request(out);
            return Turn::Ended(Ended::Stopped);
        }
        // A drain naming this daemon was written once its record stood, by a
        // newer binary: honoured by the loop. Any other was left for the
        // daemon before this one, which is already the binary it was for.
        if let Some(request) = requests::leftover_drain(self.data, std::process::id()) {
            say(
                out,
                &format!(
                    "A drain for an upgrade was asked before this daemon started ({}, at {}: \
                     {}): this daemon serves, on the binary it runs.",
                    request.by, request.at, request.why
                ),
            );
        }
        say(
            out,
            &format!(
                "The daemon serves the projects listed in {}, read every {} seconds.",
                Registry::in_data_dir(self.data).file().display(),
                self.timing.pass.as_secs()
            ),
        );
        if self.ui.is_some() {
            // A record of an earlier run, or of another port, is never read
            // as this run's UI (`ui::probe::port_to_check`).
            self.remove_ui_record(out);
        }
        let mut state = State {
            slots: Vec::new(),
            ui: UiChild {
                due: self.ui.map(|_| Instant::now()),
                ..UiChild::default()
            },
            stopping: false,
            relay: None,
            upgrading: false,
            next_pass: Instant::now(),
            digests: Digests::default(),
            probe: None,
            found: None,
        };
        loop {
            self.check_stop(&mut state, out);
            self.probed(&mut state, out);
            let now = Instant::now();
            if !state.stopping && now >= state.next_pass {
                self.pass(&mut state, out);
                self.digest(&mut state, out);
                self.look_at_binary(&mut state, out);
                state.next_pass = now + self.timing.pass;
            }
            let stopping = state.stopping;
            for slot in &mut state.slots {
                if let Some(ended) = slot.child.as_mut().and_then(|child| child.poll(out)) {
                    slot.child = None;
                    self.ended(slot, ended, stopping, out);
                }
            }
            if let Some(ended) = state.ui.child.as_mut().and_then(|child| child.poll(out)) {
                state.ui.child = None;
                self.ui_ended(&mut state.ui, ended, stopping, out);
            }
            state.slots.retain_mut(|slot| {
                let gone = !slot.listed && slot.child.is_none();
                if gone {
                    self.forget(slot, out);
                }
                !gone
            });
            if state.stopping {
                if state.slots.iter().all(|slot| slot.child.is_none()) {
                    // The UI shows the drain until every watch has ended,
                    // then is stopped; the loop ends once it is reaped.
                    match &state.ui.child {
                        None => break,
                        Some(child) if !state.ui.killed => {
                            state.ui.killed = true;
                            if let Err(error) = child.kill() {
                                say(
                                    out,
                                    &format!("warning: the web UI could not be stopped: {error}"),
                                );
                            }
                        }
                        Some(_) => {}
                    }
                }
            } else {
                let now = Instant::now();
                for slot in &mut state.slots {
                    if slot.child.is_none() && slot.due.is_some_and(|due| now >= due) {
                        if self.stop_asked() {
                            break;
                        }
                        self.start(slot, out);
                    }
                    if slot.child.is_some()
                        && slot
                            .lift_at
                            .is_some_and(|at| now >= at || self.past_start(slot))
                    {
                        self.lift(slot, out);
                    }
                }
                if let Some(launch) = &self.ui
                    && state.ui.child.is_none()
                    && state.ui.due.is_some_and(|due| now >= due)
                    && !self.stop_asked()
                {
                    self.start_ui(launch, &mut state.ui, out);
                }
            }
            // A stop asked by a signal wakes the wait; once one is asked,
            // the wait returns at once, so the loop sleeps instead.
            if process::asked().is_some() {
                thread::sleep(self.timing.tick);
            } else {
                process::wait_asked(self.timing.tick);
            }
        }
        for slot in &mut state.slots {
            self.remove_own_stop(slot, out);
        }
        // A stop asked during an upgrade's drain wins: the service manager
        // must not start the daemon again. The drain goes first, then the
        // stop request is taken and read in one step, so a stop written in
        // between is never removed unread; one written later stands for the
        // next daemon, which then ends at its start.
        self.remove_drain_request(out);
        let stopped = self.take_stop_request(out) || process::asked().is_some();
        // A daemon the service's launcher started exits 75 even for a binary
        // it found itself: the manager starts the launcher again on that
        // path, and would end a successor with the daemon (OWL-616).
        let found = state.found.take();
        let by_service =
            found.is_some() && upgrade::started_by_service(self.data, std::process::id());
        match upgrade::end_of_drain(state.upgrading, stopped, found.is_some(), by_service) {
            End::HandOver => {
                if let (Some(binary), Some(found)) = (self.binary, found) {
                    say(
                        out,
                        &format!(
                            "The daemon drained for an upgrade: every watch it ran has ended; it \
                             hands its lock to the daemon of {}, version {}.",
                            binary.path.display(),
                            found.probed.binary().shown()
                        ),
                    );
                    return Turn::HandOver(found);
                }
                Turn::Ended(Ended::Stopped)
            }
            End::Upgraded => {
                say(
                    out,
                    &format!(
                        "The daemon drained for an upgrade: every watch it ran has ended; it \
                         exits with {UPGRADE_EXIT}, so the service manager starts it again on the \
                         new binary."
                    ),
                );
                Turn::Ended(Ended::Upgraded)
            }
            End::Stopped => {
                if state.upgrading {
                    say(
                        out,
                        "A stop was asked during the upgrade's drain: the daemon ends, not to be \
                         started again.",
                    );
                }
                say(out, "The daemon stopped: every watch it ran has ended.");
                Turn::Ended(Ended::Stopped)
            }
        }
    }

    /// Asks the binary at the daemon's path its `--version`, in a thread of
    /// its own, when its stamp changed since the daemon last saw it; a file
    /// that cannot be read is no change, as while it is relinked (OWL-616).
    fn look_at_binary(&self, state: &mut State, out: &mut dyn Write) {
        let Some(binary) = self.binary else {
            return;
        };
        if state.probe.is_some() || state.found.is_some() {
            return;
        }
        let Ok(now) = Stamp::of(&binary.path) else {
            return;
        };
        if binary.seen.get() == Some(now) {
            return;
        }
        let path = binary.path.clone();
        match thread::Builder::new()
            .name("binary".to_owned())
            .spawn(move || upgrade::probe(&path))
        {
            Ok(handle) => state.probe = Some((now, handle)),
            Err(error) => {
                binary.seen.set(Some(now));
                say(
                    out,
                    &format!(
                        "warning: the binary at {} changed, but its version could not be asked \
                         ({error}): the daemon goes on, on its own binary.",
                        binary.path.display()
                    ),
                );
            }
        }
    }

    /// Acts on what the binary at the daemon's path said, once it has: a
    /// binary that supersedes the daemon has it write its own drain for the
    /// upgrade; any other answer is one line (OWL-616).
    fn probed(&self, state: &mut State, out: &mut dyn Write) {
        let Some(binary) = self.binary else {
            return;
        };
        if !state
            .probe
            .as_ref()
            .is_some_and(|(_, handle)| handle.is_finished())
        {
            return;
        }
        let Some((stamp, handle)) = state.probe.take() else {
            return;
        };
        let before = binary.seen.replace(Some(stamp));
        let said = handle
            .join()
            .unwrap_or_else(|_| Err("its look at the binary failed".to_owned()));
        if state.stopping {
            return;
        }
        let path = binary.path.display();
        let own = binary.own().shown();
        match said {
            Ok(probed) if supersedes(probed.binary(), binary.own()) => {
                let shown = probed.binary().shown();
                let request = Request::new(
                    BY_DAEMON,
                    Timestamp::now(),
                    format!("an upgrade from version {own} to {shown}, the binary at {path}"),
                )
                .for_pid(std::process::id());
                match requests::ask_drain(self.data, &request) {
                    Ok(()) => {
                        say(
                            out,
                            &format!(
                                "The binary at {path} is now version {shown}, newer than this \
                                 daemon's {own}: the daemon drains to move onto it."
                            ),
                        );
                        state.found = Some(Found { probed, stamp });
                    }
                    Err(error) => {
                        binary.seen.set(before);
                        say(
                            out,
                            &format!(
                                "warning: the binary at {path} is now version {shown}, but the \
                                 drain for the upgrade could not be written ({error}): tried \
                                 again at the next pass."
                            ),
                        );
                    }
                }
            }
            Ok(probed) => say(
                out,
                &format!(
                    "The binary at {path} changed, to version {}, which does not supersede this \
                     daemon's {own}: the daemon goes on.",
                    probed.binary().shown()
                ),
            ),
            Err(error) => say(
                out,
                &format!(
                    "warning: the binary at {path} changed but did not tell its version \
                     ({error}): the daemon goes on, on its own binary, and asks again once the \
                     file changes."
                ),
            ),
        }
    }

    /// Whether any stop, or an upgrade's drain, is asked of the daemon now.
    fn stop_asked(&self) -> bool {
        process::asked().is_some()
            || requests::stop_asked(self.data).is_some()
            || requests::drain_asked(self.data).is_some()
    }

    /// Notes a stop asked of the daemon, once, and relays a drain to each
    /// watch under way that has not been told.
    fn check_stop(&self, state: &mut State, out: &mut dyn Write) {
        if !state.stopping {
            let asked = process::asked();
            let request = requests::stop_asked(self.data);
            // A stop asked first is a stop: no upgrade then.
            let drain = (asked.is_none() && request.is_none())
                .then(|| requests::drain_asked(self.data))
                .flatten();
            if asked.is_none() && request.is_none() && drain.is_none() {
                return;
            }
            state.stopping = true;
            let running = state
                .slots
                .iter()
                .filter(|slot| slot.child.is_some())
                .count();
            if let Some(drain) = drain {
                say(
                    out,
                    &format!(
                        "Draining for an upgrade ({}: {}): each of the {running} watches under \
                         way ends once its steps under way finish, then the daemon moves onto the \
                         new binary.",
                        drain.by, drain.why
                    ),
                );
                state.upgrading = true;
                state.relay = Some(drain);
            } else if asked == Some(Degree::Checked) {
                say(
                    out,
                    &format!(
                        "Stopping: each of the {running} watches under way stops its steps, \
                         checking their runs, then the daemon ends."
                    ),
                );
            } else {
                let request = request.unwrap_or_else(|| {
                    Request::new(BY_DAEMON, Timestamp::now(), "a Ctrl-C of the daemon")
                });
                say(
                    out,
                    &format!(
                        "Draining ({}: {}): each of the {running} watches under way ends once \
                         its steps under way finish, then the daemon ends.",
                        request.by, request.why
                    ),
                );
                state.relay = Some(request);
            }
        }
        if let Some(request) = &state.relay {
            for slot in &mut state.slots {
                if slot.child.is_some() && !slot.stop_written {
                    self.stop_watch(slot, request, out);
                }
            }
        }
    }

    /// Reads the served projects: a slot for each new one, due now; the
    /// watch of each one no longer listed drained; each paused one tried
    /// again.
    fn pass(&self, state: &mut State, out: &mut dyn Write) {
        let listed = match Registry::in_data_dir(self.data).read() {
            Ok(served) => served.projects,
            Err(error) => {
                say(
                    out,
                    &format!(
                        "warning: the served projects could not be read, kept as they were: {error}"
                    ),
                );
                return;
            }
        };
        let now = Instant::now();
        for slot in &mut state.slots {
            slot.listed = listed.iter().any(|entry| {
                entry
                    .repository
                    .eq_ignore_ascii_case(&slot.repo.to_string())
            });
        }
        for entry in &listed {
            if state.slots.iter().any(|slot| {
                slot.repo
                    .to_string()
                    .eq_ignore_ascii_case(&entry.repository)
            }) {
                continue;
            }
            match Repo::parse(&entry.repository) {
                Ok(repo) => state.slots.push(Slot::new(repo, self.data, now)),
                Err(error) => say(
                    out,
                    &format!("warning: {} is not served: {error}", entry.repository),
                ),
            }
        }
        for slot in &mut state.slots {
            if !slot.listed {
                if slot.child.is_some() && !slot.stop_written {
                    let request = Request::new(
                        BY_DAEMON,
                        Timestamp::now(),
                        "it is no longer served (`owlshift stop --forget`)",
                    );
                    self.stop_watch(slot, &request, out);
                }
            } else if slot.paused.is_some() && slot.child.is_none() {
                slot.due = Some(now);
            }
        }
    }

    /// Sends the daily digest of the listed projects when it is due.
    fn digest(&self, state: &mut State, out: &mut dyn Write) {
        let Some(settings) = &self.digest else {
            return;
        };
        let projects: Vec<_> = state
            .slots
            .iter()
            .filter(|slot| slot.listed)
            .map(|slot| (&slot.repo, &slot.dirs))
            .collect();
        state.digests.check(
            self.data,
            settings,
            &projects,
            self.notifier,
            &Zoned::now(),
            out,
        );
    }

    /// Starts `slot`'s watch, its served lock taken first.
    fn start(&self, slot: &mut Slot, out: &mut dyn Write) {
        let now = Instant::now();
        if slot.lock.is_none() {
            match served::take_served(&slot.dirs) {
                Ok(Some(lock)) => {
                    slot.lock = Some(lock);
                    slot.told_held = false;
                }
                Ok(None) => {
                    if !slot.told_held {
                        say(
                            out,
                            &format!(
                                "{}: a watch typed in a terminal runs on it, so the daemon serves \
                                 it once that one ends.",
                                slot.repo
                            ),
                        );
                        slot.told_held = true;
                    }
                    slot.due = Some(now + self.timing.pass);
                    return;
                }
                Err(error) => {
                    say(
                        out,
                        &format!(
                            "warning: the served lock of {} could not be taken, tried again at \
                             the next pass: {error}",
                            slot.repo
                        ),
                    );
                    slot.due = Some(now + self.timing.pass);
                    return;
                }
            }
        }
        let command = (self.watch)(&slot.repo);
        match Supervised::start(command, &slot.repo.to_string(), AtCheckedStop::Told) {
            Ok(mut child) => {
                let pid = child.pid();
                slot.due = None;
                slot.lift_at = slot.paused.as_ref().map(|_| now + self.timing.pass);
                // A paused project's retry is quiet (OWL-650): its lines
                // are held until its end or its pause's lift says them.
                if slot.paused.is_some() {
                    child.hold();
                } else {
                    say(
                        out,
                        &format!("Started the watch of {} (pid {pid}).", slot.repo),
                    );
                }
                slot.child = Some(child);
                self.event(
                    &slot.repo,
                    EventKind::Decision,
                    data([("service", json!("watch_started")), ("pid", json!(pid))]),
                );
            }
            // Refused because a stop was asked meanwhile: not an end.
            Err(_) if self.stop_asked() => slot.due = None,
            Err(error) => {
                let end = format!("it could not start: {error}");
                self.restart(slot, Duration::ZERO, &end, out);
            }
        }
    }

    /// Acts on the end of `slot`'s watch. The lines of a quiet retry are
    /// dropped when it ends unservable for the reason its pause already
    /// says, and printed first otherwise.
    fn ended(&self, slot: &mut Slot, mut ended: ChildEnded, stopping: bool, out: &mut dyn Write) {
        let asked = slot.stop_written || stopping;
        self.remove_own_stop(slot, out);
        slot.lift_at = None;
        let end = match &ended.status() {
            Ok(status) => status.to_string(),
            Err(error) => error.clone(),
        };
        if asked {
            ended.write_held(out);
            say(
                out,
                &format!("The watch of {} ended ({end}), as asked.", slot.repo),
            );
            self.event(
                &slot.repo,
                EventKind::Decision,
                data([
                    ("service", json!("watch_ended")),
                    ("end", json!(end)),
                    ("asked", json!(true)),
                ]),
            );
            slot.due = (slot.listed && !stopping).then(Instant::now);
            return;
        }
        let code = ended.status().ok().and_then(|status| status.code());
        if code == Some(i32::from(UNSERVABLE_EXIT)) {
            let reason = ended
                .last_error
                .as_deref()
                .map(|line| line.strip_prefix("owlshift: ").unwrap_or(line).to_owned())
                .unwrap_or_else(|| "its watch could not start".to_owned());
            if slot.paused.as_deref() != Some(reason.as_str()) {
                ended.write_held(out);
            }
            // Recorded, so `owlshift status` never reads the watch as
            // running (OWL-438).
            self.event(
                &slot.repo,
                EventKind::Decision,
                data([
                    ("service", json!("watch_ended")),
                    ("end", json!(end)),
                    ("asked", json!(false)),
                    ("unservable", json!(reason)),
                ]),
            );
            self.pause(slot, &reason, out);
            return;
        }
        ended.write_held(out);
        let end = match &ended.last_error {
            Some(line) if code != Some(0) => format!("{end}: {line}"),
            _ => end,
        };
        self.restart(slot, ended.ran_for, &end, out);
    }

    /// Counts an end of `slot`'s watch, which ran `ran_for`: started again
    /// after the delay, the operator told at the end the rule says.
    fn restart(&self, slot: &mut Slot, ran_for: Duration, end: &str, out: &mut dyn Write) {
        let next = slot.restarts.ended(&self.timing.restarts, ran_for);
        slot.due = Some(Instant::now() + next.delay);
        let delay = duration_text(next.delay);
        say(
            out,
            &format!(
                "The watch of {} ended ({end}): started again in {delay}.",
                slot.repo
            ),
        );
        self.event(
            &slot.repo,
            EventKind::Decision,
            data([
                ("service", json!("watch_ended")),
                ("end", json!(end)),
                ("asked", json!(false)),
                ("ran_ms", json!(millis(ran_for))),
                ("restart_in_ms", json!(millis(next.delay))),
                ("in_a_row", json!(next.in_a_row)),
            ]),
        );
        if next.notify {
            self.tell(
                &slot.repo,
                &format!(
                    "the watch of {} ended {} times in a row, the last ({end}); started again in \
                     {delay}",
                    slot.repo, next.in_a_row
                ),
                data([
                    ("what", json!("watch_ends_in_a_row")),
                    ("in_a_row", json!(next.in_a_row)),
                    ("end", json!(end)),
                ]),
            );
        }
    }

    /// Starts the web UI on the socket the daemon holds, binding it first
    /// when it holds none, with the run secret made once per run.
    fn start_ui(&self, launch: &UiLaunch<'_>, ui: &mut UiChild, out: &mut dyn Write) {
        let port = launch.port;
        self.remove_ui_record(out);
        if ui.run.is_none() {
            match RunSecret::new() {
                Ok(run) => ui.run = Some(run),
                Err(error) => {
                    let end = format!("no run secret could be made: {error}");
                    return self.ui_not_started(ui, &end, out);
                }
            }
        }
        if let Err(error) = token::ensure_token(self.data) {
            let end = format!(
                "{} cannot be made: {error}",
                token::token_file(self.data).display()
            );
            return self.ui_not_started(ui, &end, out);
        }
        if ui.listener.is_none() {
            match process::bind_loopback(port) {
                Ok(listener) => ui.listener = Some(listener),
                Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
                    let next = ui.restarts.ended(&self.timing.restarts, Duration::ZERO);
                    ui.due = Some(Instant::now() + next.delay);
                    let delay = duration_text(next.delay);
                    say(
                        out,
                        &format!(
                            "Port {port} of 127.0.0.1, `[ui] port` of the personal file, is \
                             taken by another program: the web UI does not start; tried again \
                             in {delay}."
                        ),
                    );
                    let end = format!("port {port} of 127.0.0.1 is taken by another program");
                    self.tell_ui_ends(&next, &end, &delay, out);
                    return;
                }
                Err(error) => {
                    let end = format!("port {port} of 127.0.0.1 cannot be bound: {error}");
                    return self.ui_not_started(ui, &end, out);
                }
            }
        }
        let (Some(listener), Some(run)) = (&ui.listener, &ui.run) else {
            return;
        };
        let mut command = (launch.command)();
        let handed = process::hand_over_listener(&mut command, listener).and_then(|listener_fd| {
            let secret_fd =
                process::hand_over_on_descriptor(&mut command, run.to_hex().as_bytes())?;
            Ok((listener_fd, secret_fd))
        });
        let (listener_fd, secret_fd) = match handed {
            Ok(fds) => fds,
            Err(error) => {
                let end =
                    format!("the socket and the run secret could not be handed over: {error}");
                return self.ui_not_started(ui, &end, out);
            }
        };
        command
            .arg("--listener-fd")
            .arg(listener_fd.to_string())
            .arg("--run-secret-fd")
            .arg(secret_fd.to_string());
        // Dropped by the start, with the copies handed over.
        match Supervised::start(command, "ui", AtCheckedStop::Stopped) {
            Ok(child) => {
                say(
                    out,
                    &format!(
                        "Started the web UI on 127.0.0.1:{port} (pid {}).",
                        child.pid()
                    ),
                );
                ui.child = Some(child);
                ui.due = None;
            }
            Err(_) if self.stop_asked() => ui.due = None,
            Err(error) => {
                let end = format!("it could not start: {error}");
                self.ui_not_started(ui, &end, out);
            }
        }
    }

    /// Counts a start of the web UI that failed for `end` as an end.
    fn ui_not_started(&self, ui: &mut UiChild, end: &str, out: &mut dyn Write) {
        self.restart_ui(ui, Duration::ZERO, end, out);
    }

    /// Acts on the end of the web UI.
    fn ui_ended(&self, ui: &mut UiChild, ended: ChildEnded, stopping: bool, out: &mut dyn Write) {
        let end = match &ended.status() {
            Ok(status) => status.to_string(),
            Err(error) => error.clone(),
        };
        if stopping {
            say(out, &format!("The web UI ended ({end}), as asked."));
            ui.due = None;
            return;
        }
        let code = ended.status().ok().and_then(|status| status.code());
        let end = match &ended.last_error {
            Some(line) if code != Some(0) => format!("{end}: {line}"),
            _ => end,
        };
        self.restart_ui(ui, ended.ran_for, &end, out);
    }

    /// Counts an end of the web UI, which ran `ran_for`: started again after
    /// the delay, the operator told at the end the rule says. No event: the
    /// event log is per project.
    fn restart_ui(&self, ui: &mut UiChild, ran_for: Duration, end: &str, out: &mut dyn Write) {
        let next = ui.restarts.ended(&self.timing.restarts, ran_for);
        ui.due = Some(Instant::now() + next.delay);
        let delay = duration_text(next.delay);
        say(
            out,
            &format!("The web UI ended ({end}): started again in {delay}."),
        );
        self.tell_ui_ends(&next, end, &delay, out);
    }

    /// Tells the operator the web UI keeps ending, when the rule says so.
    fn tell_ui_ends(&self, next: &Restart, end: &str, delay: &str, out: &mut dyn Write) {
        if !next.notify {
            return;
        }
        let line = format!(
            "the web UI ended {} times in a row, the last ({end}); started again in {delay}",
            next.in_a_row
        );
        say(out, &format!("Told: {line}."));
        if let Some(notifier) = self.notifier
            && let Err(error) = notifier.notify(&format!("Owlshift: {line}"))
        {
            say(
                out,
                &format!("warning: the notification was not shown: {error}"),
            );
        }
    }

    fn remove_ui_record(&self, out: &mut dyn Write) {
        if let Err(error) = record::remove(self.data) {
            say(
                out,
                &format!(
                    "warning: {} could not be removed: {error}",
                    record::file(self.data).display()
                ),
            );
        }
    }

    /// Pauses `slot`'s project, whose watch could not start for `reason`,
    /// which no restart mends: it waits for you (OWL-650). Its pause file is
    /// written unless one stands, and the daemon's own is written again
    /// when the reason changed, so every place that shows it gives the
    /// latest; the pause is said when it starts or its reason changes, the
    /// operator notified once, and the project tried again quietly at each
    /// pass.
    fn pause(&self, slot: &mut Slot, reason: &str, out: &mut dyn Write) {
        let first = slot.paused.is_none();
        let changed = slot.paused.as_deref() != Some(reason);
        slot.paused = Some(reason.to_owned());
        slot.due = None;
        let standing = requests::pause(&slot.dirs);
        let own = matches!(&standing, Some(Some(request)) if request.by == BY_DAEMON);
        let stale = matches!(&standing, Some(Some(request)) if request.by == BY_DAEMON && request.why != reason);
        let written = if stale {
            requests::remove_own_pause(&slot.dirs).and_then(|_| {
                requests::write_pause(
                    &slot.dirs,
                    &Request::new(BY_DAEMON, Timestamp::now(), reason),
                )
            })
        } else if standing.is_none() {
            requests::write_pause(
                &slot.dirs,
                &Request::new(BY_DAEMON, Timestamp::now(), reason),
            )
        } else {
            Ok(false)
        };
        if let Err(error) = written {
            say(
                out,
                &format!(
                    "warning: the pause of {} could not be written: {error}",
                    slot.repo
                ),
            );
        }
        if changed {
            say(
                out,
                &format!(
                    "Paused {}, waiting for you: {}.",
                    slot.repo,
                    reason.strip_suffix('.').unwrap_or(reason)
                ),
            );
        }
        if first && !own {
            self.tell(
                &slot.repo,
                &format!("{} waits for you: {reason}", slot.repo),
                data([("what", json!("project_paused")), ("reason", json!(reason))]),
            );
        }
    }

    /// Whether `slot`'s watch is past its start (OWL-651): its watch record
    /// stands, which a served watch writes once every check that can refuse
    /// it has passed, just before its first pass. Read only while a paused
    /// project's watch runs; one that cannot be read is not past it.
    fn past_start(&self, slot: &Slot) -> bool {
        let Some(pid) = slot.child.as_ref().map(Supervised::pid) else {
            return false;
        };
        watch::presence::running(self.data)
            .is_ok_and(|running| running.iter().any(|watch| watch.pid == pid))
    }

    /// Lifts `slot`'s pause: a watch started while it stood paused is past
    /// its start, its record standing, or still runs a pass later, as while
    /// it waits for the network. What that watch held is said first.
    fn lift(&self, slot: &mut Slot, out: &mut dyn Write) {
        slot.lift_at = None;
        if let Some(child) = &mut slot.child {
            child.release(out);
        }
        if slot.paused.take().is_none() {
            return;
        }
        if let Err(error) = requests::remove_own_pause(&slot.dirs) {
            say(
                out,
                &format!(
                    "warning: the pause of {} could not be removed: {error}",
                    slot.repo
                ),
            );
        }
        say(
            out,
            &format!("{} is served again: its pause is lifted.", slot.repo),
        );
        self.event(
            &slot.repo,
            EventKind::Decision,
            data([("service", json!("pause_lifted"))]),
        );
    }

    /// Asks `slot`'s watch to drain, for `request`.
    fn stop_watch(&self, slot: &mut Slot, request: &Request, out: &mut dyn Write) {
        match requests::ask_watch_stop(&slot.dirs, request) {
            Ok(()) => {
                slot.stop_written = true;
                say(
                    out,
                    &format!("Draining the watch of {}: {}.", slot.repo, request.why),
                );
            }
            Err(error) => say(
                out,
                &format!(
                    "warning: the stop file of {} could not be written: {error}",
                    slot.repo
                ),
            ),
        }
    }

    /// Removes the stop file the daemon wrote for `slot`'s watch, when it
    /// still stands: the watch removes it as it drains on it, but one that
    /// ended first would find it at its next start.
    fn remove_own_stop(&self, slot: &mut Slot, out: &mut dyn Write) {
        if !std::mem::take(&mut slot.stop_written) {
            return;
        }
        if let Err(error) = watch::stop::remove(&slot.dirs) {
            say(
                out,
                &format!(
                    "warning: the stop file of {} could not be removed: {error}",
                    slot.repo
                ),
            );
        }
    }

    /// Lets go of a project no longer listed, its watch ended.
    fn forget(&self, slot: &mut Slot, out: &mut dyn Write) {
        self.remove_own_stop(slot, out);
        if let Err(error) = requests::remove_own_pause(&slot.dirs) {
            say(
                out,
                &format!(
                    "warning: the pause of {} could not be removed: {error}",
                    slot.repo
                ),
            );
        }
        slot.lock = None;
        say(out, &format!("{} is no longer served.", slot.repo));
    }

    fn remove_stop_request(&self, out: &mut dyn Write) {
        if let Err(error) = requests::remove_stop_request(self.data) {
            say(
                out,
                &format!("warning: the stop request could not be removed: {error}"),
            );
        }
    }

    /// Takes the stop request away, if one stands: whether one did. One
    /// that cannot be taken counts as standing.
    fn take_stop_request(&self, out: &mut dyn Write) -> bool {
        match requests::take_stop_request(self.data) {
            Ok(taken) => taken,
            Err(error) => {
                say(
                    out,
                    &format!("warning: the stop request could not be removed: {error}"),
                );
                true
            }
        }
    }

    fn remove_drain_request(&self, out: &mut dyn Write) {
        if let Err(error) = requests::remove_drain_request(self.data) {
            say(
                out,
                &format!("warning: the drain request could not be removed: {error}"),
            );
        }
    }

    /// Tells the operator `line`, recorded as a `warning` event with `data`.
    fn tell(&self, repo: &Repo, line: &str, data: Data) {
        self.event(repo, EventKind::Warning, data);
        if let Some(notifier) = self.notifier
            && let Err(error) = notifier.notify(&format!("Owlshift: {line}"))
        {
            self.event(repo, EventKind::Warning, notify::not_shown(&error));
        }
    }

    /// Records a project event, printed nowhere: the daemon says its own
    /// lines.
    fn event(&self, repo: &Repo, kind: EventKind, data: Data) {
        let mut nowhere = io::sink();
        EventSink::new(repo.to_string(), EventLog::in_dir(self.data), &mut nowhere)
            .emit_project(None, kind, data);
    }
}

/// Writes `line` on `out`, safe for a terminal, never failing.
pub(super) fn say(out: &mut dyn Write, line: &str) {
    let _ = writeln!(out, "{}", printable(line));
}

/// `duration` in words: seconds, or milliseconds below a second.
fn duration_text(duration: Duration) -> String {
    if duration >= Duration::from_secs(1) && duration.subsec_millis() == 0 {
        format!("{} s", duration.as_secs())
    } else {
        format!("{} ms", duration.as_millis())
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use owlshift_contracts::service::BY_PAUSE;

    use super::*;

    /// OWL-651: a daemon started while its own pause stands on a project
    /// takes it up, so it lifts it once the project serves again; a
    /// person's pause, or none, is not the daemon's to take up.
    #[test]
    fn a_new_slot_takes_up_the_daemons_own_pause_alone() {
        let data = tempfile::tempdir().unwrap();
        let repo = Repo::parse("acme/web").unwrap();
        let dirs = ProjectDirs::new(data.path(), &repo);
        let slot = || Slot::new(repo.clone(), data.path(), Instant::now());
        assert_eq!(slot().paused, None);

        let own = Request::new(BY_DAEMON, Timestamp::now(), "Sign in again.");
        assert!(requests::write_pause(&dirs, &own).unwrap());
        assert_eq!(slot().paused.as_deref(), Some("Sign in again."));

        let person = Request::new(BY_PAUSE, Timestamp::now(), "lunch");
        requests::ask_pause(data.path(), Some(&dirs), &person).unwrap();
        assert_eq!(slot().paused, None);
    }
}
