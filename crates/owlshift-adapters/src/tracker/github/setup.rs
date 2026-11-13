//! What `owlshift init` and `owlshift doctor` ask of a GitHub Issues project
//! (OWL-537; build plan "P9 · Open-source readiness", part 6): whether the
//! credential reads the repository's issues, its labels, the labels Owlshift
//! needs created, the signed-in account, and whether an admitter is a GitHub
//! account.
//!
//! The Issues permission is probed with an issue list, never a label: before
//! an installation accepts Issues, a list answers 403 while labels pass on
//! Pull requests (write) alone (G1, 2026-11-12).

use serde::Deserialize;
use serde_json::json;

use super::{GitHubTracker, Label, User, encode, parse, tracker_error};
use crate::forge::github::Verb;
use crate::tracker::{AccountFact, Error, ErrorKind};

/// The colour of a label `init` creates: GitHub's light grey.
const LABEL_COLOUR: &str = "ededed";

/// Whether the credential reads the repository's issues, as one list read
/// answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IssuesAccess {
    /// It does.
    Granted,
    /// HTTP 403: the token lacks Issues (read), which a GitHub App's
    /// installation must have accepted, or a personal token been given.
    Refused(String),
    /// HTTP 401: the credential itself is refused.
    TokenRefused(String),
    /// HTTP 404: the token does not see the repository.
    NotFound(String),
    /// HTTP 410: the repository has its issues turned off.
    Disabled(String),
}

impl GitHubTracker {
    /// Whether the credential reads the repository's issues: one plain read
    /// of the first issue of the list. Any other answer than the five
    /// statuses [`IssuesAccess`] names is an error, as GitHub's silence is.
    pub fn issues_access(&self) -> Result<IssuesAccess, Error> {
        let path = format!("{}/issues?per_page=1", self.repo_path());
        let (status, body) = self.forge.rest_status(&path).map_err(tracker_error)?;
        let message = || {
            let said = serde_json::from_str::<Said>(&body)
                .ok()
                .and_then(|said| said.message)
                .unwrap_or_default();
            format!("HTTP {status} {said}").trim_end().to_owned()
        };
        Ok(match status {
            200..=299 => IssuesAccess::Granted,
            401 => IssuesAccess::TokenRefused(message()),
            403 => IssuesAccess::Refused(message()),
            404 => IssuesAccess::NotFound(message()),
            410 => IssuesAccess::Disabled(message()),
            _ => {
                return Err(Error::new(
                    ErrorKind::Other,
                    format!("GitHub answered the issue list with {}", message()),
                ));
            }
        })
    }

    /// The names of the repository's labels, every page read.
    pub fn labels(&self) -> Result<Vec<String>, Error> {
        let path = format!("{}/labels", self.repo_path());
        let read: Vec<Label> = self.every(&path, "the repository's labels")?;
        Ok(read.into_iter().map(|label| label.name).collect())
    }

    /// Creates each of `wanted` the repository lacks, compared without
    /// regard to case as GitHub names labels, and returns the names created,
    /// in order. A create GitHub refuses as invalid, which a label made
    /// meanwhile under the same name would be, is followed by one more read
    /// of the list: a label now there is taken as it is. Any other refusal,
    /// such as a token without Issues (write), is returned as it is.
    pub fn create_missing_labels(&self, wanted: &[&str]) -> Result<Vec<String>, Error> {
        let found = self.labels()?;
        let has = |names: &[String], name: &str| names.iter().any(|n| n.eq_ignore_ascii_case(name));
        let mut created: Vec<String> = Vec::new();
        let path = format!("{}/labels", self.repo_path());
        for &name in wanted {
            if has(&found, name) || has(&created, name) {
                continue;
            }
            let body = json!({ "name": name, "color": LABEL_COLOUR });
            match self.forge.rest_write(Verb::Post, &path, &body) {
                Ok(_) => created.push(name.to_owned()),
                Err(error) if error.kind == crate::forge::ErrorKind::Other => {
                    if !has(&self.labels()?, name) {
                        return Err(Error::new(
                            ErrorKind::Other,
                            format!("the label {name:?} could not be created: {}", error.message),
                        ));
                    }
                }
                Err(error) => {
                    let error = tracker_error(error);
                    return Err(Error::new(
                        error.kind,
                        format!("the label {name:?} could not be created: {}", error.message),
                    ));
                }
            }
        }
        Ok(created)
    }

    /// The numeric id of the account the credential signs in as: `GET
    /// user`, which a GitHub App's user token reads with no permission
    /// (checked on 2026-11-13: `allows_permissionless_access=true`).
    pub fn signed_in(&self) -> Result<u64, Error> {
        let text = self.read("user")?;
        let user: User = parse(&text, &"the signed-in account")?;
        Ok(user.id)
    }

    /// Of the account ids `ids`, those GitHub knows (`GET user/<id>`): a
    /// person's (`User`) admits, any other kind, a bot or an organization,
    /// never does. An id that is not a number names no account and is not
    /// asked; an account GitHub does not know is left out.
    pub fn accounts(&self, ids: &[&str]) -> Result<Vec<AccountFact>, Error> {
        let mut known = Vec::new();
        for &id in ids {
            let numeric = !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit());
            if !numeric {
                continue;
            }
            let text = match self.read(&format!("user/{}", encode(id))) {
                Ok(text) => text,
                Err(error) if error.kind == ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            let user: User = parse(&text, &format!("account {id}"))?;
            if user.id.to_string() != id {
                continue;
            }
            known.push(AccountFact {
                id: id.to_owned(),
                app: user.kind != "User",
                active: true,
            });
        }
        Ok(known)
    }
}

/// The `message` of an error answer.
#[derive(Deserialize)]
struct Said {
    message: Option<String>,
}

#[cfg(test)]
mod tests;
