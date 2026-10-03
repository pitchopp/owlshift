//! OWL-129: Linear keeps a label named like `zone:backend/billing`, with `:`
//! and `/`, and the Linear adapter reads it back as written, so that
//! `declared_zones` gets the zone from it. Read only, on the Owlshift
//! workspace, and by hand: it never runs in CI. The key comes from
//! `LINEAR_API_KEY`, or from the dotenv file `LINEAR_ENV_FILE` names, never
//! from the command line, and is never printed.
//!
//! ```sh
//! OWLSHIFT_LIVE_LINEAR_ZONE_TICKET=OWL-129 LINEAR_ENV_FILE=$HOME/Projects/owlshift/.env \
//!   cargo test -p owlshift-adapters --test linear_live -- --ignored --nocapture
//! ```
//!
//! The ticket named must carry the label `zone:backend/billing`.

mod support;

use owlshift_adapters::tracker::Tracker;
use owlshift_adapters::tracker::linear::LinearTracker;
use owlshift_contracts::ids::TicketId;
use owlshift_core::decider::{brief_zones, declared_zones};
use owlshift_core::resource::Resource;

const LABEL: &str = "zone:backend/billing";

#[test]
#[ignore = "reads the live Linear API; needs OWLSHIFT_LIVE_LINEAR_ZONE_TICKET and a key"]
fn linear_returns_a_zone_label_as_written() {
    let id = std::env::var("OWLSHIFT_LIVE_LINEAR_ZONE_TICKET")
        .expect("OWLSHIFT_LIVE_LINEAR_ZONE_TICKET names an issue labelled `zone:backend/billing`");
    let key = support::live_key();
    support::assert_owlshift_workspace(&key);

    let ticket = LinearTracker::new(key)
        .ticket(&TicketId::new(id).unwrap())
        .unwrap();
    eprintln!("{} has the labels {:?}", ticket.id, ticket.labels);

    assert!(
        ticket.labels.iter().any(|label| label == LABEL),
        "no label named {LABEL:?} on {}: {:?}",
        ticket.id,
        ticket.labels
    );
    let labels = || ticket.labels.iter().map(String::as_str);
    let declared = declared_zones(labels()).unwrap();
    assert!(declared.contains(&Resource::Zone("backend/billing".to_owned())));
    assert!(
        brief_zones(labels())
            .zones
            .contains(&"backend/billing".to_owned())
    );
}
