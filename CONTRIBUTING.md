# Contributing to Owlshift

Thank you for your interest. Owlshift is in its design phase: read the design in [`docs/design/`](docs/design/) first, starting with [architecture](docs/design/architecture.md) and the [roadmap](docs/design/roadmap.md). The design documents are the reference; a change that contradicts them starts with a change to the document, reviewed like code.

## Where work is tracked

Until the repository opens publicly (roadmap step P9), work items live in the maintainer's Linear workspace, team `OWL`, one project per roadmap step. Each item states its blockers; an item is not started before its blockers are done.

## Development

Owlshift is a Cargo workspace; its crates live in `crates/`. Install [rustup](https://rustup.rs/): the toolchain is pinned in `rust-toolchain.toml`, and rustup installs that exact version, with rustfmt and clippy, the first time you run `cargo` in the repository.

The gate every change must pass, locally and in CI on macOS, Linux and Windows:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
cargo test --workspace --doc
```

The tests run under [cargo-nextest](https://nexte.st/) (`cargo install cargo-nextest --locked`), one process per test, as CI and `owlshift.toml` run them: some stop or lock what their own process started, which under `cargo test` reaches the other tests of the same binary. nextest runs no doctests, hence the last line.

CI adds `--locked` to the clippy and test steps, so a change to a `Cargo.toml` must come with its updated `Cargo.lock`.

`owlshift-core` performs no I/O. `crates/owlshift-core/clippy.toml` lists the standard library's file, network, process, environment, standard-stream and clock entry points, and the clippy step fails on any use of them in that crate.

### Day-to-day loop

- `cargo owl <command>` runs the CLI from source, rebuilding only what changed: `cargo owl doctor`, `cargo owl config show`. The alias lives in `.cargo/config.toml`.
- [bacon](https://dystroy.org/bacon/) re-runs a job on every save (`cargo install --locked bacon`). The jobs are in `bacon.toml`: `bacon` checks that the workspace compiles, `bacon test` runs the tests, `bacon clippy-all` runs the CI lint, and `bacon owl -- doctor` re-runs a CLI command.
- `cargo install --path crates/owlshift-cli --locked` installs the `owlshift` binary in `~/.cargo/bin` to use it as a command; run it again after pulling to update.

## Branches and pull requests

- One issue per pull request. Put the issue ID in the branch name: `owl-12-short-title`.
- The pull request says what changed, why, and how it was tested.
- CI must be green on every platform before review.
- Everything is written in English: code, comments, documentation, commit messages.
- Commit messages start with a verb in the imperative mood: "Add the doctor command", not "Added" or "Adds".

## Developer Certificate of Origin

Owlshift uses the [Developer Certificate of Origin](https://developercertificate.org/) (DCO), version 1.1, instead of a contributor license agreement. By signing off a commit, you certify that you wrote the change or otherwise have the right to submit it under the project's licence.

Every commit must carry a sign-off line matching its author:

```text
Signed-off-by: Jane Doe <jane@example.com>
```

`git commit -s` adds it for you. To fix commits that miss it:

```bash
git commit --amend -s --no-edit     # the last commit
git rebase --signoff main           # every commit of the branch
```

A pull request with an unsigned commit cannot be merged.

## Releasing

The maintainer makes releases. The workflow is `.github/workflows/release.yml`, and the formats are in the [build plan](docs/design/build-plan.md), "P9 · Open-source readiness", part 10.

**Once.** These steps were done for 0.1.0, on 2026-11-12.

1. Make Owlshift's signing key without a password, since nobody could type one in the workflow: `minisign -G -W -p minisign.pub -s owlshift-release.key`. Keep the secret key offline, for instance in a password manager. Commit `minisign.pub` at the repository's root.
2. Create the environment `release`: deployment tags `v*`, and yourself as required reviewer. Store the key in it with `gh secret set MINISIGN_SECRET_KEY --env release -R pitchopp/owlshift < owlshift-release.key`. The reviewer guards against another account's tag. It does not stop a tool that holds your own GitHub token: never let one approve a deployment.

**Each release.**

1. In a pull request, set the version in `Cargo.toml` (`[workspace.package]`) and add its `## <version>` section to `CHANGELOG.md`, which becomes the release's notes. A test fails while the version has no section.
2. Once it is merged, tag a commit of main and push the tag: `git tag v0.1.0 origin/main && git push origin v0.1.0`.
3. The run builds, then waits for your approval of the environment `release`. Approve it only for a run whose commit is a commit of main. The workflow refuses others, but that check is in the very file a run on another commit could change.
4. Check the draft release: one `owlshift-<version>-<target>.tar.gz` per target, `SHA256SUMS` and `SHA256SUMS.minisig`. Each archive holds `owlshift`, `LICENSE`, `README.md` and `THIRD-PARTY-NOTICES.txt` (`tar -tzf <archive>`). Then publish it.
5. Commit the run's `homebrew-formula` artifact, also shown in the run's summary, as `Formula/owlshift.rb` on main. The repository is its own tap. Check that the formula installs on a Mac with `brew tap pitchopp/owlshift https://github.com/pitchopp/owlshift`, `brew trust --formula pitchopp/owlshift/owlshift` and `brew install owlshift`: the release's own URLs are checked only there.

The job `notices` fails when a crate of the binary is under a licence `scripts/release/about.toml` does not accept. Accepting it is a legal decision, recorded on a ticket before the licence is added there; otherwise replace the crate.

If a run fails, delete the draft if there is one and the tag (`git push origin :refs/tags/v0.1.0`), fix the cause on main, and tag again. Approving a run of another ref, as for the probe of OWL-540, means reading that ref's `release.yml` first.

**Checking a release.** `minisign -Vm SHA256SUMS -p minisign.pub` checks the signature, then `sha256sum -c --ignore-missing SHA256SUMS` checks the archives. The Linux install script checks the archive's checksum only. That catches a damaged download, not someone able to replace the release's files: the signature is what says the files are Owlshift's.

**A leaked key.** Make a new key, replace `minisign.pub` and the secret, and release again. A binary that carries the old key (`owlshift self-update`, OWL-541) refuses the new release until it is reinstalled by hand.

## Licence

Contributions are accepted under the [Apache License 2.0](LICENSE), the licence of the project. No other agreement is required.

## Security

Do not open a public issue for a vulnerability. Use GitHub's private vulnerability reporting on this repository once it is public; until then, contact the maintainer directly.
