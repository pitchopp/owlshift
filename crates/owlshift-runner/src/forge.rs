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
/// token per machine for now. A fine-grained token needs pull requests
/// (write), checks and commit statuses (read) and metadata (read) on the
/// project's repository; the output of `gh auth token` also works.
pub const GITHUB_ACCOUNT: &str = "github";

/// The GitHub adapter for `repo`, with its token read from `keychain`.
///
/// No command calls it yet: `owlshift do` (OWL-20) will, to open the pull
/// request and read the check set the [writer](crate::writer) reports;
/// `owlshift init` (OWL-20) will store the token.
pub fn github(keychain: &Keychain, repo: Repo) -> Result<GitHubForge, String> {
    match keychain.read(GITHUB_ACCOUNT) {
        Ok(Some(token)) => Ok(GitHubForge::new(Token::new(token.expose()), repo)),
        Ok(None) => Err(format!(
            "no GitHub token in the system keychain: store one under service \
             `{SERVICE}`, account `{GITHUB_ACCOUNT}`"
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
