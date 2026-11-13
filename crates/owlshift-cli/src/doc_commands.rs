//! OWL-542: every `owlshift` command the user documentation names exists,
//! with the flags it shows, as `tests/cli.rs` pins the CI's run strings.
//!
//! The documents are every Markdown file at the repository's root, in `docs/`
//! and in `docs/adopt/`; `docs/design/` is left out, since it records history
//! and commands still to be built. A command is read from a shell code block
//! (no info string, or `bash`, `sh`, `shell`, `console` or `zsh`) and from
//! inline code, wherever its first word, after any `NAME=value` assignments,
//! is `owlshift` or a path ending in `/owlshift`. Each is parsed by the
//! binary's own [`Cli`]: an unknown subcommand, an unknown or hidden flag or a
//! value the flag refuses fails; a command shown without its required
//! argument, as `owlshift do` in a sentence, passes.

use std::path::{Path, PathBuf};

use clap::CommandFactory;
use clap::error::ErrorKind;

use super::Cli;

/// A word of a command line, or an operator that ends one.
#[derive(Debug, PartialEq)]
enum Token {
    Word(String),
    Operator,
}

/// Splits `line` as a shell would, closely enough for documentation: words
/// separated by blanks, single and double quotes kept whole (a backslash
/// escapes in double quotes and outside quotes), `&&`, `||`, `;`, `|` and `&`
/// outside quotes as operators, and a `#` that starts a word ending the line.
fn tokens(line: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = line.chars().peekable();
    let end_word = |out: &mut Vec<Token>, word: &mut String, in_word: &mut bool| {
        if *in_word {
            out.push(Token::Word(std::mem::take(word)));
            *in_word = false;
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                for q in chars.by_ref() {
                    if q == '\'' {
                        break;
                    }
                    word.push(q);
                }
            }
            '"' => {
                in_word = true;
                while let Some(q) = chars.next() {
                    match q {
                        '"' => break,
                        '\\' => {
                            if let Some(escaped) = chars.next() {
                                word.push(escaped);
                            }
                        }
                        _ => word.push(q),
                    }
                }
            }
            '\\' => {
                in_word = true;
                if let Some(escaped) = chars.next() {
                    word.push(escaped);
                }
            }
            '#' if !in_word => break,
            ';' | '|' | '&' => {
                end_word(&mut out, &mut word, &mut in_word);
                if chars.peek() == Some(&c) && c != ';' {
                    chars.next();
                }
                out.push(Token::Operator);
            }
            c if c.is_whitespace() => end_word(&mut out, &mut word, &mut in_word),
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    end_word(&mut out, &mut word, &mut in_word);
    out
}

/// The `owlshift` commands of one shell line: each piece between operators
/// whose first word, past `NAME=value` assignments, is the binary.
fn commands_of_line(line: &str) -> Vec<Vec<String>> {
    let mut commands = Vec::new();
    let mut piece: Vec<String> = Vec::new();
    let mut flush = |piece: &mut Vec<String>| {
        let words: Vec<String> = piece
            .drain(..)
            .skip_while(|word| is_assignment(word))
            .collect();
        if words
            .first()
            .is_some_and(|first| first == "owlshift" || first.ends_with("/owlshift"))
        {
            commands.push(words);
        }
    };
    for token in tokens(line) {
        match token {
            Token::Word(word) => piece.push(word),
            Token::Operator => flush(&mut piece),
        }
    }
    flush(&mut piece);
    commands
}

/// `NAME=value`, a variable set for the command that follows.
fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// The `owlshift` commands a Markdown text names, each with its line number.
fn commands(text: &str) -> Vec<(usize, Vec<String>)> {
    let mut found = Vec::new();
    // The fence that opened the current code block, and whether it is shell.
    let mut fence: Option<(String, bool)> = None;
    let mut pending = String::new();
    let mut pending_start = 0;
    for (index, line) in text.lines().enumerate() {
        let number = index + 1;
        let trimmed = line.trim_start();
        if let Some((marker, shell)) = &fence {
            if trimmed.starts_with(marker.as_str()) && trimmed[marker.len()..].trim().is_empty() {
                fence = None;
                continue;
            }
            if !shell {
                continue;
            }
            let line = trimmed.strip_prefix("$ ").unwrap_or(trimmed);
            if pending.is_empty() {
                pending_start = number;
            }
            if let Some(continued) = line.strip_suffix('\\') {
                pending.push_str(continued);
                pending.push(' ');
                continue;
            }
            pending.push_str(line);
            for command in commands_of_line(&pending) {
                found.push((pending_start, command));
            }
            pending.clear();
            continue;
        }
        if let Some(marker) = ["```", "~~~"]
            .iter()
            .find(|marker| trimmed.starts_with(**marker))
        {
            let marker: String = trimmed
                .chars()
                .take_while(|c| marker.starts_with(*c))
                .collect();
            let info = trimmed[marker.len()..]
                .split_whitespace()
                .next()
                .unwrap_or("");
            let shell = matches!(info, "" | "bash" | "sh" | "shell" | "console" | "zsh");
            fence = Some((marker, shell));
            continue;
        }
        for span in inline_code(line) {
            for command in commands_of_line(span) {
                found.push((number, command));
            }
        }
    }
    found
}

/// The inline code spans of a line: text between two runs of backticks of
/// the same length.
fn inline_code(line: &str) -> Vec<&str> {
    let mut spans = Vec::new();
    let mut rest = line;
    while let Some(open) = rest.find('`') {
        let after = &rest[open..];
        let ticks = after.chars().take_while(|c| *c == '`').count();
        let body = &after[ticks..];
        let closing = "`".repeat(ticks);
        let Some(close) = body.find(&closing) else {
            break;
        };
        spans.push(body[..close].trim());
        rest = &body[close + ticks..];
    }
    spans
}

/// Why `command` is not a command of the binary, if it is not.
fn refusal(command: &[String]) -> Option<String> {
    let mut args = command.to_vec();
    args[0] = "owlshift".to_string();
    if let Err(error) = Cli::command().try_get_matches_from(&args) {
        let incomplete = matches!(
            error.kind(),
            ErrorKind::MissingRequiredArgument
                | ErrorKind::MissingSubcommand
                | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                | ErrorKind::DisplayHelp
                | ErrorKind::DisplayVersion
        );
        if !incomplete {
            return Some(error.render().to_string());
        }
    }
    // A hidden flag parses, but is the binary's own and never typed.
    let mut cli = Cli::command();
    cli.build();
    let mut current = &cli;
    for word in &args[1..] {
        if let Some(flag) = word.strip_prefix("--") {
            let name = flag.split_once('=').map_or(flag, |(name, _)| name);
            let hidden = current
                .get_arguments()
                .any(|arg| arg.get_long() == Some(name) && arg.is_hide_set());
            if hidden {
                return Some(format!("--{name} is hidden: the binary's own, never typed"));
            }
        } else if let Some(sub) = current.find_subcommand(word) {
            current = sub;
        }
    }
    None
}

/// The user documentation: the root's, `docs/`'s and `docs/adopt/`'s
/// Markdown files.
fn documents(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for dir in [
        root.to_path_buf(),
        root.join("docs"),
        root.join("docs/adopt"),
    ] {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_file() && path.extension().is_some_and(|ext| ext == "md") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

#[test]
fn every_command_the_documentation_names_exists_with_its_flags() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut failures = Vec::new();
    let mut counted = Vec::new();
    for path in documents(&root) {
        let text = std::fs::read_to_string(&path).unwrap();
        let found = commands(&text);
        let name = path.strip_prefix(&root).unwrap().display().to_string();
        for (line, command) in &found {
            if let Some(why) = refusal(command) {
                failures.push(format!("{name}:{line}: `{}`: {why}", command.join(" ")));
            }
        }
        counted.push((name.replace('\\', "/"), found.len()));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    // An extraction that finds nothing would pass everything: the pages that
    // walk a reader through Owlshift name many commands each.
    for page in [
        "README.md",
        "SECURITY.md",
        "docs/adopt/linear.md",
        "docs/adopt/github-issues.md",
        "docs/adopt/markdown.md",
        "docs/markdown-tracker.md",
    ] {
        let count = counted
            .iter()
            .find(|(name, _)| name == page)
            .map(|(_, count)| *count);
        let floor = if page.starts_with("docs/adopt/") {
            8
        } else {
            2
        };
        assert!(
            count.is_some_and(|count| count >= floor),
            "{page}: {count:?} commands found, at least {floor} expected"
        );
    }
}

#[test]
fn a_command_is_read_from_code_blocks_and_inline_code() {
    let text = "\
Run `owlshift do DEMO-1`, then `git status`.

```bash
$ owlshift init --tracker markdown   # the first ticket too
OWLSHIFT_DATA_DIR=/tmp/d owlshift doctor && owlshift gate \\
  --at origin/main
curl -fsSL https://example.com/install.sh | sh
~/.local/bin/owlshift --version
```

```toml
# owlshift dance
```

```
owlshift init --gate \"cargo fmt --check && cargo test; make\" --gate 'a | b'
```
";
    let words = |command: &[&str]| command.iter().map(|w| w.to_string()).collect::<Vec<_>>();
    assert_eq!(
        commands(text),
        vec![
            (1, words(&["owlshift", "do", "DEMO-1"])),
            (4, words(&["owlshift", "init", "--tracker", "markdown"])),
            (5, words(&["owlshift", "doctor"])),
            (5, words(&["owlshift", "gate", "--at", "origin/main"])),
            (8, words(&["~/.local/bin/owlshift", "--version"])),
            (
                16,
                words(&[
                    "owlshift",
                    "init",
                    "--gate",
                    "cargo fmt --check && cargo test; make",
                    "--gate",
                    "a | b"
                ])
            ),
        ]
    );
}

#[test]
fn a_command_the_binary_lacks_is_refused() {
    let refused = |line: &str| {
        let command = commands_of_line(line).pop().unwrap();
        refusal(&command)
    };
    assert!(refused("owlshift dance").is_some());
    assert!(refused("owlshift init --tracker jira").is_some());
    assert!(refused("owlshift start --at-boot").is_some());
    assert!(refused("owlshift logs --last N").is_some());
    assert!(refused("owlshift watch --drain --served acme/web").is_some());
    assert!(refused("owlshift do DEMO-1 DEMO-2").is_some());
    // An option shown without its value is no command, unlike an omitted
    // positional.
    assert!(refused("owlshift logs --last").is_some());
    assert!(refused("owlshift init --tracker").is_some());
    assert!(refused("owlshift cancel DEMO-1 --reason").is_some());
    assert!(refused("owlshift gate --at").is_some());
    // Shown without what it needs, in a sentence: still a command.
    assert_eq!(refused("owlshift do"), None);
    assert_eq!(refused("owlshift config"), None);
    assert_eq!(refused("owlshift"), None);
    assert_eq!(refused("owlshift config show"), None);
    assert_eq!(refused("owlshift start --at-login"), None);
    assert_eq!(
        refused("owlshift init --tracker github --gate \"make test; make lint\""),
        None
    );
}
