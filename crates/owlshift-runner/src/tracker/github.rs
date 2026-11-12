//! The GitHub Issues tracker's [`Kind`] (OWL-535): the issues of the
//! project's own repository, reached with the project's forge credential, so
//! it has no secret of its own (build plan, "The GitHub Issues adapter").
//! `owlshift init` and `owlshift doctor` learn it with OWL-537: until then
//! `init` does not offer it and doctor has no line of its own for it.

use std::path::Path;

use owlshift_adapters::forge::Repo;
use owlshift_adapters::tracker::github::GitHubTracker;
use owlshift_adapters::tracker::linear::AdmissionFacts;
use owlshift_adapters::tracker::{Capability, Tracker};
use owlshift_contracts::config::{self, ProjectConfig};
use owlshift_contracts::ids::TicketId;
use owlshift_platform::keychain::Secret;

use super::{ForgeCredential, Kind, Opener, StatesError};
use crate::doctor::Check;
use crate::init::SecretSpec;
use crate::secrets::{TrackerAccounts, TrackerSecrets};
use crate::system::System;
use crate::ui::reply::Key;

pub struct Github;

impl Kind for Github {
    fn name(&self) -> &'static str {
        "github"
    }

    fn capabilities(&self) -> &'static [Capability] {
        GitHubTracker::CAPABILITIES
    }

    /// None: the forge's credential reaches the issues.
    fn secret_specs(&self) -> &'static [SecretSpec] {
        &[]
    }

    fn project_file_lines(&self, _team: Option<&str>) -> Result<String, String> {
        Ok("\
# Where the tickets live: \"github\" for the issues of this repository,
# reached with the project's GitHub credential; the states are labels.
kind = \"github\"
"
        .to_owned())
    }

    /// An issue's number alone: `12`, never `#12` or a team's key.
    fn check_ticket(&self, _tracker: &config::Tracker, ticket: &TicketId) -> Result<(), String> {
        let text = ticket.as_str();
        if text.bytes().all(|b| b.is_ascii_digit()) && !text.starts_with('0') {
            Ok(())
        } else {
            Err(format!(
                "{ticket} is not an issue number: the project's tickets are its GitHub issues, \
                 named by their number (`owlshift do 12`)"
            ))
        }
    }

    /// The adapter over the project's forge, opened as `owlshift do` opens
    /// its own: the GitHub App's token refreshed under the account lock, or
    /// the personal token.
    fn opener<'a>(
        &self,
        tracker: &'a config::Tracker,
        _root: &'a Path,
        _secrets: Option<&'a TrackerSecrets<Secret>>,
        forge: ForgeCredential<'a>,
    ) -> Result<Opener<'a>, String> {
        Ok(Opener::new(move || {
            let reached = crate::forge::github(forge.secret, forge.repo.clone(), forge.access)?;
            Ok(Box::new(GitHubTracker::new(reached, tracker)) as Box<dyn Tracker>)
        }))
    }

    /// None until OWL-537.
    fn doctor_lines(
        &self,
        _system: &dyn System,
        _config: &ProjectConfig,
        _accounts: Option<&TrackerAccounts>,
    ) -> Vec<Check> {
        Vec::new()
    }

    /// None: the adapter reads admission since OWL-536; doctor's lines on it
    /// come with OWL-537.
    fn admission_facts(
        &self,
        _system: &dyn System,
        _config: &ProjectConfig,
        _accounts: Option<&TrackerAccounts>,
    ) -> Option<Result<AdmissionFacts, StatesError>> {
        None
    }

    /// None: the adapter files proposals since OWL-536; doctor's line on its
    /// inbox comes with OWL-537.
    fn proposals_inbox(
        &self,
        _config: &ProjectConfig,
        _facts: Option<&Result<AdmissionFacts, StatesError>>,
    ) -> Option<Check> {
        None
    }

    fn reply_key(&self, _repo: &Repo) -> Result<Key, String> {
        Err(
            "the project's tracker is GitHub Issues: a reply from the UI goes through a Linear \
             key, and the UI cannot reply on GitHub yet"
                .into(),
        )
    }

    fn stores_app(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ticket_is_an_issue_number() {
        let tracker = ProjectConfig::parse(
            r#"
            requires = ">=0.1"
            [tracker]
            kind = "github"
            admit = { label = "agent:ready" }
            states = { ready = "agent:ready", working = "agent:working", needs_input = "agent:asked", review = "agent:review" }
            [stack]
            gate = ["make test"]
            [pipeline]
            default = "trivial"
            plan_approval = "never"
            [models]
            [policy]
            always_human = []
            "#,
        )
        .unwrap()
        .tracker;
        let check = |id: &str| Github.check_ticket(&tracker, &TicketId::new(id).unwrap());
        assert_eq!(check("12"), Ok(()));
        for refused in ["OWL-12", "012"] {
            let error = check(refused).unwrap_err();
            assert!(error.contains("not an issue number"), "{refused}: {error}");
        }
    }
}
