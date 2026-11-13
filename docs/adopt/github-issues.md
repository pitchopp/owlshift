# Adopt a project on GitHub Issues

This page takes you from an installed `owlshift` to a first pull request made by agents on your repository, with the tickets kept as the repository's own GitHub issues, then to the background service that works the issues you admit. It takes about fifteen minutes, plus the time the agents work.

The outputs below come from real runs. Paths and some long lines are shortened, marked `…`; your repository's name, paths and numbers will differ.

## Before you start

- Owlshift is installed and `owlshift doctor` runs: see the [quick start](../../README.md#quick-start).
- Claude Code is installed, and you have a Claude subscription.
- Your project is a GitHub repository with issues turned on, cloned on this machine, and you can push to it. Owlshift works the issues of that repository only, the one your clone's `origin` names.
- You can install a GitHub App on the repository (its owner, or an admin of its organization), or someone who can is at hand.

## 1. Prepare the repository

Little is needed on GitHub itself; `owlshift init` does the rest.

- **Owlshift's GitHub App**, `owl-shift`, is how Owlshift reaches GitHub: `init` signs you in through it and shows its install link when it is missing on the repository. Among its permissions is Issues (read and write). If it was installed on the repository before it asked Issues, GitHub asks the installation's owner to accept the new permission, in the account's or organization's settings under GitHub Apps; until then every issue read fails, and `owlshift doctor` says so.
- **Labels.** Owlshift shows where an issue stands with labels, and `init` creates them: `agent:ready` (the ready column, which is also how you admit an issue), `agent:working`, `agent:needs-input`, `agent:review`, and `agent:proposed` for the follow-ups it proposes. To name them differently, edit `[tracker.states]` in `owlshift.toml` and run `owlshift init` again: it keeps the file and creates the labels it names that are missing.
- **Who decides an issue** is its assignee, the first one when there are several. Owlshift asks the assignee its questions, and only the assignee's comments answer them.
- **Blockers.** An issue waits for the issues that block it: GitHub's "blocked by" relations, or a line `Blocked by #12` in its description. A blocker releases it once closed as completed, or once its pull request is merged. If a blocker is in another repository, add that repository to the app's installation too: an issue the app cannot read holds nothing.

Everything Owlshift writes on GitHub, comments and labels included, is written through your own account, marked by GitHub as made with `owl-shift`. GitHub does not notify you of your own comments, so Owlshift shows a desktop notification when an issue waits for you.

## 2. Write the project file: `owlshift init`

In your clone, at the repository's root:

```bash
cd ~/code/shop
owlshift init --tracker github
```

`init` does five things, in this order:

1. It reads the repository's root files (`Cargo.toml`, `package.json`, `pyproject.toml`, `Makefile`) to suggest a gate: your lint, formatter and test commands. It never runs them. If it finds nothing, or guesses wrong, give the gate yourself, one `--gate` per command, as in `owlshift init --tracker github --gate "make lint" --gate "make test"`.
2. It signs you in to GitHub through Owlshift's GitHub App. It prints a code and asks you to open `https://github.com/login/device` and enter it. If the app is not installed on the repository, it prints the install link and waits until you have installed it. If the code expires before you enter it, `init` says so and exits with an error: run it again for a new code.
3. It asks for the token agent runs log in to Claude Code with. Make it in another terminal with `claude setup-token`, then paste it. It is asked once per machine. Owlshift never asks for a model API key.
4. It checks that the repository's issues answer, creates the labels that are missing, one line each, and reads your GitHub account's numeric id, which it writes as the one admitter: the person whose `agent:ready` label admits an issue.
5. It writes a commented `owlshift.toml`.

The secrets go to the system keychain, never to a file. If the issues do not answer, `init` says why and writes no file. On a repository that had none of the labels, it printed:

```text
Created the label "agent:ready" on GitHub.
Created the label "agent:working" on GitHub.
Created the label "agent:needs-input" on GitHub.
Created the label "agent:review" on GitHub.
Created the label "agent:proposed" on GitHub.
Wrote ~/code/shop/owlshift.toml: review it, then commit it.
```

and the `[tracker]` table of the file it wrote reads:

```toml
kind = "github"
admit = "ready"
admitters = ["24303327"]
states = { ready = "agent:ready", working = "agent:working", needs_input = "agent:needs-input", review = "agent:review" }
```

Read `owlshift.toml`: each setting is explained in a comment above it. Check the gate under `[stack]`, which every agent's work must pass, and `admitters` under `[tracker]`: add the numeric ids of the other people whose label should admit an issue (`gh api users/<login> --jq .id` prints one). Then commit the file and push it to your default branch: the background service reads the project file of the default branch, and each run the file of the commit its branch starts from.

## 3. Check the machine and the project: `owlshift doctor`

```bash
owlshift doctor
```

Each line is marked `✓` (fine), `!` (a warning, which never blocks `owlshift do`) or `✗` (a problem to fix first); the end of the report says why each problem matters and how to fix it. Run it again until it ends with `Ready`. The lines about the tracker, on a project whose file was written before the labels existed:

```text
  ✓ tracker access      the GitHub credential reads the issues of
                        github.com/…
  ! tracker labels      github.com/… has no label
                        "agent:ready", "agent:working", "agent:needs-input",
                        "agent:review", "agent:proposed"
                        Does not block `owlshift do`: a stage whose label is
                        missing does not show, and a priority's never reads. Run
                        `owlshift init`, which creates the stage labels and
                        "agent:proposed"; create a priority's label on GitHub
                        (Issues > Labels).
  ! drain admission     `admitters` is not set under `[tracker]`
                        Does not block `owlshift do`: `owlshift watch --drain`
                        refuses to start, and would admit nothing, until it is
                        fixed. List under `admitters` the numeric ids of the
                        GitHub accounts whose label admits an issue; with none,
                        no issue is ever admitted. `owlshift init` writes the
                        account it signs in as.
```

`tracker access` must be `✓`: a `✗` there says whether the app's installation has not accepted Issues, the repository is not in the installation, or issues are turned off. The `sandbox` line must be `✓` too: it says that agent runs are confined, with `sandbox-exec` on macOS or `bwrap` on Linux.

## 4. Check the gate: `owlshift gate`

```bash
owlshift gate
```

This runs the gate once, the way it runs on an agent's work: in a fresh worktree of your default branch, confined, with nothing of your environment but what the project declares. A gate that passes on your laptop can fail here, because a tool is missing from the `PATH`, a test reads your home folder, or the dependencies were never installed; fixing it now saves an agent's run. The output names each command, its status and time:

```text
The gate of 2fe46e01ea43 (origin/main), from your owlshift.toml, the commit having none, in a fresh worktree, confined as a Build's gate:
  test -f README.md
Its whole output goes to ~/Library/Application Support/owlshift/projects/github.com/…/gate/run/gate.log.
$ test -f README.md
  ok, 0.0 s
Gate green on 2fe46e01ea43: 1 command in 2.2 s.
```

When a gate needs its dependencies installed first, list the install commands under `stack.setup` in `owlshift.toml` (the file's comments show how), and the caches it reads under `stack.caches`.

## 5. Run a first ticket: `owlshift do`

Open an issue for something small and clear, such as a typo to fix or a line to add to the README, and assign it to yourself. An issue's ticket id is its number without the `#`:

```bash
owlshift do 12
```

`owlshift do` works in the foreground and says what it does. It runs the issue through design, design review, build and verify, each step by a fresh agent run, with the gate run on the result; it prints each step as it goes. It works in its own clone and worktrees under Owlshift's data folder, never in your checkout. It pushes a branch `owlshift/12` and opens a pull request whose description ends with `Closes #12`, so merging it closes the issue. The issue's labels move as it goes, and Owlshift comments on it: the plan, then the delivery report.

A ticket can also wait rather than run. Owlshift never builds two changes to the same files at once: while one of its pull requests that touches those files is open, a new ticket that would touch them waits, and `owlshift do` says so and stops. An issue whose plan touched the README, run while an earlier pull request on the README was open, ended with:

```text
Not run: 102 waits: pull request #101 holds zone `README.md`, which its Build would take: its Build is admitted once pull request #101 no longer holds it. `owlshift why 102` says why it waits.
```

Merge or close that pull request, then run `owlshift do 102` again. Under `owlshift watch --drain` or the service, Owlshift also posts a `[owlshift] WAITING` comment on the issue saying what it waits for, and goes on by itself once that is gone.

Review the pull request and merge it yourself: Owlshift never merges unless you turn merge on green on for the project (`[forge] merge` in `owlshift.toml`).

## 6. Answer a question

When a run needs a decision only the assignee can make, Owlshift comments on the issue with the questions, under a title `[owlshift] QUESTIONS · round 1`, moves the issue to `agent:needs-input`, and shows a desktop notification.

Answer in a comment on the issue, as its assignee, one answer per question. An answer counts once you have left it unedited for 10 minutes (the project's `policy.quiet_window_minutes` changes that), or at once when it ends with `go`. Then, while the background service does not serve the project:

```bash
owlshift continue 12
```

Once the service serves the project (below), it picks your answer up by itself. Either way your answers are checked first: if one is missing or unclear, Owlshift asks again; otherwise the work goes on to a pull request. Answer as many rounds as it takes: there is no limit.

If an issue stops for a reason it cannot settle itself, such as a run that fails twice, it is parked: a comment marked `PARKED` says why. Fix what it names, then reply `continue` on the issue as its assignee, alone in the comment, or run `owlshift continue 12`.

## 7. Let the service work the backlog: `owlshift start`

With a first pull request through, let Owlshift take issues as you admit them:

```bash
owlshift start
```

`start` registers the project and starts the background service for this login session; `owlshift start --at-login` also starts it at each login. On a clean Debian 12 machine it printed:

```text
acme/shop is served from /home/dev/shop. Started the daemon for this session (pid 8671): its output is in /home/dev/.local/share/owlshift/service/daemon.log; `owlshift stop` stops it.
```

The service reads the project file of your default branch, never your checkout's, so push `owlshift.toml` first. Otherwise `owlshift status` says so, and the service tries again every minute:

```text
acme/shop (from /home/dev/shop)
  not served: origin/main (7fd1a60b01f9) holds no owlshift.toml
```

From then on:

- **Admit an issue** by putting the `agent:ready` label on it, as an account listed in `admitters`. A label anyone else puts admits nothing, so on a public repository strangers cannot start work. The `admitters` are read from the project file of your default branch, never from your checkout: a list only in your local file admits nothing. The service checks every minute; it may take a pass or two to see a label put that instant.
- An admitted issue first gets an intake run, which posts an `[owlshift] INTAKE` comment (its size, its routing, its dependencies), then waits its turn for a Build. Admitted issues are taken in priority order, blockers respected, one at a time unless `policy.max_in_flight` says more. An issue dispatched and not yet delivered keeps its place, whether it waits for your answers or for another pull request: answer it, or merge or close what it waits for, and the next one goes. `owlshift why` says which:

```text
103 [ready] in acme/shop
  2026-11-13T13:23:13.899429Z waits: the project's slot is held by 102
now: the project's slot is held by 102
```
- `owlshift status` says what the service does and which issues wait for you; `owlshift why` says why an admitted issue is not running; `owlshift logs 12` prints what Owlshift recorded for one.
- `owlshift stop` stops the service once the steps under way finish (`Stopping the daemon (pid 8671): each watch ends once its steps under way finish, then the daemon exits.`); `owlshift stop --forget` stops serving this project only.

Priorities are labels too, optional: name them under `[tracker.priorities]` in `owlshift.toml` and create them on GitHub; without them every issue has the same priority.

## Next

- Follow-ups Owlshift proposes are issues labelled `agent:proposed`. Accept one by assigning it and putting `agent:ready` on it, or close it.
- [SECURITY.md](../../SECURITY.md) says what the confinement of agent runs protects and what it does not.
- `owlshift stop --at-login` removes the service installed at login; `owlshift uninstall` removes everything Owlshift installed on this machine, the keychain entries `init` stored included. Neither touches a repository, an issue or a branch.
