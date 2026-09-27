# Owlshift — v0 build plan

Status: draft, 2026-09-27. v0 proves the core on a sample repository: one ticket from the Markdown tracker goes from intake to a green pull request through Claude Code, with every contract written down first.

## Checks to run live before writing code

Seven assumptions carry the design; each is checked with a real call before the code that depends on it, and the dated result is written down next to the decision it settles.

| # | Assumption | Depends on it | Check | Before |
| --- | --- | --- | --- | --- |
| C1 | `claude -p` runs headless on the user's own subscription login, with no API key, and honours `--model`, `--effort`, `--permission-mode` and structured output inside a worktree | The whole executor | One tiny run in a scratch repository | v0 |
| C2 | `codex exec` runs headless on a ChatGPT subscription login, gives structured output, accepts a reasoning-effort setting, confines writes to the worktree in `workspace-write`, and exits non-zero on failure | Codex reviewers | One tiny run | v1 |
| C3 | GitHub accepts pushes to a custom ref namespace, and creating an existing ref is rejected, so a claim is atomic | Claims | Two concurrent pushes to a test repository | v0 |
| C4 | Linear exposes a comment's last-edit time; an issue can be created directly in Triage with an API key; an app user works with polling only | S2 quiet window, S13, D8 | API calls on the first adopter's workspace | v1 |
| C5 | Optional reading, not a gate: Sortie's and Symphony's `WORKFLOW.md` and their adapter pitfalls | D5, Jira adapter | Read their docs and adapter code | v0 |
| C6 | `owlshift` is free where it matters: crates.io and npm were free on 2026-09-27; the GitHub name `owlshift` is taken, so the repository lives on the maintainer's personal account (`pitchopp/owlshift`), and `owlshift.dev` still needs a registrar check | D3 | Registrar check | v0 |
| C7 | Each provider's terms allow unattended headless use of a personal subscription through its own CLI; each CLI reports a reached usage limit in a detectable way (message, reset time, exit status) | Subscription-first design, S10, S17 | Read the current terms; trigger or capture a limit message for each CLI | v0 for Claude, v1 for Codex |

## Repository & workspace layout

One repository, a Cargo workspace of six crates split along the design's boundaries, so the pure core never depends on I/O.

| Crate | Holds | Depends on |
| --- | --- | --- |
| `owlshift-core` | Ticket, pipeline, stage, gate, resource; state machine; scheduler; policy. Pure functions, no I/O | nothing |
| `owlshift-contracts` | Brief, `result.json`, project and personal config, events, git ref layout, format versions; JSON Schemas generated from the types | core |
| `owlshift-adapters` | Tracker (Markdown first), forge (GitHub), harness (Claude Code, fake; Codex in v1), notifier; each behind a trait with declared capabilities | contracts |
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

## v0 CLI surface

v0 runs in the foreground only; the background service and its `start`, `stop` and `uninstall` commands arrive in v2.

| Command | v0 behaviour |
| --- | --- |
| `owlshift init` | Detects the stack, writes `owlshift.toml` for the Markdown tracker |
| `owlshift run --once` | One scan-and-dispatch cycle, then exits: the unit of every scenario test |
| `owlshift run` | The loop, in the foreground |
| `owlshift status` | Tickets by stage, runs in progress, usage so far |
| `owlshift logs [TICKET] [--follow]` | Events and run logs |
| `owlshift why TICKET` | The reason a ticket is not dispatched |
| `owlshift doctor` | Harness installed and logged in, git version, capabilities of the configured adapters |
| `owlshift config show` | Effective configuration and the origin of each value |

**The Markdown tracker** keeps one folder per ticket in the repository: `tickets/PROJ-1/ticket.md` with a front matter (stage, priority, assignee, labels, blocked_by) and the description, and `tickets/PROJ-1/comments/` with one file per comment, named by timestamp and author. Answering a question is adding a file; everything stays diffable in git.

## v0 task list & exit gate

Thirteen tasks in dependency order; v0 is done when a sample ticket reaches a green PR through Claude Code and the whole needs-input loop passes in CI without consuming any subscription.

- [ ] Run checks C1, C3, C6 and C7 (Claude part) and record the results
- [x] Create the repository with its licence (2026-09-27); contribution guide and CI matrix on macOS, Linux and Windows still to add
- [ ] Write the contracts and generate their JSON Schemas
- [ ] Core: model, state machine, scheduler (blockers, resources, caps), policy floor, with unit tests
- [ ] Markdown tracker adapter and the first conformance suite
- [ ] Fake harness and scenario runner; S1, S2, S3 and S5 as the first scenario tests
- [ ] Executor: worktree, spawn, process-tree stop, result validation, isolation check, usage-limit detection
- [ ] Claude Code harness adapter: headless run on the user's login, model, effort, permission level, usage capture
- [ ] Git claims with leases and checkpoint refs, tested against a local bare remote
- [ ] Writer for the Markdown tracker; minimal GitHub forge adapter that opens a PR and reads the complete check set
- [ ] Default role prompts: intake, design, design review, build, verify
- [ ] CLI v0 commands
- [ ] A sample repository used by the gate

**Exit gate.** On the sample repository, one Markdown ticket goes from intake to a green PR with Claude Code on the maintainer's subscription; and with the fake harness, the S2 loop (question, incomplete answer, re-ask, full answer, resume, delivery) passes end to end in CI on all three platforms.
