//! The questions `owlshift init` asks at a terminal for the values of a new
//! project file that no flag gave (OWL-596): the tracker, the gate, and the
//! Linear team, picked among the workspace's teams once the key is stored,
//! or typed with `--skip-secrets`. Each reads its answers through `ask`, as
//! [`crate::ask`] reads them, and says the rest through `say`; an answer
//! that does not fit is asked again, and one that could not be read (end of
//! input, a read error) ends the question with an error.

use std::io;

use owlshift_adapters::tracker::linear::TeamList;
#[cfg(test)]
use owlshift_adapters::tracker::linear::TeamName;
use owlshift_contracts::config::TrackerKind;
use owlshift_core::stack::Detection;

/// Shows a prompt and reads one line, as [`crate::ask`] does.
pub(super) type Ask<'a> = dyn FnMut(&str) -> io::Result<String> + 'a;

/// Why a question before any secret ended without an answer.
const UNREAD: &str = "no answer was read: nothing was stored or written";

/// The answer to `prompt`, trimmed; `None` when nothing could be read: the
/// end of input reads as an empty string, an empty line as `"\n"`.
fn answer(ask: &mut Ask<'_>, prompt: &str) -> Option<String> {
    match ask(prompt) {
        Ok(line) if !line.is_empty() => Some(line.trim().to_owned()),
        _ => None,
    }
}

/// Where the tickets live; Enter keeps Linear, the default off a terminal.
pub(super) fn tracker(ask: &mut Ask<'_>, say: &mut dyn FnMut(&str)) -> Result<TrackerKind, String> {
    say("Where do the tickets live?");
    say("  1. linear: Linear; needs a Linear API key");
    say("  2. github: this repository's GitHub Issues; nothing beyond the GitHub sign-in");
    say("  3. markdown: files in this repository; one machine only");
    loop {
        let answer = answer(ask, "Tracker [1]: ").ok_or(UNREAD)?;
        match answer.to_ascii_lowercase().as_str() {
            "" | "1" | "linear" => return Ok(TrackerKind::Linear),
            "2" | "github" => return Ok(TrackerKind::Github),
            "3" | "markdown" => return Ok(TrackerKind::Markdown),
            _ => say("Answer 1, 2 or 3, or the tracker's name."),
        }
    }
}

/// The gate typed, one command per line until an empty one. Empty when the
/// first line is: the detected gate is then kept, or, with none detected,
/// the gate stays empty, which `init` says once it wrote the file.
pub(super) fn gate(
    detected: &Detection,
    ask: &mut Ask<'_>,
    say: &mut dyn FnMut(&str),
) -> Result<Vec<String>, String> {
    let found: Vec<_> = detected
        .found
        .iter()
        .filter(|found| !found.gate.is_empty())
        .collect();
    if found.is_empty() {
        say(
            "No gate was detected. Type the lint, formatter and test commands `owlshift do` runs \
             from the repository root, one per line, an empty line ending them; none leaves the \
             gate empty.",
        );
    } else {
        say("The gate detected, run from the repository root:");
        for found in found {
            for line in &found.gate {
                say(&format!("  {}    (from {})", line.command, found.source));
            }
            for note in &found.notes {
                say(&format!("  Note: {note}."));
            }
        }
        say(
            "Press Enter to keep it, or type other commands, one per line, an empty line ending \
             them.",
        );
    }
    let mut gate = Vec::new();
    loop {
        let prompt = if gate.is_empty() {
            "Gate command: "
        } else {
            "Next gate command (empty to end): "
        };
        let line = answer(ask, prompt).ok_or(UNREAD)?;
        if line.is_empty() {
            return Ok(gate);
        }
        gate.push(line);
    }
}

/// The Linear team key typed, with `--skip-secrets`, when no key is at hand
/// to list the workspace's teams.
pub(super) fn team_key(ask: &mut Ask<'_>, say: &mut dyn FnMut(&str)) -> Result<String, String> {
    loop {
        let key = answer(
            ask,
            "Linear team key, the prefix of its ticket ids (OWL in OWL-12): ",
        )
        .ok_or(UNREAD)?;
        if !key.is_empty() && !key.contains(char::is_whitespace) {
            return Ok(key);
        }
        say("Type the team key, such as OWL.");
    }
}

/// The key of the team picked among the workspace's teams `list`, by its
/// number in the list or its key; a workspace of one team takes it, and one
/// with none is refused. When the list holds only the first page, a key not
/// listed is taken too, and the tracker check that follows reads that team.
pub(super) fn team(
    list: &TeamList,
    ask: &mut Ask<'_>,
    say: &mut dyn FnMut(&str),
) -> Result<String, String> {
    let teams = &list.teams;
    match (teams.as_slice(), list.more) {
        ([], false) => {
            return Err(
                "the Linear workspace of this key has no team: create one in Linear, then run \
                 `owlshift init` again"
                    .to_owned(),
            );
        }
        ([one], false) => {
            say(&format!(
                "The Linear workspace has one team, {} ({}): it is the project's.",
                one.key, one.name
            ));
            return Ok(one.key.clone());
        }
        _ => {}
    }
    if list.more {
        say(&format!(
            "The Linear workspace has more teams than these first {}: type the key of another \
             to take it.",
            teams.len()
        ));
    } else {
        say("The Linear workspace's teams:");
    }
    for (n, team) in teams.iter().enumerate() {
        say(&format!("  {}. {}  {}", n + 1, team.key, team.name));
    }
    loop {
        let answer = answer(ask, "Team (number or key): ").ok_or("no team was chosen")?;
        let number = answer.parse::<usize>().ok();
        if let Some(team) = number
            .and_then(|n| n.checked_sub(1))
            .and_then(|i| teams.get(i))
        {
            return Ok(team.key.clone());
        }
        if let Some(team) = teams.iter().find(|t| t.key.eq_ignore_ascii_case(&answer)) {
            return Ok(team.key.clone());
        }
        if list.more
            && number.is_none()
            && !answer.is_empty()
            && !answer.contains(char::is_whitespace)
        {
            return Ok(answer.to_ascii_uppercase());
        }
        say(&format!(
            "Answer a number from 1 to {}, or {}.",
            teams.len(),
            if list.more {
                "a team key"
            } else {
                "one of the keys listed"
            }
        ));
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use owlshift_core::stack::{RootFiles, detect};
    use owlshift_runner::init::{InitOptions, project_file};

    use super::*;

    /// Lines typed in order: `Some` is a line, `None` the end of input.
    type Script<'a> = &'a [Option<&'a str>];

    /// Answers `lines` in order. Asking past them is a test error.
    fn scripted(lines: Script<'_>) -> impl FnMut(&str) -> io::Result<String> {
        let mut lines: VecDeque<Option<String>> =
            lines.iter().map(|l| l.map(|l| format!("{l}\n"))).collect();
        move |_| {
            Ok(lines
                .pop_front()
                .expect("asked once too often")
                .unwrap_or_default())
        }
    }

    fn teams(keys: &[&str], more: bool) -> TeamList {
        TeamList {
            teams: keys
                .iter()
                .map(|key| TeamName {
                    key: (*key).to_owned(),
                    name: format!("Team {key}"),
                })
                .collect(),
            more,
        }
    }

    /// Enter keeps Linear; a number or a name in any case picks; anything
    /// else is asked again; the end of input stops `init`.
    #[test]
    fn the_tracker_is_chosen() {
        let cases: [(Script<'_>, Result<TrackerKind, String>); 4] = [
            (&[Some("")], Ok(TrackerKind::Linear)),
            (&[Some("jira"), Some("2")], Ok(TrackerKind::Github)),
            (&[Some(" Markdown ")], Ok(TrackerKind::Markdown)),
            (&[Some("x"), None], Err(UNREAD.to_owned())),
        ];
        for (answers, expected) in cases {
            let mut said = Vec::new();
            let got = tracker(&mut scripted(answers), &mut |l| said.push(l.to_owned()));
            assert_eq!(got, expected, "{answers:?}");
            assert!(
                said.iter().any(|l| l.contains("GitHub sign-in")),
                "{said:?}"
            );
        }
    }

    /// Several teams: picked by number or key, a wrong answer asked again;
    /// one: taken without a question; none: refused with a reason. A
    /// partial list says so and takes a key it does not show.
    #[test]
    fn the_team_is_picked_taken_when_alone_and_refused_when_none() {
        let several = teams(&["OWL", "SBX"], false);
        let partial = teams(&["OWL"], true);
        let cases: [(&TeamList, Script<'_>, Result<&str, &str>); 7] = [
            (&several, &[Some("2")], Ok("SBX")),
            (&several, &[Some("3"), Some("eng"), Some("owl")], Ok("OWL")),
            (&several, &[None], Err("no team was chosen")),
            (&teams(&["OWL"], false), &[], Ok("OWL")),
            (&teams(&[], false), &[], Err("has no team")),
            (&partial, &[Some("7"), Some("eng")], Ok("ENG")),
            (&partial, &[Some("1")], Ok("OWL")),
        ];
        for (list, answers, expected) in cases {
            let mut said = Vec::new();
            let got = team(list, &mut scripted(answers), &mut |l| {
                said.push(l.to_owned())
            });
            if list.more {
                assert!(said[0].contains("more teams than these"), "{said:?}");
            }
            match (got, expected) {
                (Ok(key), Ok(want)) => assert_eq!(key, want),
                (Err(error), Err(want)) => assert!(error.contains(want), "{error}"),
                (got, _) => panic!("{answers:?}: {got:?}"),
            }
        }
    }

    /// With `--skip-secrets` the key is typed; a blank answer is asked again.
    #[test]
    fn the_team_key_is_typed() {
        let got = team_key(&mut scripted(&[Some(" "), Some("ENG")]), &mut |_| {});
        assert_eq!(got.as_deref(), Ok("ENG"));
    }

    /// A detected gate is shown and Enter keeps it, as the file then
    /// renders; typed lines replace it; with none detected, nothing typed
    /// leaves the gate empty.
    #[test]
    fn the_gate_is_kept_typed_or_left_empty() {
        let cargo = detect(&RootFiles {
            names: vec!["Cargo.toml".to_owned(), "Cargo.lock".to_owned()],
            files: vec![(
                "Cargo.toml".to_owned(),
                "[package]\nname = \"x\"\n".to_owned(),
            )],
            ..RootFiles::default()
        });
        let detected: Vec<String> = cargo.gate().map(|l| l.command.clone()).collect();
        assert!(!detected.is_empty());

        let mut said = Vec::new();
        let kept = gate(&cargo, &mut scripted(&[Some("")]), &mut |l| {
            said.push(l.to_owned())
        })
        .unwrap();
        assert!(said.iter().any(|l| l.contains(&detected[0])), "{said:?}");
        let text = project_file(&InitOptions {
            tracker: TrackerKind::Markdown,
            team: None,
            gate: kept,
            detected: cargo.clone(),
            admitter: None,
        })
        .unwrap();
        assert!(text.contains(&detected[0]), "{text}");

        let typed = gate(
            &cargo,
            &mut scripted(&[Some(" make lint "), Some("make test"), Some("")]),
            &mut |_| {},
        );
        assert_eq!(typed.unwrap(), ["make lint", "make test"]);

        let mut said = Vec::new();
        let none = gate(
            &Detection::default(),
            &mut scripted(&[Some("")]),
            &mut |l| said.push(l.to_owned()),
        );
        assert_eq!(none.unwrap(), Vec::<String>::new());
        assert!(said[0].contains("No gate was detected"), "{said:?}");
        let cut = gate(
            &Detection::default(),
            &mut scripted(&[Some("make"), None]),
            &mut |_| {},
        );
        assert_eq!(cut, Err(UNREAD.to_owned()));
    }
}
