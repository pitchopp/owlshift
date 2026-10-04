//! The Owlshift runner: the daemon loop, the executor (worktrees, spawning,
//! isolation check), the writer and the local store; and, until the daemon
//! exists, `owlshift do`'s run of one ticket ([`on_demand`]) and `owlshift
//! watch`'s loop over the tickets whose questions wait ([`watch`]).

pub mod agent_env;
pub mod answer_check;
pub mod artifact;
pub mod config;
pub mod doctor;
pub mod events;
pub mod executor;
pub mod forge;
pub mod init;
pub mod notify;
pub mod on_demand;
pub mod project;
pub mod resolver;
pub mod roles;
pub mod rules;
pub mod system;
pub mod ticket_ref;
pub mod tracker;
pub mod watch;
pub mod writer;
