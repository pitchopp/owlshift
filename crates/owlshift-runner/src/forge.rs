//! Opening the project's forge adapter with its credentials.
//!
//! The runner is the only holder of forge credentials: it reads the token
//! from the system keychain and hands it to the adapter, never to an agent
//! (architecture section 8). Pushing uses the user's own git credentials,
//! as in their terminal.

use jiff::Timestamp;
use owlshift_adapters::forge::github::{GitHubForge, Token};
use owlshift_adapters::forge::github_app::{self, UserToken};
use owlshift_adapters::forge::{DefaultBranch, Error, Repo};
use owlshift_contracts::config::PersonalConfig;
use owlshift_platform::keychain::{Keychain, SERVICE, Secret};

use crate::secrets::{self, ForgeKind, ForgeSecret};

/// The keychain account, under [`SERVICE`], that every project of a machine
/// shared for the GitHub token before OWL-308, still read for a project with
/// no token of its own; a project's own account ends with this name
/// ([`crate::secrets`]). [`GITHUB_TOKEN_HELP`] says what the token needs.
pub const GITHUB_ACCOUNT: &str = "github";

/// The name of a project's keychain account, under [`SERVICE`], that holds
/// the user token of Owlshift's GitHub App, its refresh token and expiries
/// as one entry (OWL-336, architecture section 8): the project's account
/// `github.com/<owner>/<repo>/github-app` ([`crate::secrets`]). No shared
/// account of this name is read.
pub const GITHUB_APP_ACCOUNT: &str = "github-app";

/// The GitHub App `owlshift init` signs in through: Owlshift's own (D13),
/// or the one the personal file names for a fork or a private app.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitHubApp {
    /// The client ID the device flow and the refresh send.
    pub client_id: String,
    /// The slug the install link is made of.
    pub slug: String,
}

impl GitHubApp {
    /// The app of D13, each value replaced by its override in `personal`,
    /// the loaded personal file, when it sets one.
    pub fn from_personal(personal: Option<&PersonalConfig>) -> Self {
        let set = |value: Option<&String>, default: &str| {
            value.map_or_else(|| default.to_owned(), Clone::clone)
        };
        Self {
            client_id: set(
                personal.and_then(|p| p.github_app_client_id.as_ref()),
                github_app::CLIENT_ID,
            ),
            slug: set(
                personal.and_then(|p| p.github_app_slug.as_ref()),
                github_app::SLUG,
            ),
        }
    }

    /// Where the app is installed on the user's repositories.
    pub fn install_link(&self) -> String {
        github_app::install_link(&self.slug)
    }
}

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
    github_at(found, repo, Timestamp::now())
}

/// [`github`] at the instant `now`. A GitHub App entry gives its access token
/// as stored: its refresh under the account lock is GA3's (architecture
/// section 8). Until then a token past its stored expiry is not sent, and
/// the error names `owlshift init`, which signs in again. A personal token
/// is never tried in its place.
fn github_at(
    found: &ForgeSecret<Secret>,
    repo: Repo,
    now: Timestamp,
) -> Result<GitHubForge, String> {
    let sign_in_again = |what: &str| {
        format!(
            "the GitHub App's token under service `{SERVICE}`, account `{}`, {what}: run \
             `owlshift init` in a terminal to sign in again",
            found.account.token
        )
    };
    match (&found.token, found.account.kind) {
        (Some(entry), ForgeKind::App) => {
            let token = UserToken::from_entry(entry.expose())
                .map_err(|_| sign_in_again("does not read"))?;
            if token.expired(now) {
                return Err(sign_in_again("has expired"));
            }
            Ok(GitHubForge::new(Token::new(token.access()), repo))
        }
        (Some(token), ForgeKind::Personal) => {
            Ok(GitHubForge::new(Token::new(token.expose()), repo))
        }
        (None, _) => Err(format!(
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

    /// OWL-382: an app entry gives its access token, unrefreshed until GA3;
    /// one expired or unreadable names `owlshift init`, never repeats the
    /// entry, and no personal token is tried instead.
    #[test]
    fn the_apps_token_is_sent_until_its_expiry_and_never_replaced() {
        let repo = Repo::parse("pitchopp/owlshift").unwrap();
        let now = Timestamp::from_second(1_760_000_000).unwrap();
        let entry = |expires_at: i64| {
            UserToken::new(
                "ghu_SENTINEL",
                Some("ghr_SENTINEL".to_owned()),
                Some(Timestamp::from_second(expires_at).unwrap()),
                None,
            )
            .to_entry()
        };
        let found = |entry: String| ForgeSecret {
            account: ForgeAccount::project_app(&repo),
            token: Some(Secret::new(entry)),
        };

        let live = found(entry(now.as_second() + 1));
        assert_eq!(github_at(&live, repo.clone(), now).unwrap().repo(), &repo);

        for (dead, what) in [
            (found(entry(now.as_second())), "has expired"),
            (found("ghu_SENTINEL".to_owned()), "does not read"),
        ] {
            let error = github_at(&dead, repo.clone(), now).unwrap_err();
            assert!(
                error.contains(&format!(
                    "account `github.com/pitchopp/owlshift/github-app`, {what}"
                )),
                "{error}"
            );
            assert!(error.contains("`owlshift init`"), "{error}");
            assert!(!error.contains("SENTINEL"), "{error}");
        }
    }

    #[test]
    fn the_app_is_owlshifts_unless_the_personal_file_names_another() {
        let ours = GitHubApp::from_personal(None);
        assert_eq!(
            (ours.client_id.as_str(), ours.slug.as_str()),
            ("Iv23li5EhBqiuljIPbiK", "owl-shift")
        );
        assert_eq!(
            ours.install_link(),
            "https://github.com/apps/owl-shift/installations/new"
        );
        let empty = PersonalConfig::parse("").unwrap();
        assert_eq!(GitHubApp::from_personal(Some(&empty)), ours);

        let personal = PersonalConfig::parse(
            "github_app_client_id = \"Iv1.0123abcd\"\ngithub_app_slug = \"my-owlshift\"\n",
        )
        .unwrap();
        let theirs = GitHubApp::from_personal(Some(&personal));
        assert_eq!(theirs.client_id, "Iv1.0123abcd");
        assert_eq!(
            theirs.install_link(),
            "https://github.com/apps/my-owlshift/installations/new"
        );
        let slug_only = PersonalConfig::parse("github_app_slug = \"my-owlshift\"\n").unwrap();
        assert_eq!(
            GitHubApp::from_personal(Some(&slug_only)).client_id,
            "Iv23li5EhBqiuljIPbiK"
        );
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
