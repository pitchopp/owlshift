//! The Owlshift platform layer: service install and uninstall, process groups
//! and Job Objects, the keychain, platform directories, keep-awake, and the
//! OS sandbox agent runs are confined in.

pub mod awake;
pub mod confined;
pub mod files;
pub mod keychain;
pub mod paths;
pub mod process;
pub mod sandbox;
pub mod service;
/// The testkit's, which this crate cannot depend on (the testkit depends on
/// it): the executables the unit tests write, and their first start
/// (OWL-646).
#[cfg(all(test, unix))]
#[allow(dead_code, reason = "the unit tests use part of it")]
#[path = "../../owlshift-testkit/src/stand_in.rs"]
mod stand_in;
pub mod terminal;
#[cfg(all(test, unix))]
mod test_proc;
/// The testkit's, which this crate cannot depend on: how long a unit test
/// waits for a state (OWL-493, build plan "Waits in tests").
#[cfg(all(test, unix))]
#[allow(dead_code, reason = "the unit tests use part of it")]
#[path = "../../owlshift-testkit/src/wait.rs"]
mod wait;
pub mod wake;
