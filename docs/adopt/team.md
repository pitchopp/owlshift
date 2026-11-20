# Adopt a project on a team

This page is for a developer who joins a project that already runs Owlshift, with colleagues who each run their own. Every developer has a runner on their own machine, signed in with their own accounts and running agents on their own Claude subscription. The runners coordinate through the tracker and the git remote: nothing is hosted, and there is no Owlshift server.

Section 1 is for whoever writes the team's project file, once; as you join, check that it reads as shown there. Some lines below are shortened, marked `…`; your repository's name, paths and accounts will differ.

## Before you start

- Owlshift is installed and `owlshift doctor` runs: see the [quick start](../../README.md#quick-start). Claude Code is installed, and you have your own Claude subscription.
- You can push to the project's GitHub repository, and you have cloned it on this machine. Write access is enough: every developer's runner pushes its branches and its claims with that developer's own git access.
- You are a member of the project's tracker: the Linear workspace, or the GitHub repository whose issues hold the tickets.
- `owlshift.toml` is on the repository's default branch. If it is not, the project does not run Owlshift yet: first follow the page of its tracker, [Linear](linear.md) or [GitHub Issues](github-issues.md).

A team works on Linear or GitHub Issues. The [Markdown tracker](markdown.md) serves one machine: it has no background service, and nothing it writes reaches another machine.

## 1. The team's project file

Three settings of `owlshift.toml` make a project a team's. Each is changed through a pull request to the default branch: the background service reads the project file of the default branch, never your checkout's.

**The people table.** `[people]` lists each person once, under a handle, with their accounts:

```toml
[people.ann]
tracker = "8d0e7c1a-3f52-4b9e-a6d4-91b2c07e5a18"   # Linear user id; on GitHub Issues, the numeric account id
forge = "ann-dev"                                  # GitHub login; without it, none of their reviews is read

[people.lea]
tracker = "2f9c51d0-6a3e-4b7e-9d41-0c8a7e5b3f12"
forge = "lea-martin"
```

A handle is 1 to 32 lowercase letters, digits, `-` or `_`, starting with a letter. With the table, `[tracker].admitters` and each `[zones."<path>"].owner` name handles instead of accounts, and every person with a `forge` login is a reviewer whose reviews are read: `[forge].reviewers` beside the table is refused. A person who admits tickets is listed twice, once in `[people]` and once by handle in the admitters:

```toml
[tracker]
admit = { label = "agent" }
admitters = ["ann", "lea"]
```

**A label, not a state, admits a ticket.** On a team the admission gesture is personal: each developer puts the label on their own tickets, and a runner takes only the tickets its own operator admitted. A state gesture (`admit = "ready"`) counts the account of whoever moved the ticket to the ready column. On a team that is often the person who grooms the backlog, so their runner takes the ticket, or nobody's if they run none. On GitHub Issues the `agent:ready` label that `init` writes as the gesture is such a state too: set `admit = { label = "agent" }`, and `owlshift init` creates the label `agent` once the file names it.

**Claims on the remote.** Under `[policy]`:

```toml
claims = "remote"
```

Before its first run on a ticket, a runner claims it with a ref on `origin`, `refs/owlshift/claims/<ticket>`, and its service renews the claim every hour. Every runner reads the others' claims, so a ticket is never worked by two machines: two people's, or one person's laptop and desktop. A Build also waits for the files another machine's ticket holds. A claim publishes, on the remote, the ticket's identifier, its operator's handle, the runner's identifier (16 random hexadecimal digits, never a host name), when it was claimed and when it expires, and the paths the ticket holds. It never publishes the ticket's text, a plan or a question. On a public repository with a private tracker that much becomes public, which is why claims are the project's choice. Left at `"local"`, the default, two machines can work one ticket.

`[policy].max_in_flight` and `[policy].max_open_prs` count each runner's own tickets, so a project's total grows with its developers.

## 2. Set up your machine: `owlshift init`

In your clone, at the repository's root:

```bash
cd ~/code/shop
owlshift init
```

The project file is kept as it is: `init` never writes to a file that exists, and its values win over any flag.

```text
~/code/shop/owlshift.toml exists: kept as it is. Its values win over --tracker, --team and --gate.
```

It then asks for your secrets, which go to this machine's keychain. They are yours, not the team's:

- **Your Linear API key**, made in Linear's settings under Security & access. It is your runner's operator: your runner takes the tickets whose label this account put on.
- **The Linear app's client ID and secret**, shared by the person who created the workspace's OAuth application (see [Linear](linear.md#1-prepare-the-linear-team)). This page advises it. With the app, Owlshift's comments are the app's, and Linear notifies the decider its questions mention. Left empty, your runner writes through your own key: its comments look like yours, and a question that mentions a colleague still notifies them (checked live on 2026-11-19). Each developer chooses: a team where some set the app and some do not works.
- **The GitHub sign-in**, through Owlshift's GitHub App, as your own account. The pull requests your runner opens are yours, so a colleague reviews them as any of yours, and your claims are pushed with your git access.
- **The Claude Code token** for agent runs, from `claude setup-token`, once per machine.

On GitHub Issues there is no app credential to ask for: the GitHub sign-in is also the tracker's.

Once the tracker answers, `init` compares the accounts you signed in as with `[people]`. When they are not listed, it prints the entry to add, filled in with your tracker account and GitHub login:

```text
`[people]` in ~/code/shop/owlshift.toml does not list the accounts this machine signed in as. Add this entry through a pull request, choosing its handle:

[people.<handle>]
tracker = "5b7e3a90-…"
forge = "eve-dev"
```

When one of your two accounts is listed and not the other, it says ``lists one of the accounts this machine signed in as, not the other`` and prints the entry that replaces it. When your tracker account and your login are listed under two handles, it says ``One person is one entry: fix the file by hand, through a pull request.``

`owlshift init --skip-secrets` stores nothing and reads no account, so it prints no entry.

## 3. Add yourself to the team

Open a pull request that adds your `[people.<handle>]` entry and, so that your label admits tickets, your handle to `[tracker].admitters`. Until it is merged into the default branch your label admits nothing, and `owlshift why` says why your tickets wait.

## 4. Check the machine and the team: `owlshift doctor`

```bash
owlshift doctor
```

Run it until it ends with `Ready`. Besides the lines every adoption page describes, three lines tell whether the project is set up for a team. They read the project file of your checkout, so pull the default branch first:

- **`people`** appears only when the file has `[people]`. It counts the people and names you, `this machine's operator is eve`, or warns that this machine's operator `is not one of them`: add your entry (section 3).
- **`team gesture`** warns when tickets are admitted by a move to the ready state and the file lists two people or more: switch to a label (section 1).
- **`claims`** warns when claims are local and the file lists two people or more: set `claims = "remote"` (section 1).

Each warning ends with the change to make through a pull request, and points back to this page. Under remote claims a second `claims` line may say that a claim on `origin` does not read; when it was written by a later Owlshift, upgrade yours.

## 5. Admit and answer your own tickets

From here a ticket is yours as on a project of one person:

- **Admit a ticket** by putting the `agent` label on it in its ready state, and assign it to yourself: its assignee is its decider, who answers its questions.
- **Start the background service:**

  ```bash
  owlshift start
  ```

  Your runner takes the tickets you admitted, and only those. That is `takes = "mine"`, the default of your personal file. `takes = "all"` takes every admitted ticket, as a runner did before teams: keep it for a machine that serves people who run none.
- **Questions** reach the ticket's decider. Your machine shows a desktop notification for a question only when you decide the ticket. A question your runner asks a colleague reaches them through the tracker, which notifies the person it mentions. A parked ticket or a usage limit still notifies you, since only your runner can lift it.

## What colleagues see of each other's tickets

- `owlshift why` names whose a ticket is. `admitted by lea: their runner's` is a ticket a colleague admitted, which your runner leaves alone. `claimed by lea's runner until <time>: theirs to work` is one their runner works.
- `owlshift status` lists your runner's claims and the others' as last fetched, each with its operator and its expiry: ``claimed by lea's runner 0123456789abcdef (as last fetched): ENG-8, until 2026-11-21 09:00``. An expired one adds ``: `owlshift release ENG-8` releases it``.
- A Build that would touch what a colleague's ticket holds waits: `ENG-8, which lea's runner works, holds …`. Under `policy.overlap = "rebase"` a colleague's ticket is tolerated as one of yours is, and its own runner rebases its pull request.
- `owlshift ui` shows the same: the tickets other runners hold, by whom and until when, and the questions other people decide, after yours and under their names.
- **A colleague's review** of your pull request does not start work by itself. Owlshift asks you, the ticket's decider, in one question: apply it as written, set it aside, or say what to do instead. An approval asks nothing, and your own review starts the work at once.

What a claim does not show, the ticket's plan and questions, stays on the machine that works it and on the ticket.

## Hand a ticket over: `owlshift release`

A ticket stays with the runner that claimed it, waiting and parked included, until its pull request is merged or closed. To give it to someone else:

```bash
owlshift release ENG-12
```

- **On the machine that holds it,** at any time: its step under way is stopped first.
- **From another machine,** in two cases. The claim's operator may release it at any time, from any of their machines: that is also how the claims of a lost data directory are freed. Anyone `[people]` lists may release it once it has expired. Any other live claim is refused, naming its holder and its expiry: ask them.

The release deletes the claim and posts a `[owlshift] RELEASED` comment on the ticket that names who released it, who held it and how to restart it. The ticket's stage is left as it is. Then the person whose runner should take it admits it again: they move it back to the ready column and put their label on it, taking it off first if it is there. If the earlier runner pushed a branch `owlshift/<ticket>`, a new claim is refused while it is on `origin`: close its pull request and delete the branch first, or finish that pull request by hand.

## What a team does not get yet

- **No server, no shared runner.** Nothing central runs. A runner that takes every admitted ticket for people who run none, with an identity of its own on GitHub, waits for a demand.
- **No takeover.** An expired claim is never taken over by another runner: its ticket's state lives on the machine that worked it. A person releases it, and the ticket starts again from its text and its thread.
- **No runner without a desktop.** A runner on a Linux server or in a container, which keeps working when a laptop's lid is closed, is roadmap step [P12](../design/roadmap.md#p12--a-runner-without-a-desktop).

Teams are roadmap step [P10](../design/roadmap.md#p10--teams), built on `main` and not yet through its exit gate.

## Next

- [Configuration recipes](../recipes.md): code zones and their owners, which decide the tickets without an assignee; caps; merge on green.
- [Troubleshooting](../troubleshooting.md): why a ticket waits, including another runner's claim.
- The [configuration reference](../reference/configuration.md) lists `[people]`, `policy.claims` and `takes`.
