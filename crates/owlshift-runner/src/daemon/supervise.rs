//! The daemon's loop: a pass every [`Timing::pass`](super::Timing) reads the
//! served projects, and a look every tick polls the watches and the web UI,
//! the stop asked and the starts due.

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
use super::{Daemon, Ended, UNSERVABLE_EXIT, UPGRADE_EXIT, UiLaunch, requests};
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
    /// When a watch started while it stood paused counts as serving.
    lift_at: Option<Instant>,
    /// Whether the last pass listed it.
    listed: bool,
    /// Whether the daemon wrote its watch's stop file.
    stop_written: bool,
    /// Whether the operator was told a terminal's watch holds it.
    told_held: bool,
}

impl Slot {
    fn new(repo: Repo, data: &std::path::Path, now: Instant) -> Self {
        Self {
            dirs: ProjectDirs::new(data, &repo),
            repo,
            lock: None,
            child: None,
            restarts: Restarts::default(),
            due: Some(now),
            paused: None,
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
}

impl Daemon<'_> {
    /// Serves the listed projects until a stop asked of the daemon is done,
    /// saying what it does on `out`.
    pub fn run(&self, out: &mut dyn Write) -> Ended {
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
            return Ended::Stopped;
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
        };
        loop {
            self.check_stop(&mut state, out);
            let now = Instant::now();
            if !state.stopping && now >= state.next_pass {
                self.pass(&mut state, out);
                self.digest(&mut state, out);
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
                    if slot.child.is_some() && slot.lift_at.is_some_and(|at| now >= at) {
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
        if state.upgrading && !stopped {
            say(
                out,
                &format!(
                    "The daemon drained for an upgrade: every watch it ran has ended; it exits \
                     with {UPGRADE_EXIT}, so the service manager starts it again on the new \
                     binary."
                ),
            );
            return Ended::Upgraded;
        }
        if state.upgrading {
            say(
                out,
                "A stop was asked during the upgrade's drain: the daemon ends, not to be started \
                 again.",
            );
        }
        say(out, "The daemon stopped: every watch it ran has ended.");
        Ended::Stopped
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
                         way ends once its steps under way finish, then the daemon exits with \
                         {UPGRADE_EXIT}.",
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
            Ok(child) => {
                let pid = child.pid();
                slot.child = Some(child);
                slot.due = None;
                slot.lift_at = slot.paused.as_ref().map(|_| now + self.timing.pass);
                say(
                    out,
                    &format!("Started the watch of {} (pid {pid}).", slot.repo),
                );
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

    /// Acts on the end of `slot`'s watch.
    fn ended(&self, slot: &mut Slot, ended: ChildEnded, stopping: bool, out: &mut dyn Write) {
        let asked = slot.stop_written || stopping;
        self.remove_own_stop(slot, out);
        slot.lift_at = None;
        let end = match &ended.status() {
            Ok(status) => status.to_string(),
            Err(error) => error.clone(),
        };
        if asked {
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
    /// which no restart mends: its pause file written unless one stands,
    /// the operator told once, tried again at each pass.
    fn pause(&self, slot: &mut Slot, reason: &str, out: &mut dyn Write) {
        let first = slot.paused.is_none();
        slot.paused = Some(reason.to_owned());
        slot.due = None;
        let standing = requests::pause(&slot.dirs);
        let own = matches!(&standing, Some(Some(request)) if request.by == BY_DAEMON);
        if standing.is_none()
            && let Err(error) = requests::write_pause(
                &slot.dirs,
                &Request::new(BY_DAEMON, Timestamp::now(), reason),
            )
        {
            say(
                out,
                &format!(
                    "warning: the pause of {} could not be written: {error}",
                    slot.repo
                ),
            );
        }
        say(
            out,
            &format!(
                "Paused {}: {reason}. Tried again every {} seconds.",
                slot.repo,
                self.timing.pass.as_secs()
            ),
        );
        if first && !own {
            self.tell(
                &slot.repo,
                &format!(
                    "{} is paused: {reason}. Owlshift tries it again every {} seconds.",
                    slot.repo,
                    self.timing.pass.as_secs()
                ),
                data([("what", json!("project_paused")), ("reason", json!(reason))]),
            );
        }
    }

    /// Lifts `slot`'s pause: a watch started while it stood paused still
    /// runs a pass later.
    fn lift(&self, slot: &mut Slot, out: &mut dyn Write) {
        slot.lift_at = None;
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
fn say(out: &mut dyn Write, line: &str) {
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
