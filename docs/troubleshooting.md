# Troubleshooting

Find what you see below, run the command it names, apply the fix. The quoted lines are the ones Owlshift prints or posts; `<…>` stands for a value that differs on your machine.

- [`owlshift doctor` ends with `✗ Not ready`](#owlshift-doctor-ends-with--not-ready)
- [The gate passes on your laptop and fails under `owlshift gate`](#the-gate-passes-on-your-laptop-and-fails-under-owlshift-gate)
- [A ticket does not start](#a-ticket-does-not-start)
- [A ticket is parked](#a-ticket-is-parked)
- [A usage limit stopped a run](#a-usage-limit-stopped-a-run)
- [A project is quarantined or paused](#a-project-is-quarantined-or-paused)
- [Where the logs are](#where-the-logs-are)

## `owlshift doctor` ends with `✗ Not ready`

**What you see.** Each line of the report is marked `✓` (fine), `!` (a warning, which never blocks `owlshift do`) or `✗` (a problem); `·` is information. A report with a `✗` ends with:

```text
✗ Not ready: 1 problem to fix before `owlshift do`
```

and then, for each `✗` line, a block with the reason (`Why`) and the steps (`Fix`). **The command that says why:** `owlshift doctor`; run it again after each fix until it ends with ``✓ Ready: nothing blocks `owlshift do`.`` The `✗` lines you meet first:

**The `sandbox` line, on Linux.** Every agent run is confined with `bwrap`, and Owlshift starts none without it.

- `bwrap is not installed`: install bubblewrap, for example `sudo apt install bubblewrap`, or your distribution's package.
- `bwrap cannot confine a trial run: <reason>`: the system refuses `bwrap` a user namespace. The fix block gives the command to run for the error (`bwrap --unshare-pid --ro-bind / / --dev /dev --proc /proc -- true`), and on Ubuntu 23.10 and later two commands that allow user namespaces for `bwrap` alone through an AppArmor profile: copy them from the block.
- `this system has no sandbox Owlshift can use (sandbox-exec on macOS, bwrap on Linux)`: native Windows. Run Owlshift under WSL2, where it uses `bwrap`.

On macOS the line is `agent runs are confined with sandbox-exec`; `sandbox-exec cannot confine a trial run: <reason>` is fixed by looking for what keeps it from running, such as a security tool or a device management profile; `/usr/bin/sandbox-exec is missing` is fixed by updating or reinstalling macOS.

**The `secrets` line, on Linux and WSL2.** Owlshift keeps its secrets in a Secret Service such as GNOME Keyring, as macOS keeps them in the Keychain. A server or WSL2 without a desktop session runs none. The line reads `not checked: could not read the system keychain: <reason>`, and the block ends with:

```text
Fix   1) Make the system keychain available: unlock it on macOS; on Linux,
         start a Secret Service such as GNOME Keyring
      2) owlshift doctor    (to check)
```

The `claude agent login` line then fails too, with ``could not tell whether a token for agent runs is stored in the system keychain (service `owlshift`, account `claude-agent`): <error>``: `owlshift init` cannot store the token without a Secret Service. Start one in your session, and unlock it, then run `owlshift doctor` again.

**The `git` line.** It reads `<version> (<path>), older than 2.39.0`. Install git 2.39.0 or later (https://git-scm.com/downloads), then check `git --version`: an older git may come first on the `PATH`.

**The `claude agent login` line.** It reads ``no token for agent runs in the system keychain (service `owlshift`, account `claude-agent`)``. Agent runs are confined and cannot reach the Keychain where your own Claude Code login lives, so they log in with a token of your subscription that Owlshift keeps. The fix block gives the two steps:

```bash
claude setup-token
owlshift init
```

`claude setup-token` prints a token; `owlshift init` asks for it and stores it. Your own `claude` login being out (`your own login: not logged in`) is information only: agent runs do not use it.

A `claude` line reading `not found on the PATH` means Claude Code is not installed: the fix block gives the install step.

## The gate passes on your laptop and fails under `owlshift gate`

**What you see.** `owlshift gate` ends with a line such as:

```text
Gate red: `<command>`: <reason>.
Gate log: <path>
```

while the same commands pass in your terminal. **The command that says why:** the `Gate log` line names the file that holds the whole output of the run; read it from the failing command up. `owlshift gate` runs the gate the way it runs on an agent's work: in a fresh worktree of your default branch, confined, with your home folder closed and nothing of your environment but what the project declares and your personal file allows. Four causes cover almost every case.

**A tool is missing.** The log says the command is not found. The gate sees the `PATH` of the shell that ran `owlshift gate`, and, for the background service, the `PATH` that `owlshift start --at-login` ran with. A folder of your home that the `PATH` names is opened to the run, read-only, unless it holds or lies in a folder the sandbox keeps closed (credentials, the temporary folders, a folder as wide as `~/.config`). A version manager whose shims read other folders of your home needs those named too: declare the variable that names them in `stack.gate_env`, and allow it with `allow_gate_env`. A `PATH` that changed after `start --at-login` is taken by running it again.

**A test reads your home folder.** The sandbox closes your home folder to every setup, gate and agent run. A tool that reads or writes there fails with its own message: npm's `EPERM` and "run `sudo chown`", `Operation not permitted`, `Read-only file system`, or on Linux `No such file or directory`. When the failing command's output names a folder of your home, `owlshift gate` says so after the gate's result and gives the two lines that open it read-only:

```text
The sandbox closes your home to every run and gate. The failing command was refused:
  ~/.cache/demo
A folder of your home opens to runs, read-only, once the project declares it and this machine allows it:
  in ~/src/web/owlshift.toml, under [stack]:
    caches = [".cache/demo"]
  in ~/Library/Application Support/owlshift/config.toml:
    [repositories."github.com/acme/web"]
    allow_caches = [".cache/demo"]
```

At a terminal it asks whether to add them; answer `y` and it writes both files, your personal file first, then commit `owlshift.toml`. Off a terminal it only prints them. Every run of the project can then read that folder: allow caches, never secrets. A folder that holds credentials (`~/.ssh`, `~/.config/gh` and the like) is never opened, whatever the configuration. Read-only, a folder still refuses writes, so keep the tool from writing there, fill it outside the sandbox, give the tool a cache its runs write in `stack.run_caches` (the variable it reads for its cache and a folder's name, such as `CARGO_HOME = "cargo"`), or point the tool's writable folder into `$TMPDIR`, the run's own folder. On Linux a write to a new path under the home does not fail: it lands in a throwaway home that the run loses. A Build whose setup fails the same way names the folders at the end of its reason, and `owlshift init` offers the line for each cache the project file declares that your machine does not allow yet.

**npm fails with `EPERM`.** npm says your cache folder "contains root-owned files" and suggests `sudo chown`. Under Owlshift that message means the sandbox, or a package missing from the cache: `sudo` changes nothing.

- Give npm a cache its runs write: `owlshift init` writes the setup `npm ci` and `run_caches = { npm_config_cache = "npm" }` under `[stack]`, and the setup then fetches what the cache lacks.
- A project that still reads `~/.npm` through `stack.caches` cannot write a missing package into that read-only cache: replace it with the `run_caches` line above, or run `npm ci` once in your checkout so that `~/.npm` holds the project's packages, and again whenever the lock file gains one.

**Dependencies were never installed.** The gate runs in a fresh worktree, which holds no `node_modules` or virtual environment. List the install commands under `stack.setup` in `owlshift.toml`, from a frozen lock file, and the folders they read under `stack.caches`. A variable of your environment that the gate needs, such as a feature flag, goes in `stack.gate_env`, and your personal file allows it with `allow_gate_env`: credential variables are refused. `owlshift gate` reads the project file at origin's default branch: push your change to a branch and check it with `owlshift gate --at origin/<branch>` before merging it.

If a Build's gate fails the same way on the default branch, `owlshift why` says so (`base <commit>: fails its <step> command ...`) and, once it is fixed, `owlshift gate --at <commit>` checks it again.

Owlshift also files one repair ticket for that break, "Repair the base: ...", urgent, assigned to you and already admitted: the drain runs it before any other ticket, even with `policy.max_in_flight` reached, and its merge lets the held Builds go on. When the fix is outside the repository, on your machine, the ticket asks you what to do there. If its merge leaves the default branch failing the same command, Owlshift holds that commit too and files one more repair, which names the first; if that one does not fix it either, you are told that Owlshift files no more for this break, and it waits for your fix. Set `policy.base_repair = false` in `owlshift.toml` to be told only. A tracker that refuses the ticket leaves a `repair_unfiled` warning in the log.

## A ticket does not start

**What you see.** You put the `agent` label on a ticket, or ran `owlshift start`, and nothing runs. **The commands that say why:**

```bash
owlshift status
owlshift why
owlshift why ENG-12
```

`owlshift status` says what the service does; `owlshift why` says, for each admitted ticket, what it waits for, and `owlshift why ENG-12` gives that ticket's reasons over time, ending with `now: <reason>`. Both only read, and work while the service runs. The reasons, as printed:

- **The service reads no project file.** `owlshift status` shows `not served: origin/main (<commit>) holds no owlshift.toml`, or `not served: no project file was read from its default branch yet`. The service reads the project file of your default branch, never your checkout's: push `owlshift.toml` first. It tries again every minute. The `admitters` are read the same way: a list only in your local file admits nothing.
- **Not admitted.** ``its admission gesture is not from an account `[tracker].admitters` lists``: or `its admission gesture does not admit it: <why>`. Only an `admitter` can admit a ticket, by the `agent` label in its ready state; when `<why>` says the title or description was edited after the label, remove the label and put it on again. `admitted by <who>: their runner's` means another person's machine takes it, and `it carries the exclusion label` means a person kept it out.
- **No decider.** ``it has no decider: give it an assignee, or a `zone:` label whose zone has an owner``.
- **Intake has not run.** `its intake has not run yet`, or `its text changed since its intake, which runs again`: it starts once intake has run.
- **Blocked.** `blocked by <tickets>`; `its blocker <ticket> was canceled: a person removes the relation or the line`; or `its "Blocked by" line names <ticket>, which the tracker does not know`.
- **A resource is held.** `<ticket> holds <resource>, which it would touch`, or `its Build waits: <ticket> holds <resource>, which its Build would take`: the ticket goes on once the holder's pull request is merged or closed. `the project's slot is held by <ticket>` and `every run slot of this machine is taken` say the same for the project and the machine, and `the open pull requests reached their cap (<n> of <cap>)` waits for a merge.
- **Being worked, or waiting for a person.** `it is being worked, waits for an answer, is parked, or its pull request is followed`: see [A ticket is parked](#a-ticket-is-parked), or answer the questions on the ticket.
- **Another runner holds it.** Under `claims = "remote"`, `claimed by <operator>'s runner until <time>: theirs to work`: a colleague's machine, or another of yours, works the ticket, and `owlshift status` lists every claim it knows. Once that claim has expired, a person the project's `[people]` lists releases it, and the claim's operator may at any time, from any of their machines:

  ```bash
  owlshift release ENG-12
  ```

  The release posts a `[owlshift] RELEASED` comment on the ticket saying how to have it worked again: the person whose runner should take it admits it again, after the branch the earlier runner pushed is closed and deleted. A live claim of another person's is refused; ask them.
- **A pause, a quarantine or a usage limit.** See the sections below: `owlshift why` prints ``the project is quarantined (`owlshift unquarantine`)``, and `<harness> is at its usage limit until <time>`.
- **A dark wake, on macOS.** The service prints `the machine is in a dark wake (macOS keeps it up without graphics): no ticket is dispatched and no step starts; ...`. Nothing is wrong: it lifts at the next full wake, and `the machine is in a full wake again` follows in the log.

## A ticket is parked

**What you see.** The ticket moves to `Parked` (or `Needs Input`), and Owlshift comments on it with a title `[owlshift] PARKED`: `Parked: <reason>.`, what explains it (`The last failure`, `The run said` or `The forge said`) and how to restart it. The reasons it can give:

- `a run failed again, with no attempt left`
- `the answers stayed incomplete`
- `the run is blocked`
- `the run broke isolation and is quarantined`
- `the pull request stayed red after its fix passes`
- `the pull request reached its limit of repairs`
- `the forge blocks the pull request`
- `the ticket's runs reached its budget`
- `its branch could not be rebased off a broken base onto one that passes`
- `its work was stopped by a cancel`

**The command that says why:** the comment itself, then `owlshift why ENG-12` and `owlshift logs ENG-12`. **The fix:** fix what the comment names, answer the questions it lists if any, then restart the ticket in either of two ways, as the comment says: reply `continue` on the ticket, in a comment of its own, as its decider, or run:

```bash
owlshift continue ENG-12
```

A parked ticket restarts at the stage it parked at; an answer alone does not restart it. When the comment says ``**To restart it:** run `owlshift do ENG-12` to run it again.``, nothing was kept and the ticket runs again from the start. A ticket parked at its budget counts its runs afresh after the restart; raise `policy.ticket_budget_usd` in `owlshift.toml` to raise every ticket's. For a park by isolation breach, see [A project is quarantined or paused](#a-project-is-quarantined-or-paused). Where the service serves the project, `owlshift continue ENG-12 --to-watch` restarts a parked ticket and returns without running it: the service drives it from its next pass.

## A usage limit stopped a run

**What you see.** At a terminal, `owlshift do` ends with one of:

```text
Stopped: the harness reached its usage limit, which resets at <time>; run again then.
Stopped: the harness reached its usage limit; run again once it resets.
```

Under the service, a desktop notification says `<ticket>: Claude Code is at its usage limit until <time>: its runs wait until then` (or `... and gave no reset time: its runs wait, and one tries it again at <time>`), and `daemon.log` says `Claude Code is at its usage limit until <time>: no step that runs it starts before then`. **The command that says why:** `owlshift why` prints `Claude Code: at its usage limit until <time>` for each limited harness, or `no harness is limited`; `owlshift status` shows each harness's usage and reset.

**The fix.** A usage limit is not a failure and parks nothing: the ticket keeps its stage, nothing counts against it, and the limit holds only the runs of its own harness, on the whole machine. Under the service it resumes by itself after the reset. After a foreground run, run `owlshift continue ENG-12` once the limit resets. Where the provider gave no reset time, one run tries the harness again at the deadline `status` names. To keep a margin of your subscription for yourself, `usage_cap_percent` under a harness in your personal file counts that harness as at its limit while a usage window it reports is at or above that share.

## A project is quarantined or paused

**What you see.** `owlshift status` shows, under the project, one or both of:

```text
Paused: every project of this machine, by owlshift pause at <time>: <reason>. Nothing is dispatched and no step starts; `owlshift resume` lifts it.
  paused by owlshift pause at <time>: <reason>; `owlshift resume <owner/repo>` lifts it
  quarantined: <text>; `owlshift unquarantine`, in its checkout, shows it whole and clears it
```

and `owlshift why` prints `quarantine: a marker stands; every command refuses the project`.

**Paused.** A pause is something you or the service asked for: while it stands, no ticket is dispatched and no step starts, nor a merge on green; the steps under way finish, and answers, pull requests and stages are still read. Lift it with:

```bash
owlshift resume
owlshift resume acme/web
```

`owlshift resume` lifts the machine's pause, the one `owlshift pause` with no project sets; `owlshift resume acme/web` lifts that project's own. Each lifts only its own: run the one that `status` names. The service pauses a project itself when its watch cannot start (`; the daemon tries it again at each of its passes`), and `owlshift status` then says why.

**Quarantined.** A run whose isolation check did not pass leaves a marker, and every command refuses the project until a person has looked at what the run changed. A desktop notification says `<project> is quarantined: run <run> of <ticket> broke isolation (<what>). No ticket of the project runs until a person clears it with owlshift unquarantine`, or `<project> is quarantined: a run's isolation check did not pass or did not finish. ...`; without a desktop session, `owlshift status` and `owlshift doctor` show it. Each held ticket gets a comment (`Your answer counts, but Owlshift continues no ticket of this project for now`, `This ticket is ready, but Owlshift builds no ticket of this project for now`, or `This ticket's work has started, but Owlshift runs no ticket of this project for now`) that says it goes on by itself once the quarantine is cleared. In the project's repository, on the machine that ran it:

```bash
owlshift unquarantine
```

It shows what the marker says, and clears it only once you type the project's name (`owner/repo`) at a terminal: it refuses to run in a script. It answers `Cleared: the quarantine is gone, and the next command or watch pass works the project again.`, or `Kept: the quarantine stands, and Owlshift still refuses the project.` when the name was not typed. A ticket parked by the breach restarts as [A ticket is parked](#a-ticket-is-parked) says. Check what the run changed first: that is the point of the marker, and [SECURITY.md](../SECURITY.md) says what the confinement protects.

## Where the logs are

- **`owlshift logs`** prints the events Owlshift recorded, oldest first, from `events.jsonl` in the data directory: `owlshift logs ENG-12` for one ticket, `--last 20` for the last 20, `--follow` to keep printing until Ctrl-C. With nothing recorded it says `owlshift: no event recorded yet in <data directory>/events.jsonl`, which also names the folder.
- **The data directory** is `owlshift` in your local data folder: `~/Library/Application Support/owlshift` on macOS, `$XDG_DATA_HOME/owlshift` or `~/.local/share/owlshift` on Linux; `OWLSHIFT_DATA_DIR`, when set to an absolute path, replaces it. It holds the dedicated clone of each project, the worktrees, the run logs and the event log. Nothing of it is inside your checkout.
- **The caches your runs write** (`stack.run_caches`) are in `owlshift` in your cache folder: `~/Library/Caches/owlshift` on macOS, `$XDG_CACHE_HOME/owlshift` or `~/.cache/owlshift` on Linux, one folder per repository; `OWLSHIFT_CACHE_DIR`, when set to an absolute path, replaces it, for a machine whose cache folder's path holds a space, which Owlshift refuses. `owlshift doctor` gives the folder and its size; deleting it empties it.
- **The service's output** is `service/daemon.log` in the data directory, as `owlshift start` prints when it starts it: the dark wake line and watch's usage limit line are there; the quarantine and notification lines are shown on the desktop only.
- **A gate's output** is the `Gate log:` file that `owlshift gate` prints.
