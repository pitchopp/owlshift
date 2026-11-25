//! Opening the project's forge adapter with its credentials.
//!
//! The runner is the only holder of forge credentials: it reads the token
//! from the system keychain and hands it to the adapter, never to an agent
//! (architecture section 8). Pushing uses the user's own git credentials,
//! as in their terminal.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use jiff::Timestamp;
use owlshift_adapters::forge::github::{Connect, GitHubForge, HttpTransport, Token, TokenSource};
use owlshift_adapters::forge::github_app::{self, DeviceFlow, HttpLogin, UserToken};
use owlshift_adapters::forge::{DefaultBranch, Error, ErrorKind, Repo};
use owlshift_contracts::config::PersonalConfig;
use owlshift_platform::keychain::{Keychain, SERVICE, Secret};
use owlshift_platform::service::sh_quote;

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
     checks, commit statuses, actions and metadata (read) on the project's repository, and \
     actions (write) for watch to re-run a red check once";

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

/// The refusal of a GitHub App token that could not be had: waiting for the
/// network when GitHub gave no answer at all (OWL-479), passing when it
/// answered with an error a retry may clear or its budget is spent, lasting
/// otherwise; lasting until a new sign-in when GitHub refused the token or
/// its entry no longer reads (OWL-650), which its command words with
/// [`sign_in_notice`] through [`Refusal::in_checkout`].
fn refusal(error: Error) -> Refusal {
    match error.kind {
        ErrorKind::Unreachable => Refusal::unreachable(error.message),
        ErrorKind::Other | ErrorKind::RateLimited => Refusal::passing(error.message),
        ErrorKind::SignInAgain => Refusal::sign_in_to_github(error.message),
        _ => error.message.into(),
    }
}

/// The token sent for `found` at the instant `now`, without the refresh.
/// Only doctor's forge probes read it ([`default_branch`], and the GitHub
/// Issues facts, OWL-537), until GA4 puts them on the Writer's refresh, so
/// it tells a GitHub App token only a new sign-in mends
/// ([`TokenTryError::SignIn`]: its entry does not read, or its access token
/// expired with no refresh token alive) from one the refresh it does not
/// make would renew ([`TokenTryError::Expired`], OWL-650).
pub(crate) fn token_at(
    found: &ForgeSecret<Secret>,
    now: Timestamp,
) -> Result<Token, TokenTryError> {
    let account = &found.account.token;
    match (&found.token, found.account.kind) {
        (Some(entry), ForgeKind::App) => {
            let token = UserToken::from_entry(entry.expose())
                .map_err(|_| TokenTryError::SignIn(sign_in_again(account, "does not read")))?;
            if token.expired(now) {
                if token.refresh().is_none() {
                    return Err(TokenTryError::SignIn(sign_in_again(
                        account,
                        "has expired and holds no refresh token",
                    )));
                }
                if token.refresh_expires_at().is_some_and(|at| now >= at) {
                    return Err(TokenTryError::SignIn(sign_in_again(
                        account,
                        "has a refresh token past its life",
                    )));
                }
                return Err(TokenTryError::Expired(sign_in_again(
                    account,
                    "has expired",
                )));
            }
            Ok(Token::new(token.access()))
        }
        (Some(token), ForgeKind::Personal) => Ok(Token::new(token.expose())),
        (None, _) => Err(TokenTryError::Credential(no_token(found))),
    }
}

/// The technical words of the refresh-failure rule (architecture section 8)
/// for the app token under `account`, which `what` happened to: they name
/// the keychain entry, for daemon.log and the terminal, on the line before
/// the [`sign_in_notice`] a person reads (OWL-650).
pub(crate) fn sign_in_again(account: &str, what: &str) -> String {
    format!(
        "the GitHub App's token under service `{SERVICE}`, account `{account}`, {what}: run \
         `owlshift init` in a terminal to sign in again"
    )
}

/// What a person reads when only a new sign-in to GitHub lets the project
/// of `repo` move again (OWL-650): what happened, what it stops, the command
/// and the folder to run it in, `checkout` shell-quoted, and what it keeps.
/// `owlshift init` keeps an existing project file and the secrets already
/// stored, and signs in again when the stored app token no longer works.
/// When `resumes`, the daemon tries the project again by itself, and the
/// notice says so; a command typed in a terminal does not.
pub fn sign_in_notice(repo: &Repo, checkout: &Path, resumes: bool) -> String {
    let mut notice = format!(
        "Owlshift can no longer reach GitHub for {repo}, so none of its tickets move. Sign in \
         again: `cd {}` then `owlshift init`. It shows a code to enter on github.com; your \
         owlshift.toml, your other stored keys and your tickets are kept.",
        sh_quote(&checkout.display().to_string())
    );
    if resumes {
        notice.push_str(" Owlshift picks the project up again within a minute.");
    }
    notice
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
    /// The GitHub App's token only a new sign-in mends (OWL-650): its entry
    /// does not read, or its access token expired with no refresh token
    /// alive. The technical words of [`sign_in_again`].
    SignIn(String),
    /// The GitHub App's access token is past its stored expiry and its
    /// refresh token alive: the refresh, which doctor does not make, would
    /// renew it (OWL-650). The technical words of [`sign_in_again`].
    Expired(String),
    /// GitHub's answer, or no answer.
    Forge(Error),
}

impl std::fmt::Display for TokenTryError {
    /// Its words: Owlshift's, or GitHub's.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Credential(words) | Self::SignIn(words) | Self::Expired(words) => {
                f.write_str(words)
            }
            Self::Forge(error) => write!(f, "{error}"),
        }
    }
}

impl From<TokenTryError> for String {
    fn from(error: TokenTryError) -> Self {
        error.to_string()
    }
}

/// What the GitHub token of the project of `repo`, resolved and read from
/// `keychain` as `owlshift do` reads it, reads of the repository's default
/// branch (OWL-334). Read-only: one query, the token never shown.
pub fn default_branch(keychain: &Keychain, repo: &Repo) -> Result<DefaultBranch, TokenTryError> {
    let mut read = |account: &str| keychain.read(account).map_err(|error| error.to_string());
    let found = secrets::forge(repo, &mut read).map_err(TokenTryError::Credential)?;
    let token = token_at(&found, Timestamp::now())?;
    GitHubForge::new(token, repo.clone())
        .default_branch()
        .map_err(TokenTryError::Forge)
}

/// What the project's GitHub credential was granted, read from the GitHub
/// App installation's permissions, which a personal token has none of:
/// Contents (read and write) for merge on green's merge (D14, OWL-445),
/// Actions (read and write) for watch's re-run of a red check (OWL-513).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppGrants {
    /// The installation, of this id, and what it granted.
    Installed {
        installation: u64,
        contents_write: bool,
        actions_write: bool,
    },
    /// No installation of the app lists the repository.
    NotInstalled,
    /// A personal token: GitHub says nothing of its permissions.
    PersonalToken,
}

/// What the GitHub credential of the project of `repo`, resolved and read
/// from `keychain` as `owlshift do` reads it, was granted (OWL-445,
/// OWL-513): for the GitHub App's token, the installation of the app `slug`
/// that lists the repository, read-only (C12, 2026-10-19); a personal token
/// is not asked anything.
pub fn app_grants(
    keychain: &Keychain,
    repo: &Repo,
    slug: &str,
) -> Result<AppGrants, TokenTryError> {
    let mut read = |account: &str| keychain.read(account).map_err(|error| error.to_string());
    let found = secrets::forge(repo, &mut read).map_err(TokenTryError::Credential)?;
    if found.account.kind == ForgeKind::Personal {
        return Ok(AppGrants::PersonalToken);
    }
    let token = token_at(&found, Timestamp::now())?;
    let installation = GitHubForge::new(token, repo.clone())
        .app_installation(slug)
        .map_err(TokenTryError::Forge)?;
    Ok(match installation {
        None => AppGrants::NotInstalled,
        Some(installation) => AppGrants::Installed {
            installation: installation.id,
            contents_write: installation.contents_write,
            actions_write: installation.actions_write,
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
        assert!(missing.lasting(), "{missing}");
        let missing = missing.message;
        assert!(
            missing.contains("service `owlshift`, account `github.com/pitchopp/owlshift/github`"),
            "{missing}"
        );

        found.token = Some(Secret::new("ghp_test"));
        assert_eq!(github(&found, repo.clone(), &access).unwrap().repo(), &repo);
    }

    /// OWL-479: no answer waits for the network, an error a retry may clear
    /// restarts, a refusal lasts.
    #[test]
    fn a_token_that_could_not_be_had_is_refused_by_what_mends_it() {
        let of = |kind| refusal(Error::new(kind, "x"));
        assert!(of(ErrorKind::Unreachable).unanswered());
        for kind in [ErrorKind::Other, ErrorKind::RateLimited] {
            let passing = of(kind);
            assert!(!passing.lasting() && !passing.unanswered(), "{kind:?}");
        }
        for kind in [ErrorKind::Unauthorized, ErrorKind::SignInAgain] {
            assert!(of(kind).lasting(), "{kind:?}");
        }
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
        assert!(error.lasting(), "{error}");
        let error = error.message;
        assert!(
            error.contains("account `github.com/pitchopp/owlshift/github-app`, does not read"),
            "{error}"
        );
        assert!(error.contains("`owlshift init`"), "{error}");
        assert!(!error.contains("SENTINEL"), "{error}");
    }

    /// The doctor probe's read, until GA4: an app entry gives its access
    /// token until its stored expiry. Past it, only a new sign-in mends an
    /// entry that does not read or whose refresh token is past its life, or
    /// gone; one whose refresh token is alive, or has no stored life, would
    /// be renewed by the refresh doctor does not make (OWL-650). Each names
    /// its keychain entry and never repeats it.
    #[test]
    fn the_probes_token_is_sent_until_its_expiry_and_never_replaced() {
        let repo = Repo::parse("pitchopp/owlshift").unwrap();
        let now = Timestamp::from_second(1_760_000_000).unwrap();
        let at = |second: i64| Timestamp::from_second(second).unwrap();
        let entry = |expires_at: i64, refresh: Option<&str>, refresh_expires_at: Option<i64>| {
            UserToken::new(
                "ghu_SENTINEL",
                refresh.map(str::to_owned),
                Some(at(expires_at)),
                refresh_expires_at.map(at),
            )
            .to_entry()
        };
        let found = |entry: String| ForgeSecret {
            account: ForgeAccount::project_app(&repo),
            token: Some(Secret::new(entry)),
        };
        let past = now.as_second();
        let ahead = now.as_second() + 1;
        let refresh = Some("ghr_SENTINEL");

        let live = found(entry(ahead, refresh, Some(past)));
        assert_eq!(token_at(&live, now).unwrap(), Token::new("ghu_SENTINEL"));

        for (dead, what, sign_in) in [
            (found("ghu_SENTINEL".to_owned()), "does not read", true),
            (
                found(entry(past, refresh, Some(past))),
                "has a refresh token past its life",
                true,
            ),
            (
                found(entry(past, None, None)),
                "has expired and holds no refresh token",
                true,
            ),
            (
                found(entry(past, refresh, Some(ahead))),
                "has expired",
                false,
            ),
            (found(entry(past, refresh, None)), "has expired", false),
        ] {
            let error = token_at(&dead, now).unwrap_err();
            let words = match (&error, sign_in) {
                (TokenTryError::SignIn(words), true) | (TokenTryError::Expired(words), false) => {
                    words
                }
                _ => panic!("{what}: {error:?}"),
            };
            assert!(
                words.contains(&format!(
                    "account `github.com/pitchopp/owlshift/github-app`, {what}:"
                )),
                "{words}"
            );
            assert!(words.contains("`owlshift init`"), "{words}");
            assert!(!words.contains("SENTINEL"), "{words}");
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

    /// OWL-650: the notice names the repository, the folder quoted for a
    /// shell, the command and what it keeps; only the daemon's watch adds
    /// that the project is picked up again.
    #[test]
    fn the_sign_in_notice_names_the_folder_quoted_and_what_is_kept() {
        let repo = Repo::parse("acme/web").unwrap();
        let folder = Path::new("/Users/ada/My Projects/web");
        assert_eq!(
            sign_in_notice(&repo, folder, true),
            "Owlshift can no longer reach GitHub for acme/web, so none of its tickets move. Sign \
             in again: `cd '/Users/ada/My Projects/web'` then `owlshift init`. It shows a code \
             to enter on github.com; your owlshift.toml, your other stored keys and your \
             tickets are kept. Owlshift picks the project up again within a minute."
        );
        let typed = sign_in_notice(&repo, folder, false);
        assert!(
            !typed.contains("Owlshift picks the project up again within a minute."),
            "{typed}"
        );
        assert!(typed.ends_with("your tickets are kept."), "{typed}");

        let quoted = sign_in_notice(&repo, Path::new("/tmp/ada's web"), false);
        assert!(quoted.contains(r"`cd '/tmp/ada'\''s web'`"), "{quoted}");
    }

    /// OWL-650: a refused sign-in is lasting and, said in a checkout, reads
    /// as the notice with its technical words kept as the detail; any other
    /// refusal is said as it was.
    #[test]
    fn only_a_refused_sign_in_is_reworded_in_its_checkout() {
        let repo = Repo::parse("acme/web").unwrap();
        let folder = Path::new("/work/web");
        let technical = sign_in_again("github.com/acme/web/github-app", "does not read");
        let refused = refusal(Error::new(ErrorKind::SignInAgain, technical.clone()));
        assert!(refused.lasting(), "{refused}");
        let said = refused.in_checkout(&repo, folder, true);
        assert!(said.lasting(), "{said}");
        assert_eq!(said.message, sign_in_notice(&repo, folder, true));
        assert_eq!(said.detail.as_deref(), Some(technical.as_str()));
        assert_eq!(
            said.clone().in_checkout(&repo, folder, false),
            said,
            "said once"
        );

        for other in [
            refusal(Error::new(ErrorKind::Unauthorized, "bad credentials")),
            Refusal::from("no Linear API key"),
        ] {
            let kept = other.clone().in_checkout(&repo, folder, true);
            assert_eq!(kept, other);
            assert_eq!(kept.detail, None);
        }
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
