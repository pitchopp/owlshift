//! Opening the project's forge adapter with its credentials.
//!
//! The runner is the only holder of forge credentials: it reads the token
//! from the system keychain and hands it to the adapter, never to an agent
//! (architecture section 8). Pushing uses the user's own git credentials,
//! as in their terminal.

use owlshift_adapters::forge::github::{GitHubForge, Token};
use owlshift_adapters::forge::{DefaultBranch, Error, Repo};
use owlshift_platform::keychain::{Keychain, SERVICE, Secret};

use crate::secrets::{self, ForgeSecret};

/// The keychain account, under [`SERVICE`], that every project of a machine
/// shared for the GitHub token before OWL-308, still read for a project with
/// no token of its own; a project's own account ends with this name
/// ([`crate::secrets`]). [`GITHUB_TOKEN_HELP`] says what the token needs.
pub const GITHUB_ACCOUNT: &str = "github";

/// What the GitHub token must allow, as `owlshift init` tells its user.
/// Contents (read) lets GitHub read the branches a pull request is opened on
/// (OWL-334): without it, `createPullRequest` answers "not all refs are
/// readable". Actions (read) lets watch read a red check's job log for its
/// fix run (OWL-112): GitHub documents it, and live check (c) of the build
/// plan is still to confirm it with a stored token.
pub const GITHUB_TOKEN_HELP: &str = "a fine-grained token with pull requests (write), contents, \
     checks, commit statuses, actions and metadata (read) on the project's repository; the \
     output of `gh auth token` also works";

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

/// Why the project's GitHub token could not be tried on its repository
/// (OWL-334).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenTryError {
    /// The keychain could not be read, or holds no token, in Owlshift's
    /// words.
    Credential(String),
    /// GitHub's answer, or no answer.
    Forge(Error),
}

/// What the GitHub token of the project of `repo`, resolved and read from
/// `keychain` as `owlshift do` reads it, reads of the repository's default
/// branch (OWL-334). Read-only: one query, the token never shown.
pub fn default_branch(keychain: &Keychain, repo: &Repo) -> Result<DefaultBranch, TokenTryError> {
    let mut read = |account: &str| keychain.read(account).map_err(|error| error.to_string());
    let found = secrets::forge(repo, &mut read).map_err(TokenTryError::Credential)?;
    github(&found, repo.clone())
        .map_err(TokenTryError::Credential)?
        .default_branch()
        .map_err(TokenTryError::Forge)
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

    /// OWL-334: no token stored is said before any request.
    #[test]
    fn the_default_branch_is_not_asked_without_a_token() {
        let repo = Repo::parse("pitchopp/owlshift").unwrap();
        let Err(TokenTryError::Credential(reason)) = default_branch(&Keychain::in_memory(), &repo)
        else {
            panic!("a token was tried");
        };
        assert!(reason.starts_with("no GitHub token"), "{reason}");
    }
}
