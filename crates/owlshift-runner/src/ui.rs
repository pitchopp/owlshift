//! The local web UI (P8, OWL-467, architecture section 5, "The local web
//! UI"): `owlshift ui --daemon`, a child of the daemon that serves the page
//! frame and its header on a `127.0.0.1` socket the daemon binds and keeps.
//!
//! - *The token* ([`token`]): 32 random bytes in `<data>/service/ui/token`,
//!   the folder 0700 and the file 0600, on every confined run's hidden list.
//! - *The proof, the code and the session* ([`auth`]): HMAC-SHA256 under the
//!   token. `owlshift ui` proves the listener before it mints a one-time
//!   code; the page trades the code for a session secret bound to the
//!   daemon's run, through the run secret the daemon hands each UI it
//!   starts.

pub mod auth;
pub mod token;
