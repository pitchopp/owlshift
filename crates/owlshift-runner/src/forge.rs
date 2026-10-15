//! Opening the project's forge adapter with its credentials.
//!
//! The runner is the only holder of forge credentials: it reads the token
//! from the system keychain and hands it to the adapter, never to an agent
//! (architecture section 8). Pushing uses the user's own git credentials,
//! as in their terminal.

use owlshift_adapters::forge::Repo;
use owlshift_adapters::forge::github::{GitHubForge, Token};
use owlshift_platform::keychain::{SERVICE, Secret};

use crate::secrets::ForgeSecret;

/// The keychain account, under [`SERVICE`], that every project of a machine
/// shared for the GitHub token before OWL-308, still read for a project with
/// no token of its own; a project's own account ends with this name
/// ([`crate::secrets`]). [`GITHUB_TOKEN_HELP`] says what the token needs.
pub const GITHUB_ACCOUNT: &str = "github";

/// What the GitHub token must allow, as `owlshift init` tells its user.
/// Actions (read) lets watch read a red check's job log for its fix run
/// (OWL-112): GitHub documents it, and live check (c) of the build plan is
/// still to confirm it with a stored token.
pub const GITHUB_TOKEN_HELP: &str = "a fine-grained token with pull requests (write), checks, \
     commit statuses, actions and metadata (read) on the project's repository; the output of \
     `gh auth token` also works";

/// The GitHub adapter for `repo`, with the token `found`: `owlshift do`
/// opens the pull request and reads the check set the
/// [writer](crate::writer) reports with it; `owlshift init` stores the
/// token.
pub fn github(found: &ForgeSecret<Secret>, repo: Repo) -> Result<GitHubForge, String> {
    match &found.token {
        Some(token) => Ok(GitHubForge::new(Token::new(token.expose()), repo)),
        None => Err(format!(
            "no GitHub token in the system keychain: run `owlshift init` in a terminal, or \
             store one under service `{SERVICE}`, account `{}`",
            found.account.token
        )),
    }
}

#[cfg(test)]
mod tests {
    use crate::secrets::ForgeAccount;

    use super::*;

    #[test]
    fn the_github_token_is_the_one_found() {
        let repo = Repo::parse("pitchopp/owlshift").unwrap();
        let mut found = ForgeSecret {
            account: ForgeAccount::project(&repo),
            token: None,
        };
        let missing = github(&found, repo.clone()).unwrap_err();
        assert!(
            missing.contains("service `owlshift`, account `github.com/pitchopp/owlshift/github`"),
            "{missing}"
        );

        found.token = Some(Secret::new("ghp_test"));
        assert_eq!(github(&found, repo.clone()).unwrap().repo(), &repo);
    }
}
