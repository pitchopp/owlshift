# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- P2, questions on the ticket (in progress, its exit gate has not passed): a run that cannot decide stops and asks on the ticket, the decider answers there, and `owlshift continue` restarts from the checkpoint.
- P1, one ticket, one PR, on demand: `owlshift do` takes a ticket through a worktree, a Claude Code run and the project's gate to a pull request, with the Linear and GitHub adapters and no merge by agents.
- P0, foundations: the Cargo workspace, the contracts and their schemas, the test harness and scenario runner, and the CLI skeleton (`--version`, `doctor`, `config show`).
