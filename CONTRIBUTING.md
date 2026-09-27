# Contributing to Owlshift

Thank you for your interest. Owlshift is in its design phase: read the design in [`docs/design/`](docs/design/) first, starting with [architecture](docs/design/architecture.md) and the [roadmap](docs/design/roadmap.md). The design documents are the reference; a change that contradicts them starts with a change to the document, reviewed like code.

## Where work is tracked

Until the repository opens publicly (roadmap step P9), work items live in the maintainer's Linear workspace, team `OWL`, one project per roadmap step. Each item states its blockers; an item is not started before its blockers are done.

## Development

The Rust toolchain and the exact commands are set up with the Cargo workspace (issue OWL-8). The gate every change must pass, locally and in CI on macOS, Linux and Windows:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

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

## Licence

Contributions are accepted under the [Apache License 2.0](LICENSE), the licence of the project. No other agreement is required.

## Security

Do not open a public issue for a vulnerability. Use GitHub's private vulnerability reporting on this repository once it is public; until then, contact the maintainer directly.
