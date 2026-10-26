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
pub mod terminal;
#[cfg(all(test, unix))]
mod test_proc;
