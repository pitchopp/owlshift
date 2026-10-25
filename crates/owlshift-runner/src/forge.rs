//! Opening the project's forge adapter with its credentials.
//!
//! The runner is the only holder of forge credentials: it reads the token
//! from the system keychain and hands it to the adapter, never to an agent
//! (architecture section 8). Pushing uses the user's own git credentials,
//! as in their terminal.

use std::path::PathBuf;
use std::sync::Arc;

use jiff::Timestamp;
use owlshift_adapters::forge::github::{Connect, GitHubForge, HttpTransport, Token, TokenSource};
use owlshift_adapters::forge::github_app::{self, DeviceFlow, HttpLogin, UserToken};
use owlshift_adapters::forge::{DefaultBranch, Error, ErrorKind, Repo};
use owlshift_contracts::config::PersonalConfig;
use owlshift_platform::keychain::{Keychain, SERVICE, Secret};

use crate::github_token::AppToken;
use crate::refusal::Refusal;
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

/// What the personal GitHub token of the expert option must allow, as
/// `owlshift init --github-token` tells its user (OWL-336). Contents (read) lets GitHub read the branches a pull request is opened on
/// (OWL-334): without it, `createPullRequest` answers "not all refs are
/// readable". Actions (read) lets watch read a red check's job log for its
/// fix run (OWL-112): GitHub documents it, and live check (c) of the build
/// plan is still to confirm it with a stored token.
pub const GITHUB_TOKEN_HELP: &str = "a fine-grained token with pull requests (write), contents, \
     checks, commit statuses, actions and metadata (read) on the project's repository";

/// What the GitHub App's token is refreshed with (OWL-384): the keychain it
/// is stored in, the project's account lock file
/// ([`ProjectDirs::account_lock_file`](crate::project::ProjectDirs::account_lock_file))
/// and the app whose client ID the refresh sends.
#[derive(Clone, Debug)]
pub struct AppAccess {
    pub keychain: Arc<Keychain>,
    pub lock: PathBuf,
    pub app: GitHubApp,
}

/// The GitHub adapter for `repo`, with the credential `found`: `owlshift
/// do` opens the pull request and reads the check set the
/// [writer](crate::writer) reports with it, and watch follows it.
///
/// A GitHub App entry gives the adapter its token through the Writer's
/// refresh ([`crate::github_token`]): refreshed when past its stored expiry
/// or refused with HTTP 401, under the account lock of `access`. A token
/// already expired is refreshed here, which may wait up to
/// [`ACCOUNT_LOCK_WAIT`](crate::github_token::ACCOUNT_LOCK_WAIT) for the
/// lock, so a refresh GitHub refuses stops the command before any work. A
/// personal token is never tried in the app token's place.
///
/// Every refusal is lasting but a GitHub that did not answer the refresh, or
/// whose budget is spent, which a restart may mend (OWL-435, [`Refusal`]).
pub fn github(
    found: &ForgeSecret<Secret>,
    repo: Repo,
    access: &AppAccess,
) -> Result<GitHubForge, Refusal> {
    match (&found.token, found.account.kind) {
        (Some(entry), ForgeKind::App) => {
            let flow = DeviceFlow::new(HttpLogin::new(), &access.app.client_id);
            let mut source = AppToken::new(
                access.keychain.clone(),
                found.account.token.clone(),
                access.lock.clone(),
                flow,
                entry,
            )
            .map_err(refusal)?;
            source.token().map_err(refusal)?;
            let connect: Connect = Box::new(|token| Box::new(HttpTransport::new(token)));
            Ok(GitHubForge::with_app(source, connect, repo))
        }
        (Some(token), ForgeKind::Personal) => {
            Ok(GitHubForge::new(Token::new(token.expose()), repo))
        }
        (None, _) => Err(no_token(found).into()),
    }
}

/// The refusal of a GitHub App token that could not be had: passing when
/// GitHub did not answer or its budget is spent, lasting otherwise.
fn refusal(error: Error) -> Refusal {
    match error.kind {
        ErrorKind::Other | ErrorKind::RateLimited => Refusal::passing(error.message),
        _ => error.message.into(),
    }
}

/// The token sent for `found` at the instant `now`, without the refresh:
/// a GitHub App token past its stored expiry is refused, naming `owlshift
/// init`. Only doctor's forge access probe reads it ([`default_branch`]),
/// until GA4 puts that probe on the Writer's refresh.
fn token_at(found: &ForgeSecret<Secret>, now: Timestamp) -> Result<Token, String> {
    let account = &found.account.token;
    match (&found.token, found.account.kind) {
        (Some(entry), ForgeKind::App) => {
            let token = UserToken::from_entry(entry.expose())
                .map_err(|_| sign_in_again(account, "does not read"))?;
            if token.expired(now) {
                return Err(sign_in_again(account, "has expired"));
            }
            Ok(Token::new(token.access()))
        }
        (Some(token), ForgeKind::Personal) => Ok(Token::new(token.expose())),
        (None, _) => Err(no_token(found)),
    }
}

/// The words of the refresh-failure rule (architecture section 8) for the
/// app token under `account`, which `what` happened to.
pub(crate) fn sign_in_again(account: &str, what: &str) -> String {
    format!(
        "the GitHub App's token under service `{SERVICE}`, account `{account}`, {what}: run \
         `owlshift init` in a terminal to sign in again"
    )
}

fn no_token(found: &ForgeSecret<Secret>) -> String {
    format!(
        "no GitHub token in the system keychain: run `owlshift init` in a terminal, or store \
         one under service `{SERVICE}`, account `{}`",
        found.account.token
    )
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
    let token = token_at(&found, Timestamp::now()).map_err(TokenTryError::Credential)?;
    GitHubForge::new(token, repo.clone())
        .default_branch()
        .map_err(TokenTryError::Forge)
}

/// What the project's GitHub credential gives merge on green's merge (D14,
/// OWL-445): Contents (read and write), read from the GitHub App
/// installation's granted permissions, which a personal token has none of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergePermission {
    /// The installation granted Contents (read and write).
    Granted,
    /// The installation, of this id, has not granted it.
    Missing { installation: u64 },
    /// No installation of the app lists the repository.
    NotInstalled,
    /// A personal token: GitHub says nothing of its permissions.
    PersonalToken,
}

/// What the GitHub credential of the project of `repo`, resolved and read
/// from `keychain` as `owlshift do` reads it, gives merge on green (OWL-445):
/// for the GitHub App's token, the installation of the app `slug` that
/// lists the repository, read-only (C12, 2026-10-19); a personal token is
/// not asked anything.
pub fn merge_permission(
    keychain: &Keychain,
    repo: &Repo,
    slug: &str,
) -> Result<MergePermission, TokenTryError> {
    let mut read = |account: &str| keychain.read(account).map_err(|error| error.to_string());
    let found = secrets::forge(repo, &mut read).map_err(TokenTryError::Credential)?;
    if found.account.kind == ForgeKind::Personal {
        return Ok(MergePermission::PersonalToken);
    }
    let token = token_at(&found, Timestamp::now()).map_err(TokenTryError::Credential)?;
    let installation = GitHubForge::new(token, repo.clone())
        .app_installation(slug)
        .map_err(TokenTryError::Forge)?;
    Ok(match installation {
        None => MergePermission::NotInstalled,
        Some(installation) if installation.contents_write => MergePermission::Granted,
        Some(installation) => MergePermission::Missing {
            installation: installation.id,
        },
    })
}

#[cfg(test)]
mod tests {
    use crate::secrets::ForgeAccount;

    use super::*;

    fn access(keychain: Keychain, dir: &std::path::Path) -> AppAccess {
        AppAccess {
            keychain: Arc::new(keychain),
            lock: dir.join("github-app.lock"),
            app: GitHubApp::from_personal(None),
        }
    }

    #[test]
    fn the_github_token_is_the_one_found() {
        let dir = tempfile::tempdir().unwrap();
        let access = access(Keychain::in_memory(), dir.path());
        let repo = Repo::parse("pitchopp/owlshift").unwrap();
        let mut found = ForgeSecret {
            account: ForgeAccount::project(&repo),
            token: None,
        };
        let missing = github(&found, repo.clone(), &access).unwrap_err();
        assert!(missing.lasting, "{missing}");
        let missing = missing.message;
        assert!(
            missing.contains("service `owlshift`, account `github.com/pitchopp/owlshift/github`"),
            "{missing}"
        );

        found.token = Some(Secret::new("ghp_test"));
        assert_eq!(github(&found, repo.clone(), &access).unwrap().repo(), &repo);
    }

    /// OWL-384: an app entry that is not expired builds the adapter on the
    /// Writer's refresh with no request; one that does not read names
    /// `owlshift init`, repeats nothing of the entry, and no personal token
    /// is tried instead. The refresh itself is `github_token`'s tests'.
    #[test]
    fn the_apps_token_goes_through_the_refresh_and_is_never_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let access = access(Keychain::in_memory(), dir.path());
        let repo = Repo::parse("pitchopp/owlshift").unwrap();
        let found = |entry: String| ForgeSecret {
            account: ForgeAccount::project_app(&repo),
            token: Some(Secret::new(entry)),
        };
        let live = UserToken::new(
            "ghu_SENTINEL",
            Some("ghr_SENTINEL".to_owned()),
            Some(
                Timestamp::now()
                    .checked_add(jiff::SignedDuration::from_hours(1))
                    .unwrap(),
            ),
            None,
        )
        .to_entry();
        assert_eq!(
            github(&found(live), repo.clone(), &access).unwrap().repo(),
            &repo
        );
        let error = github(&found("ghu_SENTINEL".to_owned()), repo.clone(), &access).unwrap_err();
        assert!(error.lasting, "{error}");
        let error = error.message;
        assert!(
            error.contains("account `github.com/pitchopp/owlshift/github-app`, does not read"),
            "{error}"
        );
        assert!(error.contains("`owlshift init`"), "{error}");
        assert!(!error.contains("SENTINEL"), "{error}");
    }

    /// The doctor probe's read, until GA4: an app entry gives its access
    /// token until its stored expiry; one expired or unreadable names
    /// `owlshift init`, never repeats the entry.
    #[test]
    fn the_probes_token_is_sent_until_its_expiry_and_never_replaced() {
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
        assert_eq!(token_at(&live, now).unwrap(), Token::new("ghu_SENTINEL"));

        for (dead, what) in [
            (found(entry(now.as_second())), "has expired"),
            (found("ghu_SENTINEL".to_owned()), "does not read"),
        ] {
            let error = token_at(&dead, now).unwrap_err();
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
