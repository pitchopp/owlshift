//! The Markdown tracker's [`Kind`]: tickets kept as files in the project's
//! repository ("The test tracker" in the build plan), with no secret of
//! their own and nothing to read over the network.

use std::path::Path;

use owlshift_adapters::tracker::linear::AdmissionFacts;
use owlshift_adapters::tracker::markdown::MarkdownTracker;
use owlshift_adapters::tracker::{Capability, Tracker};
use owlshift_contracts::config::{self, ProjectConfig};
use owlshift_contracts::ids::TicketId;
use owlshift_platform::keychain::Secret;

use super::{Kind, Opener, StatesError};
use crate::doctor::{Check, DOES_NOT_BLOCK, Section};
use crate::init::SecretSpec;
use crate::secrets::{TrackerAccounts, TrackerSecrets};
use crate::system::System;

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
    ) -> Result<Opener<'a>, String> {
        Ok(Opener::new(move || {
            Ok(Box::new(MarkdownTracker::new(root)) as Box<dyn Tracker>)
        }))
    }

    /// None: nothing of a Markdown tracker is read over the network.
    fn doctor_lines(
        &self,
        _system: &dyn System,
        _config: &ProjectConfig,
        _accounts: Option<&TrackerAccounts>,
    ) -> Vec<Check> {
        Vec::new()
    }

    /// None: the drain admission line has only the faults that need no
    /// tracker.
    fn admission_facts(
        &self,
        _system: &dyn System,
        _config: &ProjectConfig,
        _accounts: Option<&TrackerAccounts>,
    ) -> Option<Result<AdmissionFacts, StatesError>> {
        None
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
}
