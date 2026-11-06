//! Whether the machine is in a dark wake (OWL-485, architecture section 5,
//! "The machine's" and "Dark wake").
//!
//! A Mac woken for maintenance or Power Nap without the user (a dark wake)
//! runs the CPU and the network for seconds to minutes and then sleeps again,
//! which is where a gate that runs across it fails (OWL-480). macOS shows it
//! in the `"System Capabilities"` integer of `ioreg -r -n IOPMrootDomain`, a
//! mask of what the system keeps up: 1 CPU, 2 graphics, 4 audio, 8 network.
//! Checked on 2026-11-05 on macOS 26.6.2 (build plan, "P0", L6, OWL-485): 15
//! at a full wake, with the display on and with it put to sleep while the
//! system is held awake, and 9 (CPU and network) in every sample of a dark
//! wake. So the graphics bit tells them apart, and [`parse`] reads nothing
//! else. The probe fails open: any reading it cannot make, and any platform
//! but macOS, is [`Wake::Unknown`], which never holds anything, as
//! keep-awake ([`crate::awake`]) does not hold anything back either.

/// What the machine's power state is, as far as the probe can tell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wake {
    /// A full wake: the system keeps graphics up.
    Full,
    /// A dark wake: the system is up without graphics.
    Dark,
    /// The probe could not tell: not macOS, `ioreg` failed or timed out, or
    /// its output does not hold the key.
    Unknown,
}

/// The graphics bit of `"System Capabilities"`
/// (`kIOPMSystemCapabilityGraphics`).
const GRAPHICS: u64 = 2;

/// The state `ioreg -r -n IOPMrootDomain` output shows: the integer of its
/// `"System Capabilities"` line, [`Wake::Dark`] when the graphics bit is
/// clear, [`Wake::Full`] when it is set, [`Wake::Unknown`] with no such
/// line or no integer after it.
pub fn parse(ioreg: &str) -> Wake {
    for line in ioreg.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim().trim_start_matches('|').trim() != "\"System Capabilities\"" {
            continue;
        }
        return match value.trim().parse::<u64>() {
            Ok(mask) if mask & GRAPHICS == 0 => Wake::Dark,
            Ok(_) => Wake::Full,
            Err(_) => Wake::Unknown,
        };
    }
    Wake::Unknown
}

/// The machine's state now: on macOS, [`parse`] of `ioreg -r -n
/// IOPMrootDomain` (5 s at most); [`Wake::Unknown`] anywhere else or when it
/// cannot be run.
pub fn read() -> Wake {
    imp::read()
}

#[cfg(target_os = "macos")]
mod imp {
    use std::path::Path;
    use std::time::Duration;

    use super::{Wake, parse};
    use crate::process;

    pub(super) fn read() -> Wake {
        let captured = process::run(
            Path::new("/usr/sbin/ioreg"),
            &["-r", "-n", "IOPMrootDomain"],
            None,
            Duration::from_secs(5),
        );
        match captured {
            Ok(captured) if captured.success() => parse(&String::from_utf8_lossy(&captured.stdout)),
            _ => Wake::Unknown,
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::Wake;

    pub(super) fn read() -> Wake {
        Wake::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lines of `ioreg -r -n IOPMrootDomain` around the key, as captured
    /// on 2026-11-05 on macOS 26.6.2 (L6, OWL-485).
    fn captured(value: &str) -> String {
        format!(
            "+-o IOPMrootDomain  <class IOPMrootDomain, id 0x100000381, registered, matched, active, busy 0 (1911 ms), retain 229>\n  | {{\n  |   \"IOClass\" = \"IOPMrootDomain\"\n  |   \"System Capabilities\" = {value}\n  |   \"Supported Features\" = 1\n  | }}\n"
        )
    }

    #[test]
    fn parse_reads_a_full_wake() {
        assert_eq!(parse(&captured("15")), Wake::Full);
    }

    #[test]
    fn parse_reads_a_dark_wake() {
        assert_eq!(parse(&captured("9")), Wake::Dark);
    }

    #[test]
    fn parse_reads_a_sleeping_display_as_full() {
        // Sampled with the display put to sleep and the system held awake.
        assert_eq!(parse(&captured("15")), Wake::Full);
        // Any mask with graphics up is a full wake.
        assert_eq!(parse(&captured("7")), Wake::Full);
    }

    #[test]
    fn a_missing_key_or_garbage_is_unknown() {
        assert_eq!(parse(""), Wake::Unknown);
        assert_eq!(parse("not ioreg at all\n= 15\n"), Wake::Unknown);
        assert_eq!(parse("  |   \"Supported Features\" = 15\n"), Wake::Unknown);
        assert_eq!(
            parse("  |   \"System Capabilities\" = fifteen\n"),
            Wake::Unknown
        );
        assert_eq!(parse("  |   \"System Capabilities\" = \n"), Wake::Unknown);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn read_answers_on_this_machine() {
        // Whatever the machine's state, the probe answers without panicking.
        let _ = read();
    }
}
