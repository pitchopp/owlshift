# Configuration recipes

The [adoption pages](adopt/linear.md) take you to a first pull request. This page is for what comes next: each recipe says what you want, the lines of `owlshift.toml` (or the labels) that reach it, and how to check it. The recipes are short on purpose; the file `owlshift init` wrote lists every key with a comment, and the [design](design/runtime-and-operations.md#configuration) is the reference.

## How to read the recipes

Two files hold the settings:

- `owlshift.toml`, the **project file**, at the root of your repository: committed, read by every machine that works the project. A recipe's `toml` block belongs in it.
- The **personal file**, `config.toml` in Owlshift's folder of your user configuration (`~/Library/Application Support/owlshift/` on macOS, `~/.config/owlshift/` on Linux; `owlshift config show` names it on its `personal file:` line): yours alone, never committed. A block marked `toml personal` belongs in it.

A project block shows only the tables a recipe adds or changes. Add them to the file `init` wrote; where your file already has the table, change its keys instead of writing the table twice, since TOML refuses a table defined twice. The same goes for the personal file: the Codex and caps recipes both use `[harnesses.claude]`, so write its keys under one table. This is the smallest file the blocks build on (your tracker lines, written by `init`, stay as they are):

```toml
requires = ">=0.1"

[tracker]
kind = "linear"
team = "ENG"
admit = { label = "agent" }

[tracker.states]
ready = "Todo"
working = "In Progress"
needs_input = "Needs Input"
review = "In Review"

[stack]
gate = ["make test"]

[pipeline]
default = "standard"
plan_approval = "on-fork"

[models]

[policy]
always_human = []
```

To see what Owlshift reads after you edit either file, run:

```bash
owlshift config show
```

It prints each value your files set, as written, with the file it comes from, and the resolver, always-human and ticket-budget lines with their defaults; a key you leave out, such as `max_in_flight`, does not show. It exits with an error when a file is invalid. `owlshift doctor` checks the machine and the project, and `owlshift gate` runs the gate once, confined as an agent's run is.

## A gate and setup for your stack

**Goal:** the commands every agent's work must pass, and the ones that install its dependencies, for your stack. `owlshift init` guesses them from your root files and never runs them; these are the guesses, to review.

The gate runs in order from the repository root, through the shell (`sh -c`). The `setup` commands run first, with the network open; they may change only ignored files, so install from a frozen lockfile. The sandbox closes your home folder to every run, so a tool that reads a package store there needs it declared in `caches` and allowed in your personal file.

**Rust**

```toml
[stack]
gate = [
    "cargo fmt --all -- --check",
    "cargo clippy --workspace --all-targets --locked -- -D warnings",
    "cargo test --workspace --locked",
]
run_caches = { CARGO_HOME = "cargo" }
```

Drop `--locked` when the repository has no `Cargo.lock`. `run_caches` gives the runs a cargo home of Owlshift's own, which they write: a crate your `~/.cargo` lacks is downloaded there, not refused. rustup and its tool chains stay where you installed them.

**npm**

```toml
[stack]
gate = ["npm run lint", "npm run typecheck", "npm run test"]
setup = ["npm ci"]
run_caches = { npm_config_cache = "npm" }
```

Name your own scripts: `init` writes the ones your `package.json` defines. The runs install online into npm's cache of Owlshift's own, kept for the next runs.

**pnpm (version 10)**

```toml
[stack]
gate = ["pnpm run lint", "pnpm run test"]
setup = ["pnpm install --frozen-lockfile"]
run_caches = { npm_config_store_dir = "pnpm-store" }
```

pnpm's store is a cache of Owlshift's own, which the runs write, so a package the lock file gains is downloaded, not refused. A project on another pnpm than 10 keeps the same setup without `run_caches`, since only pnpm 10 was checked; run `owlshift gate` to see whether it works for you.

**Python with uv**

```toml
[stack]
gate = ["uv run ruff check .", "uv run pytest"]
run_caches = { UV_CACHE_DIR = "uv", UV_PYTHON_INSTALL_DIR = "uv-python" }
```

`uv run` syncs the project's environment from `uv.lock` itself, updating the lock when `pyproject.toml` changed, so there is no setup. A project that wants the gate to fail on a stale lock instead writes `uv run --locked ruff check .` and `uv run --locked pytest` (`--locked` asserts that `uv.lock` stays unchanged, as `uv run --help` says). Without `uv.lock`, the tools run bare and the gate needs a `setup` that installs them. `init` guesses `ruff` and `pytest` only from a `[tool.ruff]` and a `[tool.pytest.ini_options]` table in `pyproject.toml`. Confined, uv needs both caches of its own: its packages, and the Pythons it installs, since your home's are closed.

**The caches your runs write.** Each `run_caches` entry is a variable a package manager reads for its cache and the name of a folder Owlshift makes for it, under `~/Library/Caches/owlshift/` on macOS and `~/.cache/owlshift/` on Linux, one folder per repository (`OWLSHIFT_CACHE_DIR` moves it, for a machine whose cache folder's path holds a space). `owlshift doctor` shows its size; delete the folder to empty it. Your own caches stay read-only to the runs: declare one in `stack.caches`, and allow it in your personal file with `allow_caches`, only for a tool that must read it, such as yarn 4's global cache:

```toml personal
allow_caches = [".yarn/berry"]
```

**A Makefile**

```toml
[stack]
gate = ["make lint", "make test"]
```

`init` reads the `lint`, `test` and `check` targets your Makefile defines.

**Check it:** `owlshift gate` runs the gate in a fresh worktree of your default branch (`--at origin/<branch>` tries a branch that is not merged yet) and names each command, its status and its time. Run it after you commit and push the file. When it fails for a folder of your home, it prints the `caches` and `allow_caches` lines that open it. `owlshift doctor` ends with `Ready` when nothing blocks `owlshift do`.

## Codex beside Claude Code

**Goal:** a second model family reviews the plan and the diff, and the reviews go on when Claude Code reaches its usage limit.

Claude Code runs every authoring role (intake, design, build, the resolver, the arbiter, the rebase): no key moves one of them to Codex. Codex takes part in two ways, both opt-in. First, an extra reviewer on the critic's mandate in every design review and verify pass, beside the Claude Code reviewers. A Codex that cannot serve a pass drops its seat and the ticket says so; it never stops the ticket.

```toml
[pipeline]
codex_reviewer = true

[models]
deep = { claude = "claude-opus-5-5", codex = "<a Codex model>" }
standard = { claude = "claude-sonnet-5", codex = "<a Codex model>" }
```

Write a model your Codex accepts where the example has a placeholder; a tier left out runs on the harness's own default model. Second, a fallback for the review roles alone, in your personal file: while Claude Code is at its usage limit, the design review and verify passes run on Codex instead.

```toml personal
[harnesses.claude]
fallback = "codex"
```

Codex must be installed and logged in with your own subscription. A fallback named for a harness that does not serve it is ignored.

**Check it:** `owlshift doctor` reports each harness, and has a `review fallback` line that says the fallback serves only the review roles and whether Codex can serve here. Codex's login must be kept in its `auth.json`: a login kept in the system keyring is refused. Only `[harnesses.claude].fallback` is read; a `fallback` under `[harnesses.codex]` is never used. `owlshift config show` prints the personal file's `harnesses` and the project's `codex_reviewer`.

## Two tickets that touch the same area

**Goal:** tickets that touch the same files do not run at once, or run together and are repaired afterwards.

A ticket holds the zones and named resources intake declares, then the files its branch changes. Two tickets holding one conflict, and the later waits. By default the exclusion is strict (`exclusive`). Name a **resource** for what a path pattern covers, such as a migration chain: a ticket that touches a path of the pattern holds the whole resource, and two never run together (`*` stands for one part of a path, `**` for any number of them).

```toml
[stack.resources]
migrations = "backend/*/migrations/**"
lockfile = "pnpm-lock.yaml"
```

A person can declare a **zone** on a ticket with a label `zone:<folder>`, such as `zone:backend/billing`; labels add zones and never remove one. Zones also decide who answers a ticket without an assignee:

```toml
[zones."backend/billing"]
owner = "<account id>"
```

The owner is the tracker account that decides the tickets touching that zone, written exactly: a Linear user's id, a GitHub account's numeric id, or the name on the Markdown tracker. When your file lists `[people]`, write the person's handle instead.

When your tickets often touch the same files and a rebase repairs them cheaply, let two Owlshift tickets build on one file or zone: the later one is refreshed onto the first's merge. Named resources, people's tickets, other pull requests, a ticket that holds the whole repository and a third ticket on one path still exclude. Once a ticket has a plan, it holds the plan's files; intake's zones and resources count only when no plan names files.

```toml
[policy]
overlap = "rebase"
```

**Check it:** `owlshift config show` prints `overlap`, the resources and the zones. `owlshift why TICKET` says what a waiting ticket waits for. An invalid pattern is refused when the file loads, and `config show` then exits with an error.

## A ticket's variant and tier by label

**Goal:** a ticket gets a lighter or heavier pipeline, or a stronger model, without changing the project's default.

The labels are read at each dispatch, in any letter case, and win over intake's verdict:

- `variant:trivial` (build and verify), `variant:standard` (design, design review, build and verify) or `variant:risky` (standard, with a person approving the plan).
- `tier:deep`, `tier:standard` or `tier:fast`: the model tier of the ticket's design and build runs.
- `effort:low`, `effort:medium`, `effort:high`, `effort:xhigh` or `effort:max`: how hard the model thinks.

Create the labels in your tracker, then put them on tickets. The configuration sets the defaults and maps a tier to a model:

```toml
[pipeline]
default = "standard"

[models]
deep = { claude = "claude-opus-5-5" }
standard = { claude = "claude-sonnet-5" }
fast = { claude = "claude-haiku-4-5" }
```

A tier left out runs on Claude Code's own default model, except `fast`, which falls back to the `standard` model.

**Check it:** `owlshift config show` prints the default variant and the models. The ticket's INTAKE comment says the routing it follows, and a label whose value is not a variant, tier or effort is ignored and recorded as a `warning` event.

## Plan approval, and how much the resolver decides

**Goal:** choose when a person approves a plan before Build, and how many questions Owlshift answers without you.

```toml
[pipeline]
plan_approval = "on-fork"

[policy]
resolver_delegation = "discoverable"
resolver_passes = 5
```

- `plan_approval`: for a standard ticket, `"always"` asks for approval, `"on-fork"` asks when the plan leaves a choice open, `"never"` never asks. A `risky` ticket always asks, and a `trivial` one never does, whatever this says.
- `resolver_delegation`: what the resolver decides without the ticket's decider. `"none"` sends every question to the decider. `"facts"` allows what it checks in the repository. `"discoverable"` (the default) adds what the ticket, earlier decisions and the project's rules settle. `"ticket"` adds a scope question the ticket's own text answers, quoted. `"all"` lets it decide everything but the floor on its own judgment.
- `resolver_passes`: the runs in a row the resolver may unblock in one command, 1 to 50, or `"unlimited"`, which needs a `resolver_budget_usd`.

Whatever the level, a `security` or `irreversible` question reaches a person; `data_loss`, `money` and `legal` stay with a person below `all`, and `scope` below `ticket`. List a category of your own to keep it human at every level:

```toml
[policy]
always_human = ["billing"]
```

**Check it:** `owlshift config show` prints each value with its origin. The delivery comments show the decisions the resolver took, each reversible by an answer of yours.

## Merge on green

**Goal:** Owlshift merges a pull request it follows once its checks are green, so you review only the ones that need you.

```toml
[forge]
merge = "labeled"
merge_method = "squash"
```

`merge` is `"never"` (the default, every merge is yours), `"labeled"` (only tickets carrying the label `owlshift:merge`) or `"all"`. `merge_method` is `"squash"` (the default), `"merge"` or `"rebase"`. The setting is read at the default branch's head: a ticket's own branch, and your personal file, cannot turn it on. A ticket that asked a security question is never merged by Owlshift.

**Check it:** `owlshift config show` prints the `[forge]` values; `owlshift doctor` reports the forge and its credential. A pull request left unmerged has its reason in watch's output, in the notification and in `owlshift why TICKET`, such as a missing `owlshift:merge` label; the delivery comment says who merges. `doctor` also checks that the GitHub App may merge.

## Caps: concurrent runs, usage and cost

**Goal:** bound how much runs at once and how much it costs. The caps live in both files.

In the **project file**, what the project allows:

```toml
[policy]
max_in_flight = 3
max_open_prs = 5
ticket_budget_usd = 25.0
resolver_budget_usd = 10.0
```

- `max_in_flight`: how many of the project's tickets are worked at once, 1 to 10 (1 by default).
- `max_open_prs`: how many pull requests Owlshift delivered may be open before the drain starts no new Build, 1 to 20 (3 by default).
- `ticket_budget_usd`: what one ticket's runs may cost in all; reached, the ticket parks until a person restarts it. No budget by default.
- `resolver_budget_usd`: a ceiling on what one chain of resolver runs costs; past it, the next questions go to the decider.

In your **personal file**, what this machine allows:

```toml personal
concurrent_runs = 3

[harnesses.claude]
usage_cap_percent = 90
budget_usd = 5.0
```

- `concurrent_runs`: how many steps of `owlshift watch --drain` run at once on this machine, over every project, 1 to 16 (2 by default). The project's `max_in_flight` and this cap both apply.
- `usage_cap_percent`, 1 to 100: while the share of a usage window of this harness's subscription is at or above it, no new run starts on this harness, which counts as at its usage limit.
- `budget_usd`: a dollar cap on each run of this harness, not on a day's total. Claude Code stops a run once its cost, counted at list price, is above the cap, so a run can end above it; a run stopped by it alone fails.

**Check it:** `owlshift config show` prints the caps your files set and the file of each, and nothing for a cap you left out; `owlshift doctor` shows an invalid personal file. A running service reads `concurrent_runs` and the usage cap at its next start.
