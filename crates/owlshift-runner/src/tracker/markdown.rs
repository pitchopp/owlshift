//! The Markdown tracker's [`Kind`]: tickets kept as files in the project's
//! repository ("The test tracker" in the build plan), with no secret of
//! their own and nothing to read over the network.

use std::path::Path;

use owlshift_adapters::forge::Repo;
use owlshift_adapters::tracker::linear::AdmissionFacts;
use owlshift_adapters::tracker::markdown::MarkdownTracker;
use owlshift_adapters::tracker::{Capability, Tracker};
use owlshift_contracts::config::{self, ProjectConfig};
use owlshift_contracts::ids::TicketId;
use owlshift_platform::keychain::Secret;

use super::{Connected, Doctored, ForgeCredential, Kind, Opener, Setup, StatesError};
use crate::doctor::{Check, DOES_NOT_BLOCK, Section};
use crate::init::SecretSpec;
use crate::secrets::{TrackerAccounts, TrackerSecrets};
use crate::system::System;
use crate::ui::reply::Key;

pub struct Markdown;

impl Kind for Markdown {
    fn name(&self) -> &'static str {
        "markdown"
    }

    fn capabilities(&self) -> &'static [Capability] {
        MarkdownTracker::CAPABILITIES
    }

    fn secret_specs(&self) -> &'static [SecretSpec] {
        &[]
    }

    fn project_file_lines(&self, _team: Option<&str>) -> Result<String, String> {
        Ok("\
# Where the tickets live: \"markdown\" for tickets kept in this repository,
# one folder per ticket under tickets/, or \"linear\" with its `team` key.
kind = \"markdown\"
"
        .to_owned())
    }

    /// Any ticket id: the project's tickets are the files of its checkout.
    fn check_ticket(&self, _tracker: &config::Tracker, _ticket: &TicketId) -> Result<(), String> {
        Ok(())
    }

    /// The tickets under `root`, the project's checkout.
    fn opener<'a>(
        &self,
        _tracker: &'a config::Tracker,
        root: &'a Path,
        _secrets: Option<&'a TrackerSecrets<Secret>>,
        _forge: ForgeCredential<'a>,
    ) -> Result<Opener<'a>, String> {
        Ok(Opener::new(move || {
            Ok(Box::new(MarkdownTracker::new(root)) as Box<dyn Tracker>)
        }))
    }

    /// Nothing: nothing of a Markdown tracker is read over the network, and
    /// the drain admission line has only the faults that need no tracker.
    fn doctor(
        &self,
        _system: &dyn System,
        _config: &ProjectConfig,
        _accounts: Option<&TrackerAccounts>,
        _repo: Option<&Repo>,
    ) -> Doctored {
        Doctored::default()
    }

    /// A Markdown project files a proposal at the stage `Triage` (OWL-267),
    /// read from its file alone: a warning when that is its ready state,
    /// where `propose` refuses every filing.
    fn proposals_inbox(
        &self,
        config: &ProjectConfig,
        _facts: Option<&Result<AdmissionFacts, StatesError>>,
    ) -> Option<Check> {
        const SUBJECT: &str = "proposals inbox";
        let triage = MarkdownTracker::TRIAGE;
        Some(if config.tracker.states.ready == triage {
            Check::warn(
                Section::Project,
                SUBJECT,
                format!("the ready state is {triage:?}, the stage proposals are filed at"),
                format!(
                    "{DOES_NOT_BLOCK} Owlshift cannot file the follow-ups it proposes, since a \
                 proposal there would enter the queue without a person. Name another \
                 state under `[tracker].states.ready`."
                ),
            )
        } else {
            Check::ok(
                Section::Project,
                SUBJECT,
                format!("proposals are filed as tickets at the stage {triage:?}"),
            )
        })
    }

    fn reply_key(&self, _repo: &Repo) -> Result<Key, String> {
        Err(
            "the project's tracker is not Linear: a reply from the UI goes through a Linear key"
                .into(),
        )
    }

    fn stores_app(&self) -> bool {
        false
    }

    /// Nothing to ask: the tickets are files of the checkout (OWL-537).
    fn connect(&self, _setup: &Setup<'_>) -> Result<Connected, String> {
        Ok(Connected::default())
    }
}
