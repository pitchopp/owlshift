//! The `owlshift` binary: the library's entry, its credentials read from the
//! system keychain. No other keychain can be chosen here (OWL-522).

use std::process::ExitCode;

use owlshift_platform::keychain::Keychain;

fn main() -> ExitCode {
    owlshift_cli::main_with(Keychain::system)
}
