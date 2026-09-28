//! The seam every GraphQL adapter talks through: one request at a time, over
//! a [`Transport`] that is HTTPS in production and a replay of recorded
//! exchanges in tests.
//!
//! An exchange is the request body, the HTTP status and the response body:
//! never a header, so a recording cannot hold a credential.

use serde_json::Value;

/// An HTTP answer: its status and its body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub body: String,
}

/// Sends one GraphQL request.
pub trait Transport {
    /// Posts `request`, a GraphQL request body (`query` and `variables`).
    /// `Err` means no HTTP answer at all: network, TLS or timeout.
    fn send(&self, request: &Value) -> Result<Response, String>;
}
