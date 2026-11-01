//! `owlshift init`'s GitHub sign-in through Owlshift's GitHub App (OWL-336,
//! GA2; build plan, "`init`", "The GitHub sign-in").
//!
//! A stored app token that works is kept: one that reads and that GitHub
//! accepts, refreshed first through the Writer's refresh under the account
//! lock when past its stored expiry or refused ([`crate::github_token`],
//! OWL-384). One whose refresh is refused or whose rotation is lost counts
//! as none, and `init` signs in again. The sign-in shows the device code and polls at GitHub's interval
//! until the code's life ends; then C12's installation lookup must list the
//! repository, or the install link is shown and the lookup made again every
//! [`INSTALL_POLL`] for [`INSTALL_WAIT`]. The token is stored only once an
//! installation lists the repository, so Cancel, an expired code or no
//! installation leave the keychain as it was. No token, no device code and
//! nothing of an answer's body is ever said: GitHub's failures are told by
//! their kind alone.

use std::path::PathBuf;
use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use owlshift_adapters::forge::github::{GitHubForge, Installation, Token};
use owlshift_adapters::forge::github_app::{AppError, DeviceCode, DeviceFlow, Login, Poll};
use owlshift_adapters::forge::{Error, ErrorKind, Repo};
use owlshift_platform::keychain::{Keychain, SERVICE, Secret};

use crate::forge::GitHubApp;
use crate::github_token::AppToken;
use crate::secrets::ForgeAccount;

/// How often the installation is looked up again once the install link is
/// shown.
pub const INSTALL_POLL: Duration = Duration::from_secs(5);

/// How long `init` waits for the installation once the install link is
/// shown.
pub const INSTALL_WAIT: Duration = Duration::from_secs(10 * 60);

/// The clock and the sleep the waits run on: [`SystemWait`] in production,
/// a clock that a sleep moves forward in tests.
pub trait Wait {
    fn now(&self) -> Timestamp;
    fn sleep(&self, duration: Duration);
}

/// The system clock and [`std::thread::sleep`].
#[derive(Debug, Default)]
pub struct SystemWait;

impl Wait for SystemWait {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// GitHub, as the sign-in reaches it: the app's device flow, which also
/// refreshes a stored token under the account lock file `lock`
/// ([`crate::github_token`]), and the REST API through a token (`connect`),
/// which the installation lookup reads.
pub struct GitHub<L: Login> {
    pub flow: DeviceFlow<L>,
    pub connect: Box<dyn Fn(Token) -> GitHubForge>,
    pub lock: PathBuf,
}

impl<L: Login> GitHub<L> {
    /// GitHub over HTTPS for `repo`, through `flow`, refreshing under `lock`.
    pub fn live(flow: DeviceFlow<L>, repo: &Repo, lock: PathBuf) -> Self {
        let repo = repo.clone();
        Self {
            flow,
            connect: Box::new(move |token| GitHubForge::new(token, repo.clone())),
            lock,
        }
    }
}

/// How the sign-in step ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The stored app token works, and an installation lists the
    /// repository: kept.
    Kept,
    /// Signed in, and the new token stored.
    Stored,
    /// No working app token, and no terminal to sign in on: nothing done.
    NeedsTerminal,
    /// Cancelled on GitHub: nothing stored.
    Denied,
    /// The code's life ended before it was entered: nothing stored.
    Expired,
    /// No installation of the app lists the repository past the wait, or
    /// with no terminal to wait on: nothing stored. `link` installs it.
    NotInstalled { link: String },
    /// Anything else, in Owlshift's words: nothing stored, unless a
    /// keychain write is what failed.
    Failed(String),
}

impl Outcome {
    /// Whether `init` may end well.
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Kept | Self::Stored)
    }
}

/// What is asked of the step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ask {
    /// `--replace-secrets`: sign in again over a stored app token.
    pub replace: bool,
    /// Whether a person is there to enter the code and install the app.
    pub terminal: bool,
}

/// Makes sure the project of `repo` holds a working app token under its
/// account `github-app`, a stored one refreshed under `github`'s account
/// lock, signing in through `app` on `github` when it does not; `say` is
/// told each step a person must act on or wait for.
pub fn step<L: Login>(
    keychain: &Keychain,
    repo: &Repo,
    app: &GitHubApp,
    github: &GitHub<L>,
    wait: &dyn Wait,
    ask: Ask,
    say: &mut dyn FnMut(&str),
) -> Outcome {
    let account = ForgeAccount::project_app(repo);
    if !ask.replace {
        let stored = match keychain.read(&account.token) {
            Ok(stored) => stored,
            Err(error) => return Outcome::Failed(error.to_string()),
        };
        if let Some(entry) = stored {
            match stored_installation(keychain, &account.token, &entry, app, github, wait) {
                Ok((_, Some(_))) => return Outcome::Kept,
                Ok((forge, None)) => {
                    return match installed(&forge, repo, app, wait, ask.terminal, say) {
                        Ok(()) => Outcome::Kept,
                        Err(outcome) => outcome,
                    };
                }
                Err(error) if error.kind == ErrorKind::SignInAgain => say(
                    "The GitHub App's token stored for this project can no longer be refreshed \
                     or does not read: signing in again.",
                ),
                Err(error) if error.kind == ErrorKind::Unauthorized => say(
                    "GitHub refuses the GitHub App's token stored for this project: signing in \
                     again.",
                ),
                Err(error) => return Outcome::Failed(words(&error)),
            }
        }
    }
    if !ask.terminal {
        return Outcome::NeedsTerminal;
    }
    let token = match device_sign_in(&github.flow, wait, say) {
        Ok(token) => token,
        Err(outcome) => return outcome,
    };
    let forge = (github.connect)(Token::new(token.access()));
    match forge.app_installation(&app.slug) {
        Ok(Some(_)) => {}
        Ok(None) => {
            if let Err(outcome) = installed(&forge, repo, app, wait, ask.terminal, say) {
                return outcome;
            }
        }
        Err(error) => return Outcome::Failed(words(&error)),
    }
    match keychain.store(&account.token, &Secret::new(token.to_entry())) {
        Ok(()) => Outcome::Stored,
        Err(error) => Outcome::Failed(format!(
            "signed in, but storing the GitHub App's token under service `{SERVICE}`, account \
             `{}` failed: {error}",
            account.token
        )),
    }
}

/// The installation lookup made with the app token stored as `entry`
/// under `account`, through the Writer's refresh ([`AppToken`], OWL-384):
/// refreshed first when past its stored expiry, and once more when GitHub
/// refuses it (`forge::Error` does not tell a 401 from a 403 here, so
/// either gets the one refresh). The forge it was made with, for the
/// install wait.
fn stored_installation<L: Login>(
    keychain: &Keychain,
    account: &str,
    entry: &Secret,
    app: &GitHubApp,
    github: &GitHub<L>,
    wait: &dyn Wait,
) -> Result<(GitHubForge, Option<Installation>), Error> {
    let mut source = AppToken::new(keychain, account, github.lock.clone(), &github.flow, entry)?;
    let token = source.token_at(wait.now())?;
    let forge = (github.connect)(token.clone());
    match forge.app_installation(&app.slug) {
        Err(error) if error.kind == ErrorKind::Unauthorized => {
            let Some(next) = source.refused_at(&token, wait.now())? else {
                return Err(error);
            };
            let forge = (github.connect)(next);
            forge
                .app_installation(&app.slug)
                .map(|found| (forge, found))
        }
        looked => looked.map(|found| (forge, found)),
    }
}

/// Removes the project's app token, so that the personal token just stored
/// or kept is the one used (`--github-token`); whether there was one.
pub fn remove_app_token(keychain: &Keychain, repo: &Repo) -> Result<bool, String> {
    let account = ForgeAccount::project_app(repo);
    keychain.delete(&account.token).map_err(|error| {
        format!(
            "removing the GitHub App's token under service `{SERVICE}`, account `{}` failed \
             ({error}): it is still the one used, before any personal token",
            account.token
        )
    })
}

/// The device flow, from the code shown to the token: polled at the
/// interval GitHub gives, raised on `slow_down`, and never past the code's
/// life (C10).
fn device_sign_in<L: Login>(
    flow: &DeviceFlow<L>,
    wait: &dyn Wait,
    say: &mut dyn FnMut(&str),
) -> Result<owlshift_adapters::forge::github_app::UserToken, Outcome> {
    let code = flow.request_code().map_err(ended)?;
    say(&shown(&code, wait.now()));
    let mut interval = code.interval;
    loop {
        let Some(remaining) = left(code.expires_at, wait.now()) else {
            return Err(Outcome::Expired);
        };
        wait.sleep(interval.min(remaining));
        if left(code.expires_at, wait.now()).is_none() {
            return Err(Outcome::Expired);
        }
        match flow.poll(&code, interval) {
            Ok(Poll::Pending) => {}
            Ok(Poll::SlowDown(slower)) => interval = slower,
            Ok(Poll::Granted(token)) => return Ok(token),
            Err(error) => return Err(ended(error)),
        }
    }
}

/// What a person is told to do with `code`.
fn shown(code: &DeviceCode, now: Timestamp) -> String {
    let minutes = left(code.expires_at, now).map_or(0, |left| left.as_secs().div_ceil(60));
    format!(
        "To sign in to GitHub through Owlshift's GitHub App, open {} and enter the code {}. \
         Waiting for it (the code works for {minutes} minutes).",
        code.verification_uri, code.user_code
    )
}

/// Waits for an installation of `app` to list `repo`, showing its install
/// link; an error ends the step.
fn installed(
    forge: &GitHubForge,
    repo: &Repo,
    app: &GitHubApp,
    wait: &dyn Wait,
    terminal: bool,
    say: &mut dyn FnMut(&str),
) -> Result<(), Outcome> {
    let link = app.install_link();
    let not_installed = || Outcome::NotInstalled { link: link.clone() };
    if !terminal {
        return Err(not_installed());
    }
    say(&format!(
        "Owlshift's GitHub App is not installed on github.com/{repo}: install it at {link} and \
         give it this repository. Waiting for it, up to {} minutes.",
        INSTALL_WAIT.as_secs() / 60
    ));
    let until = wait
        .now()
        .checked_add(SignedDuration::try_from(INSTALL_WAIT).expect("ten minutes"))
        .map_err(|_| Outcome::Failed("the clock is out of range".to_owned()))?;
    while let Some(remaining) = left(until, wait.now()) {
        wait.sleep(INSTALL_POLL.min(remaining));
        match forge.app_installation(&app.slug) {
            Ok(Some(_)) => return Ok(()),
            Ok(None) => {}
            Err(error) => return Err(Outcome::Failed(words(&error))),
        }
    }
    Err(not_installed())
}

/// The time from `now` to `at`; `None` once `at` is reached.
fn left(at: Timestamp, now: Timestamp) -> Option<Duration> {
    Duration::try_from(at.duration_since(now))
        .ok()
        .filter(|left| !left.is_zero())
}

/// How a device-flow request ended the step.
fn ended(error: AppError) -> Outcome {
    match error {
        AppError::Denied => Outcome::Denied,
        AppError::Expired => Outcome::Expired,
        // All are in Owlshift's words, never the answer's.
        other @ (AppError::Refused(_) | AppError::Unreachable(_) | AppError::Failed(_)) => {
            Outcome::Failed(other.to_string())
        }
    }
}

/// A forge failure by its kind alone: a REST error may carry GitHub's
/// message, which is not repeated.
fn words(error: &Error) -> String {
    match error.kind {
        ErrorKind::Unauthorized | ErrorKind::SignInAgain => {
            "GitHub refused the GitHub App's token".to_owned()
        }
        ErrorKind::RateLimited => "GitHub's rate limit is spent: try again later".to_owned(),
        ErrorKind::NotFound | ErrorKind::HeadMoved | ErrorKind::Unreachable | ErrorKind::Other => {
            "the GitHub App's installations could not be read from GitHub".to_owned()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use owlshift_adapters::forge::github::{Response, RestAnswer, Transport};
    use owlshift_adapters::forge::github_app::{LoginAnswer, UserToken};
    use serde_json::Value;

    use super::*;
    use crate::forge::GITHUB_ACCOUNT;
    use crate::secrets::project_account;

    const SENTINEL: &str = "SENTINEL";

    fn start() -> Timestamp {
        Timestamp::from_second(1_760_000_000).unwrap()
    }

    fn repo() -> Repo {
        Repo::parse("Owner/Repo").unwrap()
    }

    /// A clock that only sleeps move.
    struct FakeWait(Cell<Timestamp>);

    impl Wait for FakeWait {
        fn now(&self) -> Timestamp {
            self.0.get()
        }

        fn sleep(&self, duration: Duration) {
            let step = SignedDuration::try_from(duration).unwrap();
            self.0.set(self.0.get().checked_add(step).unwrap());
        }
    }

    /// The login endpoints, playing answers in order.
    #[derive(Default)]
    struct Logins(RefCell<VecDeque<&'static str>>);

    impl Login for &Logins {
        fn post_form(&self, _: &str, _: &[(&str, &str)]) -> Result<LoginAnswer, String> {
            let body = self.0.borrow_mut().pop_front().expect("a login answer");
            Ok(LoginAnswer::new(200, body))
        }
    }

    /// The REST API: each read of `user/installations` takes the next
    /// answer, as status and body, and the repositories of installation 9
    /// list the project's repository. The token each read carries is
    /// recorded by `connect`.
    #[derive(Default)]
    struct Rest {
        installations: Mutex<VecDeque<(u16, &'static str)>>,
    }

    /// [`Rest`], shared between the forges a run connects.
    struct Shared(Arc<Rest>);

    impl Transport for Shared {
        fn send(&self, _: &Value) -> Result<Response, String> {
            Err("no GraphQL here".to_owned())
        }

        fn get(&self, path: &str) -> Result<RestAnswer, String> {
            if path.starts_with("user/installations/9/repositories") {
                return Ok(RestAnswer::new(
                    200,
                    r#"{"total_count":1,"repositories":[{"full_name":"owner/repo"}]}"#,
                ));
            }
            assert!(path.starts_with("user/installations?"), "{path}");
            let (status, body) = self
                .0
                .installations
                .lock()
                .unwrap()
                .pop_front()
                .expect("an installations answer");
            Ok(RestAnswer::new(status, body))
        }
    }

    const INSTALLED: (u16, &str) = (
        200,
        r#"{"total_count":1,"installations":[{"id":9,"app_slug":"owl-shift"}]}"#,
    );
    const NONE: (u16, &str) = (200, r#"{"total_count":0,"installations":[]}"#);
    const REFUSED: (u16, &str) = (401, r#"{"message":"Bad credentials SENTINEL"}"#);

    const CODE: &str = r#"{"device_code":"dev_SENTINEL","user_code":"WDJB-MJHT","verification_uri":"https://github.com/login/device","expires_in":899,"interval":5}"#;
    const PENDING: &str = r#"{"error":"authorization_pending"}"#;
    const GRANTED: &str = r#"{"access_token":"ghu_SENTINEL_new","expires_in":28800,"refresh_token":"ghr_SENTINEL","refresh_token_expires_in":15724800,"token_type":"bearer"}"#;

    /// One scripted run of the step.
    struct Run {
        keychain: Keychain,
        logins: Logins,
        rest: Arc<Rest>,
        wait: FakeWait,
        said: Vec<String>,
        /// Holds the account lock's file.
        dir: tempfile::TempDir,
    }

    impl Run {
        fn new(logins: &[&'static str], installations: &[(u16, &'static str)]) -> Self {
            Self {
                keychain: Keychain::in_memory(),
                logins: Logins(RefCell::new(logins.iter().copied().collect())),
                rest: Arc::new(Rest {
                    installations: Mutex::new(installations.iter().copied().collect()),
                }),
                wait: FakeWait(Cell::new(start())),
                said: Vec::new(),
                dir: tempfile::tempdir().unwrap(),
            }
        }

        fn step(&mut self, ask: Ask) -> Outcome {
            let rest = Arc::clone(&self.rest);
            let github = GitHub {
                flow: DeviceFlow::new(&self.logins, "Iv23li5EhBqiuljIPbiK").with_clock(start),
                connect: Box::new(move |_: Token| {
                    GitHubForge::with_transport(Shared(Arc::clone(&rest)), repo())
                }),
                lock: self.dir.path().join("github-app.lock"),
            };
            let said = &mut self.said;
            let outcome = step(
                &self.keychain,
                &repo(),
                &GitHubApp::from_personal(None),
                &github,
                &self.wait,
                ask,
                &mut |line| said.push(line.to_owned()),
            );
            let shown = format!("{outcome:?} {}", self.said.join("\n"));
            assert!(!shown.contains(SENTINEL), "{shown}");
            outcome
        }

        fn app_entry(&self) -> Option<String> {
            self.keychain
                .read(&ForgeAccount::project_app(&repo()).token)
                .unwrap()
                .map(|s| s.expose().to_owned())
        }

        fn store_app(&self, expires_at: Timestamp) {
            let entry = UserToken::new("ghu_SENTINEL_old", None, Some(expires_at), None);
            self.keychain
                .store(
                    &ForgeAccount::project_app(&repo()).token,
                    &Secret::new(entry.to_entry()),
                )
                .unwrap();
        }

        /// An app token with a refresh token, as user-token expiration
        /// stores it (C13).
        fn store_refreshable(&self, expires_at: Timestamp) {
            let entry = UserToken::new(
                "ghu_SENTINEL_old",
                Some("ghr_SENTINEL_old".to_owned()),
                Some(expires_at),
                None,
            );
            self.keychain
                .store(
                    &ForgeAccount::project_app(&repo()).token,
                    &Secret::new(entry.to_entry()),
                )
                .unwrap();
        }

        fn elapsed(&self) -> i64 {
            self.wait.now().as_second() - start().as_second()
        }
    }

    const ON_A_TERMINAL: Ask = Ask {
        replace: false,
        terminal: true,
    };

    fn stored_access(entry: &str) -> String {
        UserToken::from_entry(entry).unwrap().access().to_owned()
    }

    /// Criterion 1: the code shown, polled at its interval, the token stored
    /// once an installation lists the repository.
    #[test]
    fn a_sign_in_is_stored_once_an_installation_lists_the_repository() {
        let mut run = Run::new(&[CODE, PENDING, GRANTED], &[INSTALLED]);
        assert_eq!(run.step(ON_A_TERMINAL), Outcome::Stored);
        assert_eq!(stored_access(&run.app_entry().unwrap()), "ghu_SENTINEL_new");
        assert_eq!(run.elapsed(), 10, "two polls five seconds apart");
        assert!(
            run.said[0]
                .contains("open https://github.com/login/device and enter the code WDJB-MJHT")
                && run.said[0].contains("works for 15 minutes"),
            "{:?}",
            run.said
        );
    }

    /// `slow_down` raises the interval of the polls after it (C10).
    #[test]
    fn slow_down_raises_the_interval() {
        let mut run = Run::new(
            &[
                CODE,
                r#"{"error":"slow_down","interval":10}"#,
                PENDING,
                GRANTED,
            ],
            &[INSTALLED],
        );
        assert_eq!(run.step(ON_A_TERMINAL), Outcome::Stored);
        assert_eq!(run.elapsed(), 5 + 10 + 10);
    }

    /// Criteria 2 and 3: Cancel, the code's expiry as GitHub answers it, and
    /// its life ending before GitHub says so; nothing stored, and a stored
    /// personal token untouched (criterion 8).
    #[test]
    fn cancel_and_an_expired_code_store_nothing() {
        let short = r#"{"device_code":"d","user_code":"U","verification_uri":"https://github.com/login/device","expires_in":12,"interval":5}"#;
        for (logins, outcome, elapsed) in [
            (
                vec![CODE, r#"{"error":"access_denied"}"#],
                Outcome::Denied,
                5,
            ),
            (
                vec![CODE, r#"{"error":"expired_token"}"#],
                Outcome::Expired,
                5,
            ),
            // Polls at 5 and 10 s; the third wait stops at the code's end.
            (vec![short, PENDING, PENDING], Outcome::Expired, 12),
        ] {
            let mut run = Run::new(&logins, &[]);
            let personal = project_account(&repo(), GITHUB_ACCOUNT);
            run.keychain
                .store(&personal, &Secret::new("ghp_personal"))
                .unwrap();
            assert_eq!(run.step(ON_A_TERMINAL), outcome);
            assert_eq!(run.elapsed(), elapsed);
            assert_eq!(run.app_entry(), None);
            assert!(run.keychain.contains(&personal).unwrap());
        }
    }

    /// Criteria 4 and 5: the install link shown, the lookup made again until
    /// an installation lists the repository, or nothing stored past the wait.
    #[test]
    fn the_install_link_is_shown_and_waited_on() {
        let mut run = Run::new(&[CODE, GRANTED], &[NONE, NONE, INSTALLED]);
        assert_eq!(run.step(ON_A_TERMINAL), Outcome::Stored);
        assert!(run.app_entry().is_some());
        assert_eq!(run.elapsed(), 5 + 5 + 5);
        assert!(
            run.said[1]
                .contains("install it at https://github.com/apps/owl-shift/installations/new")
                && run.said[1].contains("github.com/Owner/Repo"),
            "{:?}",
            run.said
        );

        let polls = (INSTALL_WAIT.as_secs() / INSTALL_POLL.as_secs()) as usize;
        let mut run = Run::new(&[CODE, GRANTED], &vec![NONE; polls + 1]);
        assert_eq!(
            run.step(ON_A_TERMINAL),
            Outcome::NotInstalled {
                link: "https://github.com/apps/owl-shift/installations/new".to_owned()
            }
        );
        assert_eq!(run.app_entry(), None);
        assert_eq!(run.elapsed(), 5 + 600);
        assert!(run.rest.installations.lock().unwrap().is_empty());
    }

    /// Criterion 7: a stored token that works is kept with no sign-in; one
    /// past its stored expiry or refused is replaced by a new sign-in;
    /// `--replace-secrets` signs in again.
    #[test]
    fn a_working_stored_token_is_kept_and_a_dead_one_replaced() {
        let later = Timestamp::from_second(start().as_second() + 3600).unwrap();
        let mut run = Run::new(&[], &[INSTALLED]);
        run.store_app(later);
        let before = run.app_entry();
        assert_eq!(run.step(ON_A_TERMINAL), Outcome::Kept);
        assert_eq!(run.app_entry(), before);
        assert!(run.said.is_empty(), "{:?}", run.said);

        for (expires_at, installations) in [
            (start(), vec![INSTALLED]),
            (later, vec![REFUSED, INSTALLED]),
        ] {
            let mut run = Run::new(&[CODE, GRANTED], &installations);
            run.store_app(expires_at);
            assert_eq!(run.step(ON_A_TERMINAL), Outcome::Stored);
            assert_eq!(stored_access(&run.app_entry().unwrap()), "ghu_SENTINEL_new");
            assert!(run.said[0].contains("signing in again"), "{:?}", run.said);
        }

        let mut run = Run::new(&[CODE, GRANTED], &[INSTALLED]);
        run.store_app(later);
        let replace = Ask {
            replace: true,
            ..ON_A_TERMINAL
        };
        assert_eq!(run.step(replace), Outcome::Stored);
        assert_eq!(stored_access(&run.app_entry().unwrap()), "ghu_SENTINEL_new");
    }

    /// OWL-384: a stored token past its expiry goes through the Writer's
    /// refresh and is kept, the rotated pair stored; one GitHub refuses is
    /// refreshed once and kept; one whose refresh is refused counts as
    /// none, and a new sign-in replaces it.
    #[test]
    fn a_stored_token_is_refreshed_and_kept_or_replaced_once_its_refresh_is_refused() {
        let refreshed = r#"{"access_token":"ghu_SENTINEL_rotated","expires_in":28800,"refresh_token":"ghr_SENTINEL_rotated","refresh_token_expires_in":15724800,"token_type":"bearer"}"#;
        let later = Timestamp::from_second(start().as_second() + 3600).unwrap();
        for (expires_at, installations) in [
            (start(), vec![INSTALLED]),
            (later, vec![REFUSED, INSTALLED]),
        ] {
            let mut run = Run::new(&[refreshed], &installations);
            run.store_refreshable(expires_at);
            assert_eq!(run.step(ON_A_TERMINAL), Outcome::Kept);
            assert_eq!(
                stored_access(&run.app_entry().unwrap()),
                "ghu_SENTINEL_rotated"
            );
            assert!(run.said.is_empty(), "{:?}", run.said);
        }

        let mut run = Run::new(
            &[r#"{"error":"bad_refresh_token"}"#, CODE, GRANTED],
            &[INSTALLED],
        );
        run.store_refreshable(start());
        assert_eq!(run.step(ON_A_TERMINAL), Outcome::Stored);
        assert_eq!(stored_access(&run.app_entry().unwrap()), "ghu_SENTINEL_new");
        assert!(
            run.said[0].contains("can no longer be refreshed"),
            "{:?}",
            run.said
        );
    }

    /// A stored token that no installation lists gets the install link, not
    /// a new sign-in.
    #[test]
    fn a_stored_token_without_an_installation_waits_for_one() {
        let later = Timestamp::from_second(start().as_second() + 3600).unwrap();
        let mut run = Run::new(&[], &[NONE, INSTALLED]);
        run.store_app(later);
        assert_eq!(run.step(ON_A_TERMINAL), Outcome::Kept);
        assert!(run.said[0].contains("not installed"), "{:?}", run.said);
    }

    /// Without a terminal: a working token is kept; otherwise nothing is
    /// asked of GitHub's login, and nothing is waited on.
    #[test]
    fn without_a_terminal_nothing_is_signed_in() {
        let no_terminal = Ask {
            replace: false,
            terminal: false,
        };
        let later = Timestamp::from_second(start().as_second() + 3600).unwrap();
        let mut run = Run::new(&[], &[INSTALLED]);
        run.store_app(later);
        assert_eq!(run.step(no_terminal), Outcome::Kept);

        let mut run = Run::new(&[], &[]);
        assert_eq!(run.step(no_terminal), Outcome::NeedsTerminal);
        run.store_app(start());
        assert_eq!(run.step(no_terminal), Outcome::NeedsTerminal);

        let mut run = Run::new(&[], &[NONE]);
        run.store_app(later);
        assert!(matches!(
            run.step(no_terminal),
            Outcome::NotInstalled { .. }
        ));
        assert_eq!(run.elapsed(), 0);
    }

    /// A lookup GitHub fails ends the step in Owlshift's words, the stored
    /// token kept.
    #[test]
    fn a_failed_lookup_keeps_what_is_stored() {
        let later = Timestamp::from_second(start().as_second() + 3600).unwrap();
        let mut run = Run::new(&[], &[(500, r#"{"message":"SENTINEL"}"#)]);
        run.store_app(later);
        let before = run.app_entry();
        assert!(matches!(run.step(ON_A_TERMINAL), Outcome::Failed(_)));
        assert_eq!(run.app_entry(), before);
    }

    /// `--github-token`'s removal of the app token.
    #[test]
    fn the_app_token_is_removed_once() {
        let run = Run::new(&[], &[]);
        run.store_app(start());
        assert!(remove_app_token(&run.keychain, &repo()).unwrap());
        assert!(!remove_app_token(&run.keychain, &repo()).unwrap());
    }
}
