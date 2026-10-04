//! The OAuth app a Linear workspace gives Owlshift, so that its comments and
//! stage moves come from the app's user rather than the operator's account
//! (decision D8, OWL-157): Linear does not notify a person of what their own
//! API key writes, and it does notify them of the app's comments.
//!
//! Checked live on 2026-10-04 (OWL-141 and OWL-157; build plan, check C4): a
//! token requested from [`TOKEN_ENDPOINT`] with `grant_type`
//! `client_credentials`, the app's client id and secret and the scopes
//! [`SCOPES`] answers HTTP 200, `token_type` `Bearer`, valid for 30 days with
//! no refresh token; it acts as the app user (`viewer.app` true). Revoked at
//! [`REVOKE_ENDPOINT`] with the token as a `Bearer` header, HTTP 200, after
//! which a query with it answers HTTP 401 `AUTHENTICATION_ERROR`. Requesting
//! another set of scopes revokes every token of the app, so the set is fixed.
//!
//! [`AppTransport`] requests one token when it connects, sends every request
//! with it, requests a new one once when Linear answers 401 (a token that
//! expired or was revoked), and revokes its token when it is dropped. The
//! token and the client secret never show in `Debug` output or in an error.

use std::fmt;
use std::sync::{Mutex, PoisonError};

use serde::Deserialize;
use serde_json::Value;

use super::{Response, Transport, http_agent, post_graphql};
use crate::tracker::{Error, ErrorKind};

/// Where a token is requested.
pub const TOKEN_ENDPOINT: &str = "https://api.linear.app/oauth/token";

/// Where a token is revoked.
pub const REVOKE_ENDPOINT: &str = "https://api.linear.app/oauth/revoke";

/// The scopes of every token: reading, and writing the comments, stage moves
/// and issues Owlshift writes. Never changed lightly: Linear revokes every
/// token of the app when one is requested with another set.
pub const SCOPES: &str = "read,write";

/// The OAuth app's client id and secret. Neither shows in `Debug` output.
#[derive(Clone)]
pub struct ClientCredentials {
    id: String,
    secret: String,
}

impl ClientCredentials {
    pub fn new(id: impl Into<String>, secret: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            secret: secret.into(),
        }
    }
}

impl fmt::Debug for ClientCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ClientCredentials(<redacted>)")
    }
}

/// An app token. It never shows in `Debug` output.
pub struct AccessToken(String);

impl AccessToken {
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    /// The token, for the one header that carries it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AccessToken(<redacted>)")
    }
}

/// Linear's OAuth endpoints and its GraphQL endpoint as an app sees them:
/// [`HttpOAuth`] in production, a fake in tests.
pub trait OAuth {
    /// A new token for the app.
    fn token(&self) -> Result<AccessToken, Error>;
    /// Posts one GraphQL request with `token`. `Err` means no HTTP answer.
    fn send(&self, token: &AccessToken, request: &Value) -> Result<Response, String>;
    /// Revokes `token`.
    fn revoke(&self, token: &AccessToken) -> Result<(), String>;
}

/// The production [`OAuth`]: HTTPS to Linear with the app's credentials.
pub struct HttpOAuth {
    agent: ureq::Agent,
    credentials: ClientCredentials,
}

impl HttpOAuth {
    pub fn new(credentials: ClientCredentials) -> Self {
        Self {
            agent: http_agent(),
            credentials,
        }
    }
}

impl fmt::Debug for HttpOAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpOAuth")
            .field("credentials", &self.credentials)
            .finish_non_exhaustive()
    }
}

impl OAuth for HttpOAuth {
    fn token(&self) -> Result<AccessToken, Error> {
        let form = [
            ("grant_type", "client_credentials"),
            ("client_id", self.credentials.id.as_str()),
            ("client_secret", self.credentials.secret.as_str()),
            ("scope", SCOPES),
        ];
        let unreachable =
            |e: ureq::Error| Error::new(ErrorKind::Other, format!("Linear's token endpoint: {e}"));
        let mut response = self
            .agent
            .post(TOKEN_ENDPOINT)
            .send_form(form)
            .map_err(unreachable)?;
        let status = response.status().as_u16();
        let body = response.body_mut().read_to_string().map_err(unreachable)?;
        token_answer(status, &body)
    }

    fn send(&self, token: &AccessToken, request: &Value) -> Result<Response, String> {
        post_graphql(&self.agent, &bearer(token), request)
    }

    fn revoke(&self, token: &AccessToken) -> Result<(), String> {
        let response = self
            .agent
            .post(REVOKE_ENDPOINT)
            .header("Authorization", &bearer(token))
            .send_empty()
            .map_err(|e| e.to_string())?;
        match response.status().as_u16() {
            200 => Ok(()),
            status => Err(format!("Linear answered HTTP {status} to the revocation")),
        }
    }
}

fn bearer(token: &AccessToken) -> String {
    format!("Bearer {}", token.0)
}

/// The error codes of OAuth 2.0 (RFC 6749, section 5.2) a token request may
/// answer. Any other text is not repeated: a message never carries what the
/// endpoint wrote beyond one of these.
const OAUTH_ERRORS: &[&str] = &[
    "invalid_request",
    "invalid_client",
    "invalid_grant",
    "unauthorized_client",
    "unsupported_grant_type",
    "invalid_scope",
];

/// Reads the token endpoint's answer. A refusal keeps its OAuth error code
/// alone, and is [`ErrorKind::Unauthorized`] when it says the credentials are
/// wrong; nothing else of the body reaches the error.
fn token_answer(status: u16, body: &str) -> Result<AccessToken, Error> {
    #[derive(Deserialize)]
    struct Granted {
        access_token: String,
        #[serde(default)]
        token_type: Option<String>,
    }
    #[derive(Deserialize)]
    struct Refused {
        error: String,
    }
    if status == 200 {
        return match serde_json::from_str::<Granted>(body) {
            Ok(granted)
                if !granted.access_token.is_empty()
                    && granted.access_token.chars().all(|c| c.is_ascii_graphic())
                    && granted
                        .token_type
                        .as_deref()
                        .is_none_or(|kind| kind.eq_ignore_ascii_case("bearer")) =>
            {
                Ok(AccessToken(granted.access_token))
            }
            _ => Err(Error::new(
                ErrorKind::Other,
                "Linear's token endpoint answered without a bearer token",
            )),
        };
    }
    let code = serde_json::from_str::<Refused>(body)
        .ok()
        .and_then(|refused| OAUTH_ERRORS.iter().find(|&&c| c == refused.error).copied());
    let kind = match (status, code) {
        (401, _) | (_, Some("invalid_client" | "unauthorized_client" | "invalid_grant")) => {
            ErrorKind::Unauthorized
        }
        _ => ErrorKind::Other,
    };
    Err(Error::new(
        kind,
        format!(
            "Linear refused the app's token request: HTTP {status}, {}",
            code.unwrap_or("an unrecognised error")
        ),
    ))
}

/// The [`Transport`] of an app user: every request goes with the app's
/// token. The token is requested when the transport connects, so wrong
/// credentials are found before any work; a request Linear answers 401 (the
/// token expired after its 30 days, or was revoked) gets one new token and
/// is sent once more, and an error without an answer is never retried. The
/// token is revoked when the transport is dropped, at the end of a command;
/// a revocation that fails is let go, since the token expires anyway.
pub struct AppTransport<O: OAuth = HttpOAuth> {
    oauth: O,
    /// Held for the whole of a send and its retry, so requests go one at a
    /// time and a new token replaces the old one once.
    token: Mutex<AccessToken>,
}

impl<O: OAuth> AppTransport<O> {
    /// Requests the app's token now.
    pub fn connect(oauth: O) -> Result<Self, Error> {
        let token = oauth.token()?;
        Ok(Self {
            oauth,
            token: Mutex::new(token),
        })
    }
}

impl<O: OAuth> fmt::Debug for AppTransport<O> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AppTransport(<redacted>)")
    }
}

impl<O: OAuth> Transport for AppTransport<O> {
    fn send(&self, request: &Value) -> Result<Response, String> {
        // A panic elsewhere while the lock was held leaves a token that is
        // still a token: the lock is taken back.
        let mut token = self.token.lock().unwrap_or_else(PoisonError::into_inner);
        let response = self.oauth.send(&token, request)?;
        if response.status != 401 {
            return Ok(response);
        }
        *token = self.oauth.token().map_err(|e| e.message)?;
        self.oauth.send(&token, request)
    }
}

impl<O: OAuth> Drop for AppTransport<O> {
    fn drop(&mut self) {
        let token = self.token.get_mut().unwrap_or_else(PoisonError::into_inner);
        let _ = self.oauth.revoke(token);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Arc;

    use serde_json::json;

    use super::*;

    /// What a fake OAuth saw, shared with the test after the transport is
    /// dropped.
    #[derive(Default)]
    struct Seen {
        /// The token of each send, in order.
        sent_with: Vec<String>,
        minted: u32,
        revoked: Vec<String>,
    }

    /// Mints `token-1`, `token-2`…, unless `refuse_after` tokens were
    /// minted; answers each send with the next status.
    struct FakeOAuth {
        seen: Arc<Mutex<Seen>>,
        statuses: Mutex<VecDeque<u16>>,
        refuse_after: u32,
    }

    impl FakeOAuth {
        fn new(statuses: &[u16], refuse_after: u32) -> (Self, Arc<Mutex<Seen>>) {
            let seen = Arc::new(Mutex::new(Seen::default()));
            let fake = Self {
                seen: seen.clone(),
                statuses: Mutex::new(statuses.iter().copied().collect()),
                refuse_after,
            };
            (fake, seen)
        }
    }

    impl OAuth for FakeOAuth {
        fn token(&self) -> Result<AccessToken, Error> {
            let mut seen = self.seen.lock().unwrap();
            if seen.minted == self.refuse_after {
                return Err(Error::new(ErrorKind::Unauthorized, "refused"));
            }
            seen.minted += 1;
            Ok(AccessToken(format!("token-{}", seen.minted)))
        }

        fn send(&self, token: &AccessToken, _: &Value) -> Result<Response, String> {
            self.seen.lock().unwrap().sent_with.push(token.0.clone());
            let status = self.statuses.lock().unwrap().pop_front().expect("a status");
            Ok(Response {
                status,
                body: String::new(),
            })
        }

        fn revoke(&self, token: &AccessToken) -> Result<(), String> {
            self.seen.lock().unwrap().revoked.push(token.0.clone());
            Err("revocation fails, and is let go".to_owned())
        }
    }

    fn statuses(transport: &AppTransport<FakeOAuth>, n: usize) -> Vec<u16> {
        (0..n)
            .map(|_| transport.send(&json!({})).unwrap().status)
            .collect()
    }

    /// One token for the transport's life; a 401 gets one new token and one
    /// more try, a second 401 is the answer; the token held last is revoked
    /// when the transport goes, even if the revocation fails.
    #[test]
    fn a_token_serves_until_linear_refuses_it_and_is_revoked_at_the_end() {
        let (fake, seen) = FakeOAuth::new(&[200, 401, 200, 401, 401, 200], u32::MAX);
        let transport = AppTransport::connect(fake).unwrap();
        assert_eq!(statuses(&transport, 4), [200, 200, 401, 200]);
        drop(transport);
        let seen = seen.lock().unwrap();
        assert_eq!(
            seen.sent_with,
            [
                "token-1", "token-1", "token-2", "token-2", "token-3", "token-3"
            ]
        );
        assert_eq!(seen.minted, 3);
        assert_eq!(seen.revoked, ["token-3"]);
    }

    /// Credentials refused when connecting is an error before any request;
    /// a new token refused after a 401 is the send's error, and the token
    /// held is still revoked at the end.
    #[test]
    fn a_refused_token_request_is_an_error() {
        let (fake, seen) = FakeOAuth::new(&[], 0);
        let error = AppTransport::connect(fake).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Unauthorized);
        assert!(seen.lock().unwrap().sent_with.is_empty());

        let (fake, seen) = FakeOAuth::new(&[401], 1);
        let transport = AppTransport::connect(fake).unwrap();
        assert_eq!(transport.send(&json!({})).unwrap_err(), "refused");
        drop(transport);
        assert_eq!(seen.lock().unwrap().revoked, ["token-1"]);
    }

    /// The token endpoint's answers: a bearer token, or a refusal that keeps
    /// only a known OAuth error code. A secret the endpoint would echo
    /// reaches no error, and no credential or token shows in `Debug`.
    #[test]
    fn token_answers_keep_nothing_but_a_known_code() {
        const SENTINEL: &str = "SENTINEL_never_shown";
        let granted = token_answer(
            200,
            &json!({ "access_token": "lin_oauth_x", "token_type": "Bearer",
                "expires_in": 2_591_999, "scope": "read write" })
            .to_string(),
        )
        .unwrap();
        assert_eq!(granted.expose(), "lin_oauth_x");

        let refused = |status: u16, error: &str| {
            token_answer(
                status,
                &json!({ "error": error, "error_description": SENTINEL }).to_string(),
            )
            .unwrap_err()
        };
        let wrong = refused(400, "invalid_client");
        assert_eq!(wrong.kind, ErrorKind::Unauthorized);
        assert!(
            wrong.message.ends_with("HTTP 400, invalid_client"),
            "{wrong}"
        );
        assert_eq!(refused(400, "invalid_scope").kind, ErrorKind::Other);
        let echoed = refused(400, SENTINEL);
        assert!(
            echoed.message.ends_with("an unrecognised error"),
            "{echoed}"
        );
        assert_eq!(refused(401, "x").kind, ErrorKind::Unauthorized);

        for (status, body) in [
            (200, json!({ "access_token": "a b" }).to_string()),
            (
                200,
                json!({ "access_token": "x", "token_type": "mac" }).to_string(),
            ),
            (200, "not json".to_owned()),
            (503, SENTINEL.to_owned()),
        ] {
            let error = token_answer(status, &body).unwrap_err();
            assert_eq!(error.kind, ErrorKind::Other);
            assert!(!error.to_string().contains(SENTINEL), "{error}");
        }

        let credentials = ClientCredentials::new(SENTINEL, SENTINEL);
        let token = AccessToken::new(SENTINEL);
        let shown = format!(
            "{credentials:?} {token:?} {:?}",
            HttpOAuth::new(credentials.clone())
        );
        let (fake, _) = FakeOAuth::new(&[], u32::MAX);
        let shown = format!("{shown} {:?}", AppTransport::connect(fake).unwrap());
        assert!(!shown.contains("SENTINEL"), "{shown}");
    }
}
