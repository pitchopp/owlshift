//! The Owlshift platform layer: service install and uninstall, process groups
//! and Job Objects, the keychain, platform directories, keep-awake, and the
//! OS sandbox agent runs are confined in.

pub mod confined;
pub mod keychain;
#[cfg(windows)]
pub mod launch;
pub mod paths;
pub mod process;
pub mod sandbox;
