//! The machine's usage state (OWL-404; architecture section 5, "Parallel",
//! "Usage limits"): one file per harness, `<data>/usage/<harness>.json`
//! ([`HarnessUsage`]), shared by every project's commands on this machine,
//! written under a lock of its own, `<harness>.lock`, and replaced whole, so
//! a read without the lock sees one write or the next. The rules are the
//! core's ([`owlshift_core::usage`]); [`crate::on_demand`] checks a harness
//! before each run and records what the run reported, and
//! [`crate::watch`] reads it at each pass.
//!
//! A file that cannot be read or locked holds nothing: a `warning` event,
//! once per process and reason, and runs go on, since the provider still
//! refuses a run at its limit, which spends nothing. A file written by a
//! newer Owlshift is such a file.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use jiff::Timestamp;

use owlshift_adapters::harness::claude::UsageWindow;
use owlshift_contracts::config::PersonalConfig;
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::usage::HarnessUsage;
use owlshift_core::usage::{self, Episode, EpisodeKind, Reported, Standing, State, Window};
use owlshift_core::vocab::Harness;

use crate::executor::{Outcome, RunReport};
use crate::project;
use crate::step;

/// How long a command waits for a harness's usage lock: every holder only
/// reads, decides and writes one small file.
const LOCK_WAIT: Duration = Duration::from_secs(10);

/// How often the lock is tried again meanwhile.
const LOCK_POLL: Duration = Duration::from_millis(50);

/// The usage files of this machine.
#[derive(Debug)]
pub struct UsageStore {
    dir: PathBuf,
    /// Each harness's `usage_cap_percent`, from the personal file: `None`
    /// when this store was not told of the caps, which then applies none
    /// and drops no pause a cap opened (OWL-405).
    caps: Option<BTreeMap<Harness, u8>>,
    /// The warnings given, once per process.
    warned: Mutex<BTreeSet<String>>,
}

impl UsageStore {
    /// The files in `dir`.
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            caps: None,
            warned: Mutex::default(),
        }
    }

    /// This store told of the caps of the personal file, a harness without
    /// one having none (OWL-405).
    #[must_use]
    pub fn with_caps(mut self, caps: impl IntoIterator<Item = (Harness, u8)>) -> Self {
        self.caps = Some(caps.into_iter().collect());
        self
    }

    /// `harness`'s cap, when this store was told of the caps and it has one.
    pub fn cap(&self, harness: Harness) -> Option<u8> {
        self.caps.as_ref()?.get(&harness).copied()
    }

    /// `harness`'s state at `now_ms` with the cap's pauses applied: what a
    /// run about to start, watch, `why` and `doctor` all see. Not written.
    pub fn read_at(&self, harness: Harness, now_ms: i64) -> Result<State, String> {
        let mut state = self.read(harness)?;
        self.apply_cap(harness, &mut state, now_ms);
        Ok(state)
    }

    /// `harness`'s standing at `now_ms`, the cap's pauses applied.
    pub fn standing(&self, harness: Harness, now_ms: i64) -> Result<Standing, String> {
        self.read_at(harness, now_ms)
            .map(|state| usage::standing(&state, now_ms))
    }

    /// [`Self::update`] with the cap's pauses applied before `change` and
    /// after it, so that what a run just reported counts; the pauses opened
    /// come with `change`'s answer.
    pub fn update_at<R>(
        &self,
        harness: Harness,
        now_ms: i64,
        change: impl FnOnce(&mut State) -> R,
    ) -> Result<(R, Vec<Episode>), String> {
        self.update(harness, |state| {
            let mut opened = self.apply_cap(harness, state, now_ms);
            let answer = change(state);
            opened.extend(self.apply_cap(harness, state, now_ms));
            (answer, opened)
        })
    }

    fn apply_cap(&self, harness: Harness, state: &mut State, now_ms: i64) -> Vec<Episode> {
        match &self.caps {
            Some(caps) => usage::apply_cap(state, caps.get(&harness).copied(), now_ms),
            None => Vec::new(),
        }
    }

    /// The files of the data directory `data`: `<data>/usage`, the root of
    /// the data directory being the machine's state (architecture section 5,
    /// `<state>`).
    pub fn in_data_dir(data: &Path) -> Self {
        Self::new(data.join("usage"))
    }

    /// The usage file of `harness`.
    pub fn file(&self, harness: Harness) -> PathBuf {
        self.dir.join(format!("{}.json", key(harness)))
    }

    fn lock_file(&self, harness: Harness) -> PathBuf {
        self.dir.join(format!("{}.lock", key(harness)))
    }

    /// What `harness`'s file holds, without its lock: an empty state when
    /// there is none.
    pub fn read(&self, harness: Harness) -> Result<State, String> {
        let path = self.file(harness);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(State::default());
            }
            Err(error) => return Err(format!("{}: {error}", path.display())),
        };
        let usage = HarnessUsage::parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        if usage.harness != harness {
            return Err(format!(
                "{} holds the usage of another harness",
                path.display()
            ));
        }
        Ok(usage.state())
    }

    /// Applies `change` to `harness`'s state under its lock and keeps the
    /// result, written whole when it changed; `change`'s answer.
    pub fn update<R>(
        &self,
        harness: Harness,
        change: impl FnOnce(&mut State) -> R,
    ) -> Result<R, String> {
        fs::create_dir_all(&self.dir).map_err(|e| format!("{}: {e}", self.dir.display()))?;
        let lock_path = self.lock_file(harness);
        let _lock = project::lock_file_within(&lock_path, LOCK_WAIT, LOCK_POLL)
            .map_err(|e| format!("{}: {e}", lock_path.display()))?
            .ok_or_else(|| {
                format!(
                    "{} stayed held for {} seconds",
                    lock_path.display(),
                    LOCK_WAIT.as_secs()
                )
            })?;
        let mut state = self.read(harness)?;
        let before = state.clone();
        let answer = change(&mut state);
        if state != before {
            let path = self.file(harness);
            step::write_atomically(&path, &HarnessUsage::from_state(harness, &state).render())
                .map_err(|e| format!("{}: {e}", path.display()))?;
        }
        Ok(answer)
    }

    /// Whether `warning` is new to this process, which then remembers it.
    pub(crate) fn first_time(&self, warning: String) -> bool {
        self.warned
            .lock()
            .map(|mut warned| warned.insert(warning))
            .unwrap_or(true)
    }
}

/// The name of `harness`'s files, as the personal file names it.
pub fn key(harness: Harness) -> &'static str {
    match harness {
        Harness::Claude => "claude",
        Harness::Codex => "codex",
    }
}

/// `harness` as a person reads it.
pub fn shown(harness: Harness) -> &'static str {
    match harness {
        Harness::Claude => "Claude Code",
        Harness::Codex => "Codex",
    }
}

/// `at` in milliseconds since the epoch, as the core counts.
pub fn ms(at: Timestamp) -> i64 {
    at.as_millisecond()
}

/// The instant `ms` milliseconds after the epoch.
pub fn at(ms: i64) -> Timestamp {
    Timestamp::from_millisecond(ms).unwrap_or(if ms < 0 {
        Timestamp::MIN
    } else {
        Timestamp::MAX
    })
}

/// What a finished run reported of its harness, for [`owlshift_core::usage::record`]:
/// its windows, its limit, and whether it reported anything of its harness
/// at all, a window or its usage.
pub fn reported(report: &RunReport) -> Reported {
    Reported {
        windows: report.windows.iter().map(window).collect(),
        limit: match &report.outcome {
            Outcome::UsageLimit { resets_at } => Some(resets_at.map(ms)),
            _ => None,
        },
        answered: !report.windows.is_empty() || report.usage.is_some(),
    }
}

fn window(window: &UsageWindow) -> (String, Window) {
    (
        window.name.clone(),
        Window {
            share: window.share,
            resets_at_ms: ms(window.resets_at),
            read_at_ms: ms(window.read_at),
        },
    )
}

/// The line that tells the operator that `harness`'s runs wait, `ticket`'s
/// first, until `until`; `reset` when `until` is the reset time the harness
/// reported, a deadline otherwise. Fixed words, the ticket and a time only.
pub fn line(ticket: &TicketId, harness: Harness, until: Timestamp, reset: bool) -> String {
    let harness = shown(harness);
    if reset {
        format!("{ticket}: {harness} is at its usage limit until {until}: its runs wait until then")
    } else {
        format!(
            "{ticket}: {harness} is at its usage limit and gave no reset time: its runs wait, and \
             one tries it again at {until}"
        )
    }
}

/// [`line()`] for `episode`.
pub fn episode_line(ticket: &TicketId, harness: Harness, episode: &Episode) -> String {
    if matches!(episode.kind, EpisodeKind::Cap { .. }) {
        return cap_line(ticket, harness, at(episode.until_ms));
    }
    let reset = !matches!(episode.kind, EpisodeKind::Limit { reset: false });
    line(ticket, harness, at(episode.until_ms), reset)
}

/// The line that tells the operator that `harness`'s runs wait for a usage
/// window to reset at `until`, the cap the personal file sets reached
/// (OWL-405). Fixed words, the ticket and a time only: the window's name is
/// the provider's.
pub fn cap_line(ticket: &TicketId, harness: Harness, until: Timestamp) -> String {
    format!(
        "{ticket}: {} reached its usage cap: its runs wait until its window resets at {until}",
        shown(harness)
    )
}

/// The usage caps of the personal file, each harness's
/// `usage_cap_percent`, for [`UsageStore::with_caps`] (OWL-405).
pub fn caps_of(personal: &crate::config::FileState<PersonalConfig>) -> Vec<(Harness, u8)> {
    match personal {
        crate::config::FileState::Loaded { config, .. } => config
            .harnesses
            .iter()
            .filter_map(|(harness, settings)| Some((harness, settings.usage_cap_percent?)))
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use owlshift_core::usage::{self, Admission, Recorded};

    #[test]
    fn a_state_is_kept_whole_under_its_lock_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let store = UsageStore::in_data_dir(dir.path());
        assert_eq!(store.read(Harness::Claude).unwrap(), State::default());
        let reported = Reported {
            limit: Some(None),
            answered: true,
            ..Reported::default()
        };
        let recorded = store
            .update(Harness::Claude, |state| {
                usage::record(state, &reported, "r1", 1_000)
            })
            .unwrap();
        assert!(matches!(recorded, Recorded::Limited(_)), "{recorded:?}");
        let file = dir.path().join("usage").join("claude.json");
        assert!(file.is_file());
        assert!(
            !dir.path()
                .join("usage")
                .join("claude.json.partial")
                .exists()
        );
        let state = store.read(Harness::Claude).unwrap();
        assert_eq!(state.episodes.len(), 1);
        assert_eq!(
            store
                .update(Harness::Claude, |state| usage::admit(state, "r2", 2_000))
                .unwrap(),
            Admission::Wait {
                until_ms: 1_000 + usage::DEADLINE_MS
            }
        );
        // Codex's file is its own.
        assert_eq!(store.read(Harness::Codex).unwrap(), State::default());
    }

    #[test]
    fn a_file_that_does_not_read_or_names_another_harness_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store = UsageStore::in_data_dir(dir.path());
        fs::create_dir_all(dir.path().join("usage")).unwrap();
        fs::write(store.file(Harness::Claude), "{ not json").unwrap();
        assert!(store.read(Harness::Claude).is_err());
        assert!(store.update(Harness::Claude, |_| ()).is_err());
        let codex = HarnessUsage::from_state(Harness::Codex, &State::default()).render();
        fs::write(store.file(Harness::Claude), codex).unwrap();
        let error = store.read(Harness::Claude).unwrap_err();
        assert!(error.contains("another harness"), "{error}");
    }

    #[test]
    fn a_held_lock_is_waited_for_then_reported() {
        let dir = tempfile::tempdir().unwrap();
        let store = UsageStore::in_data_dir(dir.path());
        fs::create_dir_all(dir.path().join("usage")).unwrap();
        let held = project::lock_file_within(
            &store.lock_file(Harness::Claude),
            Duration::ZERO,
            Duration::ZERO,
        )
        .unwrap()
        .unwrap();
        let waited = project::lock_file_within(
            &store.lock_file(Harness::Claude),
            Duration::from_millis(100),
            Duration::from_millis(20),
        );
        assert!(matches!(waited, Ok(None)), "{waited:?}");
        drop(held);
        assert!(store.update(Harness::Claude, |_| ()).is_ok());
    }

    #[test]
    fn a_warning_is_given_once_per_process() {
        let store = UsageStore::new(PathBuf::from("/nowhere"));
        assert!(store.first_time("usage_unread claude x".to_owned()));
        assert!(!store.first_time("usage_unread claude x".to_owned()));
        assert!(store.first_time("usage_unread claude y".to_owned()));
    }

    #[test]
    fn the_operator_reads_the_harness_and_when_it_tries_again() {
        let ticket = TicketId::new("DEMO-1").unwrap();
        let until: Timestamp = "2026-10-20T14:00:00Z".parse().unwrap();
        assert_eq!(
            line(&ticket, Harness::Claude, until, true),
            "DEMO-1: Claude Code is at its usage limit until 2026-10-20T14:00:00Z: its runs \
             wait until then"
        );
        assert!(line(&ticket, Harness::Codex, until, false).ends_with(
            "gave no reset time: its runs wait, and one tries it again at 2026-10-20T14:00:00Z"
        ));
    }
}
