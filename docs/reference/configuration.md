# Configuration reference

Owlshift reads its configuration from two plain-text files: the project file `owlshift.toml`, committed in the repository, and a personal file on each machine, never committed. Secrets are in neither file. The tracker and forge credentials, and the token agent runs log in to Claude Code with, live in each machine's system keychain, where `owlshift init` stores them.

This page lists every key of both files, with its values, its default and what it changes.

## Layers and precedence

| Layer | Where | Holds |
| --- | --- | --- |
| Floor | Built into the binary | What no configuration can loosen. The categories `security` and `irreversible` always reach a person, and agents never get tracker or forge credentials. Merge on green, the one grant a project gives, is checked by the floor. |
| Project file | `owlshift.toml`, committed at the repository's root | The project's policy, the same for every developer and runner: the tracker and its states, how tickets are admitted, the gate, the pipeline, models, policy, merge on green, zones and people. |
| Personal file | `owlshift/config.toml` in your configuration directory, never committed | What belongs to one machine: how many runs at once, which tickets it takes, the harnesses' fallbacks and caps, notifications, what a project may pass to agents. |
| Ticket labels | The tracker | One ticket's variant, tier or effort, the zones it touches, whether it is kept out or merged on green. |

The floor wins over everything. The project file comes next. The personal file never loosens the project's policy, and where both files speak to the same thing, the stricter holds:

- `concurrent_runs` caps the project's `policy.max_in_flight` on this machine.
- A project's `stack.gate_env` and `stack.caches` pass only what the personal `allow_gate_env` and `allow_caches` list.
- The personal file has no say over `[forge]`. Merge on green is read from the project file at the default branch's head, never from a ticket's branch.

Ticket labels choose, for one ticket, within what the project allows. Their prefixes are matched without regard to case, and they are read at each dispatch:

| Label | What it changes |
| --- | --- |
| `variant:<name>`: `trivial`, `standard` or `risky` | The ticket's pipeline, in place of intake's verdict and `pipeline.default` |
| `tier:<name>`: `deep`, `standard` or `fast` | The tier its runs take, in place of intake's verdict; `[models]` maps a tier to a model |
| `effort:<level>`: `low`, `medium`, `high`, `xhigh` or `max` | The effort its runs take, in place of intake's verdict |
| `zone:<folder>`, such as `zone:backend/billing` | Adds a zone the ticket touches. For a ticket without an assignee, the zone's owner decides it. |
| The admission label `tracker.admit` names | Admits the ticket to the queue `owlshift watch --drain` works |
| The label `tracker.exclude` names | Keeps the ticket out of the drain, whatever else it carries |
| `owlshift:merge` | With `forge.merge = "labeled"`, lets the runner merge the ticket's green pull request |

### Reading the effective configuration

Run `owlshift config show` in a checkout of the project. It prints:

- where each file is;
- every value either file sets, with the file it comes from;
- the categories always left to a person (the floor's, those the delegation level keeps, the project's additions);
- the resolver's settings;
- the budget on one ticket's runs.

The resolver's settings and the budget are marked `default` where no file sets them. A key that no file sets takes the default its entry below gives.

## The project file

`owlshift.toml` sits at the repository's root, and is committed. `owlshift init` writes it once, commented, and never overwrites it. A run reads the file of the commit its branch starts from. The background service and merge on green read the default branch's.

The entries below are the comments of the file `owlshift init` writes, word for word, each under the keys it describes and followed by its lines as written. A line commented out with its default value changes nothing once uncommented. A line commented out with an example value leaves the key unset until you set it. Only `[tracker]` differs between trackers. `init` writes `requires` with the major and minor version of the Owlshift that runs it, such as `">=0.1"`, and this page shows `<major>.<minor>` in its place. A key missing from an older project file takes the default its entry gives.

This section is generated from the template in `crates/owlshift-runner/src/init.rs`, and a test fails when the two differ. To change it, edit the template, then run `OWLSHIFT_UPDATE_DOCS=1 cargo test -p owlshift-runner --lib init::reference`.

<!-- BEGIN generated from the file `owlshift init` writes: OWLSHIFT_UPDATE_DOCS=1 cargo test -p owlshift-runner --lib init::reference -->

### Top level

Owlshift project configuration. Commit it: every developer and every runner of the project read this file. It holds no secret: the tracker and forge credentials, and the token agent runs log in to Claude Code with, live in each machine's system keychain (`owlshift init` stores them).

Every key that applies to this tracker is listed, with what it decides, its values and its default. A key commented out with its default value changes nothing once uncommented; one with an example value is unset until set.

#### `requires`

The Owlshift versions that can read this file: a semantic version requirement. Required.

```toml
requires = ">=<major>.<minor>"
```

### `[tracker]` for Linear

#### `tracker.kind`

Where the tickets live: "linear" (here), "github" for the issues of this repository, or "markdown" for tickets kept in it, one folder each under tickets/. Required.

```toml
[tracker]
kind = "linear"
```

#### `tracker.team`

The Linear team key: the prefix of its ticket ids, OWL in OWL-12. Required for "linear".

```toml
team = "OWL"
```

#### `tracker.admit`

How a person admits a ticket to Owlshift's queue (`owlshift do TICKET` ignores it): { label = "..." }, or "ready", the move to states.ready, which { state = "..." } would repeat; "delegation" is not built. Required.

```toml
admit = { label = "agent" }
```

#### `tracker.admitters`

The accounts whose admission counts, as the tracker reports them (a Linear user's id, the name on the Markdown tracker), or handles of [people]. Default: none, and `owlshift watch --drain` refuses to start.

```toml
# admitters = ["<account id>"]
```

#### `tracker.exclude`

A label that keeps a ticket out of the drain, whatever else it carries; never the admission label. Default: none, no ticket is kept out.

```toml
# exclude = "no-agent"
```

#### `tracker.states.ready`, `tracker.states.working`, `tracker.states.needs_input`, `tracker.states.review`, `tracker.states.parked`

The tracker states that show where a ticket stands. Required, but for `parked`, the state of a ticket Owlshift stopped. Default for `parked`: none, such a ticket shows needs_input, its PARKED comment saying why.

```toml
[tracker.states]
ready = "Todo"
working = "In Progress"
needs_input = "Needs Input"
review = "In Review"
# parked = "Parked"
```

### `[tracker]` for GitHub Issues

#### `tracker.kind`

Where the tickets live: "github" (here) for the issues of this repository, reached with its GitHub credential, the states being labels; "linear"; or "markdown", one folder per ticket under tickets/. Required.

```toml
[tracker]
kind = "github"
```

#### `tracker.admit`

How a person admits an issue to Owlshift's queue (`owlshift do TICKET` ignores it): "ready", the ready label below, which { state = "..." } would repeat, or { label = "..." }; "delegation" is not built. Required.

```toml
admit = "ready"
```

#### `tracker.admitters`

The GitHub accounts whose label admits an issue, by numeric id (or handles of [people]); `gh api user --jq .id` prints yours. Default: none, and `owlshift watch --drain` refuses to start.

```toml
# admitters = ["<your numeric GitHub account id>"]
```

#### `tracker.exclude`

A label that keeps an issue out of the drain, whatever else it carries; never the admission label. Default: none, no issue is kept out.

```toml
# exclude = "agent:skip"
```

#### `tracker.states.ready`, `tracker.states.working`, `tracker.states.needs_input`, `tracker.states.review`, `tracker.states.parked`

The labels that show where an issue stands; `owlshift init` creates the missing ones. Required, but for `parked`, the label of an issue Owlshift stopped. Default for `parked`: none, such an issue shows needs_input.

```toml
[tracker.states]
ready = "agent:ready"
working = "agent:working"
needs_input = "agent:needs-input"
review = "agent:review"
# parked = "agent:parked"
```

#### `tracker.priorities.urgent`, `tracker.priorities.high`, `tracker.priorities.medium`, `tracker.priorities.low`

The label of each priority, every one optional; an issue carrying several reads the most urgent. Default: none, every issue reads `unset`.

```toml
# [tracker.priorities]
# urgent = "priority:urgent"
# high = "priority:high"
# medium = "priority:medium"
# low = "priority:low"
```

### `[tracker]` for the Markdown tracker

#### `tracker.kind`

Where the tickets live: "markdown" (here) for tickets kept in this repository, one folder each under tickets/, "linear" with its `team` key, or "github" for the repository's issues. Required.

```toml
[tracker]
kind = "markdown"
```

The Markdown tracker serves one machine: run `owlshift do TICKET` and `owlshift continue TICKET` in a checkout. Owlshift commits none of its files, and the daemon does not serve it, so `owlshift start` refuses such a project. Its format: https://github.com/pitchopp/owlshift/blob/main/docs/markdown-tracker.md

#### `tracker.admit`

How a person admits a ticket to Owlshift's queue (`owlshift do TICKET` ignores it): { label = "..." }, or "ready", the move to states.ready, which { state = "..." } would repeat; "delegation" is not built. Required.

```toml
admit = { label = "agent" }
```

#### `tracker.admitters`

The accounts whose admission counts, as the tracker reports them (a Linear user's id, the name on the Markdown tracker), or handles of [people]. Default: none, and `owlshift watch --drain` refuses to start.

```toml
# admitters = ["<account id>"]
```

#### `tracker.exclude`

A label that keeps a ticket out of the drain, whatever else it carries; never the admission label. Default: none, no ticket is kept out.

```toml
# exclude = "no-agent"
```

#### `tracker.states.ready`, `tracker.states.working`, `tracker.states.needs_input`, `tracker.states.review`, `tracker.states.parked`

The tracker states that show where a ticket stands. Required, but for `parked`, the state of a ticket Owlshift stopped. Default for `parked`: none, such a ticket shows needs_input, its PARKED comment saying why.

```toml
[tracker.states]
ready = "Todo"
working = "In Progress"
needs_input = "Needs Input"
review = "In Review"
# parked = "Parked"
```

### `[stack]`

#### `stack.gate`

The project's full gate, run in order from the repository root on the commit Owlshift delivers: lint, formatter and tests. Required: with no command, `owlshift do` refuses to start a ticket.

```toml
[stack]
gate = [
    "make test",
]
```

#### `stack.gate_env`

Variables of your environment the gate needs, such as a feature flag, seen by the whole agent run; credential variables are refused, and a machine passes only those its personal `allow_gate_env` lists. Default: none.

```toml
# gate_env = ["FEATURE_FLAGS"]
```

#### `stack.setup`

Commands that install the gate's dependencies, run in order with the network open before a Build's agent and before the gate; they may change only ignored files, so install from a frozen lockfile. Default: none.

```toml
# setup = ["pnpm install --frozen-lockfile"]
```

#### `stack.caches`

Folders of your home, such as a package store, that the setup, the agent and the gate read, never write; a machine opens only those its personal `allow_caches` lists, never one holding credentials. Default: none.

```toml
# caches = ["Library/pnpm"]
```

#### `stack.rules`

The files agents receive as the project's rules, taken whole from the default branch, at most 32 and 64 KiB together, each required to exist. Left out, the root AGENTS.md when there is one; [] sends no rule.

```toml
# rules = ["AGENTS.md", ".claude/rules/testing.md"]
```

#### `stack.resources.<name>`

Named resources a ticket can hold, each with the paths it covers (`*` one path part, `**` any number); two tickets holding one never run together. Default: none.

```toml
# [stack.resources]
# migrations = "backend/*/migrations/**"
```

### `[pipeline]`

#### `pipeline.default`

A ticket's pipeline variant, unless intake or a label picks another: "trivial" (build and verify), "standard" (design, design review, build and verify) or "risky" (standard, a person approving the plan). Required.

```toml
[pipeline]
default = "standard"
```

#### `pipeline.plan_approval`

When a person approves the plan before Build: "always", "on-fork" (when the plan leaves a choice open) or "never". Required.

```toml
plan_approval = "on-fork"
```

#### `pipeline.findings`

Who confirms a review finding before it is acted on: "author" (the next design or fix run answers it) or "arbiter" (one arbiter run per review pass). Default: `"author"`.

```toml
# findings = "author"
```

#### `pipeline.codex_reviewer`

Whether every design review and verify pass adds a Codex reviewer beside the Claude Code ones, a Codex that cannot serve dropping its seat: true or false. Default: `false`.

```toml
# codex_reviewer = false
```

### `[models]`

#### `models.deep`, `models.standard`, `models.fast`

The model of each tier, per harness (`claude`, `codex`). Left out, a harness runs its own default model, but Claude's `fast` falls back to `standard`. A ticket's Build runs on its routing verdict's tier, or its `tier:` label's.

```toml
[models]
# deep = { claude = "claude-opus-5-5", codex = "<a Codex model>" }
# standard = { claude = "claude-sonnet-5", codex = "<a Codex model>" }
# fast = { claude = "claude-haiku-4-5", codex = "<a Codex model>" }
```

### `[policy]`

#### `policy.always_human`

Question categories always left to a person, on top of those the delegation level keeps and the floor's, `security` and `irreversible`, which reach a person at every level. Required.

```toml
[policy]
always_human = []
```

#### `policy.quiet_window_minutes`

How long the decider's latest comment must stay unedited before their answer counts: whole minutes, 1 to 1440; ending it with `go` counts it at once. Default: `10`.

```toml
# quiet_window_minutes = 10
```

#### `policy.max_in_flight`

How many of the project's tickets may be worked at once, between Ready and Watch: 1 to 10; each machine's personal `concurrent_runs` caps it too. Default: `1`.

```toml
# max_in_flight = 1
```

#### `policy.max_open_prs`

How many pull requests Owlshift delivered may be open at once before the drain starts no new Build: 1 to 20. Default: `3`.

```toml
# max_open_prs = 3
```

#### `policy.overlap`

Whether two Owlshift tickets may build on the same file at once: "exclusive" (the later one waits) or "rebase" (both build, the later one refreshed onto the first's merge). Default: `"exclusive"`.

```toml
# overlap = "exclusive"
```

#### `policy.claims`

Where a ticket is claimed, so that one runner alone works it: "local" (the ticket locks of each machine alone; nothing is pushed) or "remote" (a claim ref on origin, for a team's runners or one person's two machines; the default branch's file decides it). Under "remote", a command whose operator cannot be read, the tracker key's account or, on the Markdown tracker, `identity.tracker`, is refused: a claim names its operator. A person releases a claim with `owlshift release`; set back to "local", the claims on origin expire, a person's to release. Default: `"local"`.

```toml
# claims = "local"
```

#### `policy.resolver_delegation`

What the resolver decides without the decider: "none", "facts", "discoverable", "ticket" (also scope questions the ticket's text answers) or "all" (everything but the floor). Default: `"discoverable"`.

```toml
# resolver_delegation = "discoverable"
```

#### `policy.resolver_passes`

The runs in a row the resolver may unblock in one command: 1 to 50, or "unlimited", which needs resolver_budget_usd. Default: `5`.

```toml
# resolver_passes = 5
```

#### `policy.resolver_budget_usd`

A ceiling, in dollars, on what the resolver runs of one chain cost; past it, the next questions go to the decider. A positive number. Default: none, no ceiling; required with resolver_passes = "unlimited".

```toml
# resolver_budget_usd = 20.0
```

#### `policy.ticket_budget_usd`

What one ticket's runs may cost in all, in dollars, every run counted across its commands; reached, the ticket parks until a person restarts it. A positive number. Default: none, no budget.

```toml
# ticket_budget_usd = 100.0
```

### `[forge]`

The forge. Merge on green: the runner merges a pull request it follows, once green, into the default branch; read at that branch's head, never from a ticket's branch or the personal file.

```toml
# [forge]
```

#### `forge.merge`

Which pull requests the runner merges once green: "never", "labeled" (a ticket carrying the label `owlshift:merge`) or "all". Default: `"never"`.

```toml
# merge = "never"
```

#### `forge.merge_method`

How the runner merges: "squash", "merge" or "rebase". Default: `"squash"`.

```toml
# merge_method = "squash"
```

#### `forge.reviewers.<name>`

The forge logins whose reviews on Owlshift's pull requests are read, each mapped to its tracker account: the decider's sends the ticket back to Build. Default: none, no review is read. Refused beside [people].

```toml
# [forge.reviewers]
# octocat = "<account id>"
```

### `[zones]`

#### `zones.<name>.owner`

Code zones, keyed by their folder from the repository root: `owner` is the tracker account (or [people] handle) that decides the tickets without an assignee touching the zone. Default: none.

```toml
# [zones."backend/billing"]
# owner = "<account id>"
```

### `[people]`

#### `people.<name>.tracker`, `people.<name>.forge`

The project's people, each once under a handle (a lowercase letter, then up to 31 lowercase letters, digits, `-` or `_`): `tracker`, their account as the tracker reports it, and `forge`, their login (optional). Default: none. With it, admitters and owners name handles, every person with a `forge` login is a reviewer, and [forge.reviewers] is refused.

```toml
# [people.ann]
# tracker = "<account id>"
# forge = "<forge login>"
```

### `[roles]`

#### `roles.<name>.rules`

Rules added to one role's brief alone, after stack.rules and within its limits: build, resolver, intake, design, design_review, verify or arbiter. Default: none.

```toml
# [roles.verify]
# rules = ["docs/review.md"]
```

<!-- END generated -->

## The personal file

The personal file is `owlshift/config.toml` in your configuration directory:

- on macOS, `~/Library/Application Support/owlshift/config.toml`;
- on Linux and WSL2, `$XDG_CONFIG_HOME/owlshift/config.toml`, or `~/.config/owlshift/config.toml` without it.

`OWLSHIFT_CONFIG_DIR`, set to an absolute path, names the folder that holds `config.toml` in place of `<configuration directory>/owlshift`. The file is never committed, and it applies to every project run on this machine. Without it, every key takes its default. `owlshift init` does not write it. When the project's `stack.caches` needs `allow_caches`, `init` and `owlshift doctor` print the lines to add.

### `requires`

The Owlshift versions that can read this file: a semantic version requirement, such as `">=0.4"`. An Owlshift outside it refuses the file and says to upgrade. Default: none, every version reads it.

```toml
requires = ">=0.4"
```

### `concurrent_runs`

How many steps of `owlshift watch --drain` run at once on this machine, over every project: a whole number from 1 to 16. Each project's `policy.max_in_flight` is capped by it too. Default: `2`.

```toml
concurrent_runs = 2
```

### `takes`

Which admitted tickets `owlshift watch --drain` takes on this machine: `"mine"`, those whose admission gesture is the operator's (the account of the project's tracker key, or `identity.tracker` on the Markdown tracker), or `"all"`, every admitted ticket. `repositories` sets it for one repository. Default: `"mine"`.

```toml
takes = "mine"
```

### `keep_awake`

Whether the machine is kept awake while runs are in flight: `true` or `false`. Default: `true`.

```toml
keep_awake = true
```

### `web_tools`

Whether the read-only planning and review roles and the resolver (design, design review, verify, arbiter, resolver) get their web tools, WebFetch and WebSearch, on this machine: `true` or `false`. Their shell keeps the network either way. Intake, the answer check, Build and rebase never get them from this key. Default: `true`.

```toml
web_tools = false
```

### `allow_gate_env`

Names of variables of this machine's environment that a project may pass to its agents by declaring them in `stack.gate_env`, in any letter case. A project's declaration never widens this list: a name declared but not listed here is refused. A credential variable, a `GIT_` name, a variable Owlshift overrides, or one that makes the dynamic loader load code cannot be listed. `repositories` allows a name for one repository only. Default: none.

```toml
allow_gate_env = ["FEATURE_FLAGS"]
```

### `allow_caches`

Folders of your home, written from it with `/` between their parts, that a project may open read-only to its agents by declaring them in `stack.caches`, compared as written. A project's declaration never widens this list. A folder that is, lies in or holds a credential path cannot be listed. `repositories` allows a folder for one repository only. Default: none.

```toml
allow_caches = ["Library/pnpm"]
```

### `github_app_client_id`, `github_app_slug`

The client ID and the slug of the GitHub App `owlshift init` signs in through, in place of Owlshift's own, for a fork or a private app. The client ID is letters, digits, `.`, `_` and `-`. The slug is lowercase letters, digits and inner `-`, and makes the install link `https://github.com/apps/<slug>/installations/new`. Default: Owlshift's own app, built into the binary.

```toml
github_app_client_id = "Iv23..."
github_app_slug = "my-owlshift"
```

### `identity.tracker`, `identity.forge`

Your accounts on the tracker and on the forge. `identity.tracker`, your account as the tracker reports it, is read on the Markdown tracker only, whose tickets carry no key's account: it is the operator that `takes = "mine"` and the notifications go by, and `owlshift doctor` says when it is missing. No command reads `identity.forge` yet. Default: none.

```toml
[identity]
tracker = "maintainer"
forge = "octocat"
```

### `notifications.desktop`

Whether this machine shows a desktop notification when its operator has become the blocker: a question, a re-ask, a parked ticket, every harness at its usage limit; never progress. `true` or `false`. A machine with no desktop session shows nothing. Default: `true`.

```toml
[notifications]
desktop = true
```

### `service.digest`, `service.digest_hour`

The background service's daily notification, pointing to `owlshift status`. `digest` turns it on or off: `true` or `false`. `digest_hour` is its hour, 0 to 23, local time; a machine asleep at the hour sends it at wake. Default: `true` and `8`.

```toml
[service]
digest = true
digest_hour = 8
```

### `ui.port`

The port of 127.0.0.1 the background service binds for the local web UI: 1 to 65535. The service binds it once at its start, so a change applies at its next start. Default: `18466`.

```toml
[ui]
port = 18466
```

### `harnesses.<harness>.fallback`

The harness, `"claude"` or `"codex"`, that the review roles take while this one is at its usage limit. `<harness>` is `claude` or `codex`. Default: none.

```toml
[harnesses.claude]
fallback = "codex"
```

### `harnesses.<harness>.usage_cap_percent`

A usage cap, in percent from 1 to 100, that keeps part of a subscription for its owner. While the latest share seen of any usage window the harness's subscription reports is at or above it, and that window has not reset since, no new run starts on that harness, which counts as at its usage limit. A run already started finishes. A harness whose runs report no window is never held. Default: none, only the provider's limit applies.

```toml
[harnesses.claude]
usage_cap_percent = 80
```

### `harnesses.<harness>.budget_usd`

A dollar cap on each run of the harness, not on a day's total, whatever the billing. Owlshift passes it to Claude Code as `--max-budget-usd`, which counts a run's cost at list price on a subscription too and stops the run once its cost is above the cap, so a run can end above it. A run stopped by it alone is a failed run. Codex has no such option, so nothing enforces it there. Default: none.

```toml
[harnesses.claude]
budget_usd = 5.0
```

### `repositories.<repository>.allow_gate_env`, `repositories.<repository>.allow_caches`, `repositories.<repository>.takes`

Settings for one repository, keyed `github.com/<owner>/<name>` in any letter case. The repository of a project is read from its checkout's `origin` remote, which the committed project file cannot set. `allow_gate_env` and `allow_caches` add to the machine-wide lists, under the same rules. `takes` replaces the machine-wide `takes` for this repository. Default: none.

```toml
[repositories."github.com/acme/shop"]
allow_caches = ["Library/pnpm"]
takes = "all"
```
