//! The `owlshift` binary: the library's entry, its credentials read from the
//! system keychain, and trusting Owlshift's release key for `self-update`.
//! No other keychain or key can be chosen here (OWL-522, OWL-541).

use std::process::ExitCode;

use owlshift_platform::keychain::Keychain;

fn main() -> ExitCode {
    owlshift_cli::main_with(Keychain::system, owlshift_cli::RELEASE_KEY)
}
