//! Telling the operator that they have become the blocker (scenario S6;
//! `[notifications]` in the personal file, `runtime-and-operations.md`).
//!
//! `owlshift do` and `owlshift continue` hand the outcome of their run to
//! [`notify_blocker`] once it is printed. Only the outcomes where a person
//! must act notify: a question round, a re-ask, a reply to the decider's
//! counter-question, a parked ticket, any of those whose comment reached the
//! ticket although keeping Owlshift's state then failed, the harness at its
//! usage limit. Never
//! progress, a delivery or a green check. A notification that cannot be
//! shown is a warning event, never an error: the run's outcome and exit code
//! stay as they were.

use std::path::{Path, PathBuf};

use serde_json::json;

use owlshift_adapters::notifier::{Error, Notifier};
use owlshift_adapters::tracker::Tracker;
use owlshift_contracts::config::PersonalConfig;
use owlshift_contracts::event::EventKind;
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::result;

use crate::config::FileState;
use crate::events::{EventSink, data};
use crate::on_demand::{Delivered, Landed, Stop};
use crate::system::System;

/// The title every desktop notification shows.
pub const TITLE: &str = "Owlshift";

/// The longest ticket link a notification shows; a longer one is left out.
const LINK_MAX: usize = 300;

/// The most bytes of a notifier program's error output a warning keeps.
const STDERR_MAX: usize = 200;

/// macOS's AppleScript runner, at its fixed system path.
const OSASCRIPT: &str = "/usr/bin/osascript";

/// The script `osascript` runs: the title and the line come as its
/// arguments, never inside the script, so no quote or backslash in them
/// can change what it does. Checked live on 2026-10-03 (OWL-140).
const OSASCRIPT_SCRIPT: [&str; 3] = [
    "on run argv",
    "display notification (item 2 of argv) with title (item 1 of argv)",
    "end run",
];

/// Whether the personal file lets this machine show desktop notifications:
/// yes unless `[notifications] desktop = false`. A machine without a
/// personal file keeps the default; an invalid one never reaches a run.
pub fn desktop_enabled(personal: &FileState<PersonalConfig>) -> bool {
    match personal {
        FileState::Loaded { config, .. } => config.notifications.desktop != Some(false),
        _ => true,
    }
}

/// The line that tells the operator what waits on them after `stop`, or
/// `None` when nothing does. Fixed wording, the ticket, round numbers and a
/// reset time only: never text from a model or the tracker.
///
/// Every variant is listed, so a new way to stop must say whether it
/// notifies. A refusal does not, since nothing it names reached the ticket
/// and its message in the terminal says what to do. A comment that asks a
/// person to act and is on the ticket does, even when keeping the state
/// failed after it ([`Stop::NotKept`]): the line says so, since the next run
/// may post that comment again. The decision and the resume comments ask
/// nothing of anyone, so a failure to keep their state stays a refusal.
pub fn blocker_line(ticket: &TicketId, stop: &Stop) -> Option<String> {
    Some(match stop {
        Stop::NeedsInput { status, posted, .. } => match (status, posted) {
            (result::Status::PremiseFalse, Ok(round)) => format!(
                "{ticket}: the run found the ticket's premise false; a decision waits on the \
                 ticket (round {round})"
            ),
            (_, Ok(round)) => {
                format!("{ticket}: questions wait for an answer on the ticket (round {round})")
            }
            (_, Err(_)) => format!(
                "{ticket}: the run needs answers it could not post on the ticket; they are in \
                 the terminal"
            ),
        },
        Stop::Reasked { round, reask, .. } => {
            format!("{ticket}: questions asked again on the ticket (round {round}, re-ask {reask})")
        }
        Stop::CounterQuestion { .. } => {
            format!("{ticket}: the decider's question has a reply on the ticket; answers wait")
        }
        Stop::Parked { .. } => format!("{ticket} is parked: a person must look"),
        Stop::NotKept { landed, .. } => {
            let what = match landed {
                Landed::Questions => "questions wait for an answer on the ticket",
                Landed::PremiseFalse => {
                    "the run found the ticket's premise false; a decision waits on the ticket"
                }
                Landed::Reask => "questions were asked again on the ticket",
                Landed::Reply => "the decider's question has a reply on the ticket",
                Landed::Parked => "the ticket is parked: a person must look",
            };
            format!("{ticket}: {what}, but Owlshift's state was not kept; see the terminal")
        }
        Stop::UsageLimit { resets_at } => match resets_at {
            Some(at) => {
                format!("{ticket}: stopped at the harness's usage limit, which resets at {at}")
            }
            None => format!("{ticket}: stopped at the harness's usage limit"),
        },
        Stop::Busy(_)
        | Stop::Refused(_)
        | Stop::Unverified { .. }
        | Stop::Waiting { .. }
        | Stop::Settling { .. }
        | Stop::CheckFailed { .. }
        | Stop::Delivery(_) => return None,
    })
}

/// `url` when a notification can show it as is: an `https` link of at most
/// [`LINK_MAX`] printable ASCII characters, with no space, quote, backslash,
/// `<`, `>` or `&` that a notification service could read as markup.
fn shown_link(url: &str) -> Option<&str> {
    let plain = url
        .chars()
        .all(|c| c.is_ascii_graphic() && !matches!(c, '<' | '>' | '&' | '"' | '\'' | '\\' | '`'));
    (url.starts_with("https://") && url.len() <= LINK_MAX && plain).then_some(url)
}

/// Notifies the operator when `outcome` makes them the blocker, with the
/// ticket's link when the tracker gives one fit to show. `notifier` is
/// `None` when this machine shows no notification (`desktop = false`, or no
/// desktop session): then nothing is asked of the tracker either. A failure
/// is recorded as a warning event.
pub fn notify_blocker(
    notifier: Option<&dyn Notifier>,
    tracker: &dyn Tracker,
    ticket: &TicketId,
    outcome: &Result<Delivered, Stop>,
    sink: &mut EventSink<'_>,
) {
    let (Some(notifier), Err(stop)) = (notifier, outcome) else {
        return;
    };
    let Some(mut line) = blocker_line(ticket, stop) else {
        return;
    };
    if let Some(url) = tracker.ticket_url(ticket)
        && let Some(url) = shown_link(&url)
    {
        line.push(' ');
        line.push_str(url);
    }
    if let Err(error) = notifier.notify(&line) {
        sink.emit(
            ticket,
            None,
            EventKind::Warning,
            data([
                ("what", json!("notification_not_shown")),
                ("reason", json!(error.to_string())),
            ]),
        );
    }
}

/// The system a desktop notification is shown through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    MacOs,
    Linux,
    /// Native Windows and any other system: `owlshift do` refuses to run
    /// there, since agent runs cannot be confined, so nothing notifies.
    Other,
}

impl Os {
    /// The system this build runs on.
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else {
            Self::Other
        }
    }
}

/// Whether this Linux machine has a desktop session to show a notification
/// in: `WAYLAND_DISPLAY` or `DISPLAY` is set.
fn linux_session() -> bool {
    ["WAYLAND_DISPLAY", "DISPLAY"]
        .iter()
        .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
}

/// Shows a line through the operating system's own notification service:
/// `osascript` on macOS, `notify-send` on Linux.
pub struct DesktopNotifier<'a> {
    system: &'a dyn System,
    os: Os,
}

impl<'a> DesktopNotifier<'a> {
    /// This machine's desktop notifier, `None` when it has nowhere to show
    /// one.
    pub fn for_this_machine(system: &'a dyn System) -> Option<Self> {
        let os = Os::current();
        Self::new(system, os, os == Os::Linux && linux_session())
    }

    /// The notifier of `os`, `None` on a Linux machine without a desktop
    /// session (`linux_session` false) and on any other system but macOS.
    /// macOS has no such check: a failure there is a warning.
    pub fn new(system: &'a dyn System, os: Os, linux_session: bool) -> Option<Self> {
        match os {
            Os::MacOs => Some(Self { system, os }),
            Os::Linux if linux_session => Some(Self { system, os }),
            Os::Linux | Os::Other => None,
        }
    }

    /// The program and its arguments that show `line`.
    fn command<'l>(&self, line: &'l str) -> Result<(PathBuf, Vec<&'l str>), Error> {
        match self.os {
            Os::MacOs => {
                let mut args = Vec::new();
                for part in OSASCRIPT_SCRIPT {
                    args.extend(["-e", part]);
                }
                args.extend(["--", TITLE, line]);
                Ok((PathBuf::from(OSASCRIPT), args))
            }
            Os::Linux => {
                let program = self.system.locate("notify-send").ok_or_else(|| {
                    Error::new(
                        "`notify-send` is not on the PATH: install it (libnotify), or set \
                         `desktop = false` under `[notifications]` in the personal file",
                    )
                })?;
                Ok((program, vec![TITLE, line]))
            }
            Os::Other => Err(Error::new("this system shows no desktop notification")),
        }
    }
}

impl Notifier for DesktopNotifier<'_> {
    fn notify(&self, line: &str) -> Result<(), Error> {
        let (program, args) = self.command(line)?;
        let name = program_name(&program);
        let captured = self
            .system
            .run(&program, &args, None)
            .map_err(|error| Error::new(format!("`{name}`: {error}")))?;
        if captured.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&captured.stderr);
        let stderr = stderr.trim();
        let mut cut = stderr.len().min(STDERR_MAX);
        while !stderr.is_char_boundary(cut) {
            cut -= 1;
        }
        let exit = captured
            .code
            .map_or_else(|| "a signal".to_owned(), |code| format!("exit code {code}"));
        Err(Error::new(format!(
            "`{name}` ended with {exit}: {}",
            &stderr[..cut]
        )))
    }
}

fn program_name(program: &Path) -> String {
    program.file_name().map_or_else(
        || program.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::num::NonZeroU32;

    use jiff::Timestamp;

    use owlshift_adapters::forge::{CommitId, PrState, PullRequest};
    use owlshift_adapters::tracker::{Capability, Comment, Error as TrackerError, Ticket};
    use owlshift_contracts::event::Event;
    use owlshift_core::state::ParkReason;

    use super::*;
    use crate::events::EventLog;
    use crate::system::fake::{Answer, FakeSystem};

    const LINK: &str = "https://linear.app/owlshift/issue/OWL-7/a-title";

    /// Records each line instead of showing it.
    #[derive(Default)]
    struct Recording(RefCell<Vec<String>>);

    impl Notifier for Recording {
        fn notify(&self, line: &str) -> Result<(), Error> {
            self.0.borrow_mut().push(line.to_owned());
            Ok(())
        }
    }

    struct Failing;

    impl Notifier for Failing {
        fn notify(&self, _: &str) -> Result<(), Error> {
            Err(Error::new("`osascript` ended with exit code 1: no"))
        }
    }

    /// A tracker asked for nothing but a ticket's link, which it counts.
    struct Linked {
        url: Option<String>,
        asked: Cell<u32>,
    }

    impl Linked {
        fn new(url: Option<&str>) -> Self {
            Self {
                url: url.map(str::to_owned),
                asked: Cell::new(0),
            }
        }
    }

    impl Tracker for Linked {
        fn capabilities(&self) -> &'static [Capability] {
            &[]
        }
        fn ticket(&self, _: &TicketId) -> Result<Ticket, TrackerError> {
            unreachable!("a notification reads no ticket")
        }
        fn comments(&self, _: &TicketId) -> Result<Vec<Comment>, TrackerError> {
            unreachable!("a notification reads no comment")
        }
        fn post_comment(&self, _: &TicketId, _: &str) -> Result<Comment, TrackerError> {
            unreachable!("a notification posts nothing")
        }
        fn set_stage(&self, _: &TicketId, _: &str) -> Result<(), TrackerError> {
            unreachable!("a notification moves no stage")
        }
        fn ticket_url(&self, _: &TicketId) -> Option<String> {
            self.asked.set(self.asked.get() + 1);
            self.url.clone()
        }
    }

    fn owl_7() -> TicketId {
        TicketId::new("OWL-7").unwrap()
    }

    fn round(n: u32) -> NonZeroU32 {
        NonZeroU32::new(n).unwrap()
    }

    fn at() -> Timestamp {
        "2026-10-03T20:00:00Z".parse().unwrap()
    }

    fn needs_input(status: result::Status, posted: Result<NonZeroU32, String>) -> Stop {
        Stop::NeedsInput {
            ticket: owl_7(),
            status,
            summary: "s".to_owned(),
            questions: Vec::new(),
            posted,
        }
    }

    fn not_kept(landed: Landed) -> Stop {
        Stop::NotKept {
            landed,
            message: "model text".to_owned(),
        }
    }

    /// What `notify_blocker` shows and records for `outcome`: the lines,
    /// how often the tracker was asked for the link, the events logged.
    fn notify(
        notifier: Option<&dyn Notifier>,
        url: Option<&str>,
        outcome: &Result<Delivered, Stop>,
    ) -> (u32, Vec<Event>, String) {
        let dir = tempfile::tempdir().unwrap();
        let log = EventLog::in_dir(dir.path());
        let tracker = Linked::new(url);
        let mut out = Vec::new();
        let mut sink = EventSink::new("demo", log.clone(), &mut out);
        notify_blocker(notifier, &tracker, &owl_7(), outcome, &mut sink);
        let events = std::fs::read_to_string(log.path())
            .unwrap_or_default()
            .lines()
            .map(|line| Event::parse(line).unwrap())
            .collect();
        (tracker.asked.get(), events, String::from_utf8(out).unwrap())
    }

    /// The lines one outcome shows, with the link `LINK`.
    fn shown(stop: Stop) -> Vec<String> {
        let recording = Recording::default();
        let (_, events, _) = notify(Some(&recording), Some(LINK), &Err(stop));
        assert!(events.is_empty(), "{events:?}");
        recording.0.into_inner()
    }

    #[test]
    fn each_moment_the_operator_is_the_blocker_notifies_once() {
        let cases = [
            (
                needs_input(result::Status::Questions, Ok(round(2))),
                "OWL-7: questions wait for an answer on the ticket (round 2)",
            ),
            (
                needs_input(result::Status::PremiseFalse, Ok(round(1))),
                "OWL-7: the run found the ticket's premise false; a decision waits on the ticket \
                 (round 1)",
            ),
            (
                needs_input(result::Status::Questions, Err("Linear: down".to_owned())),
                "OWL-7: the run needs answers it could not post on the ticket; they are in the \
                 terminal",
            ),
            (
                Stop::Reasked {
                    ticket: owl_7(),
                    round: round(1),
                    reask: 2,
                    open: Vec::new(),
                },
                "OWL-7: questions asked again on the ticket (round 1, re-ask 2)",
            ),
            (
                Stop::CounterQuestion {
                    ticket: owl_7(),
                    replied: "c1".to_owned(),
                    asked: Vec::new(),
                },
                "OWL-7: the decider's question has a reply on the ticket; answers wait",
            ),
            (
                Stop::Parked {
                    reason: ParkReason::Reasks,
                    detail: "model text".to_owned(),
                    unposted: None,
                },
                "OWL-7 is parked: a person must look",
            ),
            (
                not_kept(Landed::Questions),
                "OWL-7: questions wait for an answer on the ticket, but Owlshift's state was \
                 not kept; see the terminal",
            ),
            (
                not_kept(Landed::PremiseFalse),
                "OWL-7: the run found the ticket's premise false; a decision waits on the \
                 ticket, but Owlshift's state was not kept; see the terminal",
            ),
            (
                not_kept(Landed::Reask),
                "OWL-7: questions were asked again on the ticket, but Owlshift's state was not \
                 kept; see the terminal",
            ),
            (
                not_kept(Landed::Reply),
                "OWL-7: the decider's question has a reply on the ticket, but Owlshift's state \
                 was not kept; see the terminal",
            ),
            (
                not_kept(Landed::Parked),
                "OWL-7: the ticket is parked: a person must look, but Owlshift's state was not \
                 kept; see the terminal",
            ),
            (
                Stop::UsageLimit {
                    resets_at: Some(at()),
                },
                "OWL-7: stopped at the harness's usage limit, which resets at \
                 2026-10-03T20:00:00Z",
            ),
            (
                Stop::UsageLimit { resets_at: None },
                "OWL-7: stopped at the harness's usage limit",
            ),
        ];
        for (stop, line) in cases {
            assert_eq!(shown(stop), [format!("{line} {LINK}")]);
        }
    }

    #[test]
    fn no_other_outcome_notifies_or_asks_the_tracker() {
        let delivered = Delivered {
            pull_request: PullRequest {
                number: 1,
                url: "https://github.com/o/r/pull/1".to_owned(),
                head: CommitId::new("a".repeat(40)).unwrap(),
                state: PrState::Open,
            },
            opened: true,
            verdict: None,
            comment: "c".to_owned(),
        };
        let outcomes = [
            Ok(delivered),
            Err(Stop::Busy(PathBuf::from("/p"))),
            Err(Stop::Refused(
                "OWL-7 parked (re-asks), but the PARKED comment could not be posted".to_owned(),
            )),
            Err(Stop::Unverified {
                marker: PathBuf::from("/p/unverified"),
                text: "t".to_owned(),
            }),
            Err(Stop::Waiting {
                ticket: owl_7(),
                round: 1,
                decider: "d".to_owned(),
                since: at(),
            }),
            Err(Stop::Settling {
                ticket: owl_7(),
                decider: "d".to_owned(),
                window: std::time::Duration::from_secs(600),
                counts_at: at(),
            }),
            Err(Stop::CheckFailed {
                ticket: owl_7(),
                detail: "d".to_owned(),
            }),
            Err(Stop::Delivery("d".to_owned())),
        ];
        for outcome in outcomes {
            let recording = Recording::default();
            let (asked, events, _) = notify(Some(&recording), Some(LINK), &outcome);
            assert!(recording.0.borrow().is_empty(), "{outcome:?}");
            assert_eq!((asked, events.len()), (0, 0), "{outcome:?}");
        }
    }

    #[test]
    fn desktop_false_silences_every_notification() {
        let personal = |text: &str| FileState::Loaded {
            path: PathBuf::from("/c/config.toml"),
            config: PersonalConfig::parse(text).unwrap(),
            entries: Vec::new(),
        };
        assert!(!desktop_enabled(&personal(
            "[notifications]\ndesktop = false\n"
        )));
        assert!(desktop_enabled(&personal(
            "[notifications]\ndesktop = true\n"
        )));
        assert!(desktop_enabled(&personal("")));
        assert!(desktop_enabled(&FileState::Absent(PathBuf::from("/c"))));

        // Silenced, a blocker shows nothing and asks nothing of the tracker.
        let stop = Stop::UsageLimit { resets_at: None };
        let (asked, events, _) = notify(None, Some(LINK), &Err(stop));
        assert_eq!((asked, events.len()), (0, 0));
    }

    #[test]
    fn a_notification_not_shown_is_a_warning_event() {
        let stop = Stop::UsageLimit { resets_at: None };
        let (_, events, out) = notify(Some(&Failing), None, &Err(stop));
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].kind, EventKind::Warning);
        assert_eq!(events[0].data["what"], "notification_not_shown");
        assert_eq!(
            events[0].data["reason"],
            "`osascript` ended with exit code 1: no"
        );
        assert!(out.contains("warning"), "{out}");
    }

    #[test]
    fn a_link_that_is_missing_or_unfit_to_show_is_left_out() {
        let long = format!("https://linear.app/{}", "a".repeat(LINK_MAX));
        for url in [
            None,
            Some("http://linear.app/x"),
            Some("https://linear.app/x y"),
            Some("https://linear.app/x\u{1b}[2J"),
            Some("https://linear.app/<b>x</b>"),
            Some("https://linear.app/?a=1&b=2"),
            Some("https://linear.app/caf\u{e9}"),
            Some(long.as_str()),
        ] {
            let recording = Recording::default();
            let stop = Stop::UsageLimit { resets_at: None };
            notify(Some(&recording), url, &Err(stop));
            assert_eq!(
                recording.0.into_inner(),
                ["OWL-7: stopped at the harness's usage limit"],
                "{url:?}"
            );
        }
    }

    const MAC_COMMAND: &str = "osascript -e on run argv -e display notification (item 2 of \
                               argv) with title (item 1 of argv) -e end run -- Owlshift \
                               OWL-7 \"it\" $HOME";

    #[test]
    fn macos_shows_the_line_as_an_argument_of_osascript() {
        let line = "OWL-7 \"it\" $HOME";
        let system = FakeSystem::default().answer(MAC_COMMAND, Answer::Exit(0, "", ""));
        let notifier = DesktopNotifier::new(&system, Os::MacOs, false).unwrap();
        assert_eq!(notifier.notify(line), Ok(()));

        let refused = FakeSystem::default().answer(
            MAC_COMMAND,
            Answer::Exit(1, "", "  execution error: Not authorized (-1743)\n"),
        );
        let notifier = DesktopNotifier::new(&refused, Os::MacOs, false).unwrap();
        assert_eq!(
            notifier.notify(line).unwrap_err().message,
            "`osascript` ended with exit code 1: execution error: Not authorized (-1743)"
        );

        let hung = FakeSystem::default().answer(MAC_COMMAND, Answer::TimedOut);
        let notifier = DesktopNotifier::new(&hung, Os::MacOs, false).unwrap();
        assert_eq!(
            notifier.notify(line).unwrap_err().message,
            "`osascript`: it did not answer in time"
        );
    }

    #[test]
    fn linux_shows_the_line_through_notify_send_in_a_desktop_session() {
        let system = FakeSystem::default()
            .install("notify-send")
            .answer("notify-send Owlshift OWL-7 waits", Answer::Exit(0, "", ""));
        let notifier = DesktopNotifier::new(&system, Os::Linux, true).unwrap();
        assert_eq!(notifier.notify("OWL-7 waits"), Ok(()));

        let missing = FakeSystem::default();
        let notifier = DesktopNotifier::new(&missing, Os::Linux, true).unwrap();
        let error = notifier.notify("OWL-7 waits").unwrap_err().message;
        assert!(error.contains("`desktop = false`"), "{error}");

        // Without a desktop session, or on another system, there is no
        // notifier at all.
        assert!(DesktopNotifier::new(&missing, Os::Linux, false).is_none());
        assert!(DesktopNotifier::new(&missing, Os::Other, true).is_none());
    }

    #[test]
    fn a_long_error_output_is_cut_on_a_character_boundary() {
        // The fake answers with static text only.
        let stderr: &'static str = Box::leak("é".repeat(STDERR_MAX).into_boxed_str());
        let system = FakeSystem::default()
            .install("notify-send")
            .answer("notify-send Owlshift x", Answer::Exit(2, "", stderr));
        let notifier = DesktopNotifier::new(&system, Os::Linux, true).unwrap();
        let error = notifier.notify("x").unwrap_err().message;
        let kept = error
            .strip_prefix("`notify-send` ended with exit code 2: ")
            .unwrap();
        assert_eq!(kept, "é".repeat(STDERR_MAX / 2));
    }
}
