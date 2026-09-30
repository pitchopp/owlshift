//! How a doctor [`Report`] is shown: as text for a person, grouped by
//! section and closed by a summary of what to fix and how, or as JSON for
//! scripts and the web UI (OWL-99).
//!
//! In the text, everything that comes from the machine goes through
//! [`printable`] first, as `owlshift logs` does, so no control character or
//! bidirectional override reaches the terminal; wrapping comes after, and
//! colour last.

use std::ffi::OsStr;
use std::fmt;
use std::io::IsTerminal;

use serde_json::{Value, json};

use super::{Check, Next, Report, Section, Status, Step};
use crate::config::OWLSHIFT_VERSION;
use crate::events::printable;

/// The width of the text when standard output is not a terminal, or the
/// terminal does not say.
const PLAIN_WIDTH: usize = 80;
/// Below this, lines are not narrowed any further.
const MIN_WIDTH: usize = 40;
/// Where the text of a summary block starts: `   Why   `.
const BLOCK_TEXT: usize = 9;

/// How the text report is laid out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Style {
    /// Colour the symbols and headings with ANSI escapes.
    pub colour: bool,
    /// The width lines wrap at, in characters.
    pub width: usize,
}

impl Style {
    /// No colour, 80 columns: for a file, a pipe, or a test.
    pub fn plain() -> Self {
        Self {
            colour: false,
            width: PLAIN_WIDTH,
        }
    }

    /// The style for standard output: colour and the terminal's width on a
    /// terminal, [`Style::plain`] otherwise. Never colour on Windows, whose
    /// older consoles print the escapes as they are.
    pub fn for_stdout() -> Self {
        let terminal = std::io::stdout().is_terminal();
        let colour = !cfg!(windows)
            && colour_wanted(
                terminal,
                std::env::var_os("NO_COLOR").as_deref(),
                std::env::var_os("TERM").as_deref(),
            );
        let width = terminal
            .then(owlshift_platform::terminal::stdout_width)
            .flatten()
            .unwrap_or(PLAIN_WIDTH);
        Self { colour, width }
    }

    fn paint(&self, text: &str, colour: Colour) -> String {
        if self.colour {
            format!("\x1b[{}m{text}\x1b[0m", colour.code())
        } else {
            text.to_owned()
        }
    }
}

/// Whether to colour: only on a terminal, unless `NO_COLOR` is set to
/// anything but the empty string (<https://no-color.org>) or the terminal is
/// `dumb`.
pub fn colour_wanted(terminal: bool, no_color: Option<&OsStr>, term: Option<&OsStr>) -> bool {
    terminal && no_color.is_none_or(OsStr::is_empty) && term != Some(OsStr::new("dumb"))
}

#[derive(Clone, Copy)]
enum Colour {
    Green,
    Yellow,
    Red,
    Dim,
    Bold,
    BoldGreen,
    BoldRed,
}

impl Colour {
    fn code(self) -> &'static str {
        match self {
            Self::Green => "32",
            Self::Yellow => "33",
            Self::Red => "31",
            Self::Dim => "2",
            Self::Bold => "1",
            Self::BoldGreen => "1;32",
            Self::BoldRed => "1;31",
        }
    }
}

fn symbol(status: Status) -> (&'static str, Colour) {
    match status {
        Status::Ok => ("✓", Colour::Green),
        Status::Warn => ("!", Colour::Yellow),
        Status::Fail => ("✗", Colour::Red),
        Status::Info => ("·", Colour::Dim),
    }
}

impl Report {
    /// The report as text, laid out with `style`.
    pub fn render(&self, style: &Style) -> String {
        let width = style.width.max(MIN_WIDTH);
        let mut lines = vec![format!(
            "owlshift doctor · {OWLSHIFT_VERSION} · {}",
            std::env::consts::OS
        )];
        let name_width = self
            .checks
            .iter()
            .map(|check| printable(&check.subject).chars().count())
            .max()
            .unwrap_or(0);
        // `  ✓ <subject>  <detail>`
        let column = 4 + name_width + 2;
        for section in Section::ALL {
            let checks: Vec<&Check> = self
                .checks
                .iter()
                .filter(|check| check.section == section)
                .collect();
            if checks.is_empty() {
                continue;
            }
            lines.push(String::new());
            lines.push(style.paint(section.title(), Colour::Bold));
            for check in checks {
                check_lines(&mut lines, check, style, name_width, column, width);
            }
        }
        lines.push(String::new());
        lines.push("─".repeat(width.min(60)));
        match self.failures() {
            0 => self.ready_lines(&mut lines, style, width),
            n => self.problem_lines(&mut lines, style, width, n),
        }
        let mut text = lines
            .iter()
            .map(|line| line.trim_end())
            .collect::<Vec<_>>()
            .join("\n");
        text.push('\n');
        text
    }

    fn ready_lines(&self, lines: &mut Vec<String>, style: &Style, width: usize) {
        lines.push(style.paint("✓ Ready: nothing blocks `owlshift do`.", Colour::BoldGreen));
        lines.push(String::new());
        let (command, note, then) = match self.next {
            Next::Do => (
                "owlshift do TICKET",
                "a ticket id, such as ABC-12",
                Some("If it says a secret is missing, `owlshift init` stores it."),
            ),
            Next::Init => (
                "owlshift init",
                "writes this repository's owlshift.toml and stores the secrets `owlshift do` needs",
                None,
            ),
            Next::FromRepository => (
                "owlshift doctor",
                "again, from your project's repository, to check its configuration",
                None,
            ),
        };
        let step = Step::Run {
            command: command.to_owned(),
            note: Some(note.to_owned()),
        };
        let mut next = Vec::new();
        step_lines(&mut next, &step, "", 6, width);
        if let Some(then) = then {
            next.extend(wrap(then, width - 6));
        }
        for (i, line) in next.into_iter().enumerate() {
            let label = if i == 0 { "Next  " } else { "      " };
            lines.push(format!("{label}{line}"));
        }
    }

    fn problem_lines(&self, lines: &mut Vec<String>, style: &Style, width: usize, n: usize) {
        let problems = if n == 1 { "problem" } else { "problems" };
        lines.push(style.paint(
            &format!("✗ Not ready: {n} {problems} to fix before `owlshift do`"),
            Colour::BoldRed,
        ));
        let failed = self
            .checks
            .iter()
            .filter(|check| check.status == Status::Fail);
        for (i, check) in failed.enumerate() {
            lines.push(String::new());
            lines.push(style.paint(
                &format!("{}. {}", i + 1, capitalised(&printable(&check.subject))),
                Colour::Bold,
            ));
            let why = check.why.as_deref().unwrap_or("");
            for (j, line) in wrap(&printable(why), width - BLOCK_TEXT)
                .into_iter()
                .enumerate()
            {
                let label = if j == 0 { "   Why   " } else { "         " };
                lines.push(format!("{label}{line}"));
            }
            let recheck = Step::Run {
                command: "owlshift doctor".to_owned(),
                note: Some("to check".to_owned()),
            };
            let steps = check.fix.iter().chain(std::iter::once(&recheck));
            let mut fix = Vec::new();
            for (k, step) in steps.enumerate() {
                step_lines(&mut fix, step, &format!("{}) ", k + 1), BLOCK_TEXT, width);
            }
            for (j, line) in fix.into_iter().enumerate() {
                let label = if j == 0 { "   Fix   " } else { "         " };
                lines.push(format!("{label}{line}"));
            }
        }
    }
}

/// A check's line, its detail wrapped under itself, and a warning's note.
fn check_lines(
    lines: &mut Vec<String>,
    check: &Check,
    style: &Style,
    name_width: usize,
    column: usize,
    width: usize,
) {
    let (symbol, colour) = symbol(check.status);
    let subject = printable(&check.subject);
    let padding = " ".repeat(name_width - subject.chars().count());
    let indent = " ".repeat(column);
    let detail = wrap(&printable(&check.detail), width - column);
    for (i, line) in detail.into_iter().enumerate() {
        if i == 0 {
            lines.push(format!(
                "  {} {subject}{padding}  {line}",
                style.paint(symbol, colour)
            ));
        } else {
            lines.push(format!("{indent}{line}"));
        }
    }
    if check.status == Status::Warn
        && let Some(why) = &check.why
    {
        for line in wrap(&printable(why), width - column) {
            lines.push(format!("{indent}{line}"));
        }
    }
}

/// A step's lines, without the label before them: `marker`, then the
/// command whole with its note after it, or on the next line when both do
/// not fit; or the instruction, wrapped. `start` is the column the marker is
/// written at.
fn step_lines(out: &mut Vec<String>, step: &Step, marker: &str, start: usize, width: usize) {
    let hang = " ".repeat(marker.chars().count());
    let room = width.saturating_sub(start + marker.chars().count());
    match step {
        Step::Run { command, note } => {
            let command = printable(command);
            match note.as_deref().map(printable) {
                None => out.push(format!("{marker}{command}")),
                Some(note) => {
                    let together = format!("{command}    ({note})");
                    if together.chars().count() <= room {
                        out.push(format!("{marker}{together}"));
                    } else {
                        out.push(format!("{marker}{command}"));
                        for line in wrap(&format!("({note})"), room) {
                            out.push(format!("{hang}{line}"));
                        }
                    }
                }
            }
        }
        Step::Do(text) => {
            for (i, line) in wrap(&printable(text), room).into_iter().enumerate() {
                let lead = if i == 0 { marker } else { hang.as_str() };
                out.push(format!("{lead}{line}"));
            }
        }
    }
}

/// `text` in lines of `width` characters at most, broken between words; a
/// word longer than a line, such as a path or a URL, stays whole. Line
/// breaks in `text` are kept, with each line's own indentation.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(20);
    let mut lines = Vec::new();
    for raw in text.split('\n') {
        let raw = raw.replace('\t', " ");
        if raw.chars().count() <= width {
            lines.push(raw);
            continue;
        }
        let indent: String = raw.chars().take_while(|c| *c == ' ').collect();
        let mut line = indent.clone();
        for word in words(&raw) {
            let used = line.chars().count();
            if used > indent.len() && used + 1 + word.chars().count() > width {
                lines.push(std::mem::replace(&mut line, indent.clone()));
            }
            if line.chars().count() > indent.len() {
                line.push(' ');
            }
            line.push_str(&word);
        }
        lines.push(line);
    }
    lines
}

/// The words of `line`, a span in backticks, such as `` `owlshift do` ``,
/// counting as one: a command is not broken in two.
fn words(line: &str) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    let mut open = false;
    for word in line.split(' ').filter(|word| !word.is_empty()) {
        match words.last_mut() {
            Some(last) if open => {
                last.push(' ');
                last.push_str(word);
            }
            _ => words.push(word.to_owned()),
        }
        if word.matches('`').count() % 2 == 1 {
            open = !open;
        }
    }
    words
}

fn capitalised(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The plain text: no colour, 80 columns.
impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render(&Style::plain()))
    }
}

impl Report {
    /// The report as JSON: every check with its status, section, subject,
    /// detail, why and fix steps; whether the machine is ready, how many
    /// problems it has, and the next command once ready. The strings are as
    /// the checks wrote them: escaping them for a terminal or a web page is
    /// the reader's job. The last step the text adds, running `owlshift
    /// doctor` again, is not in the JSON.
    pub fn to_json(&self) -> Value {
        json!({
            "owlshift": OWLSHIFT_VERSION,
            "ready": self.ready(),
            "problems": self.failures(),
            "next": self.ready().then(|| self.next.id()),
            "checks": self.checks.iter().map(check_json).collect::<Vec<_>>(),
        })
    }
}

fn check_json(check: &Check) -> Value {
    let fix: Vec<Value> = check
        .fix
        .iter()
        .map(|step| match step {
            Step::Run { command, note } => json!({ "run": command, "note": note }),
            Step::Do(text) => json!({ "do": text }),
        })
        .collect();
    json!({
        "status": check.status.id(),
        "section": check.section.id(),
        "subject": check.subject,
        "detail": check.detail,
        "why": check.why,
        "fix": fix,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const WHY_LOGIN: &str = "Agent runs are confined and cannot reach the Keychain, so they \
                             log in with a token of their own.";

    fn header() -> String {
        format!(
            "owlshift doctor · {OWLSHIFT_VERSION} · {}",
            std::env::consts::OS
        )
    }

    fn agent_login_missing() -> Check {
        Check::fail(
            Section::AgentIsolation,
            "claude agent login",
            "no token for agent runs in the system keychain (service `owlshift`, account \
             `claude-agent`)",
            WHY_LOGIN,
            vec![
                Step::run_noting("claude setup-token", "prints a token"),
                Step::run_noting("owlshift init", "paste the token when asked"),
            ],
        )
    }

    /// A report with one of each status, and one problem.
    fn one_problem() -> Report {
        Report {
            checks: vec![
                Check::ok(Section::Tools, "git", "2.54.0 (/usr/bin/git)"),
                Check::warn(
                    Section::Tools,
                    "codex",
                    "0.154.0 (~/bin/codex), logged in (API key); no version tested with \
                     Owlshift yet",
                    "Does not block `owlshift do`: it does not use Codex yet; the review roles \
                     will, from P5."
                        .to_owned(),
                ),
                agent_login_missing(),
                Check::info(
                    Section::Project,
                    "personal config",
                    "not found at ~/.config/owlshift/config.toml",
                ),
            ],
            next: Next::Do,
        }
    }

    /// The plain text, pinned whole: sections, symbols, the warning's note
    /// under it, wrapping at 80 columns, the summary block.
    #[test]
    fn the_plain_report_is_pinned() {
        let expected = format!(
            "{header}

Tools
  ✓ git                 2.54.0 (/usr/bin/git)
  ! codex               0.154.0 (~/bin/codex), logged in (API key); no version
                        tested with Owlshift yet
                        Does not block `owlshift do`: it does not use Codex yet;
                        the review roles will, from P5.

Agent isolation
  ✗ claude agent login  no token for agent runs in the system keychain (service
                        `owlshift`, account `claude-agent`)

Project
  · personal config     not found at ~/.config/owlshift/config.toml

────────────────────────────────────────────────────────────
✗ Not ready: 1 problem to fix before `owlshift do`

1. Claude agent login
   Why   Agent runs are confined and cannot reach the Keychain, so they log in
         with a token of their own.
   Fix   1) claude setup-token    (prints a token)
         2) owlshift init    (paste the token when asked)
         3) owlshift doctor    (to check)
",
            header = header()
        );
        let report = one_problem();
        assert_eq!(report.to_string(), expected);
        assert_eq!(report.render(&Style::plain()), expected);
    }

    #[test]
    fn several_problems_are_numbered_each_with_its_why_and_fix() {
        let mut report = one_problem();
        report.checks.insert(
            0,
            Check::fail(
                Section::Tools,
                "git",
                "not found on the PATH",
                "Nothing runs without git.",
                vec![Step::act("Install git: https://git-scm.com/downloads")],
            ),
        );
        let shown = report.to_string();
        assert!(
            shown.contains(
                "✗ Not ready: 2 problems to fix before `owlshift do`

1. Git
   Why   Nothing runs without git.
   Fix   1) Install git: https://git-scm.com/downloads
         2) owlshift doctor    (to check)

2. Claude agent login
"
            ),
            "{shown}"
        );
        assert_eq!(shown.matches("owlshift doctor    (to check)").count(), 2);
    }

    #[test]
    fn a_ready_report_names_the_next_command() {
        let mut report = one_problem();
        report.checks.remove(2);
        for (next, expected) in [
            (
                Next::Do,
                "✓ Ready: nothing blocks `owlshift do`.

Next  owlshift do TICKET    (a ticket id, such as ABC-12)
      If it says a secret is missing, `owlshift init` stores it.
",
            ),
            (
                Next::Init,
                "Next  owlshift init
      (writes this repository's owlshift.toml and stores the secrets
      `owlshift do` needs)
",
            ),
            (
                Next::FromRepository,
                "Next  owlshift doctor
      (again, from your project's repository, to check its configuration)
",
            ),
        ] {
            report.next = next;
            let shown = report.to_string();
            assert!(shown.ends_with(expected), "{shown}");
            assert!(!shown.contains("Not ready"), "{shown}");
        }
    }

    /// Narrow: prose wraps under itself, a command stays whole on its line
    /// and its note moves under it.
    #[test]
    fn prose_wraps_and_commands_do_not() {
        let command = "printf '%s\\n' 'abi <abi/4.0>,' 'include <tunables/global>' | sudo tee \
                       /etc/apparmor.d/bwrap >/dev/null";
        let report = Report {
            checks: vec![Check::fail(
                Section::AgentIsolation,
                "sandbox",
                "bwrap cannot confine a trial run: setting up uid map: Permission denied",
                "`owlshift do` does not start an agent it cannot confine.",
                vec![Step::run_noting(command, "writes the profile")],
            )],
            next: Next::Do,
        };
        let shown = report.render(&Style {
            colour: false,
            width: 40,
        });
        assert!(
            shown.contains(
                "  ✗ sandbox  bwrap cannot confine a
             trial run: setting up uid
             map: Permission denied
"
            ),
            "{shown}"
        );
        assert!(
            shown.contains(&format!(
                "   Fix   1) {command}\n            (writes the profile)\n"
            )),
            "{shown}"
        );
        // Below the minimum, the minimum.
        assert_eq!(
            shown,
            report.render(&Style {
                colour: false,
                width: 3
            })
        );
    }

    /// What the machine said reaches the terminal escaped, as in `logs`:
    /// here an escape sequence and a right-to-left override.
    #[test]
    fn machine_text_is_escaped() {
        let report = Report {
            checks: vec![Check::fail(
                Section::Project,
                "project config",
                "evil\u{1b}[2Jtext\u{202e}",
                "why\u{7}",
                vec![Step::act("do\u{1b}]0;title\u{7}")],
            )],
            next: Next::Do,
        };
        let shown = report.to_string();
        assert!(
            !shown.contains('\u{1b}') && !shown.contains('\u{7}'),
            "{shown}"
        );
        assert!(!shown.contains('\u{202e}'), "{shown}");
        assert!(shown.contains("evil\\u{1b}[2Jtext\\u{202e}"), "{shown}");
    }

    #[test]
    fn colour_only_on_a_terminal_without_no_color() {
        let set = Some(OsStr::new("1"));
        assert!(colour_wanted(true, None, None));
        assert!(colour_wanted(
            true,
            None,
            Some(OsStr::new("xterm-256color"))
        ));
        // An empty NO_COLOR does not count (no-color.org).
        assert!(colour_wanted(true, Some(OsStr::new("")), None));
        assert!(!colour_wanted(false, None, None));
        assert!(!colour_wanted(true, set, None));
        assert!(!colour_wanted(true, None, Some(OsStr::new("dumb"))));

        let report = one_problem();
        let coloured = report.render(&Style {
            colour: true,
            width: 80,
        });
        assert!(coloured.contains("\x1b[31m✗\x1b[0m"), "{coloured}");
        assert!(coloured.contains("\x1b[1mTools\x1b[0m"), "{coloured}");
        assert!(!report.to_string().contains('\x1b'));
    }

    #[test]
    fn the_json_report_has_every_field() {
        let report = one_problem();
        assert_eq!(
            report.to_json(),
            json!({
                "owlshift": OWLSHIFT_VERSION,
                "ready": false,
                "problems": 1,
                "next": null,
                "checks": [
                    {
                        "status": "ok",
                        "section": "tools",
                        "subject": "git",
                        "detail": "2.54.0 (/usr/bin/git)",
                        "why": null,
                        "fix": [],
                    },
                    {
                        "status": "warn",
                        "section": "tools",
                        "subject": "codex",
                        "detail": "0.154.0 (~/bin/codex), logged in (API key); no version \
                                   tested with Owlshift yet",
                        "why": "Does not block `owlshift do`: it does not use Codex yet; the \
                                review roles will, from P5.",
                        "fix": [],
                    },
                    {
                        "status": "fail",
                        "section": "agent_isolation",
                        "subject": "claude agent login",
                        "detail": "no token for agent runs in the system keychain (service \
                                   `owlshift`, account `claude-agent`)",
                        "why": WHY_LOGIN,
                        "fix": [
                            { "run": "claude setup-token", "note": "prints a token" },
                            { "run": "owlshift init", "note": "paste the token when asked" },
                        ],
                    },
                    {
                        "status": "info",
                        "section": "project",
                        "subject": "personal config",
                        "detail": "not found at ~/.config/owlshift/config.toml",
                        "why": null,
                        "fix": [],
                    },
                ],
            })
        );

        let mut ready = one_problem();
        ready.checks.remove(2);
        let json = ready.to_json();
        assert_eq!(json["ready"], true);
        assert_eq!(json["problems"], 0);
        assert_eq!(json["next"], "do");

        let instruction = Check::fail(
            Section::Tools,
            "git",
            "not found on the PATH",
            "Nothing runs without git.",
            vec![Step::act("Install git")],
        );
        assert_eq!(
            check_json(&instruction)["fix"],
            json!([{ "do": "Install git" }])
        );
    }
}
