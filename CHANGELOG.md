# Changelog

What changes in each release of Owlshift, newest first. Versions follow [Semantic Versioning](https://semver.org/). A release's notes on GitHub are its section below, and the release workflow refuses a tag whose version has none.

## 0.3.0

A ticket stuck behind another that has stopped no longer waits for it, a broken base is repaired before anything else, a plan can hold a person's steps, and `init` sets up many more stacks. Designed from a measure of 178 delivered tickets, where waiting on a collision was half of a ticket's time in flight (#593).

### Upgrading

Three defaults change what a runner does after the upgrade:

- A broken base files an urgent repair ticket, assigned and admitted by the runner itself, which runs first (`[policy] base_repair`, true when absent). Set it to `false` to keep only the hold (OWL-622).
- A Build run gets its web tools, as the review roles have: its brief says `network: true` unless the personal `web_tools = false` (OWL-618).
- A daemon started by `owlshift start` moves onto a newer binary at its own path by itself, once its running tickets are drained: no `stop` and `start` after `brew upgrade` or `self-update` (OWL-616).

### Tickets that wait less

- Under `[policy] overlap = "rebase"`, a parked ticket, or one whose round has waited six hours, yields what it holds: another ticket may take its files, and it is admitted again and rebased onto what was merged before its next run. Under `exclusive` nothing is yielded (OWL-621). A parked ticket that keeps what it holds says who waits for it, in its PARKED comment, `why`, the digest and the UI (OWL-623).
- With remote claims, a colleague's later overlap is recorded from its claim, so the ticket admitted first is refreshed when the colleague's change merges, and an overlap still counts once the colleague's claim is gone (OWL-636, OWL-613).
- A Build or Watch run that ends blocked is read once by the resolver, whose verdict is retry, rebase, a person's step or park, before a person is asked (OWL-620). A Watch repair run that stops blocked on a stale base is rebased (OWL-634).

### A broken base

- The repair ticket is filed on Linear, GitHub Issues or Markdown; a second one is filed if a merged repair leaves the base still broken, two a break at most, then a person is told (OWL-622, OWL-639, OWL-643).
- Several runners file one repair per break: a repair claim on `origin` decides who files, and the search for an existing repair also reads GitHub Issues' open issues (OWL-642, OWL-640).

### A person's steps

- A plan step may be a person's, before or after the merge. The Build passes it, the pull request lists it under "For a person", the before-merge steps are asked of the decider at Watch and merge on green waits for their answer, and the after-merge steps are filed as one follow-up assigned to the decider (OWL-619, OWL-625).
- A ticket that is a person's whole is handed back at intake or plan: HANDBACK on the ticket, shown as handed back in `status`, `why` and the UI, taken again only by a gesture made after it or `owlshift do` (OWL-619, OWL-626).

### `init` and the sandbox

- `stack.run_caches` gives a project's confined runs caches they may write, one tree per project under the machine's cache directory, shown by `doctor` (OWL-617).
- `init` detects Go, poetry, pip, bundler, Gradle, pdm, Maven, Composer, .NET, Elixir and Deno, with their gates and run caches, and gives every pnpm and yarn 2 or later setup the caches it needs; Dart, Flutter and Swift get a note with their lines (OWL-624, OWL-627, OWL-628, OWL-629, OWL-638, OWL-647). It also reads the projects one folder down, and those under `apps/` and `services/` (OWL-630).
- `init` notes a tool chain a version manager installs in the home (pyenv, rbenv, asdf, pipx) and which folder to declare (OWL-631). Concurrent .NET restores share one NuGet lock (OWL-644).
- Confined runs on macOS can run `mktemp` without a template, and Java finds a temporary folder (OWL-637).

### Fixes

- File locks are released explicitly, so a child process another thread forks no longer keeps one held (OWL-646, OWL-645).

## 0.2.0

Several developers can share one project, each with their own runner (roadmap step P10). Built and played on the bench; its exit gate, two runners on one project for 72 hours, runs on this release.

### Teams

- `[people.<handle>]` in `owlshift.toml` lists each person once, with their tracker account and GitHub login. With it, `[tracker].admitters` and the zones' owners name handles, and every person with a login is a reviewer; `[forge].reviewers` beside the table is refused (OWL-582).
- A runner's operator is the account of the project's tracker key, and a draining runner takes only the tickets its operator admitted: the personal `takes` is `"mine"` by default. A project whose several admitters are served by one runner sets `takes = "all"` in that machine's personal file, at the top or under `[repositories."github.com/<owner>/<name>"]` (OWL-583). This changes what a runner of a project with several admitters takes after the upgrade.
- Desktop notifications of a question reach only the ticket's decider; the UI lists what waits for other people after the operator's, under their names, and the digest counts the operator's own (OWL-584).
- A colleague's review of a pull request starts no run by itself: the ticket's decider is asked once whether to apply it, set it aside or say what to do instead; an approval asks nothing, and the decider's own review starts the loop as before (OWL-585).
- `[policy] claims = "remote"` claims each ticket with a ref on `origin`, `refs/owlshift/claims/<ticket>`, renewed every hour, so a ticket is never worked by two machines (OWL-586). Other runners' claims count in the in-flight set: a Build waits for the files another runner's ticket holds (OWL-587).
- `owlshift release <ticket>` hands a ticket over: on the machine that holds it at any time, from another machine for the operator's own claim or an expired one. Claims show in `status`, `why` and the UI, with a release button (OWL-588).
- `docs/adopt/team.md` tells a developer how to join a project that already runs Owlshift; `doctor`'s `people`, `team gesture` and `claims` lines say whether a project is set up for a team (OWL-590).

### `init` and `doctor`

- `init` asks at the terminal for the choices it needs (OWL-596), documents every project-file key with its values and default in the file it writes (OWL-594), offers the typecheck and build scripts in the gate it guesses (OWL-595), and writes npm and pnpm setups that run inside the sandbox (OWL-593, OWL-598, OWL-599).
- A sandbox refusal of a home folder names the folder and offers to open it (OWL-597). `doctor` warns when `[forge]` lists no reviewer (OWL-600).

### Fixes

- `owlshift start` waits for a daemon still copying its binary instead of giving up after 10 s (OWL-608).
- `owlshift self-update` gives the new binary 120 s to answer `--version`: on macOS its first start waits for the system's scan of the file, which took 58 s behind other new executables (OWL-615).
- A step stopped by `owlshift cancel` could be read as not canceled when the watcher's thread was descheduled at the wrong moment; it no longer is (OWL-592).
- The UI's ticket page links its header to the tracker's issue (OWL-580).

### Documentation

- The documentation is served on the public site, owlshift.dev: concepts, a FAQ, troubleshooting by symptom, configuration recipes, a configuration reference kept in step with the template `init` writes, and a CLI reference generated from the binary (OWL-601 to OWL-607).

## 0.1.1

- The local web UI (`owlshift ui`) is laid out as Owlshift's visual identity says: the frame, the overview of projects and tickets, the ticket page with its stage stepper and runs, and the spend page, in light and dark, down to a phone's width (OWL-575).

## 0.1.0

The first release. Owlshift works a team's existing backlog with coding agents, one ticket or many, asks on the ticket whenever a decision is a person's, and delivers verified pull requests that a person merges.

### Install

- macOS, Apple silicon and Intel: `brew tap pitchopp/owlshift https://github.com/pitchopp/owlshift`, `brew trust --formula pitchopp/owlshift/owlshift` (Homebrew loads no formula of a tap it was not told to trust), then `brew install owlshift`. The archive of the release can also be downloaded with `curl` and unpacked by hand.
- Linux, x86_64 and arm64, as static binaries: `curl -fsSL https://raw.githubusercontent.com/pitchopp/owlshift/main/install.sh | sh`. The script checks the archive against the release's `SHA256SUMS` before it installs `owlshift` in `~/.local/bin`.
- An install made by the script or by hand updates itself with `owlshift self-update`, which verifies the release's signature with the key built into the binary, then its checksum, before it replaces the binary; `owlshift self-update --check` only says whether a newer release is out. Homebrew installs update with `brew upgrade owlshift`.
- Each release holds one archive per target, `owlshift-<version>-<target>.tar.gz`, their `SHA256SUMS`, and `SHA256SUMS.minisig`, its signature with Owlshift's own key: check it with `minisign -Vm SHA256SUMS -p minisign.pub`, the public key at the repository's root. Each archive holds `THIRD-PARTY-NOTICES.txt`, the licences of the crates and fonts the binary carries, and the notices of the Rust standard library and, on Linux, musl.
- Prerequisites: git 2.39.0 or later, and Claude Code installed. `owlshift doctor` says what is missing and how to fix it.

### What it does

- `owlshift init` writes a commented `owlshift.toml` for a repository, with the gate its stack suggests (Cargo, npm, pnpm, yarn, bun, Python, a Makefile), signs in to GitHub through Owlshift's GitHub App, and keeps the secrets in the system keychain. `owlshift doctor` checks the machine and the project and says how to fix each problem.
- `owlshift do` runs one ticket to a verified pull request: intake, design, design review, build and verify, each review by a fresh run, with the project's gate run confined. Questions a run needs answered are posted on the ticket; `owlshift continue` picks the ticket up once its decider has answered there.
- `owlshift watch` continues tickets as their answers come, repairs its own red or conflicting pull requests, and with `--drain` takes in the admitted tickets of the ready column, in priority order, respecting blockers, several at a time without collisions. A usage limit pauses a harness and hands the review roles to a declared fallback.
- `owlshift start` serves a project with a background service, at login with `--at-login`; `stop`, `pause`, `resume`, `cancel`, `status`, `why` and `logs` drive and explain it, and `owlshift ui` opens the local web UI. `owlshift uninstall` removes exactly what was installed.
- Merge on green is opt-in per project (`[forge] merge`); by default a person merges.
- Trackers: Linear, the repository's own GitHub Issues, and Markdown files in the repository for one machine without an account. Forge: GitHub. Harnesses: Claude Code for every role, on the user's own subscription; Codex as an opt-in reviewer.
- Every agent run, and every gate and setup command, is confined: `sandbox-exec` on macOS, `bwrap` on Linux. Native Windows is refused; WSL2 is the intended path there, not yet checked with a release binary.
