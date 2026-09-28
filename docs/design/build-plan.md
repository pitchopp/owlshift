# Owlshift — build plan for P0 and P1

Status: draft, 2026-09-27. What to build first, in order: the foundations (P0), then one ticket turned into a verified pull request on demand (P1). The steps after that are described in the [roadmap](roadmap.md).

## Checks to run live before writing code

Eight assumptions carry the design; each is checked with a real call before the code that depends on it, and the dated result is written down next to the decision it settles.

| # | Assumption | Depends on it | Check | Before |
| --- | --- | --- | --- | --- |
| C1 | `claude -p` runs headless on the user's own subscription login, with no API key, and honours `--model`, `--effort`, `--permission-mode` and structured output inside a worktree | The whole executor | One tiny run in a scratch repository | P0 |
| C2 | `codex exec` runs headless on a ChatGPT subscription login, gives structured output, accepts a reasoning-effort setting, confines writes to the worktree in `workspace-write`, and exits non-zero on failure | Codex reviewers | One tiny run | P5 |
| C3 | GitHub accepts pushes to a custom ref namespace, and creating an existing ref is rejected, so a claim is atomic | Claims | Two concurrent pushes to a test repository | P0 |
| C4 | Linear exposes a comment's last-edit time; an issue can be created directly in Triage with an API key; an app user works with polling only | S2 quiet window, S13, D8 | API calls on the Owlshift workspace | P2 |
| C5 | Optional reading, not a gate: Sortie's and Symphony's `WORKFLOW.md` and their adapter pitfalls | D5, Jira adapter | Read their docs and adapter code | P0 |
| C6 | `owlshift` is free where it matters: crates.io and npm were free on 2026-09-27; the GitHub name `owlshift` is taken, so the repository lives on the maintainer's personal account (`pitchopp/owlshift`), and `owlshift.dev` still needs a registrar check | D3 | Registrar check | P0 |
| C7 | Each provider's terms allow unattended headless use of a personal subscription through its own CLI; each CLI reports a reached usage limit in a detectable way (message, reset time, exit status) | Subscription-first design, S10, S17 | Read the current terms; trigger or capture a limit message for each CLI | P0 for Claude, P5 for Codex |
| C8 | Each harness CLI reports whether it is logged in through a status command, readable without Owlshift touching a credential | `owlshift doctor` (OWL-12) | Run each status command logged in, logged out and on an API key | P0 |

### Results

Checks run on 2026-09-28 (OWL-6) with Claude Code 2.1.283 and git 2.54 on macOS. Account identifiers are left out on purpose.

**C1 — passed, with one guardrail finding.** Seven tiny `claude -p` runs in a linked worktree of a scratch repository, each launched with `env -i HOME PATH USER LANG`: no `ANTHROPIC_API_KEY`, no `ANTHROPIC_BASE_URL`, no parent session variables.

- Login: `claude auth status` reports `authMethod: claude.ai` on a Max subscription; the stream's `init` event reports `apiKeySource: none`.
- `--model haiku` and `--model sonnet` reach the named model (`modelUsage`). `--effort` is applied: the same prompt used 0 thinking tokens at `low` and 2,389 at `max`.
- `--permission-mode dontAsk` denied Write and Bash (`permission_denials`, no file created); `acceptEdits` wrote the file inside the worktree. `--json-schema` returns the object in `structured_output`.
- Failure: an unknown model exits with status 1 and `is_error: true`, `api_error_status: 404`, `terminal_reason: "api_error"`, yet `subtype: "success"`. The harness adapter reads `is_error` and the exit status, never `subtype`.
- `--bare` cannot be used: according to `claude --help`, its authentication is strictly `ANTHROPIC_API_KEY` or `apiKeyHelper`, and the subscription login (OAuth, keychain) is never read.
- **Guardrail finding.** By default a headless run loads the user's own configuration: hooks, `CLAUDE.md`, plugins and every MCP server, including claude.ai connectors to Linear, Slack and Gmail. That hands the agent tracker credentials, against the rule that agents get none. Verified mitigation, in one of the seven runs: with `--setting-sources project,local --strict-mcp-config`, the `init` event lists no MCP server, no user hook fires (the default run showed hook events), and the run succeeds on the subscription login (`apiKeySource: none`). Credentials outside Claude Code, such as the `gh` login in the system keyring, stay reachable from the agent's shell: the agent environment closes that path (OWL-22, below).

**C3 — passed.** On GitHub (`pitchopp/owlshift`, over SSH), 8 runners pushed concurrently to the same ref in each round, each with its own commit, and only under `refs/owlshift/check-c3/`:

- Create, 5 rounds: `git push --porcelain --force-with-lease=<ref>: origin <commit>:<ref>` (empty expected value: the ref must not exist). Each round had exactly one `[new reference]`, and the remote ref then held the winner's commit. All 35 losers were rejected by the server: `[remote rejected] (cannot lock ref '<ref>': reference already exists)`.
- Takeover, 3 rounds: the ref was first set to a stale lease, then all 8 runners pushed with `--force-with-lease=<ref>:<stale-oid>`. Each round had exactly one `(forced update)`. All 21 losers got `[remote rejected] (cannot lock ref '<ref>': is at <winner-oid> but expected <stale-oid>)`, which also names the new holder.
- GitHub accepts refs outside `refs/heads` and `refs/tags`. All 8 test refs were deleted afterwards, and `git ls-remote origin 'refs/owlshift/*'` returns nothing.
- Why this is a compare-and-swap: the push carries the expected old value with the new one (the zero id for "must not exist"). The server compares it with the current value under the ref lock and writes only on a match, so the check and the write are one step. A check made by the client alone would not do: every loser here was rejected by the server, after another runner's write had landed, a case a client-side check cannot see. GitHub's storage backend is not visible from outside, so for GitHub the evidence is the observed behaviour above, not its internals.
- The rejection text differs from a plain bare repository, where the same races (10 create rounds, 5 takeover rounds, one winner each) gave `(reference already exists)` and `(incorrect old value provided)`. The forge adapter decides from the exit status and the per-ref `!` status of `--porcelain`, never from the text.

**C4 (comment edit time) — passed; Triage creation and the app user stay open for P2.** Run on 2026-09-28 (OWL-13) against the Owlshift workspace with a personal API key, read-only apart from `commentCreate` calls on an issue that does not exist, which write nothing.

- Schema, by introspection: `Comment.editedAt` is "the time the comment was last edited by its author", null when never edited. `Comment.updatedAt` is not an edit time: on every comment of OWL-6 to OWL-30 it differs from `createdAt`, sometimes by being earlier, while `editedAt` is null. The adapter reads `editedAt`. `Issue.priority` is 0 (none), 1 (urgent), 2 (high), 3 (medium) or 4 (low). `Comment.user` is null for bots and integrations, which `botActor` or `externalUser` then names. `Issue.comments` also holds replies and inline comments on the description.
- `issue.comments` answers newest first, with or without `orderBy: createdAt` or `updatedAt`; the adapter sorts oldest first itself.
- Errors: an unknown issue answers HTTP 200 with code `INPUT_ERROR` and "Entity not found: Issue" on a query, "issue not found" on `commentCreate`; a rejected key answers HTTP 401 with `AUTHENTICATION_ERROR`; a malformed issue id on `commentCreate` answers `INVALID_INPUT`, "issueId must be a valid UUID or issue identifier (e.g., 'LIN-123')", so `commentCreate` takes `OWL-13` as is. Rate-limit headers: 2,500 requests and 3,000,000 complexity points per hour, resets in epoch milliseconds. A rate-limited answer was not observed.
- Libraries. HTTP: `ureq` 3.4, blocking, rustls with the bundled web roots; no async runtime is needed yet. Keychain: `keyring-core` 1.0 with `apple-native-keyring-store` (Keychain), `windows-native-keyring-store` (Credential Manager) and `zbus-secret-service-keyring-store` with `crypto-rust` (Secret Service, pure Rust, no libdbus). The `keyring` 4 facade was set aside: its own documentation sends applications to `keyring-core` and the stores, and its global default store gets in the way of an in-memory store for tests. A store, read and delete round trip passed on the macOS Keychain; the crate type-checks for Linux and Windows, and CI builds it on all three.
- Recorded fixtures of these answers, with every account pseudonymized, are in `crates/owlshift-adapters/tests/fixtures/linear/`.

**C6 — passed for registration.** The `.dev` registry's RDAP service (`https://pubapi.registry.google/rdap/domain/owlshift.dev`, where `rdap.org` redirects) answers 404 "owlshift.dev not found", while `google.dev` returns a domain record; `.dev` has no whois server. So the name is not registered. Whether a registrar prices it as premium or the registry reserves it shows only at checkout; the registrar search pages tried render availability in JavaScript only.

**C7 (Claude) — terms: ambiguous, leaning permitted. Usage limit: detectable.**

- Terms, read on 2026-09-28; a reading, not a legal conclusion. The [Consumer Terms](https://www.anthropic.com/legal/consumer-terms) (effective October 8, 2025), section 3 "Use of our Services", forbid accessing the Services "through automated or non-human means", except with an API key "or where we otherwise explicitly permit it". The Claude Code [legal and compliance page](https://code.claude.com/docs/en/legal-and-compliance) forbids third-party developers "to route requests through Free, Pro, or Max plan credentials on behalf of their users", but does not prevent a user "signing in to the unmodified Claude Code binary with their own Claude subscription"; it adds that advertised limits "assume ordinary, individual usage". The help-center article [15036540](https://support.claude.com/en/articles/15036540) (June 2026) says `claude -p` still draws "from your subscription's usage limits", so headless use on a subscription is an anticipated, metered use. Owlshift sits on the permitted side of these lines: the user's own login, the unmodified `claude` binary, no credential handled or routed by Owlshift. Residual risk: whether an unattended runner working through a queue still counts as "ordinary, individual usage", and the same article announces a change to how Agent SDK and `claude -p` usage is counted, paused on June 15, 2026. P0 proceeds; a change to either page reopens this check.
- Usage limit: the [error reference](https://code.claude.com/docs/en/errors) documents the message `You've hit your session limit · resets 3:45pm` (also weekly, Opus and Sonnet limits). The [Agent SDK reference](https://code.claude.com/docs/en/agent-sdk/typescript) documents a `rate_limit_event` whose `status` is `allowed`, `allowed_warning` or `rejected`, with `resetsAt` in epoch seconds, and an assistant error `rate_limit` for a 429 against the quota. Live: every `--output-format stream-json` run above emitted a `rate_limit_event` (`status: "allowed"`, `resetsAt`, `rateLimitType: "five_hour"`, utilisation of the five-hour and seven-day windows). Logged: headless sessions on this machine (entrypoint `sdk-cli`, Claude Code 2.1.251, 2026-08-29 and 30) recorded the limit as a synthetic assistant message with `error: "rate_limit"`, `apiErrorStatus: 429` and the text `You've hit your session limit · resets 2pm (<time zone>)`. The runner therefore reads the limit and its reset time from the stream, not from the message text.
- Open item: the exit status and the final `result` record of `claude -p` when a limit is reached were not observed; the subscription was not exhausted on purpose. OWL-14 and OWL-15 record them the first time a real run hits a limit. The failure case above (status 1, `is_error: true`, `api_error_status`) is the likely shape, not a verified one.

**OWL-14 — what the Claude Code adapter relies on, beyond C1 and C7.** Tiny `claude -p` runs on 2026-09-28 with Claude Code 2.1.283 on macOS, on `haiku` at effort `low`, in a scratch directory, each launched with `env -i HOME PATH USER LANG TMPDIR` and `--setting-sources project,local --strict-mcp-config --no-session-persistence --permission-prompts none`.

- `--output-format stream-json` needs `--verbose` with `-p`: without it the CLI exits 1 with `Error: When using --print, --output-format=stream-json requires --verbose`. The prompt can come on standard input, with no prompt argument; the adapter sends it there, which keeps it off the command line.
- The stream of a tiny run is five lines: `system/init`, two `assistant` messages, a `rate_limit_event`, `result`; standard error stays empty. `init` carries `claude_code_version`, `model` and `apiKeySource` (`none`). `result` carries `is_error`, `result` (the final text), `usage` (input, output, cache read and cache creation tokens), `total_cost_usd`, `duration_ms`, `num_turns`, `modelUsage` (the same counts per model, camelCase, with `costUSD`), `permission_denials` (with `tool_name`), `api_error_status` and `terminal_reason`.
- Read-only: `--permission-mode dontAsk --allowedTools 'Edit(./result.json)'` let the Write tool create `result.json` and denied a Write to `other.txt`, listed in `permission_denials`; C1 had already seen `dontAsk` deny Bash.
- Write in worktree: `--permission-mode acceptEdits --allowedTools Bash` let Write create a file in the working directory and Bash run `echo hi > bash.txt`, and denied a Write to `../outside.txt`. Bash itself is not confined to the directory.
- No network: `--disallowedTools WebFetch WebSearch` removes both tools from the `init` tool list; asked to fetch a page, the model answered that WebFetch is unavailable.
- Unknown model: besides C1's `result` record, the `assistant` message carries a top-level `error: "model_not_found"`, and standard error one line, `[claude-code:unrecognized_model] {…}`. The logged limit message of C7 has the same shape with `error: "rate_limit"`.
- Launched from inside a Claude Code desktop session, with its environment inherited (`CLAUDECODE`, `ANTHROPIC_BASE_URL`, `CLAUDE_EFFORT` and others), the run still reported `apiKeySource: none` and added a `system/post_turn_summary` event. Which variables a run inherits is settled by OWL-22, below: none of these.

**OWL-22 — what an agent inherits.** Checked on 2026-09-28 on macOS with gh 2.91.0, git 2.54.0 (Apple Git-157), OpenSSH 10.3p1 and Claude Code 2.1.283, on the maintainer's machine: gh logged in through the system keyring, the `osxkeychain` helper in the git configuration, pushes to GitHub over SSH (C3). The only network calls made carried no credential.

- gh: pointing `GH_CONFIG_DIR` at an empty directory does not log gh out: `gh auth token` still returned the keyring login (exit 0), gh falling back to the host's unkeyed keyring entry. Rejected. On the same machine, `GH_TOKEN` set to a placeholder wins over the keyring login and over a token in gh's configuration file (`gh auth token` prints the placeholder); `GH_ENTERPRISE_TOKEN` does the same for any other host; the API answers the placeholder with `Bad credentials (HTTP 401)`.
- git: with a helper that returns a password, plain or scoped to `https://github.com`, `git -c credential.helper= credential fill` returned none (`could not read Username … terminal prompts disabled`): an empty value in the command scope, read last, resets the helper list. With `core.askPass` naming a program that prints a password, an empty `GIT_ASKPASS` made git skip it, and `SSH_ASKPASS` too. The environment form of the command scope (`GIT_CONFIG_COUNT`, git 2.31 or later) is exercised by the test below on the three CI platforms.
- SSH: `ssh -o BatchMode=yes -o IdentityAgent=none -o PubkeyAuthentication=no -o GSSAPIAuthentication=no` to GitHub as `git` answered `Permission denied (publickey)`.
- MCP: given one server with `--mcp-config` next to the guardrail flags, the `init` event listed `{"name":"owlshift-probe","status":"failed","source":"dynamic"}`: a server that failed to start is still listed. The recorded runs, flags alone, list none.

Decision: an agent process starts from an empty environment (`owlshift_core::agent_env`). It inherits what a program needs to run on the three platforms, the harness login locations (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`), the operator's proxy and certificate settings, and the variables the project declares for its gate; a declared credential variable, `GIT_*` name or override is refused. `GH_TOKEN` and `GH_ENTERPRISE_TOKEN` then hold the placeholder `owlshift-agent-has-no-credential`, `credential.helper` is reset through `GIT_CONFIG_COUNT`, `GIT_ASKPASS` is empty, `GIT_TERMINAL_PROMPT` is 0, and `GIT_SSH_COMMAND` runs ssh with the options above. Everything else is dropped: token variables, `SSH_AUTH_SOCK`, `GIT_*`, `XDG_RUNTIME_DIR`, `DBUS_SESSION_BUS_ADDRESS` and `DISPLAY` (they locate the Secret Service), and a parent Claude Code session's variables. `HOME` stays: redirecting it would hide the harness's own login too.

- **For the executor (OWL-15).** In `owlshift_runner::agent_env`, `AgentEnv::from_runner` builds the environment and `AgentEnv::check` runs git and gh as the agent would, in the worktree. It reports credential variables (a token variable holding the placeholder passes), a password from `git credential fill`, a login from `gh auth token`, and the git settings that carry a credential the environment cannot remove: an http(s) URL with user info in a remote or a URL rewrite, an extra HTTP header, keys redacted. A probe that fails or times out counts as a finding. A finding stops the run before it starts; `AgentEnv::apply` then replaces the command's environment, and after the run `mcp_findings` fails a run whose `init` listed any MCP server. `floor::check_agent_environment` stays the check of declared names: on a whole agent environment it would flag the placeholder.
- **Tests.** `crates/owlshift-runner/tests/agent_credentials.rs` plants token variables, a parent session's variables, a git credential helper, a gh login and repository settings carrying credentials on a host. The check finds each in the host's own environment and none in the agent environment, and a child spawned with it sees none of those variables. It runs the real git and gh on the three CI platforms, gh being required there. The MCP half needs a harness login, which CI lacks: it rests on the adapter always passing the flags, unit tests on the `init` shapes above, and `crates/owlshift-runner/tests/agent_live.rs`, run by hand. On 2026-09-28 on the maintainer's machine (claude.ai connectors configured, gh login in the keyring, `osxkeychain` helper, SSH keys, launched from inside a Claude Code session) it passed: no credential finding, and the run completed on the subscription (`apiKeySource: none`) with no MCP server.
- **What stays open.** This closes what an agent reaches without going around Owlshift: its tool list, its environment, and git and gh as they run from its shell. It does not stop a process that goes looking, by unsetting an override, reading a token file or querying the system keychain, Owlshift's own Linear key included: that takes confinement by the operating system (a harness's own sandbox, a dedicated user or a container), not built yet. A harness configured through environment variables (a gateway `ANTHROPIC_BASE_URL`, Bedrock or Vertex settings) loses them, as it already loses user settings to C1's flags. Native Windows was not run live (decision D9: WSL2 first).

**C8 — passed, with one handling rule.** Run on 2026-09-28 (OWL-12) with Claude Code 2.1.283, codex-cli 0.154.0 and git 2.54.0 on macOS. Logged-out states were produced with an empty `CLAUDE_CONFIG_DIR` or `CODEX_HOME` in a scratch directory; the API-key state with a dummy key in a scratch `CODEX_HOME`. Both commands answer in about 0.1 s, with no network wait observed.

- `claude auth status` prints JSON on stdout (`--json` is the default). Logged in: exit 0, `loggedIn: true`, `authMethod: "claude.ai"`, `subscriptionType: "max"`. Logged out: exit 1, `loggedIn: false`, `authMethod: "none"`; the `--text` form says "Not logged in. Run claude auth login to authenticate." With only `ANTHROPIC_API_KEY` set: exit 0, `authMethod: "api_key"`, `apiKeySource: "ANTHROPIC_API_KEY"`. The logged-in JSON also carries the account's e-mail address, organisation id and name: personal data, which Owlshift does not deserialize.
- `codex login status` writes one line on stderr, nothing on stdout. ChatGPT login: exit 0, `Logged in using ChatGPT`. Logged out: exit 1, `Not logged in`; an `OPENAI_API_KEY` in the environment does not change that answer. API-key login: exit 0, `Logged in using an API key - ` followed by the key's first and last characters around `***`.
- Handling rule: the verdict comes from the exit status, and the method from a fixed list of known phrases or JSON values. Owlshift never echoes either command's raw output, since Codex prints part of a key.
- `git --version` prints `git version 2.54.0 (Apple Git-157)`; the version is the third word.

## Repository & workspace layout

One repository, a Cargo workspace of six shipped crates split along the design's boundaries, so the pure core never depends on I/O, plus one test-only crate.

| Crate | Holds | Depends on |
| --- | --- | --- |
| `owlshift-core` | Ticket, pipeline, stage, gate, resource; state machine; scheduler; policy. Pure functions, no I/O | `serde` and `schemars`, for derives only (the shared vocabulary) |
| `owlshift-contracts` | Brief, `result.json`, project and personal config, events, git ref layout, format versions; JSON Schemas generated from the types | core |
| `owlshift-adapters` | Tracker (Linear, plus the Markdown test tracker), forge (GitHub), harness (Claude Code; Codex in P5), notifier; each behind a trait with declared capabilities | contracts |
| `owlshift-platform` | Service install and uninstall, process groups and Job Objects, keychain, platform directories, keep-awake | nothing |
| `owlshift-runner` | Daemon loop, executor (worktrees, spawning, isolation check), writer, local store (SQLite) | all of the above |
| `owlshift-cli` | The `owlshift` binary | runner |
| `owlshift-testkit` | Test only, never published and never a dependency of a shipped crate: the fake harness program, hermetic git and the bare-remote fixture, the scenario format and runner, and the scenario tests. It is a crate of its own because Cargo gives a binary's path only to the integration tests of its own package | core, contracts, adapters |

Beside the crates: `roles/` (default role prompts in Markdown, versioned with the contract), `schemas/` (generated JSON Schemas), `tests/scenarios/` (S1 to S17 with their fixtures, played by the testkit), `docs/`.

**Git through the `git` CLI**, not a library: worktrees, pushes and credential helpers then behave exactly as in the user's own terminal. **Harnesses through their own CLI**, never through a model API: the user's subscription login is what runs. Library choices (async runtime, HTTP client, SQLite binding, CLI parser, keychain access) are made at implementation time against current documentation. Made so far: the HTTP client and keychain access (OWL-13, see C4 in the results).

**Repository hygiene from the first commit:** Apache-2.0 licence, English, a contribution guide, and a CI matrix on macOS, Linux and Windows.

## Contracts to write first

Six contracts are written, versioned and schema-checked before any logic, because every component and every future adapter talks through them.

1. **Brief** (runner to role): format version, role, the ticket (text from anyone but the decider quoted as data), the decider, the numbered question-and-answer thread with authors marked, the checkpoint (plan, ledger), resources and zones, the project rules injected for those zones, permitted actions, the project's gate commands (from `stack.gate`, so a role never reads the project config or guesses the gate from the repository; OWL-37), and where to write the result.
2. **`result.json`** (role to runner): validated against its JSON Schema, unknown fields rejected, an exit code never trusted in its place.
3. **Project and personal config**: TOML, with published JSON Schemas so editors complete and check them.
4. **Events**: timestamp, project, ticket, run, kind, data; the one stream behind logs, `why`, the UI and usage reports.
5. **Git ref layout**: `refs/owlshift/claims/<ticket>` holds the lease (holder machine, operator, expiry, format); `refs/owlshift/tickets/<ticket>` points to a commit whose tree holds the stage, plan, ledger, questions and findings.
6. **Marked comments**: a first line such as `[owlshift] QUESTIONS · round 2`, plus a machine-readable footer where the tracker can hide it; exact form checked per tracker.

The contracts live in `owlshift-contracts`; their JSON Schemas are generated into `schemas/`, and a test fails when a committed schema drifts from the types (`OWLSHIFT_UPDATE_SCHEMAS=1` rewrites them). The first cut of `result.json` below still parses unchanged; the reference is now [`schemas/result.schema.json`](../../schemas/result.schema.json). Refinements made while writing it (OWL-9): question ids are `Q1` to `Qn` in order; `status: questions` needs at least one question; `pr` is allowed only with `status: done`; a follow-up carries `title`, `why`, `evidence`, `source` (`agent`, `reviewer` or `ci`), `done_when` and `blocked_by_parent`; `artifacts` may name `plan`, `ledger`, `findings` and `report`. A document with a newer `format` is refused with an "upgrade" error. The schema cannot see the file system, so the runner reads each artifact without following any symbolic link (OWL-26): a link anywhere between the worktree and the file, even one pointing back inside, fails the run with an error naming the field. A hard link is the same leak with nothing to see (OWL-36): an artifact file with more than one name fails the run the same way, wherever its other names are, since finding them would take a search of the whole volume. The link count is read on the handle the artifact is then read from (`fstat` on Unix, `GetFileInformationByHandle` on Windows), so the file checked is the file read; it is the count the file system reports, which a network mount that under-reports it defeats. No legitimate artifact has a second name: checked on 2026-09-28 with git 2.54.0, a checkout made by `git worktree add` holds no file with more than one link outside `.git` (`find . -path ./.git -prune -o -path ./target -prune -o -type f -links +1 -print` printed nothing). It also reads at most 1 MiB of each artifact (OWL-35): an artifact seen to hold more before its end, whether huge, sparse or still growing, fails the run with an error naming the field, after reading one byte past the limit and no more. Artifacts are text a model writes, far below 1 MiB; the limit keeps a faulty or hostile role from exhausting the runner's memory or keeping it reading.

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

**What `doctor` checks, as built in P0 (OWL-12).**

- **Harnesses.** The ones declared under `[harnesses]` in the personal file are checked; when none are declared, both `claude` and `codex` are. Each must be on the `PATH` and logged in, following the rules of check C8.
- **Harness versions.** A version outside the harness's tested list (`crates/owlshift-adapters/src/harness/tested.rs`), or one doctor cannot read, is a warning, not a failure (OWL-29). Matching is exact: a suffixed build such as `2.1.283-beta.1` is not the release `2.1.283`. A version is listed once the harness contract tests pass on output recorded with it, and the Claude contract tests fail when a recorded fixture carries a version the list lacks. The list only grows. Codex has no contract tests yet, so no Codex version is tested and doctor says so.
- **Configuration files.** The project file is `owlshift.toml` at the root of the git repository holding the current directory. The personal file's location is given in [runtime & operations](runtime-and-operations.md#install-lifecycle--uninstall).
- **Adapters.** Doctor names the configured tracker and the capabilities its adapter declares, from constants, without opening the tracker or the keychain. A required capability the build does not implement yet is a warning, not a failure: `owlshift do` needs only to read tickets and comments, and refusing a project is `init`'s job (OWL-13). Adapter traits exist from a kind's second implementation; a capability contract is frozen only once three exist (principle 8), and may change until then.
- **Output and exit status.** A failed check prints how to fix it and makes the exit status 1. Warnings do not.

**The test tracker** (published as the Markdown tracker in P9) keeps one folder per ticket in the repository: `tickets/PROJ-1/ticket.md` with a front matter and the description, and `tickets/PROJ-1/comments/` with one file per comment, named by timestamp and author. Answering a question is adding a file; everything stays diffable in git. The format, as built in P0 (OWL-11):

- `ticket.md` opens with a TOML front matter between two `+++` lines, then the description. Keys: `title`, `author`, `stage` (the visible stage: a state name as mapped under `[tracker].states`), `priority` (`urgent`, `high`, `medium`, `low` or `unset`, the default), `assignee` (optional: the decider), `labels` and `blocked_by` (ticket ids), both empty by default. An unknown key is refused. TOML keeps the project to one configuration language; the choice is revisited before the tracker is published in P9, where YAML front matter is the wider convention.
- A comment is `comments/<YYYYMMDDTHHMMSSZ>-<author>.md`: the UTC time in ISO 8601 basic format, which has no colon (Windows refuses one in a file name) and sorts in time order, then the author (ASCII letters, digits, `_` and `-`). The body is the file's content. Posting never overwrites a file: a second comment by the same author in the same second is refused.
- Changing the visible stage rewrites the `stage` line of the front matter and nothing else, a one-line diff.
- A malformed file is refused with an error that names it.

**The test bench** (OWL-11) lives in `owlshift-testkit` and plays a ticket end to end without a model, a network or a tracker account.

- **The fake harness**, `owlshift-fake-harness --brief <brief.json> --reply <reply.toml>`, runs in the worktree like a real harness. The reply says what to do, in this order: wait (`delay_ms`); write files (`files`) and commit them (`commit`); copy a prepared `result.json`, valid or not, to the brief's result path (`result`); print (`stdout`, `stderr`), or report a usage limit (`usage_limit`, a reset time: it prints `owlshift-fake-harness: usage limit reached · resets <time>` on stderr and writes no result); exit (`exit_code`: 0 by default, 1 with a usage limit). It is a compiled program, not a script, so it behaves the same on the three platforms.
- **A scenario** is a TOML file in `tests/scenarios/` with a fixture folder beside it: the project repository, seeded as `main` into a local bare remote, and the prepared results. Its steps are `dispatch`, `run` (a reply for the fake harness), `comment` (a person comments) and `answer` (the answer check's verdict: only `answered` until the answer-check role arrives in P2). Any step can carry expectations on the core state (stage, waiting, round, failed runs), the tracker (visible stage, comments) and the remote (branch pushed, file contents); a failed expectation names the scenario, the step, the expected and the found value. Time is virtual, the scenario's start plus one minute per step, so a run is reproducible to the byte.
- **A stand-in driver** plays the executor (OWL-15) and the writer (OWL-18) until they exist, and only as far as the scenarios need: it writes the brief, launches the fake harness, maps the outcome onto a core event, posts the questions comment, sets the visible stage and pushes the branch. Those tickets replace it; the scenario files stay.
- **Git is hermetic.** Every git command the bench runs, the fake harness included, starts with no inherited `GIT_*` variable, `GIT_CONFIG_NOSYSTEM=1`, `HOME` and `XDG_CONFIG_HOME` in the fixture's folder, and `GIT_CONFIG_GLOBAL` pointing to a file the fixture writes: identity, no signing, `core.autocrlf=false`, and empty ignore and attributes files and hooks folder of its own. The host's configuration (signing, hooks, ignore rules, line-ending conversion) never reaches a test. Author and committer dates come from the virtual clock, so commit ids are reproducible too. Check: `git::tests::git_reads_only_the_fixture_configuration`, in `owlshift-testkit`, gives git a hostile environment (`GIT_DIR`, `GIT_CONFIG_PARAMETERS`, `GIT_CONFIG_COUNT`) and a hostile home (a global configuration, an ignore file of `*`, attributes with `eol=crlf`). It then requires every configuration entry to come from the fixture's file or the repository's own, and no host ignore rule or attribute to apply. It passed on ubuntu, macOS and Windows in CI on 2026-09-28 ([run 36407077420](https://github.com/pitchopp/owlshift/actions/runs/36407077420)). On Windows, then, no entry of Git for Windows' own system configuration was read. Git for Windows prints a configuration file's path in quotes when the path holds backslashes; the first run of the check failed on that quote alone.

**The Claude Code harness adapter** (OWL-14), in `owlshift-adapters::harness::claude`, runs one role with `claude -p` on the user's own login, following the OWL-14 results above.

- **Split with the executor.** `command` builds the invocation for a request (working directory, model, effort, permissions, result path, an optional JSON Schema, an optional dollar cap passed as `--max-budget-usd`, which the executor sets only for a harness configured for API billing, from the personal file's `budget_usd`); the executor spawns it in its own process group or Job Object and hands the child to `drive`, which writes the prompt on standard input, streams every output line to a callback (the executor's run log) and returns the run. The executor stops a run by stopping the process group; `drive` returns at most two seconds after the child exits, even when a process it started holds the output open.
- **Permissions.** Read-only is `dontAsk` plus an allow rule for the result file alone; write in worktree is `acceptEdits` plus Bash; no network removes the web tools; a browser is refused. A result path that would be a pattern in a rule (anything but ASCII letters, digits, `.`, `_`, `-` and `/`, or a `.` segment) is refused. C1's guardrail flags are always passed; the worktree's own `.claude/settings*.json` still applies, and the agent environment (OWL-22 in the results above) covers what lies outside Claude Code. `Run::mcp_servers` names the servers of the `init` event.
- **Outcome.** Completed needs exit status 0 and a final record with `is_error: false`. Otherwise a usage limit still in force at the end of the stream (a `rate_limit_event` with status `rejected`, or an assistant message with `error: "rate_limit"`, not lifted by a later `allowed` report) makes the run a usage limit, with the reset time and window of the rejection when there was one, never read from the message text. Anything else is a failure: no final record, no `is_error` flag, a signal, the prompt not delivered, or an error with its API status, terminal reason and error kind. Completed means the harness finished; the role's `result.json` is still validated by the executor.
- **Usage.** Tokens, cost estimate, duration and turns come from the final record, per model too; a run without one has unknown usage, not zero. The last rate-limit report gives the window, its reset time and the five-hour and seven-day utilisation. `apiKeySource: none` means the subscription.
- **Tests.** Contract tests replay the scrubbed recordings in `crates/owlshift-adapters/tests/fixtures/claude/` in CI; the usage-limit one is constructed from C7's shapes until a real limit is recorded. A tiny real run is an ignored test, run by hand with `OWLSHIFT_LIVE_CLAUDE=1`.

**The GitHub forge adapter** (OWL-17), in `owlshift-adapters::forge`, pushes the ticket's branch, opens its pull request and reads its complete check set. It has no merge, close or approve operation.

- **What it relies on,** checked read-only on `pitchopp/owlshift` on 2026-09-28 with `gh api`. GraphQL's `statusCheckRollup.contexts` on the head commit lists the check runs of every app and the commit statuses (`CheckRun | StatusContext`) in one paginated connection with `totalCount` and `isRequired(pullRequestNumber:)`; paged two at a time, pull request #16 gave four contexts over two pages, the same four check runs as REST `commits/<sha>/check-runs`. `mergeable` is `MERGEABLE`, `CONFLICTING` or `UNKNOWN`; `mergeStateStatus` is `DIRTY`, `UNKNOWN`, `BLOCKED`, `BEHIND`, `UNSTABLE`, `HAS_HOOKS` or `CLEAN`. The check run statuses, conclusions and commit status states are the schema's enums, read by introspection. An unknown pull request answers HTTP 200 with an error of type `NOT_FOUND`; a rejected token answers HTTP 401 with a REST-shaped body, "Bad credentials". The repository is private on a free plan: reading branch protection or rulesets answers 403, and the branch is not protected, so no check there is required.
- **API and credential.** GitHub's GraphQL API, since one query gives every check, whether required, and the merge state; REST needs three endpoints paginated separately. The token lives in the system keychain (service `owlshift`, account `github`): a fine-grained token with pull requests (write), checks, commit statuses and metadata (read), or the output of `gh auth token`. Owlshift does not read `gh`'s own login. The push goes through the `git` CLI and the user's own git credentials.
- **Push.** `git push --porcelain <remote> <commit>:refs/heads/<branch>` pushes exactly the verified commit, never a force. The outcome comes from the exit status and the ref's porcelain flag (C3): created, fast-forward, up to date, or rejected; no flag for the branch is a failure, reported with the end of git's standard error, any credential in a URL removed. Tested against a local bare remote with the bench's hermetic git.
- **Complete.** Every context of the pull request's head commit, read to the last page. The list is refused rather than returned partial when a page does not advance, the count differs from `totalCount`, or the head moves while reading; and the head must be the commit the runner pushed.
- **Verdict.** Not open when the pull request is closed or merged; red when a check failed or the pull request conflicts; pending when a check is not finished (a check run not `COMPLETED`, a status `PENDING` or `EXPECTED`) or mergeability is `UNKNOWN`; no checks when there are none; green otherwise. A check passes on `SUCCESS`, `NEUTRAL` or `SKIPPED`; any other conclusion or state, including one GitHub adds later, is a failure that a human sees. Every check counts, required or not. `mergeStateStatus` is reported, not judged: `BLOCKED` and `BEHIND` depend on reviews and branch protection that Owlshift cannot satisfy by itself.
- **Known limit.** A required check that has not reported yet has no context, so it cannot be listed; `mergeStateStatus` then reads `BLOCKED`. Reading the required checks themselves needs admin rights or, on a private repository, a paid plan.
- **Opening.** `find_open_pull_request` looks for an open pull request from the branch of this repository into the base, so a resumed delivery does not open a second one; `open_pull_request` passes the runner's title and body verbatim. Draft pull requests are optional: a private repository on a free plan has none.
- **Tests.** Fixtures recorded read-only on `pitchopp/owlshift`, in `crates/owlshift-adapters/tests/fixtures/github/`; the answer to `createPullRequest` is made up in the schema's shape and flagged, since recording it would open a pull request. An ignored live test pushes, opens, finds and reads a pull request on a disposable repository named by `OWLSHIFT_LIVE_GITHUB`, then closes it and deletes the branch.

## P0 tasks & exit gate

Each task is an issue in the Owlshift Linear workspace (team `OWL`), with its blockers recorded as relations.

- [x] Create the repository with its licence (2026-09-27)
- [x] OWL-5 · Set up the Owlshift Linear workspace: team `OWL`, states including Needs Input and Triage, labels, one project per step (2026-09-27)
- [x] OWL-6 · Run checks C1, C3, C6 and C7 (Claude part) and record the results (2026-09-28)
- [x] OWL-7 · Settle decision D10 (contribution terms: DCO, 2026-09-28); add the contribution guide
- [x] OWL-8 · Cargo workspace with the six crates; CI matrix on macOS, Linux and Windows (format, lint, tests) (2026-09-27)
- [x] OWL-9 · Write the contracts and generate their JSON Schemas (2026-09-28)
- [x] OWL-10 · Core: model, state machine, scheduler skeleton, policy floor, with unit tests (2026-09-28)
- [x] OWL-11 · Fake harness, test tracker, local bare remote and scenario runner; one scenario end to end (2026-09-28)
- [x] OWL-12 · CLI: `--version`, `doctor`, `config show` (2026-09-28)

**Exit gate.** CI green on the three platforms; one scenario runs end to end with the fake harness and the test tracker; `owlshift doctor` reports the maintainer's machine correctly.

## P1 tasks & exit gate

- [x] OWL-13 · Linear tracker adapter: read a ticket, post a comment, with recorded fixtures and a conformance suite (2026-09-28, [#16](https://github.com/pitchopp/owlshift/pull/16))
- [x] OWL-14 · Claude Code harness adapter: headless run on the user's login, model, effort, permission level, usage capture (2026-09-28, [#15](https://github.com/pitchopp/owlshift/pull/15))
- [ ] OWL-15 · Executor: worktree, brief, spawn, process-tree stop, result validation, isolation check
- [ ] OWL-16 · Gate commands from the project config before delivery
- [ ] OWL-17 · GitHub forge adapter: push the branch, open the PR, read the complete check set
- [ ] OWL-18 · Writer: delivery report on the ticket
- [x] OWL-19 · Default build role prompt (2026-09-28, [#13](https://github.com/pitchopp/owlshift/pull/13))
- [ ] OWL-20 · CLI: `init`, `do`, `logs`

**Exit gate.** Three real Owlshift tickets delivered as PRs by `owlshift do` on the maintainer's subscription, with no guardrail breach.
