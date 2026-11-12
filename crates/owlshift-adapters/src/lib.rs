//! The Owlshift adapters: tracker, forge, harness and notifier, each behind a
//! trait with declared capabilities; and Owlshift's own published releases,
//! which `owlshift self-update` reads.
//!
//! The traits are extracted when a kind has its second implementation; until
//! then each adapter is a concrete type or a set of functions.

pub mod forge;
pub mod graphql;
pub mod harness;
pub mod notifier;
pub mod release;
pub mod tracker;
