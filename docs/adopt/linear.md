# Adopt a project on Linear

This page takes you from an installed `owlshift` to a first pull request made by agents on your repository, with the tickets in a Linear team, then to the background service that works the tickets you admit. It takes about twenty minutes, plus the time the agents work.

The outputs below come from real runs. Paths and some long lines are shortened, marked `…`; your repository's name, paths and team will differ.

## Before you start

- Owlshift is installed and `owlshift doctor` runs: see the [quick start](../../README.md#quick-start).
- Claude Code is installed, and you have a Claude subscription.
- Your project is a GitHub repository, cloned on this machine, that you can push to; Owlshift opens its pull requests there. You can install a GitHub App on it (its owner, or an admin of its organization), or someone who can is at hand.
- Your tickets live in one Linear team, whose key is the prefix of their ids: `ENG` in `ENG-12`.

## 1. Prepare the Linear team

Owlshift shows where a ticket stands by moving it between workflow states, and asks its questions in comments. In the team's settings, under Workflow:

- **The states.** By default Owlshift uses `Todo` (ready), `In Progress` (working), `Needs Input` (waiting for an answer) and `In Review` (a pull request is open). Linear has no `Needs Input` state at first: add it in the Started group, beside `In Progress`. Or keep your own names and map them under `[tracker].states` in `owlshift.toml` after `init`. A `Parked` state is optional: without one, a parked ticket shows `Needs Input`.
- **Triage**, turned on for the team, is where the follow-ups Owlshift proposes arrive, for a person to accept or decline. Without it they go to the team's first backlog state.
- **A label `agent`**, on the team or the workspace: putting it on a ticket is how you admit it to Owlshift's queue (you can choose another gesture in `owlshift.toml`).

Then, in Linear's settings under Security & access, create a **personal API key** for Owlshift. `init` asks for it. Owlshift comments through that key's account, so its comments look like that person's.

Optionally, a workspace admin can create an OAuth application in Linear's API settings with client credentials turned on. Given its client ID and secret, Owlshift comments as that app's user, mentions the decider in its questions, and Linear notifies them. Without it, Owlshift shows a desktop notification on the machine that runs it when a ticket waits for someone.

**Who decides a ticket** is its assignee. Owlshift asks the assignee its questions, and only the assignee's comments answer them.

## 2. Write the project file: `owlshift init`

In your clone, at the repository's root, with your team's key:

```bash
cd ~/code/shop
owlshift init --team ENG
```

Linear is the default tracker, so `--tracker linear` can be left out. `init` does five things, in this order:

1. It reads the repository's root files (`Cargo.toml`, `package.json`, `pyproject.toml`, `Makefile`) to suggest a gate: your lint, formatter and test commands. It never runs them. If it finds nothing, or guesses wrong, give the gate yourself, one `--gate` per command, as in `owlshift init --team ENG --gate "make lint" --gate "make test"`.
2. It asks for the Linear API key, then for the optional app's client ID and secret: leave both empty to go without.
3. It signs you in to GitHub through Owlshift's GitHub App. It prints a code and asks you to open `https://github.com/login/device` and enter it. If the app is not installed on the repository, it prints the install link and waits until you have installed it.
4. It asks for the token agent runs log in to Claude Code with. Make it in another terminal with `claude setup-token`, then paste it. It is asked once per machine. Owlshift never asks for a model API key.
5. It reads the team's workflow states with the key, then writes a commented `owlshift.toml`. If Linear does not answer, it says why and writes no file.

The secrets go to the system keychain, never to a file. Here is what it printed for a project whose secrets were left for later (`owlshift init --team ENG --gate "test -f README.md" --skip-secrets`):

```text
Wrote ~/code/shop/owlshift.toml: review it, then commit it.
No secret stored (--skip-secrets).
The tracker was not checked (--skip-secrets): `owlshift doctor` checks it.
Next: `owlshift doctor` checks git, the harnesses and the Claude Code token for agent runs; `owlshift do TICKET` runs a ticket.
Nothing detected was run: `owlshift gate` runs the gate to check it.
```

Read `owlshift.toml`: each setting is explained in a comment above it. Check the gate under `[stack]`, which every agent's work must pass, and the state names under `[tracker]`. Then add the people whose `agent` label admits a ticket, by their Linear user id, under `[tracker]`:

```toml
admitters = ["2f9c51d0-6a3e-4b7e-9d41-0c8a7e5b3f12"]
```

The id is a user's `id` in Linear's API: with your API key in `LINEAR_API_KEY`, `curl -s https://api.linear.app/graphql -H "Authorization: $LINEAR_API_KEY" -H "Content-Type: application/json" -d '{"query":"{ viewer { id name } }"}'` prints yours. Commit the file and push it to your default branch: the background service reads the project file of the default branch, and each run the file of the commit its branch starts from.

## 3. Check the machine and the project: `owlshift doctor`

```bash
owlshift doctor
```

Each line is marked `✓` (fine), `!` (a warning, which never blocks `owlshift do`) or `✗` (a problem to fix first); the end of the report says why each problem matters and how to fix it. Run it again until it ends with `Ready`. The `tracker states` line checks that each state named in `owlshift.toml` is exactly one of the team's; `drain admission` checks the `agent` label and the admitters; `proposals inbox` says where proposals go. The `sandbox` line must be `✓`: it says that agent runs are confined, with `sandbox-exec` on macOS or `bwrap` on Linux.

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

Create a ticket for something small and clear, such as a typo to fix or a line to add to the README, and assign it to yourself. Then:

```bash
owlshift do ENG-12
```

`owlshift do` works in the foreground and says what it does. It runs the ticket through intake, design, design review, build and verify, each step by a fresh agent run, with the gate run on the result. It works in its own clone and worktrees under Owlshift's data folder, never in your checkout. It pushes a branch `owlshift/eng-12` and opens a pull request; if your workspace uses Linear's GitHub integration, the branch's name links the pull request to the ticket. The ticket moves through the states as it goes, and Owlshift comments on it: the plan, then the delivery report.

Review the pull request and merge it yourself: Owlshift never merges unless you turn merge on green on for the project (`[forge] merge` in `owlshift.toml`).

## 6. Answer a question

When a run needs a decision only the assignee can make, Owlshift comments on the ticket with the questions, under a title `[owlshift] QUESTIONS · round 1`, moves the ticket to `Needs Input`, and notifies: through Linear when the optional app is set up, and with a desktop notification.

Answer in a comment on the ticket, as its assignee, one answer per question. An answer counts once you have left it unedited for 10 minutes (the project's `policy.quiet_window_minutes` changes that), or at once when it ends with `go`. Then, while the background service does not serve the project:

```bash
owlshift continue ENG-12
```

Once the service serves the project (below), it picks your answer up by itself. Either way your answers are checked first: if one is missing or unclear, Owlshift asks again; otherwise the work goes on to a pull request. Answer as many rounds as it takes: there is no limit.

If a ticket stops for a reason it cannot settle itself, such as a run that fails twice, it is parked: it moves to `Parked` (or `Needs Input`), with a comment marked `PARKED` that says why. Fix what it names, then reply `continue` on the ticket as its assignee, alone in the comment, or run `owlshift continue ENG-12`.

## 7. Let the service work the backlog: `owlshift start`

With a first pull request through, let Owlshift take tickets as you admit them:

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

- **Admit a ticket** by putting the `agent` label on it in its ready state (`Todo`), as one of the `admitters`. A label anyone else puts admits nothing. The `admitters` are read from the project file of your default branch, never from your checkout: a list only in your local file admits nothing. The service checks every minute.
- Admitted tickets are taken in priority order, blockers respected (Linear's "blocked by" relations, or a line `Blocked by ENG-10` in the description), one at a time unless `policy.max_in_flight` says more.
- `owlshift status` says what the service does and which tickets wait for you; `owlshift why` says why an admitted ticket is not running; `owlshift logs ENG-12` prints what Owlshift recorded for one.
- `owlshift stop` stops the service once the steps under way finish (`Stopping the daemon (pid 8671): each watch ends once its steps under way finish, then the daemon exits.`); `owlshift stop --forget` stops serving this project only.

## Next

- Follow-ups Owlshift proposes arrive in the team's Triage. Accept one by giving it an assignee and the `agent` label, or decline it.
- [SECURITY.md](../../SECURITY.md) says what the confinement of agent runs protects and what it does not.
- `owlshift stop --at-login` removes the service installed at login; `owlshift uninstall` removes everything Owlshift installed on this machine, the keychain entries `init` stored included. Neither touches a repository, a ticket or a branch.
