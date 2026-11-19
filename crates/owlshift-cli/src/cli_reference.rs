//! OWL-604: `docs/reference/cli.md`, every command with its arguments and
//! help text, rendered from the binary's own [`Cli`] so the help text stays
//! the single source.
//!
//! A test renders the page and compares it with the committed file; setting
//! `OWLSHIFT_UPDATE_CLI_REFERENCE=1` makes it rewrite the file instead.
//! Hidden commands and flags are left out, as are clap's own `help` and
//! `version`. No version is printed: a release must not change the page.

use std::fmt::Write as _;
use std::path::PathBuf;

use clap::{Arg, Command, CommandFactory};

use super::Cli;

/// The command that rewrites the page, named in its header and in the failure.
const UPDATE_COMMAND: &str =
    "OWLSHIFT_UPDATE_CLI_REFERENCE=1 cargo nextest run -p owlshift-cli --locked cli_reference";

fn page_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/reference/cli.md")
}

/// The commands of `cmd` that a reader can type: neither hidden nor clap's
/// own `help`.
fn visible_subcommands(cmd: &Command) -> impl Iterator<Item = &Command> {
    cmd.get_subcommands()
        .filter(|sub| !sub.is_hide_set() && sub.get_name() != "help")
}

/// The arguments of `cmd` that a reader can type: neither hidden nor clap's
/// own `--help` and `--version`.
fn visible_args(cmd: &Command) -> impl Iterator<Item = &Arg> {
    cmd.get_arguments()
        .filter(|arg| !arg.is_hide_set() && !matches!(arg.get_id().as_str(), "help" | "version"))
}

/// The text of a help string as one line: clap has joined a paragraph's
/// lines already, and a wrapped inline code span would escape the command
/// check. A ticket id in parentheses, as ` (OWL-537)`, is dropped: the
/// reader has no use for it. The final period of a paragraph is put back.
fn plain(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text.trim();
    while let Some(open) = rest.find(" (OWL-") {
        let tail = &rest[open + 6..];
        match tail.find(')') {
            Some(close) if tail[..close].chars().all(|c| c.is_ascii_digit()) => {
                out.push_str(&rest[..open]);
                rest = &tail[close + 1..];
            }
            _ => {
                out.push_str(&rest[..open + 6]);
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    // clap drops the final period of a one-paragraph help: put it back.
    if !out.ends_with(['.', '!', '?', ':', '`', ')']) {
        out.push('.');
    }
    out.replace('\n', " ")
}

/// How the reader types `arg`: `<TICKET>`, `[OWNER/REPO]`, `-f, --follow`,
/// `--last <N>`.
fn spelling(arg: &Arg) -> String {
    let value = arg
        .get_value_names()
        .and_then(|names| names.first().map(|name| name.to_string()))
        .unwrap_or_else(|| arg.get_id().as_str().to_uppercase());
    if arg.is_positional() {
        return if arg.is_required_set() {
            format!("<{value}>")
        } else {
            format!("[{value}]")
        };
    }
    let mut names = Vec::new();
    if let Some(short) = arg.get_short() {
        names.push(format!("-{short}"));
    }
    if let Some(long) = arg.get_long() {
        names.push(format!("--{long}"));
    }
    let mut spelled = names.join(", ");
    if arg.get_action().takes_values() {
        let _ = write!(spelled, " <{value}>");
    }
    spelled
}

/// The anchor GitHub gives the heading `owlshift a b`.
fn anchor(path: &str) -> String {
    path.replace(' ', "-")
}

fn section(out: &mut String, cmd: &mut Command, path: &str, depth: usize) {
    let heading = "#".repeat(depth + 1);
    let _ = writeln!(out, "{heading} `{path}`\n");
    let help = cmd.get_long_about().or_else(|| cmd.get_about());
    if let Some(help) = help {
        let _ = writeln!(out, "{}\n", plain(&help.to_string()));
    }
    let usage = cmd.render_usage().to_string();
    let usage = usage.strip_prefix("Usage: ").unwrap_or(&usage);
    let _ = writeln!(out, "```text\n{usage}\n```\n");
    let args: Vec<&Arg> = visible_args(cmd).collect();
    if !args.is_empty() {
        let _ = writeln!(out, "Arguments and options:\n");
        for arg in args {
            let help = arg
                .get_long_help()
                .or_else(|| arg.get_help())
                .map(|help| plain(&help.to_string()))
                .unwrap_or_default();
            let _ = writeln!(out, "- `{}`: {help}", spelling(arg));
            let values: Vec<_> = arg
                .get_possible_values()
                .into_iter()
                .filter(|value| !value.is_hide_set())
                .collect();
            for value in values {
                let note = value
                    .get_help()
                    .map(|help| format!(": {}", plain(&help.to_string())))
                    .unwrap_or_default();
                let _ = writeln!(out, "  - `{}`{note}", value.get_name());
            }
        }
        out.push('\n');
    }
    let subs: Vec<String> = visible_subcommands(cmd)
        .map(|sub| sub.get_name().to_string())
        .collect();
    for name in subs {
        let sub = cmd.find_subcommand_mut(&name).unwrap();
        section(out, sub, &format!("{path} {name}"), depth + 1);
    }
}

/// The page, as the binary's commands render it.
fn render() -> String {
    let mut cli = Cli::command();
    cli.build();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# Command reference\n\n\
         <!-- Generated from the binary's own command definitions: do not edit by hand. -->\n\n\
         This page lists every `owlshift` command with its arguments, options and help text, \
         as `owlshift help do` prints them for `do`. It is generated from the binary, and a test \
         fails when it differs from the commands. To update it after changing a command, run:\n\n\
         ```sh\n{UPDATE_COMMAND}\n```\n\n\
         Every command also takes `--help`, and `owlshift --version` prints the version. \
         Commands and options meant for Owlshift's own processes are left out. In a usage \
         line, `<NAME>` is required and `[NAME]` optional; the help text says which options \
         go together or exclude each other.\n"
    );
    if let Some(about) = cli.get_about() {
        let _ = writeln!(out, "{}\n", plain(&about.to_string()));
    }
    let _ = writeln!(out, "## Commands\n");
    for sub in visible_subcommands(&cli) {
        let name = sub.get_name();
        let _ = writeln!(
            out,
            "- [`owlshift {name}`](#{})",
            anchor(&format!("owlshift {name}"))
        );
    }
    out.push('\n');
    let names: Vec<String> = visible_subcommands(&cli)
        .map(|sub| sub.get_name().to_string())
        .collect();
    for name in names {
        let sub = cli.find_subcommand_mut(&name).unwrap();
        section(&mut out, sub, &format!("owlshift {name}"), 1);
    }
    out
}

#[test]
fn the_page_is_what_the_binary_renders() {
    let rendered = render();
    let path = page_path();
    if std::env::var("OWLSHIFT_UPDATE_CLI_REFERENCE").is_ok_and(|value| value == "1") {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &rendered).unwrap();
        return;
    }
    let committed = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .replace("\r\n", "\n");
    assert!(
        committed == rendered,
        "docs/reference/cli.md differs from the binary's commands. Rewrite it with:\n\n    {UPDATE_COMMAND}\n"
    );
}

#[test]
fn hidden_and_automatic_entries_stay_out() {
    let page = render();
    let mut cli = Cli::command();
    cli.build();
    let mut hidden = Vec::new();
    let mut stack = vec![&cli];
    while let Some(cmd) = stack.pop() {
        hidden.extend(
            cmd.get_arguments()
                .filter(|arg| arg.is_hide_set())
                .filter_map(|arg| arg.get_long().map(|long| format!("--{long}"))),
        );
        for sub in cmd.get_subcommands() {
            if sub.is_hide_set() {
                hidden.push(format!("owlshift {}", sub.get_name()));
            } else {
                stack.push(sub);
            }
        }
    }
    // The hidden flags exist today: an empty walk would prove nothing.
    assert!(hidden.contains(&"--from-ui".to_string()), "{hidden:?}");
    for name in &hidden {
        let found = page.match_indices(name.as_str()).any(|(at, _)| {
            !page[at + name.len()..]
                .starts_with(|c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        });
        assert!(!found, "{name} is hidden but on the page");
    }
    assert!(!page.contains("- `--help`") && !page.contains("- `-V, --version`"));
    assert!(
        !page
            .lines()
            .any(|line| line.starts_with('#') && line.ends_with(" help`"))
    );
    assert!(!page.contains("(OWL-"), "a ticket id is on the page");
    assert!(page.contains("`--tracker <TRACKER>`") && page.contains("`github`"));
    assert!(page.contains("### `owlshift config show`") && page.contains("## `owlshift config`"));
}

#[test]
fn every_command_the_page_names_exists() {
    let page = render();
    let found = super::doc_commands::commands(&page);
    // The help text names many commands: an extraction that finds none would
    // pass everything.
    assert!(found.len() >= 5, "{found:?}");
    for (line, command) in found {
        if let Some(why) = super::doc_commands::refusal(&command) {
            panic!("page line {line}: `{}`: {why}", command.join(" "));
        }
    }
}
