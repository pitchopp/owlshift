//! The Linear tracker's [`Kind`]: a team's issues, read and written through
//! a Linear API key and, when one is stored, the Linear app Owlshift writes
//! as (decision D8, OWL-157).

use owlshift_adapters::tracker::Capability;
use owlshift_adapters::tracker::linear::LinearTracker;
use owlshift_contracts::config;
use owlshift_contracts::ids::TicketId;

use super::Kind;
use crate::init::{LINEAR_KEY, SecretSpec};

pub struct Linear;

impl Kind for Linear {
    fn name(&self) -> &'static str {
        "linear"
    }

    fn capabilities(&self) -> &'static [Capability] {
        LinearTracker::CAPABILITIES
    }

    fn secret_specs(&self) -> &'static [SecretSpec] {
        &[LINEAR_KEY]
    }

    fn project_file_lines(&self, team: Option<&str>) -> Result<String, String> {
        let quote = |text: &str| toml::Value::String(text.to_owned()).to_string();
        let team = team
            .map(str::trim)
            .filter(|team| !team.is_empty())
            .ok_or("a Linear project needs its team key: pass --team, such as --team OWL")?;
        Ok(format!(
            "\
# Where the tickets live: \"linear\", or \"markdown\" for tickets kept in
# this repository, one folder per ticket under tickets/.
kind = \"linear\"
# The Linear team key: the prefix of its ticket ids, OWL in OWL-12.
team = {}
",
            quote(team)
        ))
    }

    /// On Linear, a ticket id starts with its team's key (`OWL` in
    /// `OWL-12`), compared without regard to case, as Linear reads
    /// identifiers.
    fn check_ticket(&self, tracker: &config::Tracker, ticket: &TicketId) -> Result<(), String> {
        let Some(team) = &tracker.team else {
            return Ok(());
        };
        let key = ticket.as_str().rsplit_once('-').map(|(key, _)| key);
        if key.is_some_and(|key| key.eq_ignore_ascii_case(team)) {
            Ok(())
        } else {
            Err(format!(
                "{ticket} is not a ticket of team {team}, the project's `tracker.team`: \
                 `owlshift do` runs this project's tickets only"
            ))
        }
    }
}
