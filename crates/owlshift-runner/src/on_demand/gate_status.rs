//! The gate's status (OWL-665; architecture section 5, "The gate's
//! status"): on a project that opted in, the runner sets the commit status
//! `owlshift/gate` to `success` on each commit it pushes once its own gate
//! passed on that tree, so the project's CI can skip the suite Owlshift
//! already ran. It never stops a delivery or a repair.

use super::*;
use owlshift_adapters::forge::GATE_STATUS_CONTEXT;

/// The most characters GitHub takes in a commit status's description: one
/// more is refused with HTTP 422 (observed on 2026-11-27).
pub(crate) const MAX_DESCRIPTION: usize = 140;

/// The description of the gate's status: that the gate passed, in
/// `elapsed_s` seconds when known, and its `commands`, cut to
/// [`MAX_DESCRIPTION`] characters with an ellipsis.
pub(crate) fn description(commands: &[String], elapsed_s: Option<u64>) -> String {
    let passed = match elapsed_s {
        Some(seconds) => format!("Owlshift's gate passed in {}", duration(seconds)),
        None => "Owlshift's gate passed".to_owned(),
    };
    let whole = format!("{passed}: {}", commands.join("; "));
    if whole.chars().count() <= MAX_DESCRIPTION {
        return whole;
    }
    let mut cut: String = whole.chars().take(MAX_DESCRIPTION - 1).collect();
    cut.push('…');
    cut
}

/// `seconds` as a person reads a gate's time: `42 s`, `4 min 23 s`, `1 h 2 min`.
fn duration(seconds: u64) -> String {
    match seconds {
        0..60 => format!("{seconds} s"),
        60..3600 => format!("{} min {} s", seconds / 60, seconds % 60),
        _ => format!("{} h {} min", seconds / 3600, seconds % 3600 / 60),
    }
}

impl OnDemand<'_> {
    /// Sets the gate's status on `pushed`, the commit a push of `p`'s
    /// branch just landed, when the project's `[forge] publish_gate_status`
    /// read at `p`'s base is on and `gate` passed with at least one command
    /// on its tree: a `tracker_write` event once set. A gate not recorded
    /// (a refresh rebase, a push made again whose report kept none) sets
    /// nothing; a gate with no command, a switch that cannot be read, a
    /// floor refusal or a forge that refuses or does not answer sets nothing
    /// and is a `warning` event (`gate_status_unset`). Nothing here stops the
    /// delivery or the repair.
    pub(super) fn publish_gate_status(
        &self,
        p: &Prepared,
        pushed: &CommitId,
        gate: &Gate,
        elapsed_s: Option<u64>,
        sink: &mut EventSink<'_>,
    ) {
        let unset = |sink: &mut EventSink<'_>, reason: String| {
            sink.emit(
                &p.ticket,
                None,
                EventKind::Warning,
                data([
                    ("what", json!("gate_status_unset")),
                    ("commit", json!(pushed.as_str())),
                    ("reason", json!(reason)),
                ]),
            );
        };
        match project::gate_status_at(&self.executor.git, &p.checkout, &p.base) {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => {
                return unset(
                    sink,
                    format!("forge.publish_gate_status could not be read: {error}"),
                );
            }
        }
        let commands = match gate {
            Gate::NotRecorded => return,
            Gate::Passed(commands) if commands.is_empty() => {
                return unset(
                    sink,
                    "the project's gate has no command, so it tested nothing".to_owned(),
                );
            }
            Gate::Passed(commands) => commands,
        };
        let description = description(commands, elapsed_s);
        match Writer::new(self.tracker).set_gate_status(self.forge, pushed, &description) {
            Ok(()) => {
                sink.emit(
                    &p.ticket,
                    None,
                    EventKind::TrackerWrite,
                    data([
                        ("target", json!("forge")),
                        ("action", json!("gate_status")),
                        ("commit", json!(pushed.as_str())),
                        ("context", json!(GATE_STATUS_CONTEXT)),
                        ("description", json!(description)),
                    ]),
                );
            }
            Err(error) => unset(sink, error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commands(list: &[&str]) -> Vec<String> {
        list.iter().map(|c| (*c).to_owned()).collect()
    }

    #[test]
    fn the_description_names_the_time_and_the_commands() {
        let gate = commands(&["cargo test", "sh ci/gate.sh lint"]);
        assert_eq!(
            description(&gate, Some(263)),
            "Owlshift's gate passed in 4 min 23 s: cargo test; sh ci/gate.sh lint"
        );
        assert_eq!(
            description(&gate[..1], Some(42)),
            "Owlshift's gate passed in 42 s: cargo test"
        );
        assert_eq!(
            description(&gate[..1], Some(3720)),
            "Owlshift's gate passed in 1 h 2 min: cargo test"
        );
        assert_eq!(
            description(&gate[..1], None),
            "Owlshift's gate passed: cargo test"
        );
    }

    /// GitHub refuses a description past 140 characters (HTTP 422): a long
    /// gate is cut on a character, never inside one.
    #[test]
    fn a_long_description_is_cut_to_githubs_bound() {
        let long = commands(&[&"é".repeat(200)]);
        let cut = description(&long, Some(5));
        assert_eq!(cut.chars().count(), MAX_DESCRIPTION);
        assert!(cut.ends_with('…'), "{cut}");
        let exact = commands(&[&"x".repeat(MAX_DESCRIPTION - "Owlshift's gate passed: ".len())]);
        let kept = description(&exact, None);
        assert_eq!(kept.chars().count(), MAX_DESCRIPTION);
        assert!(!kept.ends_with('…'), "{kept}");
    }
}
