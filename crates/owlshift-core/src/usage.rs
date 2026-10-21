//! A harness's usage state, machine-wide (OWL-404; architecture section 5,
//! "Parallel", "Usage limits"): the usage windows its runs report, its limit
//! episodes and the probe after a deadline, and from them whether a run may
//! start on it. The runner keeps one [`State`] per harness in a file of its
//! own, read and written under a lock of its own; this module only decides.
//! Times are milliseconds since the Unix epoch.
//!
//! - *Windows.* Each report of a window replaces the one kept only when its
//!   reset is later, or the same and read later ([`Window::replaces`]).
//! - *Episodes.* A run that ends at the provider's limit opens one, until the
//!   reset time it reports, or, without one, until a deadline [`DEADLINE_MS`]
//!   after it was seen; a reset time already past holds nothing. A limit seen
//!   again continues the episode, one outage told once ([`claim_told`]): while
//!   a reset stands or within [`DEADLINE_MS`] of it, and always after a
//!   deadline, which no run since proved over. A cap's pause
//!   ([`EpisodeKind::Cap`]) holds the harness until its window's reset;
//!   [`apply_cap`] opens those (OWL-405).
//! - *The probe.* Once a deadline without a reset time passes, the first run
//!   admitted marks itself the probe, with a deadline of its own, and runs;
//!   every other run waits until it reports or its deadline passes. A probe
//!   that meets the limit continues the episode, one that ran ends it, one
//!   whose run reported nothing of its harness proves nothing and lets the
//!   next run probe.

use std::collections::BTreeMap;

use crate::vocab::{Harness, Role};

/// How long a limit without a reset time holds its harness before a probe
/// runs, and how long a probe holds every other run before the next one
/// probes: 30 minutes. A run that meets its limit is refused at once and
/// spends nothing, so a second probe while a long first one runs costs at
/// most one refused run.
pub const DEADLINE_MS: i64 = 30 * 60 * 1000;

/// One usage window as a run's report gave it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Window {
    /// The share of the window used, from 0 to 1.
    pub share: f64,
    /// When the window resets.
    pub resets_at_ms: i64,
    /// When the report was read from the run's stream, not when the run
    /// ended; compared only with another report's.
    pub read_at_ms: i64,
}

impl Window {
    /// Whether this report replaces `kept`, the one held for the same
    /// window: its reset is later, or the same and it was read later.
    pub fn replaces(&self, kept: &Window) -> bool {
        self.resets_at_ms > kept.resets_at_ms
            || (self.resets_at_ms == kept.resets_at_ms && self.read_at_ms > kept.read_at_ms)
    }
}

/// What holds a harness.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EpisodeKind {
    /// The provider's usage limit, which a run of the harness ended at.
    /// `reset`: the episode's end is the reset time the harness reported;
    /// otherwise a deadline, after which a probe runs.
    Limit { reset: bool },
    /// A cap's pause: the latest share seen of `window` at or above the
    /// harness's `usage_cap_percent`, until that window's reset
    /// ([`apply_cap`]).
    Cap { window: String },
}

/// A time the harness is held.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Episode {
    pub kind: EpisodeKind,
    /// When the outage was first seen.
    pub since_ms: i64,
    /// Its end: the reset, or the deadline.
    pub until_ms: i64,
    /// The operator was notified of it.
    pub told: bool,
}

impl Episode {
    /// A limit without a reset time: past its deadline it stands until a
    /// probe ends it.
    fn awaits_probe(&self) -> bool {
        self.kind == EpisodeKind::Limit { reset: false }
    }

    fn is_limit(&self) -> bool {
        matches!(self.kind, EpisodeKind::Limit { .. })
    }

    /// Whether it holds the harness at `now`, a probe aside.
    fn stands(&self, now: i64) -> bool {
        now < self.until_ms || self.awaits_probe()
    }
}

/// The run that probes a harness past a deadline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Probe {
    /// The token the run marked itself with.
    pub id: String,
    /// Until when every other run waits for it.
    pub deadline_ms: i64,
}

/// One harness's usage state.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct State {
    /// The windows its runs reported, by name (`five_hour`, `seven_day`).
    pub windows: BTreeMap<String, Window>,
    pub episodes: Vec<Episode>,
    pub probe: Option<Probe>,
}

/// Whether a run may start on a harness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Standing {
    Free,
    /// A deadline without a reset time passed: the next run probes.
    ProbeDue,
    /// Held until then: the latest end standing, or a probe's deadline.
    Limited {
        until_ms: i64,
    },
}

/// Whether a run may start on the harness of `state` at `now`.
pub fn standing(state: &State, now: i64) -> Standing {
    let mut until: Option<i64> = None;
    let mut probe_due = false;
    for episode in &state.episodes {
        if now < episode.until_ms {
            until = until.max(Some(episode.until_ms));
        } else if episode.awaits_probe() {
            match &state.probe {
                Some(probe) if now < probe.deadline_ms => {
                    until = until.max(Some(probe.deadline_ms));
                }
                _ => probe_due = true,
            }
        }
    }
    match until {
        Some(until_ms) => Standing::Limited { until_ms },
        None if probe_due => Standing::ProbeDue,
        None => Standing::Free,
    }
}

/// What a run about to start on a harness does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    Run,
    /// It runs as the probe: [`admit`] marked it.
    Probe,
    /// It does not start: the harness is held until then.
    Wait {
        until_ms: i64,
    },
}

/// Admits a run about to start on the harness of `state` at `now`, its
/// token `probe_id`: a run past a deadline marks itself the probe.
pub fn admit(state: &mut State, probe_id: &str, now: i64) -> Admission {
    prune(state, now);
    match standing(state, now) {
        Standing::Free => Admission::Run,
        Standing::ProbeDue => {
            state.probe = Some(Probe {
                id: probe_id.to_owned(),
                deadline_ms: now.saturating_add(DEADLINE_MS),
            });
            Admission::Probe
        }
        Standing::Limited { until_ms } => Admission::Wait { until_ms },
    }
}

/// What a finished run of the harness reported.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Reported {
    /// Its usage windows, by name.
    pub windows: Vec<(String, Window)>,
    /// It ended at the provider's limit: `Some` with the reset time it
    /// reported, if any.
    pub limit: Option<Option<i64>>,
    /// It reported anything of its harness, a window or its usage: a probe
    /// that reported nothing, its setup failing first or its harness never
    /// reaching the provider, proves nothing.
    pub answered: bool,
}

/// What [`record`] changed of the episodes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Recorded {
    /// Nothing.
    Nothing,
    /// The run met the limit: the episode that holds the harness now.
    Limited(Episode),
    /// The probe ran: its episode ended.
    Freed,
    /// The probe reported nothing of its harness: its mark is gone, and the
    /// next run probes.
    Unproven,
}

/// Whether `share` (0 to 1) is at or above a cap of `cap` percent: a share
/// of 0.29 holds a cap of 29 whatever the float says.
pub fn at_cap(share: f64, cap: u8) -> bool {
    share * 100.0 + 1e-6 >= f64::from(cap)
}

/// Opens, keeps or drops the pauses of a usage cap (OWL-405) at `now`: each
/// window whose latest share is at or above `cap` and has not reset holds the
/// harness until its reset, one pause per window. A pause whose window no
/// longer stands at the cap, or that no cap justifies any more, is dropped.
/// A window that reset and reaches the cap again in its next cycle gets a
/// new, untold pause. Returns the pauses opened.
pub fn apply_cap(state: &mut State, cap: Option<u8>, now: i64) -> Vec<Episode> {
    let at = |name: &str, windows: &BTreeMap<String, Window>| {
        cap.is_some_and(|cap| windows.get(name).is_some_and(|w| at_cap(w.share, cap)))
    };
    let windows = &state.windows;
    state.episodes.retain(|episode| match &episode.kind {
        EpisodeKind::Cap { window } => at(window, windows),
        EpisodeKind::Limit { .. } => true,
    });
    let mut opened = Vec::new();
    for (name, window) in &state.windows {
        if !at(name, &state.windows) || window.resets_at_ms <= now {
            continue;
        }
        let held = state.episodes.iter().any(|episode| {
            matches!(&episode.kind, EpisodeKind::Cap { window: w } if w == name)
                && episode.until_ms == window.resets_at_ms
        });
        if held {
            continue;
        }
        // Another cycle of the window: its old pause is over.
        state.episodes.retain(
            |episode| !matches!(&episode.kind, EpisodeKind::Cap { window: w } if w == name),
        );
        let episode = Episode {
            kind: EpisodeKind::Cap {
                window: name.clone(),
            },
            since_ms: now,
            until_ms: window.resets_at_ms,
            told: false,
        };
        state.episodes.push(episode.clone());
        opened.push(episode);
    }
    opened
}

/// Records what a run whose token is `probe_id` reported at `now`.
pub fn record(state: &mut State, reported: &Reported, probe_id: &str, now: i64) -> Recorded {
    for (name, window) in &reported.windows {
        match state.windows.get(name) {
            Some(kept) if !window.replaces(kept) => {}
            _ => {
                state.windows.insert(name.clone(), *window);
            }
        }
    }
    prune(state, now);
    let probe = state.probe.take_if(|probe| probe.id == probe_id);
    let Some(reset) = reported.limit else {
        return match probe {
            None => Recorded::Nothing,
            Some(_) if reported.answered => {
                state.episodes.retain(|episode| !episode.awaits_probe());
                Recorded::Freed
            }
            Some(_) => Recorded::Unproven,
        };
    };
    let (end, reset) = match reset {
        Some(at) => (at, true),
        None => (now.saturating_add(DEADLINE_MS), false),
    };
    let current = state.episodes.iter().position(Episode::is_limit);
    // The same outage: a limit without a reset time, which no run since
    // proved over, or a reset that stands or ended within a deadline.
    let continued = current.filter(|&at| {
        let episode = &state.episodes[at];
        episode.awaits_probe() || now < episode.until_ms.saturating_add(DEADLINE_MS)
    });
    let episode = match continued {
        Some(at) => {
            let episode = &mut state.episodes[at];
            if episode.until_ms <= now || end >= episode.until_ms {
                episode.until_ms = end;
                episode.kind = EpisodeKind::Limit { reset };
            }
            episode.clone()
        }
        None => {
            state.episodes.retain(|episode| !episode.is_limit());
            let episode = Episode {
                kind: EpisodeKind::Limit { reset },
                since_ms: now,
                until_ms: end,
                told: false,
            };
            state.episodes.push(episode.clone());
            episode
        }
    };
    Recorded::Limited(episode)
}

/// Clears the mark of the probe `probe_id`, whose run never started: the
/// next run probes.
pub fn unstarted(state: &mut State, probe_id: &str) {
    state.probe.take_if(|probe| probe.id == probe_id);
}

/// Marks every episode that holds the harness at `now` told, and returns
/// the one ending last when one of them was not: what the operator is told
/// of, once per outage. `None` when none holds it or all were told.
pub fn claim_told(state: &mut State, now: i64) -> Option<Episode> {
    let mut untold = false;
    for episode in state.episodes.iter_mut().filter(|e| e.stands(now)) {
        untold |= !episode.told;
        episode.told = true;
    }
    if !untold {
        return None;
    }
    state
        .episodes
        .iter()
        .filter(|e| e.stands(now))
        .max_by_key(|e| e.until_ms)
        .cloned()
}

/// Drops the episodes over at `now`: a reset or a cap's end past by a
/// deadline, so that a limit met just after its end continues it. A limit
/// without a reset time stands until a probe ends it.
fn prune(state: &mut State, now: i64) {
    state.episodes.retain(|episode| {
        episode.awaits_probe() || now < episode.until_ms.saturating_add(DEADLINE_MS)
    });
}

/// The harness a run of `role` uses when nothing is limited: Claude Code
/// for every role (D4). The opt-in Codex reviewer is a seat beside a pass,
/// never a role's own run; a review role on the fallback harness is
/// [`effective_harness`]'s.
pub fn own_harness(role: Role) -> Harness {
    let _ = role;
    Harness::Claude
}

/// The harness a run gets, as watch and the step's child both ask it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effective {
    Run(Harness),
    /// No harness: the run waits until then at least.
    Wait {
        until_ms: i64,
    },
}

/// Whether `role` is one of the two review roles, Design review and Verify:
/// the only ones a harness at its limit hands over (D4, OWL-406).
pub fn is_review_role(role: Role) -> bool {
    matches!(role, Role::DesignReview | Role::Verify)
}

/// The harness a run of `role`, on `own` when nothing is limited, gets,
/// given each harness's standing: its own when it may start, a probe
/// included. When `own` is limited, a review role gets `fallback` (the
/// harness the personal file names, `[harnesses.<name>].fallback`, which
/// the caller offers only when it is installed and logged in for agent runs)
/// while that one is not limited (a probe due is served by the run itself); otherwise none. No
/// other role falls back (OWL-406).
pub fn effective_harness(
    role: Role,
    own: Harness,
    fallback: Option<Harness>,
    standing_of: impl Fn(Harness) -> Standing,
) -> Effective {
    match standing_of(own) {
        Standing::Free | Standing::ProbeDue => Effective::Run(own),
        Standing::Limited { until_ms } => match fallback {
            Some(other)
                if other != own
                    && is_review_role(role)
                    && matches!(standing_of(other), Standing::Free | Standing::ProbeDue) =>
            {
                Effective::Run(other)
            }
            _ => Effective::Wait { until_ms },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: i64 = 60 * 1000;

    fn window(share: f64, resets: i64, read: i64) -> Window {
        Window {
            share,
            resets_at_ms: resets,
            read_at_ms: read,
        }
    }

    fn limit(reset: Option<i64>) -> Reported {
        Reported {
            limit: Some(reset),
            answered: true,
            ..Reported::default()
        }
    }

    fn ran() -> Reported {
        Reported {
            answered: true,
            ..Reported::default()
        }
    }

    #[test]
    fn a_window_report_replaces_the_kept_one_when_later() {
        let kept = window(0.4, 100, 10);
        assert!(window(0.1, 200, 5).replaces(&kept), "a later reset");
        assert!(
            window(0.5, 100, 11).replaces(&kept),
            "the same reset, read later"
        );
        assert!(
            !window(0.9, 100, 10).replaces(&kept),
            "read at the same time"
        );
        assert!(!window(0.9, 99, 20).replaces(&kept), "an earlier reset");

        let mut state = State::default();
        let reported = |windows: Vec<(&str, Window)>| Reported {
            windows: windows
                .into_iter()
                .map(|(name, w)| (name.to_owned(), w))
                .collect(),
            ..Reported::default()
        };
        record(
            &mut state,
            &reported(vec![
                ("five_hour", kept),
                ("seven_day", window(0.3, 900, 10)),
            ]),
            "",
            0,
        );
        record(
            &mut state,
            &reported(vec![("five_hour", window(0.2, 100, 9))]),
            "",
            0,
        );
        assert_eq!(state.windows["five_hour"], kept, "an older read is ignored");
        assert_eq!(state.windows["seven_day"].share, 0.3);
    }

    #[test]
    fn a_limit_with_its_reset_holds_until_then() {
        let mut state = State::default();
        let recorded = record(&mut state, &limit(Some(90 * MIN)), "r1", 10 * MIN);
        let Recorded::Limited(episode) = recorded else {
            panic!("{recorded:?}");
        };
        assert_eq!(episode.kind, EpisodeKind::Limit { reset: true });
        assert_eq!((episode.since_ms, episode.until_ms), (10 * MIN, 90 * MIN));
        assert_eq!(
            admit(&mut state, "r2", 89 * MIN),
            Admission::Wait { until_ms: 90 * MIN }
        );
        assert_eq!(admit(&mut state, "r3", 90 * MIN), Admission::Run);
        assert_eq!(standing(&state, 90 * MIN), Standing::Free);
    }

    #[test]
    fn a_reset_already_past_holds_nothing() {
        let mut state = State::default();
        record(&mut state, &limit(Some(5 * MIN)), "r1", 10 * MIN);
        assert_eq!(state.episodes[0].kind, EpisodeKind::Limit { reset: true });
        assert_eq!(standing(&state, 10 * MIN), Standing::Free);
        assert_eq!(claim_told(&mut state, 10 * MIN), None);
    }

    #[test]
    fn a_limit_without_a_reset_is_probed_once_after_its_deadline() {
        let mut state = State::default();
        record(&mut state, &limit(None), "r1", 0);
        assert_eq!(
            admit(&mut state, "early", 29 * MIN),
            Admission::Wait { until_ms: 30 * MIN }
        );
        // The first run past the deadline probes; every other waits for it.
        assert_eq!(standing(&state, 30 * MIN), Standing::ProbeDue);
        assert_eq!(admit(&mut state, "probe", 30 * MIN), Admission::Probe);
        assert_eq!(
            admit(&mut state, "other", 31 * MIN),
            Admission::Wait { until_ms: 60 * MIN }
        );
        // A run that is not the probe ends nothing.
        assert_eq!(
            record(&mut state, &ran(), "other", 32 * MIN),
            Recorded::Nothing
        );
        assert_eq!(
            standing(&state, 32 * MIN),
            Standing::Limited { until_ms: 60 * MIN }
        );
        // The probe ran: the episode is over.
        assert_eq!(
            record(&mut state, &ran(), "probe", 40 * MIN),
            Recorded::Freed
        );
        assert_eq!(state.episodes, Vec::new());
        assert_eq!(state.probe, None);
        assert_eq!(admit(&mut state, "next", 40 * MIN), Admission::Run);
    }

    #[test]
    fn a_probe_that_dies_or_outlives_its_deadline_lets_the_next_run_probe() {
        let mut state = State::default();
        record(&mut state, &limit(None), "r1", 0);
        assert_eq!(admit(&mut state, "dies", 30 * MIN), Admission::Probe);
        assert_eq!(admit(&mut state, "next", 60 * MIN), Admission::Probe);
        assert_eq!(state.probe.as_ref().unwrap().id, "next");
        // The first probe reporting late changes nothing: it is no longer
        // the probe.
        assert_eq!(
            record(&mut state, &ran(), "dies", 61 * MIN),
            Recorded::Nothing
        );
        assert_eq!(state.episodes.len(), 1);
    }

    #[test]
    fn a_probe_that_reports_nothing_or_never_starts_proves_nothing() {
        let mut state = State::default();
        record(&mut state, &limit(None), "r1", 0);
        assert_eq!(admit(&mut state, "p1", 30 * MIN), Admission::Probe);
        let silent = Reported::default();
        assert_eq!(
            record(&mut state, &silent, "p1", 31 * MIN),
            Recorded::Unproven
        );
        assert_eq!(state.episodes.len(), 1);
        assert_eq!(admit(&mut state, "p2", 31 * MIN), Admission::Probe);
        unstarted(&mut state, "p2");
        assert_eq!(state.probe, None);
        assert_eq!(standing(&state, 31 * MIN), Standing::ProbeDue);
    }

    #[test]
    fn a_probe_that_meets_the_limit_continues_the_outage_told_once() {
        let mut state = State::default();
        record(&mut state, &limit(None), "r1", 0);
        assert!(claim_told(&mut state, MIN).is_some());
        assert_eq!(claim_told(&mut state, 2 * MIN), None, "told once");
        assert_eq!(admit(&mut state, "probe", 30 * MIN), Admission::Probe);
        let Recorded::Limited(episode) = record(&mut state, &limit(None), "probe", 31 * MIN) else {
            panic!("{state:?}");
        };
        assert_eq!((episode.since_ms, episode.until_ms), (0, 61 * MIN));
        assert!(episode.told);
        assert_eq!(state.probe, None);
        assert_eq!(claim_told(&mut state, 32 * MIN), None);
        // With a reset time this time.
        assert_eq!(admit(&mut state, "p2", 61 * MIN), Admission::Probe);
        record(&mut state, &limit(Some(300 * MIN)), "p2", 62 * MIN);
        assert_eq!(
            state.episodes[0],
            Episode {
                kind: EpisodeKind::Limit { reset: true },
                since_ms: 0,
                until_ms: 300 * MIN,
                told: true,
            }
        );
    }

    #[test]
    fn a_limit_seen_again_extends_the_episode_and_a_later_one_is_new() {
        let mut state = State::default();
        record(&mut state, &limit(Some(60 * MIN)), "r1", 0);
        claim_told(&mut state, 0);
        // A run started before met it too, with a later reset.
        record(&mut state, &limit(Some(90 * MIN)), "r2", MIN);
        assert_eq!(state.episodes.len(), 1);
        assert_eq!(state.episodes[0].until_ms, 90 * MIN);
        assert!(state.episodes[0].told);
        // An earlier reset leaves the end as it is.
        record(&mut state, &limit(Some(70 * MIN)), "r3", 2 * MIN);
        assert_eq!(state.episodes[0].until_ms, 90 * MIN);
        // Just after the reset: the same outage.
        record(&mut state, &limit(Some(200 * MIN)), "r4", 100 * MIN);
        assert!(state.episodes[0].told);
        assert_eq!(state.episodes[0].since_ms, 0);
        // Long after: a new outage, told anew.
        let Recorded::Limited(new) = record(&mut state, &limit(None), "r5", 500 * MIN) else {
            panic!("{state:?}");
        };
        assert_eq!((new.since_ms, new.told), (500 * MIN, false));
        assert_eq!(state.episodes, vec![new]);
    }

    #[test]
    fn a_deadline_left_unprobed_continues_its_outage_and_a_reset_long_past_does_not() {
        let mut state = State::default();
        record(&mut state, &limit(None), "r1", 0);
        claim_told(&mut state, 0);
        // Nothing ran for a day; the first run probes and meets the limit:
        // nothing proved the outage over.
        assert_eq!(admit(&mut state, "probe", 1440 * MIN), Admission::Probe);
        let Recorded::Limited(episode) = record(&mut state, &limit(None), "probe", 1441 * MIN)
        else {
            panic!("{state:?}");
        };
        assert_eq!((episode.since_ms, episode.told), (0, true));
        // The probe runs; a limit long after is a new outage.
        admit(&mut state, "p2", 1471 * MIN);
        assert_eq!(
            record(&mut state, &ran(), "p2", 1472 * MIN),
            Recorded::Freed
        );
        record(&mut state, &limit(Some(1600 * MIN)), "r2", 1500 * MIN);
        claim_told(&mut state, 1500 * MIN);
        let Recorded::Limited(new) = record(&mut state, &limit(None), "r3", 1700 * MIN) else {
            panic!("{state:?}");
        };
        assert_eq!((new.since_ms, new.told), (1600 * MIN + 100 * MIN, false));
    }

    #[test]
    fn a_cap_holds_its_harness_until_its_window_resets() {
        let mut state = State {
            episodes: vec![Episode {
                kind: EpisodeKind::Cap {
                    window: "five_hour".to_owned(),
                },
                since_ms: 0,
                until_ms: 50 * MIN,
                told: false,
            }],
            ..State::default()
        };
        assert_eq!(
            standing(&state, MIN),
            Standing::Limited { until_ms: 50 * MIN }
        );
        assert_eq!(
            claim_told(&mut state, MIN).map(|e| e.until_ms),
            Some(50 * MIN)
        );
        // A limit beside it: held until the later end.
        record(&mut state, &limit(Some(80 * MIN)), "r1", 2 * MIN);
        assert_eq!(
            standing(&state, 60 * MIN),
            Standing::Limited { until_ms: 80 * MIN }
        );
        assert_eq!(admit(&mut state, "r2", 80 * MIN), Admission::Run);
        // Pruned once past a deadline after its end.
        admit(&mut state, "r3", 111 * MIN);
        assert_eq!(state.episodes, Vec::new());
    }

    #[test]
    fn every_role_runs_on_its_own_harness_unless_it_is_held() {
        let free = |_| Standing::Free;
        let held = |harness| match harness {
            Harness::Claude => Standing::Limited { until_ms: 7 },
            Harness::Codex => Standing::Free,
        };
        let all = [
            Role::Intake,
            Role::Resolver,
            Role::AnswerCheck,
            Role::Design,
            Role::DesignReview,
            Role::Build,
            Role::Verify,
            Role::Rebase,
            Role::Arbiter,
        ];
        for role in all {
            assert_eq!(own_harness(role), Harness::Claude);
            for fallback in [None, Some(Harness::Codex)] {
                assert_eq!(
                    effective_harness(role, own_harness(role), fallback, free),
                    Effective::Run(Harness::Claude)
                );
            }
            // No fallback offered: every role waits, even with Codex free.
            assert_eq!(
                effective_harness(role, Harness::Claude, None, held),
                Effective::Wait { until_ms: 7 }
            );
        }
        let due = |_| Standing::ProbeDue;
        assert_eq!(
            effective_harness(Role::Build, Harness::Codex, None, due),
            Effective::Run(Harness::Codex)
        );
    }

    #[test]
    fn only_the_review_roles_fall_back_and_only_to_a_free_harness() {
        let codex = Some(Harness::Codex);
        let held = |harness| match harness {
            Harness::Claude => Standing::Limited { until_ms: 7 },
            Harness::Codex => Standing::Free,
        };
        for role in [Role::DesignReview, Role::Verify] {
            assert!(is_review_role(role));
            assert_eq!(
                effective_harness(role, Harness::Claude, codex, held),
                Effective::Run(Harness::Codex)
            );
        }
        for role in [
            Role::Intake,
            Role::Resolver,
            Role::AnswerCheck,
            Role::Design,
            Role::Build,
            Role::Rebase,
            Role::Arbiter,
        ] {
            assert!(!is_review_role(role));
            assert_eq!(
                effective_harness(role, Harness::Claude, codex, held),
                Effective::Wait { until_ms: 7 },
                "{role:?}"
            );
        }
        // The fallback held too: the review waits for its own harness.
        let both = |harness| match harness {
            Harness::Claude => Standing::Limited { until_ms: 7 },
            Harness::Codex => Standing::Limited { until_ms: 9 },
        };
        assert_eq!(
            effective_harness(Role::Verify, Harness::Claude, codex, both),
            Effective::Wait { until_ms: 7 }
        );
        // Only due a probe: the review's run is that probe, or the fallback
        // would never be tried again after an outage without a reset time.
        let due = |harness| match harness {
            Harness::Claude => Standing::Limited { until_ms: 7 },
            Harness::Codex => Standing::ProbeDue,
        };
        assert_eq!(
            effective_harness(Role::Verify, Harness::Claude, codex, due),
            Effective::Run(Harness::Codex)
        );
        // A harness never falls back to itself.
        assert_eq!(
            effective_harness(Role::Verify, Harness::Claude, Some(Harness::Claude), held),
            Effective::Wait { until_ms: 7 }
        );
    }

    fn with_windows(windows: &[(&str, Window)]) -> State {
        State {
            windows: windows
                .iter()
                .map(|(name, w)| ((*name).to_owned(), *w))
                .collect(),
            ..State::default()
        }
    }

    #[test]
    fn a_window_at_the_cap_holds_the_harness_until_its_reset() {
        let mut state = with_windows(&[
            ("five_hour", window(0.8, 50 * MIN, 1)),
            ("seven_day", window(0.3, 900 * MIN, 1)),
        ]);
        assert!(at_cap(0.29, 29) && at_cap(0.8, 80) && !at_cap(0.799, 80));
        assert!(apply_cap(&mut state, None, MIN).is_empty());
        assert_eq!(standing(&state, MIN), Standing::Free);
        let opened = apply_cap(&mut state, Some(80), MIN);
        assert_eq!(opened.len(), 1, "the seven-day share is below the cap");
        assert_eq!(
            opened[0].kind,
            EpisodeKind::Cap {
                window: "five_hour".to_owned()
            }
        );
        assert_eq!(
            standing(&state, 49 * MIN),
            Standing::Limited { until_ms: 50 * MIN }
        );
        // Applied again, it is the same pause, told mark kept.
        claim_told(&mut state, 2 * MIN);
        assert!(apply_cap(&mut state, Some(80), 3 * MIN).is_empty());
        assert!(state.episodes[0].told);
        assert_eq!(admit(&mut state, "r", 50 * MIN), Admission::Run);
    }

    #[test]
    fn a_pause_ends_with_its_cap_or_its_share_and_a_new_cycle_opens_a_new_one() {
        let mut state = with_windows(&[("five_hour", window(0.9, 50 * MIN, 1))]);
        apply_cap(&mut state, Some(80), MIN);
        claim_told(&mut state, MIN);
        // A higher cap, or none, ends it.
        let mut raised = state.clone();
        apply_cap(&mut raised, Some(95), 2 * MIN);
        assert_eq!(standing(&raised, 2 * MIN), Standing::Free);
        let mut none = state.clone();
        apply_cap(&mut none, None, 2 * MIN);
        assert_eq!(standing(&none, 2 * MIN), Standing::Free);
        // A window already reset opens nothing.
        let mut past = with_windows(&[("five_hour", window(0.9, 50 * MIN, 1))]);
        assert!(apply_cap(&mut past, Some(80), 60 * MIN).is_empty());
        // The next cycle of the window, again at the cap: a new, untold pause.
        record(
            &mut state,
            &Reported {
                windows: vec![("five_hour".to_owned(), window(0.85, 350 * MIN, 60 * MIN))],
                ..Reported::default()
            },
            "",
            60 * MIN,
        );
        let opened = apply_cap(&mut state, Some(80), 60 * MIN);
        assert_eq!(opened.len(), 1);
        assert_eq!((opened[0].until_ms, opened[0].told), (350 * MIN, false));
        assert_eq!(state.episodes.len(), 1);
        // The next cycle starts low: nothing holds.
        record(
            &mut state,
            &Reported {
                windows: vec![("five_hour".to_owned(), window(0.1, 650 * MIN, 400 * MIN))],
                ..Reported::default()
            },
            "",
            400 * MIN,
        );
        apply_cap(&mut state, Some(80), 400 * MIN);
        assert_eq!(state.episodes, Vec::new());
    }
}
