//! Owlshift's GitHub App (architecture D13, OWL-336): its client ID and
//! slug, and the device flow that gets a user token for it with the client
//! ID alone, no client secret on the user's machine.
//!
//! What this client relies on was checked live on 2026-10-19 with
//! `scripts/github-app-checks.sh` (build plan, Results, C9, C10, C13, C14):
//!
//! - `POST login/device/code` with the client ID alone answers 200 with a
//!   device code, a user code, `verification_uri`, `expires_in` (899) and
//!   `interval` (5) (C9).
//! - The token endpoint answers every poll with HTTP 200, the outcome in
//!   `error`: `authorization_pending` before the code is entered,
//!   `slow_down` inside the interval with the interval raised (to 10), the
//!   token once entered, `access_denied` on Cancel (C10). `expired_token`
//!   past the code's life was not observed: it is the standard's answer,
//!   assumed.
//! - With user-token expiration on, the token answer holds `access_token`,
//!   `expires_in` (28,800), `refresh_token`, `refresh_token_expires_in`
//!   (15,724,800), an empty `scope` and `token_type` `bearer`. The refresh
//!   grant with the client ID alone is accepted and rotates both tokens; a
//!   refresh token already used, or used by a second refresh at once, is
//!   refused with `bad_refresh_token` or `incorrect_client_credentials`, as
//!   HTTP 200 (C13). With expiration off, no `expires_in` and no refresh
//!   token come back (C14).
//!
//! The device flow's loop, which shows the code and waits, is `owlshift
//! init`'s (GA2): this module answers one request at a time. No token, no
//! device code and nothing of an answer's body beyond a known error code
//! ever reaches an error or `Debug` output.

use std::fmt;
use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};

/// The client ID of Owlshift's GitHub App, `owl-shift` (D13), which the
/// personal file's `github_app_client_id` overrides.
pub const CLIENT_ID: &str = "Iv23li5EhBqiuljIPbiK";

/// The slug of Owlshift's GitHub App (D13), which the personal file's
/// `github_app_slug` overrides; it names the install link ([`install_link`]).
pub const SLUG: &str = "owl-shift";

/// Where a device code is asked for.
pub const DEVICE_CODE_URL: &str = "https://github.com/login/device/code";

/// Where the device flow is polled and a token refreshed.
pub const TOKEN_URL: &str = "https://github.com/login/oauth/access_token";

/// The device flow's grant type (RFC 8628).
pub const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// The interval between polls when GitHub gives none (RFC 8628, 3.2).
const DEFAULT_INTERVAL: Duration = Duration::from_secs(5);

/// What `slow_down` adds to the interval when its answer names no larger one
/// (RFC 8628, 3.5).
const SLOW_DOWN_STEP: Duration = Duration::from_secs(5);

/// The longest life an answer may give a code or a token: a year, past the
/// refresh token's 182 days (C13). Anything longer is a malformed answer.
const MAX_LIFE_SECONDS: u64 = 366 * 24 * 3600;

/// The most bytes of a login answer read.
const MAX_ANSWER_BYTES: u64 = 64 * 1024;

/// The error codes the device flow and the refresh grant may answer, from
/// GitHub's documentation of the device flow and RFC 6749 and 8628. Only
/// these are ever repeated in a message; GitHub's list differs from the
/// standard's (`incorrect_client_credentials`, `bad_refresh_token`), so it
/// is kept apart from the Linear app's. Those not seen live on 2026-10-19
/// are documented only.
const KNOWN_ERRORS: &[&str] = &[
    "authorization_pending",
    "slow_down",
    "expired_token",
    "access_denied",
    "unsupported_grant_type",
    "incorrect_client_credentials",
    "incorrect_device_code",
    "device_flow_disabled",
    "bad_refresh_token",
    "bad_verification_code",
    "unauthorized_client",
    "invalid_request",
    "invalid_client",
    "invalid_grant",
    "temporarily_unavailable",
];

/// The page where the app with `slug` is installed on an account's
/// repositories. The slug's characters (lowercase letters, digits and `-`,
/// checked where an override is read) need no encoding.
pub fn install_link(slug: &str) -> String {
    format!("https://github.com/apps/{slug}/installations/new")
}

/// GitHub's answer to a form post: its status and body. The body may hold
/// tokens, so `Debug` shows the status alone.
pub struct LoginAnswer {
    pub status: u16,
    pub body: String,
}

impl LoginAnswer {
    pub fn new(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
        }
    }
}

impl fmt::Debug for LoginAnswer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoginAnswer")
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

/// GitHub's login endpoints: [`HttpLogin`] in production, a script in
/// tests.
pub trait Login {
    /// Posts `form` to `url`, asking for JSON. `Err` means no answer, in
    /// words that carry nothing of the form.
    fn post_form(&self, url: &str, form: &[(&str, &str)]) -> Result<LoginAnswer, String>;
}

/// The production [`Login`]: HTTPS to `github.com`.
pub struct HttpLogin {
    agent: ureq::Agent,
}

impl HttpLogin {
    pub fn new() -> Self {
        Self {
            agent: super::github::agent(Duration::from_secs(30)),
        }
    }
}

impl Default for HttpLogin {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for HttpLogin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HttpLogin")
    }
}

impl Login for HttpLogin {
    fn post_form(&self, url: &str, form: &[(&str, &str)]) -> Result<LoginAnswer, String> {
        // A ureq error may name the request; it is not repeated.
        let unreachable = |_| format!("GitHub's login endpoint {url} could not be reached");
        let mut response = self
            .agent
            .post(url)
            .header("Accept", "application/json")
            .send_form(form.iter().copied())
            .map_err(unreachable)?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_ANSWER_BYTES)
            .read_to_string()
            .map_err(|_| format!("GitHub's answer from {url} could not be read"))?;
        Ok(LoginAnswer { status, body })
    }
}

/// Why a device-flow or refresh request did not give what was asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AppError {
    /// The user cancelled on GitHub's device page (`access_denied`).
    Denied,
    /// The device code passed its life (`expired_token`).
    Expired,
    /// GitHub refused the refresh token, with this error code when it is a
    /// known one: revoked, past its life or already used (C13). Trying again
    /// cannot help: the user signs in again (architecture section 8, the
    /// refresh-failure rule).
    Refused(Option<&'static str>),
    /// Anything else: no answer, an HTTP status other than 200, an answer
    /// that does not read, or an error a retry may clear. In Owlshift's
    /// words, never the answer's.
    Failed(String),
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Denied => f.write_str("the sign-in was cancelled on GitHub"),
            Self::Expired => f.write_str("the sign-in code expired before it was entered"),
            Self::Refused(code) => write!(
                f,
                "GitHub refused the GitHub App's refresh token ({})",
                code.unwrap_or("an unrecognised error")
            ),
            Self::Failed(reason) => f.write_str(reason),
        }
    }
}

impl std::error::Error for AppError {}

/// A device code, as GitHub gave it: the user enters [`Self::user_code`] at
/// [`Self::verification_uri`]. The device code itself never shows in
/// `Debug` output.
#[derive(Clone)]
pub struct DeviceCode {
    device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    /// When the code stops being accepted, by the client's clock when it
    /// was asked for.
    pub expires_at: Timestamp,
    /// How long to wait before the first poll and between polls.
    pub interval: Duration,
}

impl fmt::Debug for DeviceCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceCode")
            .field("user_code", &self.user_code)
            .field("verification_uri", &self.verification_uri)
            .field("expires_at", &self.expires_at)
            .field("interval", &self.interval)
            .finish_non_exhaustive()
    }
}

/// A user token of the GitHub App: the access token, and with user-token
/// expiration on (C13) its expiry, the refresh token and that one's expiry.
/// No token shows in `Debug` output.
#[derive(Clone, PartialEq, Eq)]
pub struct UserToken {
    access: String,
    refresh: Option<String>,
    expires_at: Option<Timestamp>,
    refresh_expires_at: Option<Timestamp>,
}

impl fmt::Debug for UserToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UserToken")
            .field("refresh", &self.refresh.as_ref().map(|_| "<redacted>"))
            .field("expires_at", &self.expires_at)
            .field("refresh_expires_at", &self.refresh_expires_at)
            .finish_non_exhaustive()
    }
}

/// The keychain entry's shape: one value, so a rotation is one write
/// (architecture section 8). Expiries in Unix seconds.
#[derive(Serialize, Deserialize)]
struct Entry {
    v: u32,
    access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refresh_token_expires_at: Option<i64>,
}

/// The version of [`Entry`] written.
const ENTRY_VERSION: u32 = 1;

impl UserToken {
    pub fn new(
        access: impl Into<String>,
        refresh: Option<String>,
        expires_at: Option<Timestamp>,
        refresh_expires_at: Option<Timestamp>,
    ) -> Self {
        Self {
            access: access.into(),
            refresh,
            expires_at,
            refresh_expires_at,
        }
    }

    /// The access token, for the one header that carries it.
    pub fn access(&self) -> &str {
        &self.access
    }

    /// The refresh token, for the refresh grant alone.
    pub fn refresh(&self) -> Option<&str> {
        self.refresh.as_deref()
    }

    /// When the access token expires; `None` when it does not (C14).
    pub fn expires_at(&self) -> Option<Timestamp> {
        self.expires_at
    }

    /// When the refresh token expires.
    pub fn refresh_expires_at(&self) -> Option<Timestamp> {
        self.refresh_expires_at
    }

    /// Whether the access token is past its expiry at `now`.
    pub fn expired(&self, now: Timestamp) -> bool {
        self.expires_at.is_some_and(|at| now >= at)
    }

    /// The token as the one keychain entry that holds it: compact JSON, one
    /// word. It holds both tokens: a secret, stored and never shown.
    pub fn to_entry(&self) -> String {
        let entry = Entry {
            v: ENTRY_VERSION,
            access_token: self.access.clone(),
            refresh_token: self.refresh.clone(),
            expires_at: self.expires_at.map(Timestamp::as_second),
            refresh_token_expires_at: self.refresh_expires_at.map(Timestamp::as_second),
        };
        serde_json::to_string(&entry).expect("an entry always serializes")
    }

    /// Reads a keychain entry written by [`Self::to_entry`]. The error never
    /// repeats any of the entry.
    pub fn from_entry(entry: &str) -> Result<Self, AppError> {
        let unreadable = || {
            AppError::Failed(
                "the stored GitHub App token does not read as one Owlshift wrote".to_owned(),
            )
        };
        let read: Entry = serde_json::from_str(entry).map_err(|_| unreadable())?;
        if read.v != ENTRY_VERSION || !is_token(&read.access_token) {
            return Err(unreadable());
        }
        if read.refresh_token.as_deref().is_some_and(|t| !is_token(t)) {
            return Err(unreadable());
        }
        let time = |seconds: Option<i64>| {
            seconds
                .map(|s| Timestamp::from_second(s).map_err(|_| unreadable()))
                .transpose()
        };
        Ok(Self {
            access: read.access_token,
            refresh: read.refresh_token,
            expires_at: time(read.expires_at)?,
            refresh_expires_at: time(read.refresh_token_expires_at)?,
        })
    }
}

/// A token as GitHub sends it: printable ASCII, no space.
fn is_token(token: &str) -> bool {
    !token.is_empty() && token.chars().all(|c| c.is_ascii_graphic())
}

/// What one poll of the device flow gave.
#[derive(Debug, PartialEq, Eq)]
pub enum Poll {
    /// The code is not entered yet: poll again after the same interval.
    Pending,
    /// Polled too soon: poll again after this interval, the larger one.
    SlowDown(Duration),
    /// Signed in.
    Granted(UserToken),
}

/// The device flow of a GitHub App, with its client ID alone (C9).
pub struct DeviceFlow<L: Login = HttpLogin> {
    login: L,
    client_id: String,
    now: fn() -> Timestamp,
}

impl<L: Login> fmt::Debug for DeviceFlow<L> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceFlow")
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
}

impl<L: Login> DeviceFlow<L> {
    /// The flow of the app `client_id`, through `login`, on the system
    /// clock.
    pub fn new(login: L, client_id: impl Into<String>) -> Self {
        Self {
            login,
            client_id: client_id.into(),
            now: Timestamp::now,
        }
    }

    /// The same flow on the clock `now`, which turns the lives GitHub gives
    /// into instants.
    pub fn with_clock(self, now: fn() -> Timestamp) -> Self {
        Self { now, ..self }
    }

    /// Asks GitHub for a device code.
    pub fn request_code(&self) -> Result<DeviceCode, AppError> {
        #[derive(Deserialize)]
        struct Given {
            device_code: String,
            user_code: String,
            verification_uri: String,
            expires_in: u64,
            #[serde(default)]
            interval: Option<u64>,
        }
        let answer = self.post(DEVICE_CODE_URL, &[("client_id", &self.client_id)])?;
        if let Some(code) = error_code(&answer.body) {
            return Err(failed("the device code request", code));
        }
        let malformed = || AppError::Failed("GitHub's device code answer does not read".into());
        let given: Given = serde_json::from_str(&answer.body).map_err(|_| malformed())?;
        if !is_token(&given.device_code)
            || !is_token(&given.user_code)
            || !given.verification_uri.starts_with("https://")
            || !is_token(&given.verification_uri)
        {
            return Err(malformed());
        }
        let interval = match given.interval {
            None | Some(0) => DEFAULT_INTERVAL,
            Some(seconds) if seconds <= MAX_LIFE_SECONDS => Duration::from_secs(seconds),
            Some(_) => return Err(malformed()),
        };
        Ok(DeviceCode {
            device_code: given.device_code,
            user_code: given.user_code,
            verification_uri: given.verification_uri,
            expires_at: self.after(given.expires_in).ok_or_else(malformed)?,
            interval,
        })
    }

    /// Polls once for the token of `code`, which `interval` is the current
    /// wait of (C10).
    pub fn poll(&self, code: &DeviceCode, interval: Duration) -> Result<Poll, AppError> {
        let form = [
            ("client_id", self.client_id.as_str()),
            ("device_code", code.device_code.as_str()),
            ("grant_type", DEVICE_GRANT),
        ];
        let answer = self.post(TOKEN_URL, &form)?;
        match error_code(&answer.body) {
            None => self.token(&answer.body).map(Poll::Granted),
            Some(Some("authorization_pending")) => Ok(Poll::Pending),
            Some(Some("slow_down")) => {
                #[derive(Deserialize)]
                struct Slower {
                    #[serde(default)]
                    interval: Option<u64>,
                }
                let given = serde_json::from_str::<Slower>(&answer.body)
                    .ok()
                    .and_then(|slower| slower.interval)
                    .filter(|&seconds| seconds <= MAX_LIFE_SECONDS)
                    .map(Duration::from_secs);
                Ok(Poll::SlowDown(match given {
                    Some(given) if given > interval => given,
                    _ => interval + SLOW_DOWN_STEP,
                }))
            }
            Some(Some("access_denied")) => Err(AppError::Denied),
            Some(Some("expired_token")) => Err(AppError::Expired),
            Some(code) => Err(failed("the sign-in", code)),
        }
    }

    /// Exchanges `refresh_token` for a new pair, with the client ID alone
    /// (C13). Any error GitHub answers is [`AppError::Refused`]: it answers
    /// a refusal as HTTP 200 with an `error`, and nothing but a new sign-in
    /// gets past it. A token used by a second refresh at once is refused
    /// too (`bad_refresh_token`), which the account lock of GA3 prevents.
    pub fn refresh(&self, refresh_token: &str) -> Result<UserToken, AppError> {
        let form = [
            ("client_id", self.client_id.as_str()),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ];
        let answer = self.post(TOKEN_URL, &form)?;
        match error_code(&answer.body) {
            None => self.token(&answer.body),
            Some(Some("temporarily_unavailable")) => {
                Err(failed("the refresh", Some("temporarily_unavailable")))
            }
            Some(code) => Err(AppError::Refused(code)),
        }
    }

    /// Posts `form`; anything but HTTP 200 is a failure that names the
    /// status alone.
    fn post(&self, url: &str, form: &[(&str, &str)]) -> Result<LoginAnswer, AppError> {
        let answer = self.login.post_form(url, form).map_err(AppError::Failed)?;
        if answer.status != 200 {
            return Err(AppError::Failed(format!(
                "GitHub answered HTTP {} at {url}",
                answer.status
            )));
        }
        Ok(answer)
    }

    /// Reads a token answer (C10, C13, C14).
    fn token(&self, body: &str) -> Result<UserToken, AppError> {
        #[derive(Deserialize)]
        struct Granted {
            access_token: String,
            #[serde(default)]
            token_type: Option<String>,
            #[serde(default)]
            expires_in: Option<u64>,
            #[serde(default)]
            refresh_token: Option<String>,
            #[serde(default)]
            refresh_token_expires_in: Option<u64>,
        }
        let malformed =
            || AppError::Failed("GitHub's token answer holds no usable bearer token".into());
        let granted: Granted = serde_json::from_str(body).map_err(|_| malformed())?;
        let bearer = granted
            .token_type
            .as_deref()
            .is_none_or(|kind| kind.eq_ignore_ascii_case("bearer"));
        if !bearer
            || !is_token(&granted.access_token)
            || granted
                .refresh_token
                .as_deref()
                .is_some_and(|t| !is_token(t))
        {
            return Err(malformed());
        }
        let at = |life: Option<u64>| {
            life.map(|s| self.after(s).ok_or_else(malformed))
                .transpose()
        };
        Ok(UserToken {
            access: granted.access_token,
            refresh: granted.refresh_token,
            expires_at: at(granted.expires_in)?,
            refresh_expires_at: at(granted.refresh_token_expires_in)?,
        })
    }

    /// The instant `seconds` from now, for a life of 1 s to a year.
    fn after(&self, seconds: u64) -> Option<Timestamp> {
        if seconds == 0 || seconds > MAX_LIFE_SECONDS {
            return None;
        }
        let seconds = i64::try_from(seconds).ok()?;
        (self.now)()
            .checked_add(SignedDuration::from_secs(seconds))
            .ok()
    }
}

/// The `error` of an answer: `None` when it has none, `Some(None)` when it
/// has one that is not a known code, which is never repeated.
fn error_code(body: &str) -> Option<Option<&'static str>> {
    #[derive(Deserialize)]
    struct Refused {
        error: Option<String>,
    }
    let error = serde_json::from_str::<Refused>(body).ok()?.error?;
    Some(KNOWN_ERRORS.iter().find(|&&known| known == error).copied())
}

fn failed(what: &str, code: Option<&'static str>) -> AppError {
    AppError::Failed(format!(
        "GitHub refused {what}: {}",
        code.unwrap_or("an unrecognised error")
    ))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use super::*;

    /// A secret no message may repeat.
    const SENTINEL: &str = "ghu_SENTINEL";

    fn now() -> Timestamp {
        Timestamp::from_second(1_760_000_000).unwrap()
    }

    /// A request sent: its URL and form.
    type Sent = (String, Vec<(String, String)>);

    /// Plays answers in order and records each request.
    #[derive(Default)]
    struct Script {
        answers: RefCell<VecDeque<Result<LoginAnswer, String>>>,
        sent: RefCell<Vec<Sent>>,
    }

    impl Script {
        fn answering(answers: impl IntoIterator<Item = &'static str>) -> Self {
            let script = Self::default();
            for body in answers {
                script.then(200, body);
            }
            script
        }

        fn then(&self, status: u16, body: &str) {
            self.answers
                .borrow_mut()
                .push_back(Ok(LoginAnswer::new(status, body)));
        }
    }

    impl Login for &Script {
        fn post_form(&self, url: &str, form: &[(&str, &str)]) -> Result<LoginAnswer, String> {
            self.sent.borrow_mut().push((
                url.to_owned(),
                form.iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect(),
            ));
            self.answers
                .borrow_mut()
                .pop_front()
                .expect("an answer is scripted")
        }
    }

    fn flow(script: &Script) -> DeviceFlow<&Script> {
        DeviceFlow::new(script, CLIENT_ID).with_clock(now)
    }

    /// C9's answer, as recorded on 2026-10-19 (values made up).
    const C9: &str = r#"{"device_code":"3584d83530557fdd1f46af8289938c8ef79f9dc5","user_code":"WDJB-MJHT","verification_uri":"https://github.com/login/device","expires_in":899,"interval":5}"#;

    /// C10's and C13's token answer, with user-token expiration on.
    const GRANTED: &str = r#"{"access_token":"ghu_access1","expires_in":28800,"refresh_token":"ghr_refresh1","refresh_token_expires_in":15724800,"scope":"","token_type":"bearer"}"#;

    fn code(script: &Script) -> DeviceCode {
        script.then(200, C9);
        flow(script).request_code().unwrap()
    }

    fn form(script: &Script, index: usize) -> Vec<(String, String)> {
        script.sent.borrow()[index].1.clone()
    }

    fn pairs(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn the_app_is_owl_shift_and_its_install_link_comes_from_the_slug() {
        assert_eq!((CLIENT_ID, SLUG), ("Iv23li5EhBqiuljIPbiK", "owl-shift"));
        assert_eq!(
            install_link(SLUG),
            "https://github.com/apps/owl-shift/installations/new"
        );
    }

    /// C9: the client ID alone, no secret, and the code's life on the clock.
    #[test]
    fn a_device_code_is_asked_with_the_client_id_alone() {
        let script = Script::default();
        let code = code(&script);
        assert_eq!(script.sent.borrow()[0].0, DEVICE_CODE_URL);
        assert_eq!(form(&script, 0), pairs(&[("client_id", CLIENT_ID)]));
        assert_eq!(code.user_code, "WDJB-MJHT");
        assert_eq!(code.verification_uri, "https://github.com/login/device");
        assert_eq!(code.interval, Duration::from_secs(5));
        assert_eq!(code.expires_at.as_second(), now().as_second() + 899);
        let shown = format!("{code:?}");
        assert!(!shown.contains("3584d835"), "{shown}");
    }

    #[test]
    fn a_device_code_without_an_interval_waits_five_seconds() {
        let script = Script::answering([
            r#"{"device_code":"d","user_code":"U","verification_uri":"https://github.com/login/device","expires_in":899,"interval":0}"#,
        ]);
        assert_eq!(
            flow(&script).request_code().unwrap().interval,
            DEFAULT_INTERVAL
        );
    }

    #[test]
    fn a_refused_or_malformed_device_code_answer_fails_in_owlshifts_words() {
        let script = Script::answering([
            r#"{"error":"device_flow_disabled","error_description":"ghu_SENTINEL"}"#,
            r#"{"error":"ghu_SENTINEL"}"#,
            r#"{"device_code":"d","user_code":"U","verification_uri":"https://github.com/login/device","expires_in":0}"#,
            r#"{"device_code":7,"user_code":"ghu_SENTINEL"}"#,
        ]);
        script.then(502, SENTINEL);
        let flow = flow(&script);
        let mut said = Vec::new();
        for _ in 0..5 {
            let error = flow.request_code().unwrap_err();
            said.push(format!("{error} {error:?}"));
        }
        assert!(said[0].contains("device_flow_disabled"), "{}", said[0]);
        assert!(said[1].contains("an unrecognised error"), "{}", said[1]);
        assert!(said[4].contains("HTTP 502"), "{}", said[4]);
        for said in said {
            assert!(!said.contains(SENTINEL), "{said}");
        }
    }

    /// C10: pending, slowed down, then the token; the device code and the
    /// grant type go with each poll.
    #[test]
    fn the_polls_answer_pending_slow_down_then_the_token() {
        let script = Script::default();
        let code = code(&script);
        script.then(200, r#"{"error":"authorization_pending"}"#);
        script.then(
            200,
            r#"{"error":"slow_down","error_description":"Too many requests","interval":10}"#,
        );
        script.then(200, GRANTED);
        let flow = flow(&script);
        let five = Duration::from_secs(5);
        assert_eq!(flow.poll(&code, five).unwrap(), Poll::Pending);
        assert_eq!(
            flow.poll(&code, five).unwrap(),
            Poll::SlowDown(Duration::from_secs(10))
        );
        let Poll::Granted(token) = flow.poll(&code, Duration::from_secs(10)).unwrap() else {
            panic!("no token");
        };
        assert_eq!(script.sent.borrow()[1].0, TOKEN_URL);
        assert_eq!(
            form(&script, 1),
            pairs(&[
                ("client_id", CLIENT_ID),
                ("device_code", "3584d83530557fdd1f46af8289938c8ef79f9dc5"),
                ("grant_type", DEVICE_GRANT),
            ])
        );
        // C13: an 8-hour token, refreshable for 182 days.
        assert_eq!(token.access(), "ghu_access1");
        assert_eq!(token.refresh(), Some("ghr_refresh1"));
        assert_eq!(
            token.expires_at().unwrap().as_second(),
            now().as_second() + 28_800
        );
        assert_eq!(
            token.refresh_expires_at().unwrap().as_second(),
            now().as_second() + 15_724_800
        );
        let shown = format!("{token:?}");
        assert!(
            !shown.contains("ghu_access1") && !shown.contains("ghr_refresh1"),
            "{shown}"
        );
    }

    /// RFC 8628, 3.5: `slow_down` never shortens the wait; with no larger
    /// interval named, five seconds more.
    #[test]
    fn slow_down_only_ever_lengthens_the_interval() {
        let script = Script::default();
        let code = code(&script);
        script.then(200, r#"{"error":"slow_down","interval":10}"#);
        script.then(200, r#"{"error":"slow_down"}"#);
        let flow = flow(&script);
        let fifteen = Duration::from_secs(15);
        assert_eq!(
            flow.poll(&code, fifteen).unwrap(),
            Poll::SlowDown(Duration::from_secs(20))
        );
        assert_eq!(
            flow.poll(&code, fifteen).unwrap(),
            Poll::SlowDown(Duration::from_secs(20))
        );
    }

    /// C10: Cancel, and a code past its life (assumed, not observed).
    #[test]
    fn cancel_and_an_expired_code_end_the_sign_in() {
        let script = Script::default();
        let code = code(&script);
        script.then(200, r#"{"error":"access_denied"}"#);
        script.then(200, r#"{"error":"expired_token"}"#);
        script.then(200, r#"{"error":"incorrect_device_code"}"#);
        let flow = flow(&script);
        let five = Duration::from_secs(5);
        assert_eq!(flow.poll(&code, five).unwrap_err(), AppError::Denied);
        assert_eq!(flow.poll(&code, five).unwrap_err(), AppError::Expired);
        let Err(AppError::Failed(reason)) = flow.poll(&code, five) else {
            panic!("not a failure");
        };
        assert!(reason.contains("incorrect_device_code"), "{reason}");
    }

    /// C14: with expiration off, no expiry and no refresh token.
    #[test]
    fn a_token_without_expiry_reads() {
        let script = Script::default();
        let code = code(&script);
        script.then(
            200,
            r#"{"access_token":"ghu_forever","scope":"","token_type":"Bearer"}"#,
        );
        let Poll::Granted(token) = flow(&script).poll(&code, DEFAULT_INTERVAL).unwrap() else {
            panic!("no token");
        };
        assert_eq!((token.refresh(), token.expires_at()), (None, None));
        assert!(!token.expired(Timestamp::MAX));
    }

    #[test]
    fn a_token_answer_that_does_not_read_repeats_none_of_it() {
        let script = Script::default();
        let code = code(&script);
        for body in [
            r#"{"access_token":"ghu_SENTINEL","token_type":"mac"}"#,
            r#"{"access_token":"","token_type":"bearer"}"#,
            r#"{"access_token":"ghu_SENTINEL","expires_in":"ghu_SENTINEL"}"#,
            r#"{"access_token":"ghu_SENTINEL","expires_in":99999999999}"#,
            r#"{"access_token":"ghu_SENTINEL","refresh_token":"has space"}"#,
            r#"{"scope":""}"#,
        ] {
            script.then(200, body);
            let error = flow(&script).poll(&code, DEFAULT_INTERVAL).unwrap_err();
            let said = format!("{error} {error:?}");
            assert!(said.contains("no usable bearer token"), "{body}: {said}");
            assert!(!said.contains(SENTINEL), "{said}");
        }
    }

    /// C13: the refresh grant with the client ID alone rotates both tokens.
    #[test]
    fn a_refresh_takes_the_client_id_alone_and_rotates_both_tokens() {
        let script = Script::answering([
            r#"{"access_token":"ghu_access2","expires_in":28800,"refresh_token":"ghr_refresh2","refresh_token_expires_in":15724800,"scope":"","token_type":"bearer"}"#,
        ]);
        let token = flow(&script).refresh("ghr_refresh1").unwrap();
        assert_eq!(
            form(&script, 0),
            pairs(&[
                ("client_id", CLIENT_ID),
                ("grant_type", "refresh_token"),
                ("refresh_token", "ghr_refresh1"),
            ])
        );
        assert_eq!(
            (token.access(), token.refresh()),
            ("ghu_access2", Some("ghr_refresh2"))
        );
    }

    /// C13: a refresh token already used, or used twice at once, is refused
    /// as HTTP 200; any error is a refusal but GitHub's being unavailable,
    /// and an unknown code is not repeated.
    #[test]
    fn a_refused_refresh_says_sign_in_again_and_an_outage_does_not() {
        let script = Script::answering([
            r#"{"error":"bad_refresh_token","error_description":"The refresh token passed is incorrect or expired."}"#,
            r#"{"error":"incorrect_client_credentials"}"#,
            r#"{"error":"ghu_SENTINEL"}"#,
            r#"{"error":"temporarily_unavailable"}"#,
        ]);
        script.then(500, SENTINEL);
        let flow = flow(&script);
        let refresh = || flow.refresh("ghr_refresh1").unwrap_err();
        assert_eq!(refresh(), AppError::Refused(Some("bad_refresh_token")));
        assert_eq!(
            refresh(),
            AppError::Refused(Some("incorrect_client_credentials"))
        );
        let unknown = refresh();
        assert_eq!(unknown, AppError::Refused(None));
        assert!(!unknown.to_string().contains(SENTINEL), "{unknown}");
        assert!(matches!(refresh(), AppError::Failed(_)));
        let outage = refresh();
        assert!(matches!(&outage, AppError::Failed(reason) if reason.contains("HTTP 500")));
        assert!(!format!("{outage} {outage:?}").contains(SENTINEL));
    }

    #[test]
    fn no_answer_is_a_failure() {
        let script = Script::default();
        script
            .answers
            .borrow_mut()
            .push_back(Err("GitHub's login endpoint could not be reached".into()));
        assert!(matches!(
            flow(&script).refresh("ghr_refresh1"),
            Err(AppError::Failed(_))
        ));
    }

    #[test]
    fn a_token_reads_back_from_its_keychain_entry() {
        let token = UserToken::new(
            "ghu_access1",
            Some("ghr_refresh1".into()),
            Some(now()),
            Some(Timestamp::from_second(1_775_000_000).unwrap()),
        );
        let entry = token.to_entry();
        assert!(!entry.contains(char::is_whitespace), "{entry}");
        assert!(entry.starts_with(r#"{"v":1,"#), "{entry}");
        assert_eq!(UserToken::from_entry(&entry).unwrap(), token);

        let bare = UserToken::new("ghu_forever", None, None, None);
        assert_eq!(UserToken::from_entry(&bare.to_entry()).unwrap(), bare);
    }

    #[test]
    fn the_expiry_is_passed_at_the_instant_itself() {
        let token = UserToken::new("ghu_access1", None, Some(now()), None);
        assert!(token.expired(now()));
        assert!(!token.expired(Timestamp::from_second(now().as_second() - 1).unwrap()));
    }

    #[test]
    fn an_entry_that_does_not_read_repeats_none_of_it() {
        for entry in [
            "ghu_SENTINEL",
            r#"{"v":1,"access_token":7,"refresh_token":"ghu_SENTINEL"}"#,
            r#"{"v":2,"access_token":"ghu_SENTINEL"}"#,
            r#"{"v":1,"access_token":"ghu_SENTINEL","expires_at":"ghu_SENTINEL"}"#,
            r#"{"v":1,"access_token":"ghu SENTINEL"}"#,
            r#"{"v":1,"access_token":"ghu_SENTINEL","expires_at":99999999999999}"#,
        ] {
            let error = UserToken::from_entry(entry).unwrap_err();
            let said = format!("{error} {error:?}");
            assert!(said.contains("does not read"), "{entry}: {said}");
            assert!(!said.contains("SENTINEL"), "{said}");
        }
    }

    #[test]
    fn a_login_answer_never_shows_its_body() {
        let shown = format!("{:?}", LoginAnswer::new(200, GRANTED));
        assert!(!shown.contains("ghu_access1"), "{shown}");
    }
}
