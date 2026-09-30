# Owlshift — roadmap

Status: draft, 2026-09-27. Twelve steps (P0 to P11), each shippable, testable and usable on its own before the next one starts. Owlshift works on its own backlog from P1 and on Locary's from P5.

| Step | Name | What you can do at the end |
| --- | --- | --- |
| P0 | Foundations | Build and test the project on three platforms; `owlshift doctor` tells you whether a machine is ready |
| P1 | One ticket, one PR, on demand | `owlshift do OWL-12` turns one ticket into a verified pull request, on your own Claude subscription |
| P2 | Questions on the ticket | The agent stops and asks on the ticket; you answer there; `owlshift resume` picks up from the checkpoint |
| P3 | Watch mode | `owlshift watch` notices your answers and resumes by itself, and repairs its own red PRs |
| P4 | The drain | Owlshift pulls admitted tickets from the ready column, in priority order, respecting blockers, one at a time |
| P5 | Quality pipeline | Plan, review by another model family, build, verify; plan approval by policy; per-ticket routing |
| P6 | Parallel | Several tickets at once without collisions; usage limits pause a harness and fall back to another |
| P7 | Background service | Starts at login, survives sleep and crashes, notifies you only when you are the blocker |
| P8 | Local web UI | Configuration, tickets, agent and orchestrator logs in the browser |
| P9 | Open-source readiness | First public 0.x release: GitHub Issues, Markdown tracker, `init` detection, a second project |
| P10 | Teams | Several developers on one project; a team-server mode |
| P11 | Menu-bar app | A Tauri shell around the web UI, with native notifications |

Three choices shape this order. The central bet, the question loop, comes at P2, before anything else is polished. Parallelism comes late (P6): run serially, tickets cannot collide, so the backlog drains from P4 without the hardest scheduling problem solved. And `/parallel`-style batch orchestration becomes unnecessary for Owlshift's own backlog at P4, and for Locary's at P6.

## P0 · Foundations

A repository that people and agents can build in safely, and a first command that is already useful.

- Live checks C1, C3, C6 and C7 (Claude part) run and recorded ([build plan](build-plan.md)).
- Cargo workspace with its six crates; CI on macOS, Linux and Windows running format, lint and tests.
- Contracts written and schema-checked: brief, `result.json`, project and personal config (TOML), events, git ref layout.
- Test harness: a fake harness, an in-repository test tracker, a local bare git remote, a scenario runner.
- CLI skeleton: `owlshift --version`, `owlshift doctor` (git, `claude` and `codex` installed, `codex` logged in, the token agent runs log in to Claude Code with stored by `owlshift init`), `owlshift config show`.
- Project tooling: the Owlshift Linear workspace (states including Needs Input and Triage, labels, one project per step), a contribution guide, decision D10.

**Exit gate.** CI green on the three platforms; one scenario runs end to end with the fake harness and the test tracker; `owlshift doctor` reports the maintainer's machine correctly.

## P1 · One ticket, one PR, on demand

`owlshift do OWL-12` replaces launching one agent by hand.

- Linear tracker adapter: read a ticket, post a comment.
- Claude Code harness: headless run on the user's login, model, effort, permission level, usage capture.
- Executor: worktree, brief, result validation, isolation check; the project's gate commands before delivery.
- GitHub forge adapter: push the branch, open the PR, read the complete check set.
- The floor from day one: no merge, no tracker credentials for agents, an exit code never trusted.
- Delivery report on the ticket. Scenario S1, triggered by hand.

**Exit gate.** Three real Owlshift tickets delivered as PRs by `owlshift do`, with no guardrail breach. From here, Owlshift builds Owlshift.

## P2 · Questions on the ticket

The central bet, used by hand.

- `questions` results become a marked, numbered comment; the ticket moves to Needs Input with its return stage recorded.
- Checkpoint on a git ref; `owlshift resume OWL-12` restarts from it without redoing finished steps.
- Answer check (answered, partial, unanswered, counter-question), re-ask of what is missing, late comments re-read before delivery.
- Resolver: discoverable questions decided and logged as reversible decisions; always-human categories enforced.
- Being told a question is waiting: the Linear app identity if check C4 passes, otherwise a local desktop notification. Scenarios S2 and S7.

**Exit gate.** Three tickets through at least two question rounds, one of them with an incomplete answer correctly re-asked.

## P3 · Watch mode

`owlshift watch` in a terminal replaces typing `resume`.

- Foreground loop over the tickets Owlshift started: quiet window, `go`, automatic resume.
- After the PR: a red check starts a fix run (two review passes at most); a conflict starts a rebase run. Scenario S9.

**Exit gate.** A full day of use without typing `resume`.

## P4 · The drain

Owlshift picks its own work, one ticket at a time.

- Admission gesture, ready-column polling in priority order, blockers from relations and from the text convention.
- Intake stage: early questions, dependencies turned into relations, the routing record.
- Cap on open PRs awaiting review; follow-up proposals to Triage, deduplicated. Scenarios S3, S4, S6, S12, S13.

**Exit gate.** Ten Owlshift tickets drained serially, at least one started by the merge of its blocker.

## P5 · Quality pipeline

PR quality at least on par with the current `/parallel` workflow.

- Stages design, design review, build, verify; variants `trivial`, `standard`, `risky`; plan approval policy.
- Codex harness for review roles; the cross-family rule; per-ticket routing (harness, tier, effort); false-premise stop. Scenarios S8 and S17 (routing part).

**Exit gate.** Five PRs judged at least as good as `/parallel` output by the maintainer; first Locary tickets delivered.

## P6 · Parallel

Owlshift replaces `/parallel` on Locary.

- Zones and resources declared at intake; collisions checked against every ticket in flight, human work and open PRs included; `owlshift why`.
- Machine-wide caps across projects; usage-limit detection, pause until reset, fallback harness. Scenarios S5, S10, S16, S17.

**Exit gate.** A week on Locary with three concurrent tickets and no collision.

## P7 · Background service

Set it and forget it.

- `start --at-login`, `stop`, `uninstall` with its manifest, `doctor` cleanup, self-healing launcher, single instance.
- Claims with leases on git refs, recovery after sleep or crash, keep-awake, notifier adapter and daily digest, `self-update`. Scenario S11.

**Exit gate.** Seven days unattended with no double dispatch.

## P8 · Local web UI

The overview, ticket, orchestrator and configuration screens described in [runtime & operations](runtime-and-operations.md), served on `127.0.0.1` behind a per-install token.

**Exit gate.** A day of operation followed from the UI alone.

## P9 · Open-source readiness

- GitHub Issues adapter, public Markdown tracker, conformance suites, `init` stack detection. Scenario S15.
- A second project on another stack (decision D6); the Docker server image; signed release 0.1; the repository made public.

**Exit gate.** The second project adopts Owlshift with configuration only, no change to the core.

## P10 · Teams

Deciders, identity map across tracker and forge, per-developer runners, team-server mode, agent identities (Linear app user, GitHub App). Scenario S14.

**Exit gate.** Two developers share one project for two weeks.

## P11 · Menu-bar app

A Tauri 2 shell around the P8 UI: tray icon, native notifications, login item, answering a gate from the app.

**Exit gate.** The app replaces the terminal for daily use on macOS and Windows.

## Scenario coverage

| Scenario | Step |
| --- | --- |
| S1 | P1 |
| S2, S7 | P2 (automatic resume in P3) |
| S9 | P3 |
| S3, S4, S6, S12, S13 | P4 |
| S8 | P5 |
| S5, S10, S16, S17 | P6 |
| S11 | P7 |
| S15 | P9 |
| S14 | P10 |
