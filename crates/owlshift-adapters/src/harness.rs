//! What Owlshift knows about each harness CLI before running a role on it:
//! its program name, how it reports its login, and how a user fixes a
//! missing or logged-out CLI.
//!
//! Login state comes from the CLI's own status command, never from its
//! credential files. The rules below are those of live check C8 in
//! `docs/design/build-plan.md` (2026-09-28): the exit status gives the
//! verdict, a fixed list of known values gives the method, and the command's
//! output is never echoed, since Codex prints part of an API key and Claude
//! Code prints the account's e-mail address and organisation.

use serde::Deserialize;

use owlshift_contracts::Harness;

/// The program name of a harness CLI.
pub fn program(harness: Harness) -> &'static str {
    match harness {
        Harness::Claude => "claude",
        Harness::Codex => "codex",
    }
}

/// The arguments of the status command that reports the CLI's login.
pub fn login_status_args(harness: Harness) -> &'static [&'static str] {
    match harness {
        Harness::Claude => &["auth", "status", "--json"],
        Harness::Codex => &["login", "status"],
    }
}

/// How to install a missing CLI.
pub fn install_hint(harness: Harness) -> &'static str {
    match harness {
        Harness::Claude => "install Claude Code: https://code.claude.com/docs/en/setup",
        Harness::Codex => {
            "install Codex: npm install -g @openai/codex (see https://developers.openai.com/codex/cli)"
        }
    }
}

/// How to log a CLI in.
pub fn login_hint(harness: Harness) -> &'static str {
    match harness {
        Harness::Claude => "run `claude auth login`",
        Harness::Codex => "run `codex login`",
    }
}

/// The login state a status command reported.
///
/// Every label is a `&'static str` from a fixed list, so nothing the command
/// printed can end up in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Login {
    LoggedIn {
        /// How the CLI is authenticated, such as `claude.ai` or `API key`.
        method: &'static str,
        /// The subscription, when the CLI reports a known one.
        plan: Option<&'static str>,
    },
    LoggedOut,
    /// The status command answered in a way C8 did not record.
    Unknown,
}

/// Reads the output of [`login_status_args`] for a harness.
pub fn parse_login(harness: Harness, code: Option<i32>, stdout: &[u8], stderr: &[u8]) -> Login {
    match harness {
        Harness::Claude => parse_claude(code, stdout),
        Harness::Codex => parse_codex(code, stdout, stderr),
    }
}

/// The only fields read from `claude auth status --json`; the others,
/// personal data among them, are skipped by the parser.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeStatus {
    logged_in: bool,
    #[serde(default)]
    auth_method: Option<String>,
    #[serde(default)]
    subscription_type: Option<String>,
}

fn parse_claude(code: Option<i32>, stdout: &[u8]) -> Login {
    let Ok(status) = serde_json::from_slice::<ClaudeStatus>(stdout) else {
        return Login::Unknown;
    };
    match (code, status.logged_in) {
        (Some(0), true) => Login::LoggedIn {
            method: match status.auth_method.as_deref() {
                Some("claude.ai") => "claude.ai",
                Some("api_key") => "API key",
                _ => "another method",
            },
            plan: match status.subscription_type.as_deref() {
                Some("pro") => Some("pro"),
                Some("max") => Some("max"),
                Some("team") => Some("team"),
                Some("enterprise") => Some("enterprise"),
                _ => None,
            },
        },
        (Some(1), false) => Login::LoggedOut,
        _ => Login::Unknown,
    }
}

fn parse_codex(code: Option<i32>, stdout: &[u8], stderr: &[u8]) -> Login {
    // C8: the answer is one line on stderr; stdout is read as a fallback.
    let text = if stderr.iter().all(u8::is_ascii_whitespace) {
        stdout
    } else {
        stderr
    };
    let text = String::from_utf8_lossy(text);
    let line = text.trim_start().lines().next().unwrap_or("").trim();
    match code {
        Some(0) if line.starts_with("Logged in using ChatGPT") => Login::LoggedIn {
            method: "ChatGPT",
            plan: None,
        },
        Some(0) if line.starts_with("Logged in using an API key") => Login::LoggedIn {
            method: "API key",
            plan: None,
        },
        Some(1) if line == "Not logged in" => Login::LoggedOut,
        _ => Login::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A logged-in answer carrying personal data, shaped as C8 recorded it.
    const CLAUDE_LOGGED_IN: &str = r#"{
      "loggedIn": true,
      "authMethod": "claude.ai",
      "apiProvider": "firstParty",
      "email": "ada@example.com",
      "orgId": "0f0e0d0c-org-id",
      "orgName": "Ada's Organization",
      "subscriptionType": "max"
    }"#;

    const CLAUDE_LOGGED_OUT: &str = r#"{
      "loggedIn": false,
      "authMethod": "none",
      "apiProvider": "firstParty"
    }"#;

    fn claude(code: i32, stdout: &str) -> Login {
        parse_login(Harness::Claude, Some(code), stdout.as_bytes(), b"")
    }

    fn codex(code: i32, stderr: &str) -> Login {
        parse_login(Harness::Codex, Some(code), b"", stderr.as_bytes())
    }

    #[test]
    fn claude_answers() {
        assert_eq!(
            claude(0, CLAUDE_LOGGED_IN),
            Login::LoggedIn {
                method: "claude.ai",
                plan: Some("max")
            }
        );
        assert_eq!(claude(1, CLAUDE_LOGGED_OUT), Login::LoggedOut);
        let api_key =
            r#"{"loggedIn": true, "authMethod": "api_key", "apiKeySource": "ANTHROPIC_API_KEY"}"#;
        assert_eq!(
            claude(0, api_key),
            Login::LoggedIn {
                method: "API key",
                plan: None
            }
        );
    }

    #[test]
    fn claude_answers_that_do_not_agree_or_parse_are_unknown() {
        assert_eq!(claude(1, CLAUDE_LOGGED_IN), Login::Unknown);
        assert_eq!(claude(0, CLAUDE_LOGGED_OUT), Login::Unknown);
        assert_eq!(claude(0, "Logged in as ada@example.com"), Login::Unknown);
    }

    #[test]
    fn codex_answers() {
        assert_eq!(
            codex(0, "Logged in using ChatGPT\n"),
            Login::LoggedIn {
                method: "ChatGPT",
                plan: None
            }
        );
        assert_eq!(
            codex(0, "Logged in using an API key - sk-dummy***0fake\n"),
            Login::LoggedIn {
                method: "API key",
                plan: None
            }
        );
        assert_eq!(codex(1, "Not logged in\n"), Login::LoggedOut);
    }

    #[test]
    fn codex_answers_that_do_not_agree_or_match_are_unknown() {
        assert_eq!(codex(1, "Logged in using ChatGPT\n"), Login::Unknown);
        assert_eq!(codex(0, "Signed in as ada@example.com\n"), Login::Unknown);
        assert_eq!(codex(2, "error: unexpected argument\n"), Login::Unknown);
    }
}
