//! The seam every GraphQL adapter talks through: one request at a time, over
//! a [`Transport`] that is HTTPS in production and a replay of recorded
//! exchanges in tests.
//!
//! An exchange is the request body, the HTTP status and the response body,
//! with the rate-limit headers alone ([`RateHeaders`]): never another header,
//! so a recording cannot hold a credential.

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

/// Sends one GraphQL request.
pub trait Transport {
    /// Posts `request`, a GraphQL request body (`query` and `variables`).
    /// `Err` means no HTTP answer at all: network, TLS or timeout.
    fn send(&self, request: &Value) -> Result<Response, String>;
}
