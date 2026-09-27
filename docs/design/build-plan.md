# Owlshift — build plan for P0 and P1

Status: draft, 2026-09-27. What to build first, in order: the foundations (P0), then one ticket turned into a verified pull request on demand (P1). The steps after that are described in the [roadmap](roadmap.md).

## Checks to run live before writing code

Seven assumptions carry the design; each is checked with a real call before the code that depends on it, and the dated result is written down next to the decision it settles.

| # | Assumption | Depends on it | Check | Before |
| --- | --- | --- | --- | --- |
| C1 | `claude -p` runs headless on the user's own subscription login, with no API key, and honours `--model`, `--effort`, `--permission-mode` and structured output inside a worktree | The whole executor | One tiny run in a scratch repository | P0 |
| C2 | `codex exec` runs headless on a ChatGPT subscription login, gives structured output, accepts a reasoning-effort setting, confines writes to the worktree in `workspace-write`, and exits non-zero on failure | Codex reviewers | One tiny run | P5 |
| C3 | GitHub accepts pushes to a custom ref namespace, and creating an existing ref is rejected, so a claim is atomic | Claims | Two concurrent pushes to a test repository | P0 |
| C4 | Linear exposes a comment's last-edit time; an issue can be created directly in Triage with an API key; an app user works with polling only | S2 quiet window, S13, D8 | API calls on the Owlshift workspace | P2 |
| C5 | Optional reading, not a gate: Sortie's and Symphony's `WORKFLOW.md` and their adapter pitfalls | D5, Jira adapter | Read their docs and adapter code | P0 |
| C6 | `owlshift` is free where it matters: crates.io and npm were free on 2026-09-27; the GitHub name `owlshift` is taken, so the repository lives on the maintainer's personal account (`pitchopp/owlshift`), and `owlshift.dev` still needs a registrar check | D3 | Registrar check | P0 |
| C7 | Each provider's terms allow unattended headless use of a personal subscription through its own CLI; each CLI reports a reached usage limit in a detectable way (message, reset time, exit status) | Subscription-first design, S10, S17 | Read the current terms; trigger or capture a limit message for each CLI | P0 for Claude, P5 for Codex |

## Repository & workspace layout

One repository, a Cargo workspace of six crates split along the design's boundaries, so the pure core never depends on I/O.

| Crate | Holds | Depends on |
| --- | --- | --- |
| `owlshift-core` | Ticket, pipeline, stage, gate, resource; state machine; scheduler; policy. Pure functions, no I/O | nothing |
| `owlshift-contracts` | Brief, `result.json`, project and personal config, events, git ref layout, format versions; JSON Schemas generated from the types | core |
| `owlshift-adapters` | Tracker (Linear, plus a test tracker), forge (GitHub), harness (Claude Code, fake; Codex in P5), notifier; each behind a trait with declared capabilities | contracts |
| `owlshift-platform` | Service install and uninstall, process groups and Job Objects, keychain, platform directories, keep-awake | nothing |
| `owlshift-runner` | Daemon loop, executor (worktrees, spawning, isolation check), writer, local store (SQLite) | all of the above |
| `owlshift-cli` | The `owlshift` binary | runner |

Beside the crates: `roles/` (default role prompts in Markdown, versioned with the contract), `schemas/` (generated JSON Schemas), `tests/scenarios/` (S1 to S17 with fixtures), `fixtures/fake-harness/`, `docs/`.

**Git through the `git` CLI**, not a library: worktrees, pushes and credential helpers then behave exactly as in the user's own terminal. **Harnesses through their own CLI**, never through a model API: the user's subscription login is what runs. Library choices (async runtime, HTTP client, SQLite binding, CLI parser, keychain access) are made at implementation time against current documentation.

**Repository hygiene from the first commit:** Apache-2.0 licence, English, a contribution guide, and a CI matrix on macOS, Linux and Windows.

## Contracts to write first

Six contracts are written, versioned and schema-checked before any logic, because every component and every future adapter talks through them.

1. **Brief** (runner to role): format version, role, the ticket (text from anyone but the decider quoted as data), the decider, the numbered question-and-answer thread with authors marked, the checkpoint (plan, ledger), resources and zones, the project rules injected for those zones, permitted actions, and where to write the result.
2. **`result.json`** (role to runner): validated against its JSON Schema, unknown fields rejected, an exit code never trusted in its place.
3. **Project and personal config**: TOML, with published JSON Schemas so editors complete and check them.
4. **Events**: timestamp, project, ticket, run, kind, data; the one stream behind logs, `why`, the UI and usage reports.
5. **Git ref layout**: `refs/owlshift/claims/<ticket>` holds the lease (holder machine, operator, expiry, format); `refs/owlshift/tickets/<ticket>` points to a commit whose tree holds the stage, plan, ledger, questions and findings.
6. **Marked comments**: a first line such as `[owlshift] QUESTIONS · round 2`, plus a machine-readable footer where the tracker can hide it; exact form checked per tracker.

A first cut of `result.json`, to be refined while writing the schema:

```json
{
  "format": 1,
  "status": "questions",
  "summary": "Plan drafted; two choices need the decider.",
  "questions": [
    {
      "id": "Q1",
      "category": "scope",
      "context": "The ticket asks for X; the code already does Y in module Z.",
      "text": "Extend Y, or build X separately?",
      "options": ["Extend Y", "Build X separately"],
      "recommendation": "Extend Y"
    }
  ],
  "decisions": [
    { "question": "Test framework", "decision": "Reuse the existing one", "basis": "Project convention" }
  ],
  "followups": [],
  "artifacts": { "plan": "plan.md", "ledger": "ledger.json" },
  "pr": null
}
```

`status` takes one of `done`, `questions`, `blocked`, `premise_false` or `failed`. When `status` is `done` after Verify, `pr` carries the branch, title and body; the runner opens the PR itself. A run stopped by a usage limit is not a `result.json` status: the runner detects it from the CLI output and records it as an interruption to resume.

## CLI surface for P0 and P1

Everything runs in the foreground; `resume` arrives in P2, `watch` in P3, the background service and its `start`, `stop` and `uninstall` commands in P7.

| Command | Step | Behaviour |
| --- | --- | --- |
| `owlshift --version` | P0 | Version and format versions |
| `owlshift doctor` | P0 | git version, `claude` and `codex` installed and logged in, capabilities of the configured adapters |
| `owlshift config show` | P0 | Effective configuration and the origin of each value |
| `owlshift init` | P1 | Writes a commented `owlshift.toml` for the repository |
| `owlshift do TICKET` | P1 | Runs one ticket to a verified PR, in the foreground |
| `owlshift logs [TICKET] [--follow]` | P1 | Events and run logs |

**The test tracker** (published as the Markdown tracker in P9) keeps one folder per ticket in the repository: `tickets/PROJ-1/ticket.md` with a front matter (stage, priority, assignee, labels, blocked_by) and the description, and `tickets/PROJ-1/comments/` with one file per comment, named by timestamp and author. Answering a question is adding a file; everything stays diffable in git.

## P0 tasks & exit gate

Each task is an issue in the Owlshift Linear workspace (team `OWL`), with its blockers recorded as relations.

- [x] Create the repository with its licence (2026-09-27)
- [x] OWL-5 · Set up the Owlshift Linear workspace: team `OWL`, states including Needs Input and Triage, labels, one project per step (2026-09-27)
- [ ] OWL-6 · Run checks C1, C3, C6 and C7 (Claude part) and record the results
- [ ] OWL-7 · Settle decision D10 (contribution terms); add the contribution guide
- [ ] OWL-8 · Cargo workspace with the six crates; CI matrix on macOS, Linux and Windows (format, lint, tests)
- [ ] OWL-9 · Write the contracts and generate their JSON Schemas
- [ ] OWL-10 · Core: model, state machine, scheduler skeleton, policy floor, with unit tests
- [ ] OWL-11 · Fake harness, test tracker, local bare remote and scenario runner; one scenario end to end
- [ ] OWL-12 · CLI: `--version`, `doctor`, `config show`

**Exit gate.** CI green on the three platforms; one scenario runs end to end with the fake harness and the test tracker; `owlshift doctor` reports the maintainer's machine correctly.

## P1 tasks & exit gate

- [ ] OWL-13 · Linear tracker adapter: read a ticket, post a comment, with recorded fixtures and a conformance suite
- [ ] OWL-14 · Claude Code harness adapter: headless run on the user's login, model, effort, permission level, usage capture
- [ ] OWL-15 · Executor: worktree, brief, spawn, process-tree stop, result validation, isolation check
- [ ] OWL-16 · Gate commands from the project config before delivery
- [ ] OWL-17 · GitHub forge adapter: push the branch, open the PR, read the complete check set
- [ ] OWL-18 · Writer: delivery report on the ticket
- [ ] OWL-19 · Default build role prompt
- [ ] OWL-20 · CLI: `init`, `do`, `logs`

**Exit gate.** Three real Owlshift tickets delivered as PRs by `owlshift do` on the maintainer's subscription, with no guardrail breach.
