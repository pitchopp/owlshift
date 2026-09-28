//! Opening the project's forge adapter with its credentials.
//!
//! The runner is the only holder of forge credentials: it reads the token
//! from the system keychain and hands it to the adapter, never to an agent
//! (architecture section 8). Pushing uses the user's own git credentials,
//! as in their terminal.

use owlshift_adapters::forge::Repo;
use owlshift_adapters::forge::github::{GitHubForge, Token};
use owlshift_platform::keychain::{Keychain, SERVICE};

/// The keychain account, under [`SERVICE`], that holds the GitHub token: one
/// token per machine for now. [`GITHUB_TOKEN_HELP`] says what it needs.
pub const GITHUB_ACCOUNT: &str = "github";

/// What the GitHub token must allow, as `owlshift init` tells its user.
pub const GITHUB_TOKEN_HELP: &str = "a fine-grained token with pull requests (write), checks, \
     commit statuses and metadata (read) on the project's repository; the output of \
     `gh auth token` also works";

/// The GitHub adapter for `repo`, with its token read from `keychain`:
/// `owlshift do` opens the pull request and reads the check set the
/// [writer](crate::writer) reports with it; `owlshift init` stores the
/// token.
pub fn github(keychain: &Keychain, repo: Repo) -> Result<GitHubForge, String> {
    match keychain.read(GITHUB_ACCOUNT) {
        Ok(Some(token)) => Ok(GitHubForge::new(Token::new(token.expose()), repo)),
        Ok(None) => Err(format!(
            "no GitHub token in the system keychain: run `owlshift init` in a terminal, or \
             store one under service `{SERVICE}`, account `{GITHUB_ACCOUNT}`"
        )),
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use owlshift_platform::keychain::Secret;

    use super::*;

    #[test]
    fn the_github_token_comes_from_the_keychain() {
        let keychain = Keychain::in_memory();
        let repo = Repo::parse("pitchopp/owlshift").unwrap();
        let missing = github(&keychain, repo.clone()).unwrap_err();
        assert!(
            missing.contains("service `owlshift`, account `github`"),
            "{missing}"
        );

        keychain
            .store(GITHUB_ACCOUNT, &Secret::new("ghp_test"))
            .unwrap();
        assert_eq!(github(&keychain, repo.clone()).unwrap().repo(), &repo);
    }
}
