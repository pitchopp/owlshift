<h1 align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/brand/owlshift-lockup-on-dark.svg">
    <img alt="Owlshift" src="assets/brand/owlshift-lockup.svg" width="360">
  </picture>
</h1>

<p align="center"><strong>Your backlog works the night shift.</strong></p>

Owlshift is an open-source runner that works a team's existing backlog continuously with coding agents. It pulls ready tickets from your tracker, takes each one through a plan-review-build-verify pipeline, stops and asks **on the ticket** whenever a decision is yours, and delivers a verified pull request. Humans merge.

- **Your tracker stays the interface.** No new board to adopt: questions, answers and delivery reports live on the ticket.
- **The human decision loop is the product.** Questions can come at any stage; answers are checked before anything resumes; there is no limit on rounds.
- **Scheduling is enforced.** Blockers gate dispatch, and declared resources keep two in-flight tickets (agent or human) from colliding.
- **Bring your own agents, on your own subscription.** Claude Code, Codex, or a mix, per project, per role or per ticket. Owlshift drives the CLIs on your subscription (Codex through your login, Claude Code agent runs through a `claude setup-token` token that `owlshift init` stores, never your own Claude Code login) and never asks for an API key.
- **Runs on your machine.** One light native background service; zero tokens at rest.

## Status

Release 0.1.0 is the first ([changelog](CHANGELOG.md)). Owlshift takes tickets from Linear, from GitHub Issues, or from Markdown files in your repository, runs each through intake, design, design review, build and verify with Claude Code, asks on the ticket when a decision is yours, and opens a verified pull request. A background service drains a project's ready tickets, several at a time without collisions, and repairs its own red or conflicting pull requests. It runs on macOS and Linux, and on Windows through WSL2. Owlshift is still young: the roadmap's later steps (shared team servers, more trackers and forges, a desktop app) are in [`docs/design/`](docs/design/).

## Requirements

- **Operating system:** macOS or Linux, on Apple silicon, Intel, x86_64 or arm64; Windows through WSL2, running as Linux. Native Windows is refused, since Owlshift cannot confine an agent there.
- **On Linux, bubblewrap (`bwrap`):** every agent run is confined with it, and Owlshift runs no agent without it. `owlshift doctor` says whether it can confine a run, and how to fix it when it cannot.
- **On Linux, a Secret Service** such as GNOME Keyring, where Owlshift keeps its secrets, as macOS keeps them in the Keychain. A server or WSL2 without a desktop session may need one started.
- **git 2.39.0 or later.**
- **Claude Code (`claude`) installed, and a Claude subscription:** Claude Code runs every role, reviewers included. Agent runs log in with a long-lived token of your subscription that you make with `claude setup-token`; Owlshift never asks for a model API key. Codex is optional.
- **A repository on GitHub,** where Owlshift pushes branches and opens pull requests through its GitHub App, which you sign in to during `owlshift init`.

## Quick start

Install the latest release.

```bash
# macOS
brew tap pitchopp/owlshift https://github.com/pitchopp/owlshift
brew trust --formula pitchopp/owlshift/owlshift
brew install owlshift

# Linux, and Windows through WSL2: installs ~/.local/bin/owlshift
curl -fsSL https://raw.githubusercontent.com/pitchopp/owlshift/main/install.sh | sh
```

Homebrew asks for `brew trust` because the repository is its own tap, not one of Homebrew's. The Linux script checks the archive against the release's `SHA256SUMS` before it installs anything; [SECURITY.md](SECURITY.md) says how to check the release's signature too. Then check the install:

```bash
owlshift --version
owlshift doctor
```

Outside a repository, `owlshift doctor` checks the machine: git, Claude Code, and whether agent runs can be confined. To update later, run `brew upgrade owlshift` on macOS, or `owlshift self-update` for an install made by the script.

To build from source instead, with Rust 1.98 or later: `cargo install --git https://github.com/pitchopp/owlshift --locked owlshift-cli`.

### Adopt a project

Then follow the page of the tracker your tickets live in. Each takes you from `owlshift init` in your repository to a first pull request, then to the background service:

| Your tickets live in | Page |
| --- | --- |
| Linear | [Adopt a project on Linear](docs/adopt/linear.md) |
| GitHub Issues, in the repository itself | [Adopt a project on GitHub Issues](docs/adopt/github-issues.md) |
| Nowhere yet, or you want to try without an account | [Adopt a project with Markdown tickets](docs/adopt/markdown.md) |

The commands, in the order the pages use them:

- `owlshift init`: write a commented `owlshift.toml` for the repository, with a gate guessed from its stack, sign in to GitHub, and keep the secrets in the system keychain.
- `owlshift doctor`: check this machine and this project, and say how to fix each problem.
- `owlshift gate`: run the project's gate once, confined as an agent's run, to check it before any agent depends on it.
- `owlshift do TICKET`: run one ticket to a verified pull request, in the foreground; its questions are posted on the ticket.
- `owlshift continue TICKET`: go on once the ticket's questions are answered there.
- `owlshift start`: serve the project with the background service, which takes the tickets you admit; `owlshift status`, `owlshift why` and `owlshift logs` say what it does and why a ticket waits; `owlshift stop` stops it.

`owlshift --help` lists every command, and `owlshift help` followed by a command's name, as in `owlshift help do`, explains it. [SECURITY.md](SECURITY.md) says what the confinement of agent runs protects, and what it does not.

## Design

The design is in [`docs/design/`](docs/design/):

| Document | What it holds |
| --- | --- |
| [Design & architecture](docs/design/architecture.md) | Positioning, principles, core model, pipeline and gates, architecture, adapters, state and coordination, policy, cost, technology, milestones, decisions |
| [Scenarios](docs/design/scenarios.md) | Seventeen user stories with acceptance criteria; each becomes an end-to-end test |
| [Runtime & operations](docs/design/runtime-and-operations.md) | How it runs, platforms, install and uninstall, updates, configuration, observability, testing |
| [Roadmap](docs/design/roadmap.md) | Twelve shippable steps, P0 to P11, with their exit gates |
| [Build plan](docs/design/build-plan.md) | Checks to run first, workspace layout, contracts, CLI, tasks for each step |
| [Visual identity](docs/design/visual-identity.md) | Personality, voice, logo, colours, typefaces, and how each surface applies them |

The "Development" section of [CONTRIBUTING.md](CONTRIBUTING.md#development) covers building and testing Owlshift itself.

## Project tracking

The backlog lives in a Linear workspace (team `OWL`), with one project per roadmap step. It moves to public GitHub issues when the repository opens (step P9).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Every commit carries a Developer Certificate of Origin sign-off (`git commit -s`).

## License

[Apache-2.0](LICENSE).
