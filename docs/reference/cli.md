# Command reference

<!-- Generated from the binary's own command definitions: do not edit by hand. -->

This page lists every `owlshift` command with its arguments, options and help text, as `owlshift help do` prints them for `do`. It is generated from the binary, and a test fails when it differs from the commands. To update it after changing a command, run:

```sh
OWLSHIFT_UPDATE_CLI_REFERENCE=1 cargo nextest run -p owlshift-cli --locked cli_reference
```

Every command also takes `--help`, and `owlshift --version` prints the version. Commands and options meant for Owlshift's own processes are left out. In a usage line, `<NAME>` is required and `[NAME]` optional; the help text says which options go together or exclude each other.

Works a team's backlog with coding agents, and asks a human on the ticket when a decision is theirs.

## Commands

- [`owlshift doctor`](#owlshift-doctor)
- [`owlshift config`](#owlshift-config)
- [`owlshift init`](#owlshift-init)
- [`owlshift do`](#owlshift-do)
- [`owlshift continue`](#owlshift-continue)
- [`owlshift watch`](#owlshift-watch)
- [`owlshift forget`](#owlshift-forget)
- [`owlshift cancel`](#owlshift-cancel)
- [`owlshift release`](#owlshift-release)
- [`owlshift gate`](#owlshift-gate)
- [`owlshift unquarantine`](#owlshift-unquarantine)
- [`owlshift why`](#owlshift-why)
- [`owlshift start`](#owlshift-start)
- [`owlshift stop`](#owlshift-stop)
- [`owlshift pause`](#owlshift-pause)
- [`owlshift resume`](#owlshift-resume)
- [`owlshift status`](#owlshift-status)
- [`owlshift ui`](#owlshift-ui)
- [`owlshift uninstall`](#owlshift-uninstall)
- [`owlshift daemon`](#owlshift-daemon)
- [`owlshift self-update`](#owlshift-self-update)
- [`owlshift logs`](#owlshift-logs)

## `owlshift doctor`

Check whether this machine is ready: git, the harness CLIs and their logins, the agent runs' isolation and Claude Code token, the configuration files. Says why each problem matters and how to fix it.

```text
owlshift doctor [OPTIONS]
```

Arguments and options:

- `--json`: Print the report as JSON, for scripts.

## `owlshift config`

Read the configuration.

```text
owlshift config <COMMAND>
```

### `owlshift config show`

Print the effective configuration and the file each value comes from.

```text
owlshift config show
```

## `owlshift init`

Write a commented owlshift.toml for this repository, then store its tracker and forge secrets and the agent runs' Claude Code token `owlshift do` needs in the system keychain.

```text
owlshift init [OPTIONS]
```

Arguments and options:

- `--tracker <TRACKER>`: Where the tickets live. Asked at a terminal when left out; Linear otherwise.
  - `linear`
  - `markdown`
  - `github`: The issues of this repository on GitHub.
- `--team <TEAM>`: The Linear team key, the prefix of its ticket ids: OWL in OWL-12. At a terminal, picked among the workspace's teams when left out.
- `--gate <COMMAND>`: One command of the project's gate (lint, formatter, tests), run from the repository root; repeat it for each command, in order. Asked at a terminal when left out.
- `--skip-secrets`: Write the project file only, and store no secret.
- `--replace-secrets`: Ask again for the secrets already stored, this project's and the Claude Code token for agent runs, and replace them, signing in to GitHub again. An empty answer keeps the one in use. The tracker and GitHub accounts every project of this machine used to share are never written.
- `--github-token`: Ask for a personal GitHub token instead of signing in through Owlshift's GitHub App, the expert option: it removes the project's GitHub App token, so the personal token is the one used.
- `--replace-agent-token`: Ask only for a new Claude Code token for agent runs, the one every project of this machine shares, and store it in place of the stored one, as when it leaked, expired or was revoked. Nothing else is asked, written or checked, and it runs in any folder.

## `owlshift do`

Run one ticket to a verified pull request, in the foreground. Its questions are posted on the ticket.

```text
owlshift do <TICKET>
```

Arguments and options:

- `<TICKET>`: The ticket, such as OWL-12.

## `owlshift continue`

Continue a ticket once its decider has answered on it: once their reply has been left unedited for 10 minutes, or ends with `go`, check the answers, then ask again what is missing or run on to a verified pull request. Restarts a parked ticket.

```text
owlshift continue [OPTIONS] <TICKET>
```

Arguments and options:

- `<TICKET>`: The ticket, such as OWL-12.
- `--to-watch`: Restart a parked ticket and return without running it: `owlshift watch` drives it from its next pass. On a ticket that is not parked, runs nothing and says what watch does with it.

## `owlshift watch`

Watch this project's tickets whose questions wait, in the foreground, and continue each one as `owlshift continue` would once its decider's reply counts; checked every minute. A first Ctrl-C ends it once the steps under way finish, a second (or SIGTERM) stops them and checks their runs, a third (or SIGQUIT) stops everything at once. It also resumes a ticket at Intake, Design, Design review, Build or Verify that an answered round (round > 0) left shown working or needing input, a refusal after the answer check included. A parked ticket, or one left at Build before any question round, still needs `owlshift continue`; `owlshift continue --to-watch` restarts a park and leaves it to watch.

```text
owlshift watch [OPTIONS]
```

Arguments and options:

- `--drain`: Also take admitted tickets in from the ready column, intake first, and dispatch them to Build one at a time.

## `owlshift forget`

Start over what Owlshift keeps on a ticket between commands, when it has no room left or cannot be read: its asks and decisions go, their comments staying on the ticket; its round count and the refusals its next runs are told stay. Refused while another command works the project.

```text
owlshift forget <TICKET>
```

Arguments and options:

- `<TICKET>`: The ticket, such as OWL-12.

## `owlshift cancel`

Cancel a ticket's work: its step under way stops within 5 seconds, its runs stopped and checked, and the project's watch parks it, with a PARKED comment saying who canceled and why. `owlshift continue` restarts it from where it was stopped. Refused for a ticket parked already, one whose delivery ended, and one waiting in the ready column: removing its admission keeps it there.

```text
owlshift cancel [OPTIONS] <TICKET>
```

Arguments and options:

- `<TICKET>`: The ticket, such as OWL-12.
- `--reason <TEXT>`: Why, said in the PARKED comment.

## `owlshift release`

Release a ticket's claim on the git remote, so that another runner can take it: on the machine that holds it at any time, its step under way stopped first; from another machine when the claim's operator is yours, or once it expired, by a person the project's `[people]` lists. Any other live claim is refused, with its holder and its expiry. A RELEASED comment on the ticket says who released it and how to have it worked again; its stage is left as it is.

```text
owlshift release <TICKET>
```

Arguments and options:

- `<TICKET>`: The ticket, such as OWL-12.

## `owlshift gate`

Run the project's gate once, as a Build's gate runs it: in a fresh worktree of origin's default branch, confined, with the variables the project declares and you allow. Prints each command's last lines, status and time, then removes the worktree. The gate is the project file's at that commit, or yours when it has none. Refused while another command works the project.

```text
owlshift gate [OPTIONS]
```

Arguments and options:

- `--at <COMMIT>`: Check this commit instead: a full commit id, or a branch of origin such as `origin/fix-gate`

## `owlshift unquarantine`

Clear this project's quarantine: the marker a run whose isolation check did not pass leaves, which makes every command refuse the project. Shows what the marker says, then clears it once you type the project's name (owner/repo) at a terminal. The dedicated checkout and the worktrees stay as they are. Refused while another command works the project.

```text
owlshift unquarantine
```

## `owlshift why`

Say why this project's admitted tickets are not running: blockers, the ticket or pull request that holds what they would touch, the caps, a harness at its usage limit, the quarantine, and the people's tickets left out for want of zones; then the machine's run slots and the harnesses limited. With a ticket, its reasons over time. Read only, takes no lock, and works while `owlshift watch` runs.

```text
owlshift why [TICKET]
```

Arguments and options:

- `[TICKET]`: Only this ticket, such as OWL-12, with its reasons over time.

## `owlshift start`

Serve this project with the background service, and start the service for this login session when it does not run: it keeps one `owlshift watch --drain` per served project alive, without a terminal, each on the project file of its default branch. Its output goes to `service/daemon.log` in the data directory.

```text
owlshift start [OPTIONS]
```

Arguments and options:

- `--at-login`: Also install the service to start at each login, as a user LaunchAgent (macOS) or a systemd user unit (Linux): `owlshift stop --at-login` or `owlshift uninstall` removes it.

## `owlshift stop`

Stop the background service: each watch ends once its steps under way finish, then the service exits. With --forget, stop serving this project only.

```text
owlshift stop [OPTIONS]
```

Arguments and options:

- `--forget`: Stop serving the project of the current directory, the service going on with the others.
- `--at-login`: Also remove the service installed at login, once the daemon has ended: this waits for the steps under way to finish.

## `owlshift pause`

Pause every project of this machine, or only OWNER/REPO: from each watch's next pass, no ticket is dispatched and no step starts, nor a merge on green; the steps under way finish, and answers, pull requests and stages are still read. The service keeps running.

```text
owlshift pause [OPTIONS] [OWNER/REPO]
```

Arguments and options:

- `[OWNER/REPO]`: Pause only this project, such as acme/web.
- `--reason <TEXT>`: Why, shown by `owlshift status` and the digest.

## `owlshift resume`

Lift the pause of every project of this machine, or of OWNER/REPO only: each watch goes on at its next pass.

```text
owlshift resume [OWNER/REPO]
```

Arguments and options:

- `[OWNER/REPO]`: Lift only this project's pause, such as acme/web.

## `owlshift status`

Say what the background service serves and does: the daemon, each served project's watch (running, restarting and why, paused and why, draining) and the file it runs on, the tickets waiting for a person, the steps under way, each harness's usage and reset, the latest digest. Read only, takes no lock.

```text
owlshift status [OPTIONS] [OWNER/REPO]
```

Arguments and options:

- `--waits`: Instead, what each ticket in flight or admitted waits on and behind whom: the holders with what waits behind each, the queues for a slot, the other waits.
- `--json`: With --waits: print it as JSON (`schemas/status-waits.schema.json`)
- `[OWNER/REPO]`: With --waits: only this project, such as acme/web, served or not.

## `owlshift ui`

Open the local web UI in your browser, logged in: checks that the daemon's web UI listens on 127.0.0.1 at `[ui] port` (18466 unless the personal file sets it), then opens a link with a one-time code, valid for a minute.

```text
owlshift ui [OPTIONS]
```

Arguments and options:

- `--print`: Print the link instead of opening it.
- `--new-token`: Replace the web UI's token first, which ends every session.

## `owlshift uninstall`

Remove what Owlshift installed on this machine, as its install manifest lists it: the service at login and the keychain entries `owlshift init` stored. A daemon that runs is stopped first, its runs checked. Repositories, tickets and branches are never touched.

```text
owlshift uninstall [OPTIONS]
```

Arguments and options:

- `--purge`: Also remove the data directory (the dedicated clones, worktrees, run logs and events) and the personal file.

## `owlshift daemon`

The background service itself, in the foreground: `owlshift start` runs it. One per data directory; a second exits at once. Ctrl-C ends it once its watches drained, a second (or SIGTERM) stops their steps checking their runs, a third stops everything now.

```text
owlshift daemon
```

## `owlshift self-update`

Update an install made by hand, by install.sh or from an archive, to the latest release: its signature and checksum verified with the key built into this binary, then the binary replaced at its own path. An install by Homebrew or cargo is refused, with the command that updates it; an older release is never installed.

```text
owlshift self-update [OPTIONS]
```

Arguments and options:

- `--check`: Only say whether a newer release is published; change nothing.

## `owlshift logs`

Print the events `owlshift do`, `continue`, `watch` and `forget` recorded, oldest first.

```text
owlshift logs [OPTIONS] [TICKET]
```

Arguments and options:

- `[TICKET]`: Only this ticket's events.
- `--last <N>`: Only the last N events, after the ticket filter (at least 1)
- `-f, --follow`: Keep printing new events as they are recorded, until Ctrl-C.

