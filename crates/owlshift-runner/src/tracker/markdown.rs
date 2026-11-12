//! The Markdown tracker's [`Kind`]: tickets kept as files in the project's
//! repository ("The test tracker" in the build plan), with no secret of
//! their own and nothing to read over the network.

use owlshift_adapters::tracker::Capability;
use owlshift_adapters::tracker::markdown::MarkdownTracker;
use owlshift_contracts::config;
use owlshift_contracts::ids::TicketId;

use super::Kind;
use crate::init::SecretSpec;

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
}
