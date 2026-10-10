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

- `owlshift doctor`: check whether this machine is ready (git, the harness CLIs and their logins, the agent runs' isolation and Claude Code token, the configuration files), and say why each problem matters and how to fix it.
- `owlshift config show`: print the effective configuration and the file each value comes from.
- `owlshift init`: write a commented `owlshift.toml` for this repository, then store the tracker and forge secrets and the agent runs' Claude Code token that `owlshift do` needs in the system keychain.
- `owlshift do TICKET`: run one ticket to a verified pull request, in the foreground; questions the run needs answered are posted on the ticket.
- `owlshift continue TICKET`: once the ticket's questions are answered there and the answer has been left unedited for 10 minutes, or the project's `policy.quiet_window_minutes` (or ends with `go`), check the answers, then ask again what is missing or run on to a verified pull request; restarts a parked ticket.
- `owlshift watch`: in the foreground, continue each ticket whose questions wait, as `owlshift continue` would, once its decider's reply counts, until Ctrl-C. A parked ticket, or one left at Build, still needs `owlshift continue`.
- `owlshift forget TICKET`: start over what Owlshift keeps on a ticket when that record has no room left or cannot be read; its asks and decisions go, their comments staying on the ticket, and its round count and the refusals its next runs are told stay.
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

## Project tracking

The backlog lives in a Linear workspace (team `OWL`), with one project per roadmap step. It moves to public GitHub issues when the repository opens (step P9).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Every commit carries a Developer Certificate of Origin sign-off (`git commit -s`).

## License

[Apache-2.0](LICENSE).
