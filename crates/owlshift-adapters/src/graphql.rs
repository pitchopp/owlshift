//! The seam every GraphQL adapter talks through: one request at a time, over
//! a [`Transport`] that is HTTPS in production and a replay of recorded
//! exchanges in tests.
//!
//! An exchange is the request body, the HTTP status and the response body,
//! with the rate-limit headers alone ([`RateHeaders`]): never another header,
//! so a recording cannot hold a credential.

use std::io::Read;

use serde_json::Value;

/// An HTTP answer: its status and its body, and the rate-limit headers a
/// forge reads ([`RateHeaders`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub body: String,
    pub rate: RateHeaders,
}

impl Response {
    /// An answer without rate-limit headers.
    pub fn new(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
            rate: RateHeaders::default(),
        }
    }
}

/// The rate-limit headers of an answer, and no other: they hold no
/// credential, so a recording that keeps them still holds none (OWL-209).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RateHeaders {
    /// `x-ratelimit-remaining`.
    pub remaining: Option<u64>,
    /// `x-ratelimit-reset`, in seconds since the epoch.
    pub reset: Option<i64>,
    /// `retry-after`, in seconds.
    pub retry_after: Option<u64>,
}

/// An answer to a REST read (OWL-112): its status, its rate-limit headers,
/// and its body, which the caller streams.
pub struct RestAnswer {
    pub status: u16,
    pub rate: RateHeaders,
    pub body: Box<dyn Read>,
}

impl RestAnswer {
    /// An answer without rate-limit headers.
    pub fn new(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            rate: RateHeaders::default(),
            body: Box::new(std::io::Cursor::new(body.into())),
        }
    }
}

/// Sends one GraphQL request, or one REST read.
pub trait Transport {
    /// Posts `request`, a GraphQL request body (`query` and `variables`).
    /// `Err` means no HTTP answer at all: network, TLS or timeout.
    fn send(&self, request: &Value) -> Result<Response, String>;

    /// Reads `path` of the forge's REST API, such as
    /// `repos/<owner>/<name>/actions/jobs/<id>/logs`, following a redirect
    /// without the credential. `Err` means no HTTP answer at all. Refused by
    /// default: only GitHub's own transport and the fakes that stand in for
    /// a job's log read REST.
    fn get(&self, path: &str) -> Result<RestAnswer, String> {
        Err(format!("this transport reads no REST path ({path})"))
    }

    /// Puts the JSON `body` at `path` of the forge's REST API, such as
    /// `repos/<owner>/<name>/pulls/<n>/merge` (OWL-444), and returns the
    /// answer whole. `Err` means no HTTP answer at all, so whether the
    /// forge acted is unknown. Refused by default: only GitHub's own
    /// transport and the fakes that stand in for a merge write REST.
    fn put(&self, path: &str, body: &Value) -> Result<Response, String> {
        let _ = body;
        Err(format!("this transport writes no REST path ({path})"))
    }
}
