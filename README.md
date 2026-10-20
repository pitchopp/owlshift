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

Early development. Steps P0 and P1 of the roadmap are built: the CLI runs one ticket to a verified pull request in the foreground. P2, questions on the ticket, is in progress: a run's questions are posted on the ticket, and `owlshift continue` picks the ticket up once they are answered. P3 has started with `owlshift watch`, which does that by itself. The commands available today:

- `owlshift doctor`: check whether this machine is ready (git, the harness CLIs and their logins, the agent runs' isolation and Claude Code token, the configuration files, whether the GitHub credential can read the repository's branches), and say why each problem matters and how to fix it. Designed, not built yet: lines for Owlshift's GitHub App, installed on the repository, its token valid and each permission it asks, and a warning on a personal token, the expert option ([design](docs/design/build-plan.md), "GitHub App sign-in (OWL-336)").
- `owlshift config show`: print the effective configuration and the file each value comes from.
- `owlshift init`: write a commented `owlshift.toml` for this repository, then store its tracker and forge secrets (under keychain accounts named by its GitHub repository) and the agent runs' Claude Code token that `owlshift do` needs in the system keychain. Designed, not built yet: signing in to GitHub through Owlshift's GitHub App, with a code to enter on GitHub and no token to make, a personal token through `--github-token` staying the expert option.
- `owlshift do TICKET`: run one ticket to a verified pull request, in the foreground; questions the run needs answered are posted on the ticket. The gate and rules come from the `owlshift.toml` of the commit the ticket's branch was made from, so a change to them applies once merged ([design](docs/design/architecture.md), "Which commit configures a run").
- `owlshift continue TICKET`: once the ticket's questions are answered there and the answer has been left unedited for 10 minutes, or the project's `policy.quiet_window_minutes` (or ends with `go`), check the answers, then ask again what is missing or run on to a verified pull request; restarts a parked ticket.
- `owlshift watch`: in the foreground, continue each ticket whose questions wait, as `owlshift continue` would, once its decider's reply counts, until Ctrl-C. It also resumes a ticket at Intake, Design, Design review, Build or Verify that an answered round (round > 0) left shown working or needing input, a refusal after the answer check included. A parked ticket, or one left at Build before any question round, still needs `owlshift continue`. With `--drain`, it also takes in the admitted tickets of the ready column (`[tracker].admit` by an account `[tracker].admitters` lists), intake first, and builds them one at a time.
- `owlshift forget TICKET`: start over what Owlshift keeps on a ticket when that record has no room left or cannot be read; its asks and decisions go, their comments staying on the ticket, and its round count and the refusals its next runs are told stay.
- `owlshift gate [--at COMMIT]`: run the project's gate once, as a Build's gate runs it: in a fresh worktree of origin's default branch (or of `COMMIT`, a full commit id or a branch such as `origin/fix-gate`), confined, with the variables `stack.gate_env` declares and `allow_gate_env` allows and the home caches `stack.caches` declares and `allow_caches` allows, after the project's `stack.setup` commands; the gate is the project file's at that commit, or yours when it has none. It prints each command's last lines, status and time, then removes the worktree, unless the gate broke isolation, which leaves the project refused until a person looks.
- `owlshift unquarantine`: clear the quarantine a run whose isolation check did not pass leaves on this project, which makes every command refuse it; Owlshift notifies you of it once, and tells each ticket it holds up. It shows what the marker says, then clears it once you type the project's name (`owner/repo`) at a terminal; the dedicated checkout and the worktrees stay as they are.
- `owlshift logs [TICKET] [--last N] [--follow]`: print the events `owlshift do`, `owlshift continue`, `owlshift watch` and `owlshift forget` recorded, oldest first, optionally only one ticket's, only the last N with `--last N`, and keep printing new ones with `--follow`.

There is no packaged release yet. Install from source:

```bash
cargo install --path crates/owlshift-cli --locked
```

The "Development" section of [CONTRIBUTING.md](CONTRIBUTING.md#development) covers the rest. The design is in [`docs/design/`](docs/design/):

| Document | What it holds |
| --- | --- |
| [Design & architecture](docs/design/architecture.md) | Positioning, principles, core model, pipeline and gates, architecture, adapters, state and coordination, policy, cost, technology, milestones, decisions |
| [Scenarios](docs/design/scenarios.md) | Seventeen user stories with acceptance criteria; each becomes an end-to-end test |
| [Runtime & operations](docs/design/runtime-and-operations.md) | How it runs, platforms, install and uninstall, updates, configuration, observability, testing |
| [Roadmap](docs/design/roadmap.md) | Twelve shippable steps, P0 to P11, with their exit gates |
| [Build plan](docs/design/build-plan.md) | Checks to run first, workspace layout, contracts, CLI, tasks for P0 and P1 |
| [Visual identity](docs/design/visual-identity.md) | Personality, voice, logo, colours, typefaces, and how each surface applies them |

## Requirements

- **Operating system:** macOS or Linux; Windows through WSL2, running as Linux ([Platforms](docs/design/runtime-and-operations.md#platforms)).
- **git:** 2.39.0 or later, a prerequisite that `owlshift doctor` checks ([Install, lifecycle & uninstall](docs/design/runtime-and-operations.md#install-lifecycle--uninstall)).
- **Claude Code (`claude`),** on your own subscription, which runs every role, reviewers included; Codex is not required, and a project can add a Codex reviewer to each review with `pipeline.codex_reviewer` ([Install, lifecycle & uninstall](docs/design/runtime-and-operations.md#install-lifecycle--uninstall)).
- **Tracker and forge accounts:** one on the tracker whose tickets Owlshift works and one on the forge that hosts the repository; `owlshift init` keeps their secrets in the system keychain ([Adapters & contracts](docs/design/architecture.md#6-adapters--contracts)).
- **Rust toolchain 1.98 or later**, only for a build from source ([Technology choices](docs/design/architecture.md#10-technology-choices)).

## Quick start

From a clone of the repository you want worked, once `owlshift` is installed:

```bash
owlshift init
owlshift doctor
owlshift do OWL-12
owlshift watch
```

- `owlshift init`: write a commented `owlshift.toml` and store the tracker and forge secrets and the agent runs' Claude Code token in the system keychain.
- `owlshift doctor`: check that this machine is ready, and say how to fix what is not.
- `owlshift do OWL-12`: run that ticket to a verified pull request, in the foreground; if the run needs a decision, it posts its questions on the ticket.
- `owlshift watch`: continue the ticket once its questions are answered there, until Ctrl-C.

## Project tracking

The backlog lives in a Linear workspace (team `OWL`), with one project per roadmap step. It moves to public GitHub issues when the repository opens (step P9).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Every commit carries a Developer Certificate of Origin sign-off (`git commit -s`).

## License

[Apache-2.0](LICENSE).
