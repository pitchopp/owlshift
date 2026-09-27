# Owlshift

**Your backlog works the night shift.**

Owlshift is an open-source runner that works a team's existing backlog continuously with coding agents. It pulls ready tickets from your tracker, takes each one through a plan-review-build-verify pipeline, stops and asks **on the ticket** whenever a decision is yours, and delivers a verified pull request. Humans merge.

- **Your tracker stays the interface.** No new board to adopt: questions, answers and delivery reports live on the ticket.
- **The human decision loop is the product.** Questions can come at any stage; answers are checked before anything resumes; there is no limit on rounds.
- **Scheduling is enforced.** Blockers gate dispatch, and declared resources keep two in-flight tickets (agent or human) from colliding.
- **Bring your own agents, on your own subscription.** Claude Code, Codex, or a mix, per project, per role or per ticket. Owlshift drives the CLIs you are already logged in to and never asks for an API key.
- **Runs on your machine.** One light native background service; zero tokens at rest.

## Status

Design phase. Nothing to install yet. The design is in [`docs/design/`](docs/design/):

| Document | What it holds |
| --- | --- |
| [Design & architecture](docs/design/architecture.md) | Positioning, principles, core model, pipeline and gates, architecture, adapters, state and coordination, policy, cost, technology, milestones, decisions |
| [Scenarios](docs/design/scenarios.md) | Seventeen user stories with acceptance criteria; each becomes an end-to-end test |
| [Runtime & operations](docs/design/runtime-and-operations.md) | How it runs, platforms, install and uninstall, updates, configuration, observability, testing |
| [Roadmap](docs/design/roadmap.md) | Twelve shippable steps, P0 to P11, with their exit gates |
| [Build plan](docs/design/build-plan.md) | Checks to run first, workspace layout, contracts, CLI, tasks for P0 and P1 |

## License

[Apache-2.0](LICENSE).
