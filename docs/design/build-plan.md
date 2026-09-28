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

### Results

Checks run on 2026-09-28 (OWL-6) with Claude Code 2.1.283 and git 2.54 on macOS. Account identifiers are left out on purpose.

**C1 — passed, with one guardrail finding.** Seven tiny `claude -p` runs in a linked worktree of a scratch repository, each launched with `env -i HOME PATH USER LANG`: no `ANTHROPIC_API_KEY`, no `ANTHROPIC_BASE_URL`, no parent session variables.

- Login: `claude auth status` reports `authMethod: claude.ai` on a Max subscription; the stream's `init` event reports `apiKeySource: none`.
- `--model haiku` and `--model sonnet` reach the named model (`modelUsage`). `--effort` is applied: the same prompt used 0 thinking tokens at `low` and 2,389 at `max`.
- `--permission-mode dontAsk` denied Write and Bash (`permission_denials`, no file created); `acceptEdits` wrote the file inside the worktree. `--json-schema` returns the object in `structured_output`.
- Failure: an unknown model exits with status 1 and `is_error: true`, `api_error_status: 404`, `terminal_reason: "api_error"`, yet `subtype: "success"`. The harness adapter reads `is_error` and the exit status, never `subtype`.
- `--bare` cannot be used: according to `claude --help`, its authentication is strictly `ANTHROPIC_API_KEY` or `apiKeyHelper`, and the subscription login (OAuth, keychain) is never read.
- **Guardrail finding.** By default a headless run loads the user's own configuration: hooks, `CLAUDE.md`, plugins and every MCP server, including claude.ai connectors to Linear, Slack and Gmail. That hands the agent tracker credentials, against the rule that agents get none. Verified mitigation, in one of the seven runs: with `--setting-sources project,local --strict-mcp-config`, the `init` event lists no MCP server, no user hook fires (the default run showed hook events), and the run succeeds on the subscription login (`apiKeySource: none`). Credentials outside Claude Code, such as the `gh` login in the system keyring, stay reachable from the agent's shell; the executor (OWL-15) has to close that path too.

**C3 — passed.** On GitHub (`pitchopp/owlshift`, over SSH), 8 runners pushed concurrently to the same ref in each round, each with its own commit, and only under `refs/owlshift/check-c3/`:

- Create, 5 rounds: `git push --porcelain --force-with-lease=<ref>: origin <commit>:<ref>` (empty expected value: the ref must not exist). Each round had exactly one `[new reference]`, and the remote ref then held the winner's commit. All 35 losers were rejected by the server: `[remote rejected] (cannot lock ref '<ref>': reference already exists)`.
- Takeover, 3 rounds: the ref was first set to a stale lease, then all 8 runners pushed with `--force-with-lease=<ref>:<stale-oid>`. Each round had exactly one `(forced update)`. All 21 losers got `[remote rejected] (cannot lock ref '<ref>': is at <winner-oid> but expected <stale-oid>)`, which also names the new holder.
- GitHub accepts refs outside `refs/heads` and `refs/tags`. All 8 test refs were deleted afterwards, and `git ls-remote origin 'refs/owlshift/*'` returns nothing.
- Why this is a compare-and-swap: the push carries the expected old value with the new one (the zero id for "must not exist"). The server compares it with the current value under the ref lock and writes only on a match, so the check and the write are one step. A check made by the client alone would not do: every loser here was rejected by the server, after another runner's write had landed, a case a client-side check cannot see. GitHub's storage backend is not visible from outside, so for GitHub the evidence is the observed behaviour above, not its internals.
- The rejection text differs from a plain bare repository, where the same races (10 create rounds, 5 takeover rounds, one winner each) gave `(reference already exists)` and `(incorrect old value provided)`. The forge adapter decides from the exit status and the per-ref `!` status of `--porcelain`, never from the text.

**C6 — passed for registration.** The `.dev` registry's RDAP service (`https://pubapi.registry.google/rdap/domain/owlshift.dev`, where `rdap.org` redirects) answers 404 "owlshift.dev not found", while `google.dev` returns a domain record; `.dev` has no whois server. So the name is not registered. Whether a registrar prices it as premium or the registry reserves it shows only at checkout; the registrar search pages tried render availability in JavaScript only.

**C7 (Claude) — terms: ambiguous, leaning permitted. Usage limit: detectable.**

- Terms, read on 2026-09-28; a reading, not a legal conclusion. The [Consumer Terms](https://www.anthropic.com/legal/consumer-terms) (effective October 8, 2025), section 3 "Use of our Services", forbid accessing the Services "through automated or non-human means", except with an API key "or where we otherwise explicitly permit it". The Claude Code [legal and compliance page](https://code.claude.com/docs/en/legal-and-compliance) forbids third-party developers "to route requests through Free, Pro, or Max plan credentials on behalf of their users", but does not prevent a user "signing in to the unmodified Claude Code binary with their own Claude subscription"; it adds that advertised limits "assume ordinary, individual usage". The help-center article [15036540](https://support.claude.com/en/articles/15036540) (June 2026) says `claude -p` still draws "from your subscription's usage limits", so headless use on a subscription is an anticipated, metered use. Owlshift sits on the permitted side of these lines: the user's own login, the unmodified `claude` binary, no credential handled or routed by Owlshift. Residual risk: whether an unattended runner working through a queue still counts as "ordinary, individual usage", and the same article announces a change to how Agent SDK and `claude -p` usage is counted, paused on June 15, 2026. P0 proceeds; a change to either page reopens this check.
- Usage limit: the [error reference](https://code.claude.com/docs/en/errors) documents the message `You've hit your session limit · resets 3:45pm` (also weekly, Opus and Sonnet limits). The [Agent SDK reference](https://code.claude.com/docs/en/agent-sdk/typescript) documents a `rate_limit_event` whose `status` is `allowed`, `allowed_warning` or `rejected`, with `resetsAt` in epoch seconds, and an assistant error `rate_limit` for a 429 against the quota. Live: every `--output-format stream-json` run above emitted a `rate_limit_event` (`status: "allowed"`, `resetsAt`, `rateLimitType: "five_hour"`, utilisation of the five-hour and seven-day windows). Logged: headless sessions on this machine (entrypoint `sdk-cli`, Claude Code 2.1.251, 2026-08-29 and 30) recorded the limit as a synthetic assistant message with `error: "rate_limit"`, `apiErrorStatus: 429` and the text `You've hit your session limit · resets 2pm (<time zone>)`. The runner therefore reads the limit and its reset time from the stream, not from the message text.
- Open item: the exit status and the final `result` record of `claude -p` when a limit is reached were not observed; the subscription was not exhausted on purpose. OWL-14 and OWL-15 record them the first time a real run hits a limit. The failure case above (status 1, `is_error: true`, `api_error_status`) is the likely shape, not a verified one.

## Repository & workspace layout

One repository, a Cargo workspace of six crates split along the design's boundaries, so the pure core never depends on I/O.

| Crate | Holds | Depends on |
| --- | --- | --- |
| `owlshift-core` | Ticket, pipeline, stage, gate, resource; state machine; scheduler; policy. Pure functions, no I/O | `serde` and `schemars`, for derives only (the shared vocabulary) |
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

The contracts live in `owlshift-contracts`; their JSON Schemas are generated into `schemas/`, and a test fails when a committed schema drifts from the types (`OWLSHIFT_UPDATE_SCHEMAS=1` rewrites them). The first cut of `result.json` below still parses unchanged; the reference is now [`schemas/result.schema.json`](../../schemas/result.schema.json). Refinements made while writing it (OWL-9): question ids are `Q1` to `Qn` in order; `status: questions` needs at least one question; `pr` is allowed only with `status: done`; a follow-up carries `title`, `why`, `evidence`, `source` (`agent`, `reviewer` or `ci`), `done_when` and `blocked_by_parent`; `artifacts` may name `plan`, `ledger`, `findings` and `report`. A document with a newer `format` is refused with an "upgrade" error.

The first cut:

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
- [x] OWL-6 · Run checks C1, C3, C6 and C7 (Claude part) and record the results (2026-09-28)
- [x] OWL-7 · Settle decision D10 (contribution terms: DCO, 2026-09-28); add the contribution guide
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
