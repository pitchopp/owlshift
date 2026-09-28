//! The Owlshift adapters: tracker, forge, harness and notifier, each behind a
//! trait with declared capabilities.
//!
//! The traits are extracted when a kind has its second implementation; until
//! then each adapter is a concrete type or a set of functions.

pub mod harness;
pub mod tracker;
