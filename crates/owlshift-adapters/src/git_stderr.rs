//! The one-line summary of a failed git command's standard error (OWL-652).
//!
//! A failed remote command ends with fixed boilerplate that names no cause:
//! `Please make sure you have the correct access rights` and `and the
//! repository exists.`. The line before it does (`Permission denied
//! (publickey).`, `Connection closed by … port 22`, `ERROR: Repository not
//! found.`), and it is what tells a transient blip from a revoked key or a
//! renamed repository. [`summary`] drops the boilerplate and keeps the
//! lines that remain.

/// The longest summary, in characters, the ellipsis included.
pub const MAX_CHARS: usize = 400;

/// How many lines of the cause a summary keeps, at most.
const KEPT_LINES: usize = 3;

/// The lines git prints after the cause of a failed remote command (checked
/// live with `git ls-remote <missing path>`, git 2.54, `LC_ALL=C`, which every
/// git command of the runner sets).
const BOILERPLATE: [&str; 3] = [
    "fatal: Could not read from remote repository.",
    "Please make sure you have the correct access rights",
    "and the repository exists.",
];

/// What `stderr`, git's standard error, says about why a command failed, on
/// one line: its last three lines once git's fixed boilerplate is dropped
/// (all of them when nothing else is left), joined with ` / `, with the
/// `user:password@` part of every URL removed, and at most [`MAX_CHARS`]
/// characters. `no error output` when there is none.
pub fn summary(stderr: &str) -> String {
    // A lone carriage return ends a progress line: it is a line break here.
    let lines: Vec<&str> = stderr
        .split(['\n', '\r'])
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let causes: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|line| !BOILERPLATE.contains(line))
        .collect();
    let shown = if causes.is_empty() { &lines } else { &causes };
    if shown.is_empty() {
        return "no error output".to_owned();
    }
    let text = shown[shown.len().saturating_sub(KEPT_LINES)..].join(" / ");
    // Redact before cutting: a cut inside `user:token@host` would leave the
    // token without its `@`, and nothing to recognise it by.
    cap(&redact_userinfo(&text))
}

fn cap(text: &str) -> String {
    if text.chars().count() <= MAX_CHARS {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(MAX_CHARS - 1).collect();
    out.push('…');
    out
}

/// Removes the `user:password@` part of every URL in `text`.
fn redact_userinfo(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("://") {
        let (before, after) = rest.split_at(at + 3);
        out.push_str(before);
        let authority_end = after
            .find(|c: char| c == '/' || c.is_whitespace())
            .unwrap_or(after.len());
        let authority = &after[..authority_end];
        match authority.rfind('@') {
            Some(i) => out.push_str(&authority[i + 1..]),
            None => out.push_str(authority),
        }
        rest = &after[authority_end..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const REFUSED: &str = "git@github.com: Permission denied (publickey).\n\
        fatal: Could not read from remote repository.\n\n\
        Please make sure you have the correct access rights\n\
        and the repository exists.\n";

    #[test]
    fn a_refused_key_keeps_its_cause() {
        assert_eq!(
            summary(REFUSED),
            "git@github.com: Permission denied (publickey)."
        );
    }

    #[test]
    fn a_dropped_connection_keeps_its_line() {
        let closed = "Connection closed by 140.82.121.4 port 22\n\
            fatal: Could not read from remote repository.\n\n\
            Please make sure you have the correct access rights\n\
            and the repository exists.\n";
        assert_eq!(summary(closed), "Connection closed by 140.82.121.4 port 22");
        let reset = "kex_exchange_identification: read: Connection reset by peer\r\n\
            Connection reset by 140.82.121.4 port 22\r\n\
            fatal: Could not read from remote repository.\r\n\r\n\
            Please make sure you have the correct access rights\r\n\
            and the repository exists.\r\n";
        assert_eq!(
            summary(reset),
            "kex_exchange_identification: read: Connection reset by peer / \
             Connection reset by 140.82.121.4 port 22"
        );
    }

    #[test]
    fn a_missing_repository_keeps_its_cause() {
        let missing = "ERROR: Repository not found.\n\
            fatal: Could not read from remote repository.\n\n\
            Please make sure you have the correct access rights\n\
            and the repository exists.\n";
        assert_eq!(summary(missing), "ERROR: Repository not found.");
    }

    #[test]
    fn only_the_last_three_causes_are_kept() {
        assert_eq!(summary("a\nb\nc\nd\n"), "b / c / d");
    }

    #[test]
    fn boilerplate_alone_is_kept_and_silence_is_told() {
        assert_eq!(
            summary(
                "Please make sure you have the correct access rights\nand the repository exists.\n"
            ),
            "Please make sure you have the correct access rights / and the repository exists."
        );
        assert_eq!(summary(" \n\r\n"), "no error output");
    }

    #[test]
    fn a_lone_carriage_return_is_a_line_break() {
        assert_eq!(
            summary("remote: 1%\rremote: 100%\rfatal: bad"),
            "remote: 1% / remote: 100% / fatal: bad"
        );
    }

    #[test]
    fn credentials_are_removed_from_every_url() {
        assert_eq!(
            redact_userinfo("a https://x@h/p and ssh://git@h:22/p, http://h/q"),
            "a https://h/p and ssh://h:22/p, http://h/q"
        );
        let text =
            summary("fatal: unable to access 'https://u:ghp_secret@github.com/o/r.git/': 403");
        assert_eq!(
            text,
            "fatal: unable to access 'https://github.com/o/r.git/': 403"
        );
    }

    #[test]
    fn the_summary_is_capped_in_characters_after_redaction() {
        // A credentialed URL that straddles the cut must not leak its token.
        let line = format!("{}https://user:ghp_tok@host/x", "é".repeat(MAX_CHARS - 10));
        let text = summary(&line);
        assert!(
            text.chars().count() <= MAX_CHARS,
            "{}",
            text.chars().count()
        );
        assert!(text.ends_with('…'), "{text}");
        assert!(!text.contains("ghp_") && !text.contains("user:"), "{text}");
        let short = "x".repeat(MAX_CHARS);
        assert_eq!(summary(&short), short);
        assert!(!summary("a\n".repeat(500).as_str()).contains('\n'));
    }
}
