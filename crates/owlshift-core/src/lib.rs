//! The Owlshift core: ticket, pipeline, stage, gate and resource; the state
//! machine, the scheduler and the policy.
//!
//! Everything here is a pure function. This crate performs no I/O: no files,
//! network, processes, environment, standard streams or clock. Callers pass in
//! what the core needs to know, including the current time. The rule is
//! enforced by `clippy.toml` in this crate's directory.

pub mod vocab;
