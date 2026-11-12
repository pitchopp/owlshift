# Changelog

What changes in each release of Owlshift, newest first. Versions follow [Semantic Versioning](https://semver.org/). A release's notes on GitHub are its section below, and the release workflow refuses a tag whose version has none.

## 0.1.0

The first release. Owlshift works a team's existing backlog with coding agents, one ticket or many, asks on the ticket whenever a decision is a person's, and delivers verified pull requests that a person merges.

### Install

- macOS, Apple silicon and Intel: `brew tap pitchopp/owlshift https://github.com/pitchopp/owlshift`, `brew trust --formula pitchopp/owlshift/owlshift` (Homebrew loads no formula of a tap it was not told to trust), then `brew install owlshift`. The archive of the release can also be downloaded with `curl` and unpacked by hand.
- Linux, x86_64 and arm64, as static binaries: `curl -fsSL https://raw.githubusercontent.com/pitchopp/owlshift/main/install.sh | sh`. The script checks the archive against the release's `SHA256SUMS` before it installs `owlshift` in `~/.local/bin`.
- An install made by the script or by hand updates itself with `owlshift self-update`, which verifies the release's signature with the key built into the binary, then its checksum, before it replaces the binary; `owlshift self-update --check` only says whether a newer release is out. Homebrew installs update with `brew upgrade owlshift`.
- Each release holds one archive per target, `owlshift-<version>-<target>.tar.gz`, their `SHA256SUMS`, and `SHA256SUMS.minisig`, its signature with Owlshift's own key: check it with `minisign -Vm SHA256SUMS -p minisign.pub`, the public key at the repository's root. Each archive holds `THIRD-PARTY-NOTICES.txt`, the licences of the crates and fonts the binary carries.
- Prerequisites: git 2.39.0 or later, and Claude Code installed. `owlshift doctor` says what is missing and how to fix it.

### What it does

- `owlshift init` writes a commented `owlshift.toml` for a repository, with the gate its stack suggests (Cargo, npm, pnpm, yarn, bun, Python, a Makefile), signs in to GitHub through Owlshift's GitHub App, and keeps the secrets in the system keychain. `owlshift doctor` checks the machine and the project and says how to fix each problem.
- `owlshift do` runs one ticket to a verified pull request: intake, design, design review, build and verify, each review by a fresh run, with the project's gate run confined. Questions a run needs answered are posted on the ticket; `owlshift continue` picks the ticket up once its decider has answered there.
- `owlshift watch` continues tickets as their answers come, repairs its own red or conflicting pull requests, and with `--drain` takes in the admitted tickets of the ready column, in priority order, respecting blockers, several at a time without collisions. A usage limit pauses a harness and hands the review roles to a declared fallback.
- `owlshift start` serves a project with a background service, at login with `--at-login`; `stop`, `pause`, `resume`, `cancel`, `status`, `why` and `logs` drive and explain it, and `owlshift ui` opens the local web UI. `owlshift uninstall` removes exactly what was installed.
- Merge on green is opt-in per project (`[forge] merge`); by default a person merges.
- Trackers: Linear, and Markdown files in the repository for one machine without an account. Forge: GitHub. Harnesses: Claude Code for every role, on the user's own subscription; Codex as an opt-in reviewer.
- Every agent run, and every gate and setup command, is confined: `sandbox-exec` on macOS, `bwrap` on Linux. Native Windows is refused; WSL2 is the intended path there, not yet checked with a release binary.
