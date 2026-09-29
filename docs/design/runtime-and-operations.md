# Owlshift — runtime & operations

Status: draft, 2026-09-27. Owlshift ships as one native binary that runs as a light background service on each machine, with a CLI and a local web UI on top; Docker is for servers and optional sandboxing only.

## How it runs

One process per machine, `owlshift daemon`, started by the operating system's service manager, serves every adopted project; it runs no model itself.

**Every cycle** (about 60 seconds, configurable):

1. One query per tracker for tickets changed since the last cursor, one per forge for PR and check changes.
2. A comparison with the local store: an answer posted, a ticket ready, a blocker cleared, a PR merged or red.
3. Nothing to do: it waits for the next cycle, using no CPU and zero tokens. Its memory footprint is expected in the tens of megabytes, to be measured.

**When there is work:**

1. It creates a worktree and writes the brief.
2. It spawns the harness CLI (`claude -p …` or `codex exec …`) as a child process, inside a process group on macOS and Linux and a Job Object on Windows, so the whole process tree can be stopped. The mechanism is `owlshift_platform::process::ProcessTree`, which the doctor's probes already use at their deadline (OWL-31). On Windows the child is created suspended, placed in the job, then resumed, so it cannot start a process outside the job: stable Rust gives no other way, since `ChildExt::main_thread_handle` and the proc-thread attribute list are unstable (checked on rustc 1.98.1, 2026-09-28). On native Windows a confined agent command is refused, native confinement being set aside (D9, [build plan](build-plan.md#results)), so there the Job Object holds the doctor's probes. On Unix a descendant that calls `setsid` or `setpgid` leaves the group and is not stopped. Being in a group of its own also puts the child out of reach of the terminal's Ctrl-C, so the `owlshift` CLI calls `process::stop_trees_on_signal` (OWL-43): on SIGINT, SIGQUIT, SIGTERM or SIGHUP it stops every live tree, then ends as the signal would have ended it. On Windows the child shares the console and gets Ctrl-C itself, which is not enough. This was checked on windows-latest on 2026-09-29 (OWL-47, [run 36602854369](https://github.com/pitchopp/owlshift/actions/runs/36602854369), the test at commit 971ce22): `owlshift doctor` got a console Ctrl-C while its git probe hung with a child, and ended 21 ms later with `STATUS_CONTROL_C_EXIT`. A probe that kept Ctrl-C's default action died of it within 21 ms, with its child. A probe that ignored Ctrl-C, and the child it started, which inherits that, were still running 5 s later in all seven runs. So on Windows `stop_trees_on_signal` installs a console handler: on Ctrl-C or Ctrl-Break it terminates every live tree's Job Object, then ends the process as Ctrl-C does. Ctrl-Break takes the same path, not checked live; closing the console is left to the system, which ends every process attached to it.

   A hard kill of Owlshift itself (SIGKILL, a crash, the system ending it for want of memory) runs no handler. Decided on 2026-09-29 (OWL-86): on Linux and macOS the CLI also calls `process::stop_trees_when_killed`, first thing in `main`. It starts a sentinel, `/bin/sh` running a short loop in a process group of its own, and tells it, through a pipe only Owlshift holds, of each tree as it becomes live and as its handle is dropped or the signal handler stops it. When Owlshift ends, however it ends, the system closes the pipe, and the sentinel kills the process group of every tree still live, as `ProcessTree::kill` does. So a tree still live when Owlshift ends is stopped whatever the end, a normal exit included; a tree whose handle was dropped is left running, as `ProcessTree` promises. The sentinel is a best effort, not a guardrail: if it cannot start, Owlshift prints a warning and runs on without it; once it is killed, or stops reading, it protects nothing more, and Owlshift never waits on it; a kill in the gap between a tree's start and the moment the sentinel is told of it, well under a millisecond, and a descendant that left the tree's group escape it. Checked on 2026-09-29: the loop, run under dash and under macOS's `/bin/sh` with an empty environment, killed an announced group and spared a withdrawn one (macOS 26.6); a SIGKILL on the process group `owlshift doctor` leads, while its probe hung with a child, left both running before the change (macOS 26.6), and with the sentinel both were gone within 5 s of Owlshift's end, the bound the test checks, on macOS 26.6 and on ubuntu-latest (dash as `/bin/sh`) and macos-latest ([run 36620251554](https://github.com/pitchopp/owlshift/actions/runs/36620251554), the tests at commit afb304e), where a hard kill of a process owning two trees also stopped the live one and spared the one whose handle was dropped. Rejected: Linux's `PR_SET_PDEATHSIG`, which reaches only the root, not the processes it starts, and fires when the thread that spawned the root ends (per prctl(2), not checked live); a subreaper, which adopts orphans while it lives and does nothing once it is killed. Both are Linux only, where one sentinel serves both systems. Native Windows is out of scope for this guarantee: a hard kill of `owlshift.exe` leaves its trees running there.

   A sentinel that is gone is reported, not replaced. Decided on 2026-09-30 (OWL-88): Owlshift keeps the sentinel's process handle and reads, without waiting, whether it still runs (`process::sentinel_status`: running, ended and how, or never started); one that ended is reaped then. `owlshift doctor` reports its own sentinel on a `sentinel` line, as a warning when it ended or could not start, never as a failure, and `owlshift do` prints a warning at the end of a run during which it ended; a start failure is already warned of when the command begins. Neither refuses to work, since the sentinel is a best effort, not a guardrail. It is not restarted: on macOS a new pipe is made close-on-exec only after it is created, so a process another thread starts at that moment, not every one of which goes through the live trees' lock, could inherit its write end and hide Owlshift's end from the new sentinel; and whatever killed it, a person or a clean-up tool, would be fought without anyone knowing. A sentinel that is stopped but not ended is not seen: it still runs, and the writes to it, non-blocking, start failing only once its pipe is full. Checked on 2026-09-30 on macOS 26.6.2 with rustc 1.98.1, on a `/bin/sh` loop reading a non-blocking pipe in a group of its own: a non-blocking wait saw it running, and after a SIGKILL saw it ended by signal 9; a write to its pipe then failed with EPIPE, and the writer lived on, since Rust ignores SIGPIPE before `main`; stopped with SIGSTOP, it was still seen running, and the writes failed with EAGAIN after 69,608 bytes. The test that kills the sentinel of a running `owlshift doctor` and finds the warning, with doctor ending normally after writing to the dead sentinel's pipe, runs on ubuntu-latest and macos-latest.
3. It streams the harness output to a per-run log file and waits.
4. It validates `result.json`, checks isolation, and hands the result to the Writer.

**Harness authentication is the CLI's own.** The child process inherits the user's normal CLI configuration, so a run consumes the user's own Claude or ChatGPT subscription, exactly as if they had typed the command. Owlshift never asks for, stores or passes a model API key; a user who prefers API billing configures it in the CLI itself, and Owlshift does not need to know.

**Around it:**

- **Single instance.** A lock allows one daemon per machine; a newer binary asks the running one to drain and exit.
- **Control channel.** A Unix socket (macOS, Linux) or a named pipe (Windows) serves the CLI, the web UI and later the app.
- **Usage limits.** When a CLI reports that the subscription's usage limit is reached, the daemon records the reset time it reports, pauses that harness until then, and routes roles to their fallback. The interrupted run resumes from its checkpoint after the reset.
- **Sleep.** A sleeping machine freezes the daemon; on wake it catches up. A run cut off mid-way loses nothing: its lease expires and the ticket resumes from the last pushed checkpoint.
- **Keep awake, optional.** While a run is active, a power assertion (the mechanism behind `caffeinate` on macOS) prevents idle sleep; released when runs end.
- **Webhooks** are an optional speed-up on a server with a public address; polling always works.

## Platforms

Native binary on every developer machine; Windows goes through WSL2 first; Docker is for servers and optional sandboxing, never the desktop default.

| Platform | Support | Background service | Notes |
| --- | --- | --- | --- |
| macOS | First class from P0 | A user LaunchAgent, label `dev.owlshift.daemon` | Minimum macOS version to set (D12) |
| Linux | First class from P0: CI and servers | A systemd unit (user on desktops, system on servers) | Also the Docker image base |
| Windows | Through WSL2 from P1, running as Linux | systemd inside WSL, or a logon task that starts it | Native Windows later, on demand |

**Why WSL2 first on Windows.** The risk is not Owlshift's own code but what it launches: project tooling (Makefiles, bash scripts) rarely runs on native Windows, and native support of each harness CLI and its sandbox must be checked one by one. Native Windows adds Job Objects for process trees, paths over 260 characters in worktrees, files locked while open, and antivirus slowing git.

**Why not Docker on desktops.**

- Docker Desktop runs a permanent Linux VM that reserves gigabytes of memory, against tens of megabytes for the daemon, and must be running for Owlshift to work.
- Worktrees, git operations and dependency installs are markedly slower through the VM's shared folders.
- Harness CLIs are logged in to the user's subscription on the host (on macOS, in the Keychain); a container would need copied tokens.
- Many projects already use Docker: an agent in a container would need Docker-in-Docker or the host's Docker socket, which is root-equivalent access.
- No notifications, tray icon, keep-awake or real browser from a container.
- Docker Desktop requires a paid subscription in companies above 250 employees or USD 10 million revenue, a brake for an open-source tool.

**Where Docker fits.**

1. **Server mode:** an official image and a compose file for a Linux server, where there is no VM overhead. The harness CLIs are logged in inside the server once, as on any machine.
2. **Optional sandbox per project:** each run in a disposable container, a layer of defence on top of the harnesses' own sandboxes, worth it for public repositories.

## Install, lifecycle & uninstall

Four commands cover the whole life of an install, and uninstalling removes exactly what was installed, because background services that outlive their binary are the classic failure of this kind of tool.

```bash
brew install owlshift        # or cargo install, or the Linux install script
owlshift init                # in each repository to adopt
owlshift start --at-login    # plain `start` runs for this session only
owlshift uninstall --purge   # --purge also removes history and worktrees
```

Prerequisites: git, and at least one harness CLI installed and logged in (`claude`, `codex`). Owlshift reuses that login, so the user's own subscription.

**Against lingering services:**

1. **Start at login is opt-in.** `start` alone runs for the session; `start --at-login` installs the service and says so.
2. **An install manifest** lists every service file, data directory, worktree and keychain entry Owlshift creates; `uninstall` removes exactly that list. `--purge` never touches repositories or tickets.
3. **A self-healing launcher.** The service starts a thin launcher that first checks the binary still exists; if not, it removes its own service definition and exits instead of failing in a loop.
4. **Visible and findable.** A stable label (`launchctl list | grep owlshift`), shown in macOS Login Items; `owlshift doctor` lists any leftover and offers to clean it.
5. **One instance.** A lock prevents two daemons, including an old and a new version, from running at once.

**Where files live.** Platform-standard directories: Application Support on macOS, the XDG directories on Linux. The personal configuration file is `owlshift/config.toml` in the user's configuration directory: `~/Library/Application Support` on macOS, `$XDG_CONFIG_HOME` or `~/.config` on Linux, `%APPDATA%` on Windows (OWL-12). `OWLSHIFT_CONFIG_DIR`, when set to a non-empty absolute path, overrides that directory: `<value>/config.toml` is read instead of the platform default, replacing `<config_dir>/owlshift` rather than the platform's parent config directory. A relative or empty value is treated as unset. This exists so tests and tooling can redirect the personal configuration file deterministically on every platform: on Windows, the `dirs` crate resolves the configuration directory through the OS known-folder API, which no other environment variable redirects, so `OWLSHIFT_CONFIG_DIR` is the only way to isolate a test from a real personal file there (OWL-30). Worktrees live under Owlshift's data directory, one per project and ticket, never inside the user's checkout. The data directory is `owlshift` in the user's local data directory: `~/Library/Application Support` on macOS, `$XDG_DATA_HOME` or `~/.local/share` on Linux, `%LOCALAPPDATA%` on Windows. `OWLSHIFT_DATA_DIR`, set to an absolute path, replaces it, by the same rule as `OWLSHIFT_CONFIG_DIR`. It holds, per project, the dedicated clone the worktrees hang off, the worktrees and the run logs, and, until the local store exists, the event log `events.jsonl` (OWL-20, [build plan](build-plan.md#cli-surface-for-p0-and-p1)). Tracker and forge secrets live in the system keychain: macOS Keychain, Secret Service on Linux, Credential Manager on Windows. Model credentials stay with each harness CLI.

**Deadlines of the runner's git.** Every git command the runner runs for itself stops, with its whole process tree, after 120 s (`GIT_TIMEOUT`), except the first clone of a project: `owlshift do` clones the project into the data directory once, and a large repository can take longer than 120 s to arrive, so that one command has its own deadline of 30 minutes (`CLONE_TIMEOUT`, OWL-60), still stopping the whole tree. A clone that hits it fails with the deadline in the message and its partial folder is removed, retried for up to 10 s because a stopped git's files can stay locked for a moment on Windows. The clone is also marked `unfinished-clone` in the project's directory until it succeeds, so a partial folder that could not be removed, or that a crash or a Ctrl-C left, is removed by the next `owlshift do` before it clones anew, and never taken for the project's checkout; if it still cannot be removed, `do` refuses and names the folder (OWL-80). The fetch of a later `do` keeps the 120 s: it is incremental, it runs before every ticket, and a stalled network should stop it fast; if a real project's fetch proves to need more, it is a one-line change to give it the clone's deadline.

With the Tauri app (P11), start at login should go through the operating system's app login-item mechanism, so deleting the app removes it; to verify when the app is built.

## Updates & versions

One update path per install method, a restart that never loses work, and a version number on every format that outlives a binary.

**The binary.**

- Installed by a package manager: `brew upgrade owlshift`. Installed by hand: `owlshift self-update`, which downloads a signed release and verifies its checksum. `self-update` refuses on a package-managed install and points to the package manager, so the two never fight.
- The service points to a stable path (`/opt/homebrew/bin/owlshift`), never to a versioned directory, so an upgrade cannot break it.
- Graceful restart: stop dispatching, let running roles finish or checkpoint, exit; the service manager starts the new binary. Safe because state lives in the tracker, git and SQLite.
- Rollback: the previous version stays installable (`brew install owlshift@0.4`), and the local store is backed up before each migration.
- Semantic versioning; releases are tagged, signed and carry a changelog.

**Formats that outlive a binary.**

| Format | Protection |
| --- | --- |
| Project file | `requires = ">=0.4"`: a teammate with an older binary gets a clear error |
| Local store (SQLite) | Migrated at start, backed up first |
| Claims, artifacts, marked comments | Carry a format version; a runner refuses a ticket written by a newer incompatible version and says "upgrade" |
| Brief and `result.json` | Versioned contract between the runner and every role prompt |

**Harness CLIs update themselves, silently.** A flag can disappear between two days (`codex review` stopped accepting `-m`), and so can the wording of a usage-limit message. So every run records the harness version; `owlshift doctor` flags a version never tested; a nightly CI job runs the harness contract tests against the latest CLI releases, so a breaking change is caught upstream of users.

## Configuration

Configuration is plain text in two files plus the keychain; every screen that edits it later writes to those same files, which stay the reviewable record.

**`owlshift init`** detects the stack (`package.json`, `pyproject.toml`, `Makefile`, `Cargo.toml`…), connects the tracker (OAuth where offered, else a token), stores tracker and forge secrets in the system keychain, writes a commented project file, and registers the project with the daemon. It checks that the harness CLIs are logged in; it never asks for a model API key. In P1 (OWL-20), `init` writes the commented project file from its flags and stores the tracker and forge secrets, asking for them on a terminal only. Stack detection (P9), a check that the tracker answers and the registration with the daemon (P7) come later, and the harness logins are `owlshift doctor`'s to check. Before the first `owlshift do`, the operator also makes, once, the Claude Code login that agent runs use: agent runs are confined and cannot reach the Keychain, so they use a second login of the same account, made with `claude auth login` with `CLAUDE_CONFIG_DIR` naming `agent-login/claude` beside the personal file (inside the sandbox on macOS; OWL-41). `owlshift do` refuses to start without it, before it clones anything, and prints the exact command, as `owlshift doctor` does.

| Layer | Where | Holds |
| --- | --- | --- |
| Floor | Built into the binary | What no configuration can loosen ([architecture](architecture.md), section 8) |
| Project file `owlshift.toml` | Committed in the repository | Tracker and state mapping, admission gesture, gate commands, resources and zones, pipeline and plan approval, role prompt overrides in `.owlshift/roles/`, tiers mapped to models, policy additions, caps |
| Personal file | The user's config directory, never committed | Identity on tracker and forge, installed harnesses and their fallbacks, concurrent runs, usage caps, an optional dollar budget for API-billed harnesses, keep-awake, notifications |
| Ticket labels | The tracker | One ticket's variant, tier, or exclusion |

Precedence: the floor wins, then the project, then the personal file, which may lower caps and budgets but never loosen project policy. `owlshift config show` prints the effective value of every key and the file it comes from.

```toml
# owlshift.toml
requires = ">=0.1"

[tracker]
kind = "linear"
team = "LOC"
admit = { label = "agent" }
states = { ready = "Todo", working = "In Progress", needs_input = "Needs Input", review = "In Review" }

[stack]
gate = ["make lint", "make test"]
resources = { migrations = "backend/*/migrations/**" }

[pipeline]
default = "standard"
plan_approval = "on-fork"   # always | on-fork | never

[models]
deep     = { claude = "claude-opus-5-5" }
standard = { claude = "claude-sonnet-5", codex = "gpt-5.6-sol" }
fast     = { claude = "claude-haiku-4-5" }

[policy]
always_human = ["billing", "auth"]   # adds to the floor, never removes
```

## Observability & the local web UI

Every action is a recorded event, readable from the CLI from P1 and from a local web UI from P8; the tray app of P11 wraps that same UI instead of rebuilding it.

**Events and logs.** Each scan, decision, dispatch, run start and end, usage, gate and tracker write is a structured event in the local store. Each run's full harness output is captured to its own log file. On the ticket, comments give the human-readable trace. Until the local store exists, the events of `owlshift do` go to `events.jsonl` in the data directory, one JSON line each, and `owlshift logs` reads that file. It shows each run's log directory, not the logs themselves (OWL-20).

| Command | Answers |
| --- | --- |
| `owlshift status` | What runs, what waits for me, usage today and when each harness's limit resets |
| `owlshift logs --follow` | The live event stream |
| `owlshift logs PROJ-123` | Everything about one ticket, runs included |
| `owlshift why PROJ-123` | Why a ticket is not starting: a blocker, a held zone, a cap, a harness at its limit |
| `owlshift pause`, `resume` | Stop or restart dispatch; running roles finish |
| `owlshift retry`, `cancel` | Act on one run |
| `owlshift doctor` | Capabilities, harness versions and logins, leftovers |

`why` is the most useful of them: a scheduler that cannot explain a wait looks broken.

**The local web UI (P8)** is served by the daemon on `127.0.0.1` only, protected by a per-install token so no website open in the browser can drive it. It is identical on every platform and reachable on a server through an SSH tunnel.

- **Overview:** tickets ready, running, waiting for a human, in review, and what blocks what.
- **Ticket:** the stage timeline, each run with its live log, usage and result, the questions and answers.
- **Orchestrator:** scans, decisions, errors, adapter health (tracker reachable, harness logged in, usage limit reached), usage per harness.
- **Config:** the effective configuration with the origin of each value and validation errors; an edit writes to the file.
- **Controls:** pause, resume, retry or cancel a run, release a claim.

**The tray app (P11)** is a Tauri shell around the same UI, adding the menu-bar icon, native notifications and the login item.

**Telemetry:** none sent anywhere. An OpenTelemetry export can be switched on by a team for its own collector.

## Testing strategy

Almost everything is tested without network or tokens; the parts that touch the operating system or a live service are thin and tested on every platform.

| Layer | How | When |
| --- | --- | --- |
| Core logic: scheduler, state machine, policy, answer routing | Unit tests on pure functions | Every commit |
| Scenarios S1 to S17 | End to end on the Markdown tracker, a local bare git remote and a **fake harness** (a small test program that writes a prepared `result.json`, with optional delays, failures and usage-limit messages; see the [build plan](build-plan.md#cli-surface-for-p0-and-p1)) | Every commit, in seconds |
| Tracker and forge adapters | Recorded HTTP fixtures, plus a conformance suite every adapter must pass | Every commit |
| Live services | A sandbox tracker workspace and a test GitHub repository | Nightly |
| Harness CLIs | Contract tests with a tiny real prompt against the latest CLI releases, on the maintainer's subscription | Nightly |
| Operating-system integration | Service install and uninstall leave nothing behind (checked against the manifest); a process tree is fully stopped; an expired lease is taken over | Every commit, on macOS, Linux and Windows runners |
| Real use | Owlshift's own backlog from P1, Locary's from P5 | Continuous |

The fake harness is the keystone: it makes the needs-input loop, re-asks, crashes and usage-limit fallbacks reproducible without consuming a subscription.

## Prior art

Owlshift combines three well-established patterns rather than inventing one: poll far and run locally like a CI runner, serve a local web UI like Syncthing, and pair a daemon with a CLI and a tray app like Tailscale or Ollama.

| Tool | What it shares with Owlshift |
| --- | --- |
| GitHub Actions self-hosted runner | Polls a remote service and runs jobs locally; `svc.sh install` and `svc.sh uninstall` manage a launchd, systemd or Windows service |
| GitLab Runner | The same model: `gitlab-runner install`, `start`, `stop`, `uninstall` |
| Tailscale | A daemon (`tailscaled`), a CLI talking to it through a local API, a menu-bar app |
| Ollama | A local server on `localhost:11434`, a CLI over HTTP, a menu-bar app that starts it |
| Syncthing | One Go binary, a web UI on `localhost:8384` protected by an API key, tray apps wrapping that UI |
| Vibe Kanban, Sortie | Category peers: a local web UI; a single binary with SQLite |
| GitButler | A Rust and Tauri desktop developer tool |

The known weak spot of the pattern, services left behind after uninstall, shows up with `brew services` and with a runner folder deleted before `svc.sh uninstall`; the lifecycle section above is designed against it. These descriptions come from each tool's public documentation and were not re-checked for this draft.
