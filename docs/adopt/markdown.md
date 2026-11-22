# Adopt a project with Markdown tickets

This page takes you from an installed `owlshift` to a first pull request made by agents on your repository, with tickets kept as Markdown files in the repository itself: no tracker account needed. It takes about fifteen minutes, plus the time the agents work.

Read the limits first: the Markdown tracker serves **one machine**. You run `owlshift do` and `owlshift continue` yourself, in a checkout; Owlshift writes the ticket files but never commits them; and the background service does not serve such a project, so `owlshift start` refuses it. To have a backlog drained in the background, use [Linear](linear.md) or [GitHub Issues](github-issues.md). The ticket format is described in full in [The Markdown tracker](../markdown-tracker.md).

The outputs below come from real runs. Paths and some long lines are shortened, marked `…`; your repository's name and paths will differ.

## Before you start

- Owlshift is installed and `owlshift doctor` runs: see the [quick start](../../README.md#quick-start).
- Claude Code is installed, and you have a Claude subscription.
- Your project is a git repository on GitHub that you can push to, cloned on this machine. The tickets live in the repository, but the pull requests are opened on GitHub.

## 1. Write the project file: `owlshift init`

In your clone, at the repository's root:

```bash
cd ~/code/shop
owlshift init
```

At a terminal, `init` asks for what no flag gave: answer `3` (or `markdown`) when it asks where the tickets live, or pass `--tracker markdown` to skip the question. In a script nothing is asked, and the tracker is Linear unless `--tracker markdown` says otherwise. `init` does four things:

1. It reads the repository's root files (`Cargo.toml`, `go.mod`, `package.json`, `pyproject.toml`, `requirements.txt`, `Gemfile`, `gradlew`, `pom.xml`, `composer.json`, a `.sln` or `.csproj`, `Makefile`) and writes the gate it suggests, your lint, formatter and test commands, in a commented `owlshift.toml`. It never runs them. It shows the gate found first: press Enter to keep it, or type your own commands, one per line, an empty line ending them; with `--gate`, one per command, as in `owlshift init --tracker markdown --gate "make lint" --gate "make test"`, nothing is asked.
2. It signs you in to GitHub through Owlshift's GitHub App. It prints a code and asks you to open `https://github.com/login/device` and enter it. If the app is not yet installed on the repository, it prints the install link and waits until you have installed it. If the code expires before you enter it, `init` says so and exits with an error: run it again for a new code.
3. It asks for the token agent runs log in to Claude Code with. Make it in another terminal with `claude setup-token`, then paste it. It is asked once per machine. Owlshift never asks for a model API key.
4. It writes an example ticket, `tickets/DEMO-1/ticket.md`.

The secrets go to the system keychain, never to a file. On a machine where both were already stored, `init` asks nothing; here is what it printed for a project whose secrets were left for later (`--skip-secrets`):

```text
Wrote ~/demo/owlshift.toml: review it, then commit it.
Wrote ~/demo/tickets/DEMO-1/ticket.md as an example: replace its assignee `maintainer`, the ticket's decider, with your own name, then run `owlshift do DEMO-1`. Nothing is committed.
The Markdown tracker serves one machine: run `owlshift do` and `owlshift continue` in a checkout. Owlshift commits none of its files, and the daemon does not serve it, so `owlshift start` refuses such a project. Format: https://github.com/pitchopp/owlshift/blob/main/docs/markdown-tracker.md
No secret stored (--skip-secrets).
Next: `owlshift doctor` checks git, the harnesses and the Claude Code token for agent runs; `owlshift do TICKET` runs a ticket.
Nothing detected was run: `owlshift gate` runs the gate to check it.
```

Read `owlshift.toml`: it lists every setting, each with a comment above it that says what it decides, its values and its default; a setting commented out with its default value changes nothing once uncommented. The [configuration reference](../reference/configuration.md) lists the same settings, and those of your personal file. Check the gate under `[stack]`: it is what every agent's work must pass. Then commit the file and push it to your default branch. Each run reads the project file of the commit its branch starts from, and until the default branch holds one, your local file is used.

## 2. Check the machine and the project: `owlshift doctor`

```bash
owlshift doctor
```

Each line is marked `✓` (fine), `!` (a warning, which never blocks `owlshift do`) or `✗` (a problem to fix first); the end of the report says why each problem matters and how to fix it. Run it again until it ends with `Ready`. On a fresh Debian 12 machine without Claude Code or a Secret Service, it ended with:

```text
✗ Not ready: 2 problems to fix before `owlshift do`

1. Claude
   Why   `owlshift do` runs every agent in Claude Code, so it must be installed.
         Agent runs log in with the token the `claude agent login` line checks.
   Fix   1) Install Claude Code: https://code.claude.com/docs/en/setup
         2) owlshift doctor    (to check)

2. Secrets
   Why   `owlshift do` reads the project's tracker and forge secrets in the
         system keychain before each run.
   Fix   1) Make the system keychain available: unlock it on macOS; on Linux,
            start a Secret Service such as GNOME Keyring
         2) owlshift doctor    (to check)
```

Once fixed, the report ends with:

```text
✓ Ready: nothing blocks `owlshift do`.
```

A few warnings are expected on a Markdown project and can be left: `drain admission` (`admitters` is not set; it serves the background service, which a Markdown project does not use), `forge reviewers` and `service`. The `sandbox` line must be `✓`: it says that agent runs are confined, with `sandbox-exec` on macOS or `bwrap` on Linux.

## 3. Check the gate: `owlshift gate`

```bash
owlshift gate
```

This runs the gate once, the way it runs on an agent's work: in a fresh worktree of your default branch, confined, with nothing of your environment but what the project declares. A gate that passes on your laptop can fail here, because a tool is missing from the `PATH`, a test reads your home folder, or the dependencies were never installed; fixing it now saves an agent's run. The output names each command, its status and time:

```text
The gate of 7fd1a60b01f9 (origin/master), from your owlshift.toml, the commit having none, in a fresh worktree, confined as a Build's gate:
  test -f README
Its whole output goes to ~/.local/share/owlshift/projects/github.com/…/gate/run/gate.log.
$ test -f README
  ok, 0.0 s
Gate green on 7fd1a60b01f9: 1 command in 1.6 s.
```

When a gate needs its dependencies installed first, list the install commands under `stack.setup` in `owlshift.toml` (the file's comments show how). The gate runs with your home folder closed, so a package manager gets a cache of Owlshift's own that the runs write, declared in `stack.run_caches` as the variable it reads for its cache and a folder's name: the installs fetch what the cache lacks and keep it for the next runs. A cache of your home is reached only through `stack.caches`, read-only, and only on a machine whose personal configuration lists that folder in `allow_caches`; until it does, every run and gate refuses the project, and `owlshift doctor` gives the lines to add.

- **npm.** `owlshift init` writes the setup `npm ci` and `run_caches = { npm_config_cache = "npm" }`. If npm fails with `EPERM` anyway, see [Troubleshooting](../troubleshooting.md#the-gate-passes-on-your-laptop-and-fails-under-owlshift-gate).
- **yarn and bun.** For yarn 4 on its global cache (a `.yarnrc.yml`, and `packageManager` naming yarn 4 or later or `enableGlobalCache: true`), `init` writes `caches = [".yarn/berry"]`, which works the same way; for another yarn 2 or later its output notes when to declare it. yarn 1 and bun need no cache: they fetch the packages from the registry in each run.
- **pnpm.** For pnpm 10, `owlshift init` writes the setup `pnpm install --frozen-lockfile` and `run_caches = { npm_config_store_dir = "pnpm-store" }`, the store being a cache the runs write; a project on another pnpm keeps the setup without it, as only pnpm 10 was checked.
- **Rust and uv.** `owlshift init` writes `run_caches = { CARGO_HOME = "cargo" }` for cargo, and `UV_CACHE_DIR` and `UV_PYTHON_INSTALL_DIR` for uv.

If the gate fails here though it passes on your laptop, or npm fails with `EPERM`, see [Troubleshooting](../troubleshooting.md#the-gate-passes-on-your-laptop-and-fails-under-owlshift-gate).

## 4. Run a first ticket: `owlshift do`

Open `tickets/DEMO-1/ticket.md`. Replace `maintainer` with your own name in `author` and `assignee`: the assignee is the ticket's decider, the person Owlshift asks when a decision is needed. Then replace the description with something small and clear, such as a typo to fix or a line to add to the README. Keep the front matter between the two `+++` lines:

```markdown
+++
title = "Say how to run the tests in the README"
author = "alex"
stage = "Todo"
assignee = "alex"
labels = ["agent"]
+++

Add a "Running the tests" section to README.md that gives the command.
```

Then run it:

```bash
owlshift do DEMO-1
```

`owlshift do` works in the foreground and says what it does. It runs the ticket through design, design review, build and verify, each step by a fresh agent run, with the gate run on the result; it prints each step as it goes. It works in its own clone and worktrees under Owlshift's data folder, never in your checkout, and it pushes a branch `owlshift/demo-1` and opens a pull request on GitHub. The ticket's `stage` moves as it goes, and Owlshift writes its comments, such as the plan and the delivery report, as files in `tickets/DEMO-1/comments/`: a `[owlshift] PLAN` comment that points to the whole plan in `tickets/DEMO-1/documents/1.md`, then a `[owlshift] DELIVERY` report with the pull request, its checks, the gate's result, the decisions taken without you and what remains for you to do.

A one-line change to the README took a little over a minute and about 0.28 USD of Claude usage. The run ended with:

```text
Delivered: pull request #101 https://github.com/…/pull/101 (opened), checks not finished yet; the delivery report is on the ticket. Owlshift never merges: review and merge it.
```

Review the pull request and merge it yourself: Owlshift never merges unless you turn that on.

## 5. Answer a question

When a run needs a decision only you can make, the command posts the questions as a comment file, sets the stage to `Needs Input`, shows a desktop notification, and ends. The comment is a file such as `tickets/DEMO-1/comments/20261113T101500Z-owlshift.md`, whose title line reads `[owlshift] QUESTIONS · round 1`.

Answer with a new file in the same folder: its name is the current UTC time, then your name (the ticket's assignee), and its front matter names the comment you answer.

```bash
date -u +%Y%m%dT%H%M%SZ    # prints, say, 20261113T103000Z
```

```markdown
+++
reply_to = "20261113T101500Z-owlshift"
+++

Q1: the shorter wording. Q2: yes, keep the old flag. go
```

Saved as `tickets/DEMO-1/comments/20261113T103000Z-alex.md`. An answer counts once you have left it unedited for 10 minutes (the project's `policy.quiet_window_minutes` changes that), or at once when it ends with `go`. Then:

```bash
owlshift continue DEMO-1
```

`continue` checks your answers. If one is missing or unclear, it asks again in a new comment; otherwise the work goes on to a pull request. Answer as many rounds as it takes: there is no limit.

If a ticket stops for a reason it cannot settle itself, such as a run that fails twice, its stage becomes `Needs Input` with a comment marked `PARKED` that says why. Fix what it names, then `owlshift continue DEMO-1` restarts it.

### How much Owlshift decides alone

Out of the box Owlshift asks rather than guesses, works one ticket at a time and never merges. These keys of `owlshift.toml`, each explained in the file, let it do more alone:

- `pipeline.plan_approval`: `"never"` lets a plan through without your approval; `"on-fork"`, which `init` writes, asks only when the plan leaves a choice open; `"always"` asks every time.
- `policy.resolver_delegation`: what the resolver decides without you, from `"none"` to `"all"` (`"discoverable"` by default). `policy.resolver_passes` (5 runs by default) and `policy.resolver_budget_usd` bound how long it goes on before it asks.
- `policy.ticket_budget_usd`: what one ticket's runs may cost before it parks; no budget by default.
- `policy.max_in_flight`: how many tickets are worked at once, 1 by default.
- `[forge] merge`: `"labeled"` (tickets carrying the label `owlshift:merge`) or `"all"` turns merge on green on: Owlshift merges a pull request it follows once its checks are green, into your default branch, by `merge_method` (`"squash"` by default). `"never"`, the default, leaves every merge to you.

Whatever they say, a question on security or on an irreversible action always reaches a person, and so do the categories listed in `policy.always_human`. Under merge on green, a ticket that asked a security question is never merged by Owlshift, nor one whose other such question no person settled.

## 6. The background service

`owlshift start` serves a Linear or GitHub Issues project in the background. A Markdown project is refused, by design:

```text
owlshift: the Markdown tracker serves one machine and is not served by the daemon: its tickets are files in your checkout, which the daemon does not read. Run `owlshift do TICKET` or `owlshift continue TICKET` in that checkout instead (see https://github.com/pitchopp/owlshift/blob/main/docs/markdown-tracker.md)
```

Keep using `owlshift do` and `owlshift continue` in your checkout. To move to a tracker the service can drain, follow the [Linear](linear.md) or [GitHub Issues](github-issues.md) page.

## Next

- [Configuration recipes](../recipes.md) set Owlshift up for your stack and your team: the gate, Codex, tickets that touch the same area, labels, plan approval, merge on green and caps.
- More tickets: one folder each, `tickets/DEMO-2/`, `tickets/DEMO-3/`, in the format of [The Markdown tracker](../markdown-tracker.md). Follow-ups Owlshift proposes appear as new folders at the stage `Triage`; accept one by giving it an assignee and moving its stage to `Todo`.
- Commit the ticket files when you want them in the repository's history: Owlshift never does.
- `owlshift logs DEMO-1` prints what Owlshift recorded for the ticket.
- [SECURITY.md](../../SECURITY.md) says what the confinement of agent runs protects and what it does not.
- `owlshift uninstall` removes what Owlshift installed on this machine, the keychain entries `init` stored included; it never touches a repository, a ticket or a branch.
