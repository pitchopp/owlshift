//! The `owlshift` command line on a bench data directory (OWL-522):
//! `owlshift_cli::main_with`, the whole of the shipped `owlshift` binary,
//! with a keychain held in memory in place of the system's. It holds the
//! bench project's GitHub token and Claude Code's token for agent runs, both
//! fake: the commands that reach a forge or a model with them fail, and the
//! web UI's Restart, `continue <TICKET> --to-watch --served <OWNER/REPO>
//! --from-ui`, reaches neither.
//!
//! Only `do`, `continue` and `watch` open this keychain. `init`, `doctor`,
//! the service's commands and `ui` read the system's, as `owlshift` does, and
//! a command that runs this program again (a watch's steps, the service's
//! install) runs this bench, not `owlshift`. Never shipped: this crate is
//! test only, and the shipped binary's entry passes the system keychain.

use std::process::ExitCode;

use owlshift_adapters::forge::Repo;
use owlshift_platform::keychain::{Keychain, KeychainError, Secret};
use owlshift_runner::executor::harness::CLAUDE_AGENT_ACCOUNT;
use owlshift_runner::secrets::ForgeAccount;
use owlshift_testkit::step_child::REPO;

/// What each entry holds: plainly not a token.
const FAKE: &str = "test-token-not-real";

/// A keychain held in memory with the bench project's two entries.
fn bench_keychain() -> Result<Keychain, KeychainError> {
    let keychain = Keychain::in_memory();
    let repo = Repo::parse(REPO).expect("the bench's repository");
    keychain.store(&ForgeAccount::project(&repo).token, &Secret::new(FAKE))?;
    keychain.store(CLAUDE_AGENT_ACCOUNT, &Secret::new(FAKE))?;
    Ok(keychain)
}

fn main() -> ExitCode {
    owlshift_cli::main_with(bench_keychain)
}
