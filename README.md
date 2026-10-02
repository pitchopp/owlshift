# Owlshift

**Your backlog works the night shift.**

Owlshift is an open-source runner that works a team's existing backlog continuously with coding agents. It pulls ready tickets from your tracker, takes each one through a plan-review-build-verify pipeline, stops and asks **on the ticket** whenever a decision is yours, and delivers a verified pull request. Humans merge.

- **Your tracker stays the interface.** No new board to adopt: questions, answers and delivery reports live on the ticket.
- **The human decision loop is the product.** Questions can come at any stage; answers are checked before anything resumes; there is no limit on rounds.
- **Scheduling is enforced.** Blockers gate dispatch, and declared resources keep two in-flight tickets (agent or human) from colliding.
- **Bring your own agents, on your own subscription.** Claude Code, Codex, or a mix, per project, per role or per ticket. Owlshift drives the CLIs on your subscription (Codex through your login, Claude Code agent runs through a `claude setup-token` token that `owlshift init` stores, never your own Claude Code login) and never asks for an API key.
- **Runs on your machine.** One light native background service; zero tokens at rest.

## Status

Early development. Steps P0 and P1 of the roadmap are built: the CLI runs one ticket to a verified pull request in the foreground. The commands available today:

- `owlshift doctor`: check whether this machine is ready (git, the harness CLIs and their logins, the agent runs' isolation and Claude Code token, the configuration files), and say why each problem matters and how to fix it.
- `owlshift config show`: print the effective configuration and the file each value comes from.
- `owlshift init`: write a commented `owlshift.toml` for this repository, then store the tracker and forge secrets and the agent runs' Claude Code token that `owlshift do` needs in the system keychain.
- `owlshift do TICKET`: run one ticket to a verified pull request, in the foreground.
- `owlshift logs [TICKET] [--last N] [--follow]`: print the events `owlshift do` recorded, oldest first, optionally only one ticket's, only the last N with `--last N`, and keep printing new ones with `--follow`.

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

## Project tracking

The backlog lives in a Linear workspace (team `OWL`), with one project per roadmap step. It moves to public GitHub issues when the repository opens (step P9).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Every commit carries a Developer Certificate of Origin sign-off (`git commit -s`).

## License

[Apache-2.0](LICENSE).
