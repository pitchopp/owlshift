# Owlshift — roadmap

Status: draft, 2026-09-27. Twelve steps (P0 to P11), each shippable, testable and usable on its own before the next one starts. Owlshift works on its own backlog from P1 and on Locary's from P5.

| Step | Name | What you can do at the end |
| --- | --- | --- |
| P0 | Foundations | Build and test the project on three platforms; `owlshift doctor` tells you whether a machine is ready |
| P1 | One ticket, one PR, on demand | `owlshift do OWL-12` turns one ticket into a verified pull request, on your own Claude subscription |
| P2 | Questions on the ticket | The agent stops and asks on the ticket; you answer there; `owlshift continue` picks up from the checkpoint |
| P3 | Watch mode | `owlshift watch` notices your answers and resumes by itself, and repairs its own red PRs |
| P4 | The drain | Owlshift pulls admitted tickets from the ready column, in priority order, respecting blockers, one at a time |
| P5 | Quality pipeline | Plan, review by another model, build, verify; plan approval by policy; per-ticket routing |
| P6 | Parallel | Several tickets at once without collisions; usage limits pause a harness and fall back to another |
| P7 | Background service | Starts at login, survives sleep, logouts and restarts, notifies you only when you are the blocker |
| P8 | Local web UI | Configuration, tickets, agent and orchestrator logs in the browser |
| P9 | Open-source readiness | First public 0.x release: GitHub Issues, Markdown tracker, `init` detection, a second project |
| P10 | Teams | Several developers on one project; a team-server mode |
| P11 | Menu-bar app | A Tauri shell around the web UI, with native notifications |

Three choices shape this order. The central bet, the question loop, comes at P2, before anything else is polished. Parallelism comes late (P6): run serially, tickets cannot collide, so the backlog drains from P4 without the hardest scheduling problem solved. And `/parallel`-style batch orchestration becomes unnecessary for Owlshift's own backlog at P4, and for Locary's at P6.

## P0 · Foundations

A repository that people and agents can build in safely, and a first command that is already useful.

- Live checks C1, C3, C6 and C7 (Claude part) run and recorded ([build plan](build-plan.md)).
- Cargo workspace with its six crates; CI on macOS, Linux and Windows running format, lint and tests.
- Contracts written and schema-checked: brief, `result.json`, project and personal config (TOML), events, git ref layout.
- Test harness: a fake harness, an in-repository test tracker, a local bare git remote, a scenario runner.
- CLI skeleton: `owlshift --version`, `owlshift doctor` (git, `claude` and `codex` installed, `codex` logged in, the token agent runs log in to Claude Code with stored by `owlshift init`), `owlshift config show`.
- Project tooling: the Owlshift Linear workspace (states including Needs Input and Triage, labels, one project per step), a contribution guide, decision D10.

**Exit gate.** CI green on the three platforms; one scenario runs end to end with the fake harness and the test tracker; `owlshift doctor` reports the maintainer's machine correctly.

## P1 · One ticket, one PR, on demand

`owlshift do OWL-12` replaces launching one agent by hand.

- Linear tracker adapter: read a ticket, post a comment.
- Claude Code harness: headless run logged in with the stored `claude setup-token` token, model, effort, permission level, usage capture.
- Executor: worktree, brief, result validation, isolation check; the project's gate commands before delivery.
- GitHub forge adapter: push the branch, open the PR, read the complete check set.
- The floor from day one: no merge (until a project opts into merge on green, D14, P7), no tracker credentials for agents, an exit code never trusted.
- Delivery report on the ticket. Scenario S1, triggered by hand.

**Exit gate.** One real Owlshift ticket delivered as a PR by `owlshift do`, and one run that cannot finish stopped without delivering, with no guardrail breach. Relaxed from three delivered tickets on 2026-10-01 (OWL-113): without questions on the ticket, a run that needs a decision can only stop, so more P1 runs were left to P2; the runs are recorded in the [build plan](build-plan.md) ("P1 tasks & exit gate"). From here, Owlshift builds Owlshift.

## P2 · Questions on the ticket

The central bet, used by hand.

- `questions` results become a marked, numbered comment; the ticket moves to Needs Input with its return stage recorded.
- Checkpoint on a git ref; `owlshift continue OWL-12` restarts from it without redoing finished steps.
- Answer check (answered, partial, unanswered, counter-question), re-ask of what is missing, late comments re-read before delivery.
- Resolver: discoverable questions decided and logged as reversible decisions; always-human categories enforced.
- Being told a question is waiting: both, since check C4 passed (decision D8). Owlshift comments as a Linear app user, so Linear notifies the decider on the channels they chose there, and a local desktop notification tells the operator on their machine. Scenarios S2 and S7.

**Exit gate.** Three tickets through at least two question rounds, one of them with an incomplete answer correctly re-asked. Passed on 2026-10-07 (OWL-142); the runs are recorded in the [build plan](build-plan.md) ("P2 tasks & exit gate").

## P3 · Watch mode

`owlshift watch` in a terminal replaces typing `continue`.

- Foreground loop over the tickets Owlshift started: quiet window, `go`, automatic resume.
- After the PR: a red check starts a fix run (two fix passes at most); a conflict starts a rebase run. Scenario S9; the rules are [architecture](architecture.md) section 5, "Watch after delivery" (OWL-208).
- After the PR: the decider's reply to a DECISION comment starts a correction run that makes the pull request follow it. Scenario S7; the rules are [architecture](architecture.md) section 5, "A reversed decision" (OWL-146).

**Deviation from S9** (OWL-208, 2026-10-10). P3 repairs what CI and the forge report. A red check starts a fix run, a conflict a rebase run, and a run that needs a prerequisite or an arbitration asks the decider. The rest of S9 waits:

- A confirmed finding, and the review passes S9 bounds, need P5's review roles. Until then, "two review passes" means two fix passes: fixes pushed for red checks since the pull request was last green. P3 also caps all the repairs of one delivery at six.
- The residue past the bound becomes a proposed follow-up only once P4 files follow-ups. P3 parks the ticket instead, with a PARKED comment. P4 keeps that park for a red check (OWL-244): passing checks are the ticket's own acceptance, which S13 forbids deferring to a follow-up, so the residue S9 proposes is a confirmed finding's, from P5.
- Human review comments on the pull request are not read before P5. Telling whose review counts also needs P10's identity map.
- S9 and OWL-211 send these questions to needs input "returning to review". In P3 they return to Watch, whose visible stage is review, so this is the same thing.
- OWL-211's "through the P2 question loop (… resolver …)" is narrowed: the resolver is skipped. A fix or rebase run's questions go to the decider, as a late-comment run's do, since they are acts and arbitrations the agent cannot settle. The rest of the loop is unchanged: the answer check, re-asks and replies. Reversed on 2026-10-18 (OWL-342): those questions go through the resolver first, as every role's do.

**Exit gate.** Three Owlshift tickets run under `owlshift watch`, from `do` to a merged or parked pull request, meeting all of these:

- Every reply that counts is continued by watch.
- Every delivered pull request that goes red or conflicts is repaired or parked by watch.
- No `continue` is typed except where watch leaves the ticket to a person by design: restarting a parked ticket, or a ticket left at Build by a usage limit, a failed delivery or a refused push before any question round ([architecture](architecture.md) section 5, "Which tickets"). Each such `continue` is recorded with its reason. Since 2026-10-18 (OWL-340), watch also resumes a ticket left at Design, Design review, Build or Verify with an answered round (round > 0) while the tracker still shows it working; the parked ticket and a Build left at round 0 stay a person's. Since 2026-10-19 (OWL-367), whatever its round: only the parked ticket stays a person's.
- At least one delivered pull request is repaired after a red check, staged if none occurs, as P2's rounds were.
- One pull request is repaired after a conflict, if a conflict occurs.

Reworded on 2026-10-10 (OWL-208) from "A full day of use without typing `continue`", which these restarts, left to a person by design, would fail whatever P3 builds. Narrowed on 2026-10-13 (OWL-212) from a full working day to three tickets, as P2's gate counted tickets: the rules above are exercised by the tickets, not by the hours, and the staged red check covers the repair a short run may not meet. Passed on 2026-10-13 (OWL-212); the runs are recorded in the [build plan](build-plan.md) ("P3 tasks & exit gate").

## P4 · The drain

Owlshift picks its own work, one ticket at a time.

- Admission gesture, ready-column polling in priority order, blockers from relations and from the text convention.
- Intake stage: early questions, dependencies turned into relations, the routing record.
- Cap on open PRs awaiting review; follow-up proposals to Triage, deduplicated. Scenarios S3, S4, S6, S12, S13.
- `owlshift watch --drain` runs it, in the foreground; the rules are [architecture](architecture.md) section 5, "The drain" (OWL-244), and the [build plan](build-plan.md), "P4 · The drain", says who builds what and how the exit gate is run.

**Readings and deviations** (OWL-244, 2026-10-13).

- S6's "nothing new starts" at the pull request cap stops new Builds; intake goes on, since it opens no pull request and asks its questions early.
- S3's intake records routing and dependencies; resources and zones wait for P6, as that step already says (declared at intake since its design, OWL-398). The routing verdict is recorded and shown in P4, and followed from P5.
- S12's `owlshift pause` comes with the background service (P7): until then, stopping watch stops the drain. The exclusion label is P4's.
- Admission by delegation waits: decision D8's Linear app takes no delegation scope. A label or the ready state admits.
- A ticket left at Build by a usage limit, a failed delivery or a refused push holds the drain's one slot until watch resumes it ([architecture](architecture.md) section 5, "Which tickets"), or a person types `continue`. Watch did so with an answered round (round > 0) only (OWL-340), a Build left at round 0 needing `continue`, as P3 leaves those restarts to a person; since 2026-10-19 it does so whatever the round (OWL-367, "One step a pass"), since watch runs one stage step a pass and the drain's own tickets wait at their stage between two steps. S6's Build cut by a usage limit is resumed once the limit has reset, and a `do` typed by hand that a stop left there is picked up by a running watch too.

**Exit gate.** Ten Owlshift tickets drained serially, at least one started by the merge of its blocker.

## P5 · Quality pipeline

PR quality at least on par with the current `/parallel` workflow.

- Stages design, design review, build, verify; variants `trivial`, `standard`, `risky`; plan approval policy.
- Reviewers on another model than the author's, on Claude Code; per-ticket routing (harness, tier, effort); false-premise stop. Scenarios S8 and S17 (routing part).
- The rules are [architecture](architecture.md) section 5, "The quality pipeline" (OWL-296), and the [build plan](build-plan.md), "P5 · Quality pipeline", says who builds what and how the exit gate is run.

**Readings and deviations** (OWL-296, 2026-10-15).

- Verify reviews the diff before the pull request opens; the complete CI result stays Watch's, through P3's fix runs. Browser QA, "when declared", waits for P6, where the browser becomes a resource; P6 leaves it to a later step of its own (P6's readings, below).
- S9's confirmed finding and its two review passes are P5's; a finding is triaged by the author's own fix run by default, or confirmed by an arbiter run when the project sets `pipeline.findings = "arbiter"`.
- Human review comments on the pull request, deferred by P3, are read from P5, from the forge accounts `[forge].reviewers` lists; P10's identity map replaces that list.
- S8 stops on a premise the repository or its documents contradict. Intake stays without network: a premise only the outside world can check is a question. Recording the dated finding where the project keeps external facts waits for a key that names that place.
- S17's rule that a reviewer runs on another family applies when one is available and configured. Decided by the maintainer on 2026-10-18 (OWL-347): P5's reviewers all run on Claude Code, on another model than the author's when the configuration names one, so that no second subscription is required; the Codex critic P5 first built is unwired, and a reviewer of another family comes back as an opt-in complement: since OWL-348, `pipeline.codex_reviewer` adds a Codex reviewer to each pass, beside the Claude Code reviewers.
- S17's routing part: the routing verdict's tier and effort are followed; the authoring roles stay on Claude Code (D4), so a verdict naming Codex for them is shown and not followed. Its fallback part, a harness at its usage limit switching to another, is P6's, as that step says, for the review roles only (P6's readings, below).
- Locary is in a Linear workspace of its own, while a machine kept one Linear key and one GitHub credential for all its projects (keychain accounts `linear` and `github`): its first tickets need secrets per project, named by the project's GitHub repository since OWL-308 ([build plan](build-plan.md), "P5 · Quality pipeline", part K).
- Locary's first delivery (2026-10-17) showed that a fine-grained token cannot read the status check rollup watch follows. Since OWL-336 `owlshift init` signs in through Owlshift's own GitHub App with the device flow, its token kept per project under `github-app`, and a personal token is the expert option ([architecture](architecture.md), D13; [build plan](build-plan.md), "GitHub App sign-in (OWL-336)").

**Exit gate.** Five PRs judged at least as good as `/parallel` output by the maintainer; first Locary tickets delivered.

## P6 · Parallel

Owlshift replaces `/parallel` on Locary.

- Zones and resources declared at intake; collisions checked against every ticket in flight, human work and open PRs included; `owlshift why`.
- Machine-wide caps across projects; usage-limit detection, pause until reset, fallback harness. Scenarios S5, S10, S16, S17.
- The rules are [architecture](architecture.md) section 5, "Parallel", and section 8, "Isolation among steps" (OWL-398), and the [build plan](build-plan.md), "P6 · Parallel", says who builds what and how the exit gate is run.

**Readings and deviations** (OWL-398, decided with the maintainer on 2026-10-20).

- One `owlshift watch --drain` per project, each step of a ticket in a child process, the machine's caps shared through files: P7's single runner is not brought forward (P7 builds a supervisor instead, which runs these watches: OWL-432). Section 5's "Deployment" stays P7's, and what P7 keeps of P6 is written there ("What P7 keeps").
- S5: tickets in flight never hold conflicting resources as declared and read, against Owlshift's tickets, other open pull requests and people's tickets. Zones declared at intake are a prediction: a run's changes outside them are found once its step ends, the later ticket then waits, and a conflict is repaired by a rebase run. So two tickets can change the same folder for one step, a deviation from "never hold overlapping code zones"; the exit gate judges outcomes. The single browser slot is not taken by anything in P6 (below), and the digest that shows waits is P7's.
- S5 (OWL-428, decided with the maintainer on 2026-10-22, found at the exit gate), a deviation from S5's "a ready ticket waits": a ticket holds nothing before its Build, whose admission takes the plan's files rather than intake's folders, so Designs and Design reviews run beside the Builds whose zones they would have waited for ([architecture](architecture.md) section 5, "Resources admitted at Build"). Replayed on Locary's drain, the backlog drains in 67 hours instead of 192.
- S5 and S12: a ticket a person works with neither a pull request nor a `zone:` label is not counted in flight, and is warned of in `owlshift why` and by an event. Holding the whole repository for it would stop the drain whenever a person works a ticket by hand.
- S6: the configurable cap on concurrent runs is built (`concurrent_runs`, and `policy.max_in_flight` per project); the digest stays P7's.
- S10: the usage part is built, a harness paused until its reset, checked before each run, the operator notified once per limit. "Its roles switch to their declared fallback" holds for the review roles only, as S17 below.
- S16: machine-wide caps on concurrent runs and on each subscription's usage are built; an idle project costs nothing, as from P3. Priorities compared across projects wait for a single runner, which P7 does not build (OWL-432): the first watch to take a free slot runs. Port ranges and the browser slot come with browser QA.
- S17's fallback part: a declared fallback harness takes over Design review's and Verify's reviewers only. The authoring roles stay on Claude Code (D4) and wait for the reset, and so do the arbiter, the resolver, intake and the answer check.
- Browser QA, which P5 left here, and port ranges are a later step of their own: P6's gate is about collisions, and browser QA is a role with its own tooling. `Resource::Browser` stays modelled and untaken.
- Path-scoped rules, which the build plan tied to zones at intake: zones arrive, but injecting a project's rules per zone needs a project-file key and brief changes no collision depends on, so they are left for a later ticket.
- Claims with leases on git refs stay P7's, as that step says: on one machine, a lock per ticket keeps two commands off one ticket. P7's readings move them to P10 (OWL-432).

**Exit gate.** A week on Locary with three concurrent tickets and no collision. Judged on outcomes ([build plan](build-plan.md), "P6 tasks & exit gate"): no file changed by two Owlshift pull requests in flight together, no rebase run caused by a sibling's merge on the same files, no isolation breach caused by a sibling step; the week's throughput and cost are compared with a `/parallel` lot of like size. Shortened on 2026-10-23 (OWL-408) by the maintainer from a week to the 2.9 days run, three tickets or more in flight on two of them: 23 pull requests merged with no collision settled what the gate checks. Passed on 2026-10-23; the runs are recorded in the [build plan](build-plan.md) ("P6 tasks & exit gate").

## P7 · Background service

Set it and forget it.

- `start --at-login`, `stop`, `uninstall` with its manifest, `doctor` cleanup, self-healing launcher, single instance.
- Recovery after sleep or a stop, keep-awake, `pause`, `resume` and `status`, the daily digest. Scenario S11.
- Merge on green, opt-in per project (decision D14, [architecture](architecture.md) section 5, "Merge on green"). Scenario S18. Not part of P7's exit gate: it is a Watch feature placed here because P7 is the next step built, and the gate stays about running unattended.
- The rules are [architecture](architecture.md) section 5, "The background service" (OWL-432), and [runtime and operations](runtime-and-operations.md), "How it runs" and "Install, lifecycle & uninstall"; the [build plan](build-plan.md), "P7 · Background service", says who builds what and how the exit gate is run.

**Readings and deviations** (OWL-432, decided with the maintainer on 2026-10-23).

- A supervisor, not a single runner: `owlshift daemon` holds what one process per machine must hold (one instance, the service to install and remove, the digest) and runs one `owlshift watch --drain` per served project as its child, P6's runtime unchanged. The single runner that compares two projects' priorities is not built: S16's priorities stay P6's reading, the first watch to take a free slot runs.
- S11: a stop by the service manager (a logout, a shutdown, `launchctl bootout`, `systemctl --user stop`) is a checked stop: each step's runs are stopped and their isolation checked before the watch exits, so the ticket resumes from its checkpoint at the next start. A crash, a hard kill or a power loss still leaves the run unchecked, which stays the quarantine of section 8, notified: a deviation from S11's "nothing is lost", which the exit gate measures. Checking such a run afterwards, from a persisted snapshot, is left for a later ticket if the gate shows it is needed.
- S11's claims with leases on git refs move to P10: on one machine P6's ticket locks keep a ticket to one runner, and a lease only matters between runners.
- `self-update` moves to P9, which makes the first signed release; P7 restarts onto a binary the package manager replaced.
- The notifier adapter keeps the desktop channel of P2 alone; the daily digest is one notification that points to `owlshift status`. Other channels come later.
- No control channel: `pause`, `stop` and the upgrade's drain are files the watches read at each pass, `status` reads the files P6 already keeps. The local API arrives with the web UI (P8).
- A served project's configuration is read from its default branch in Owlshift's own clone, as its gate and rules already are; a watch typed in a terminal keeps reading the checkout it runs in.

**Exit gate.** Seven days unattended with no double dispatch.

## P8 · Local web UI

The overview, ticket, orchestrator and configuration screens described in [runtime & operations](runtime-and-operations.md), served on `127.0.0.1` behind a per-install token.

**Exit gate.** A day of operation followed from the UI alone.

## P9 · Open-source readiness

- GitHub Issues adapter, public Markdown tracker, conformance suites, `init` stack detection. Scenario S15.
- A second project on another stack (decision D6); the Docker server image; signed release 0.1; the repository made public.
- `owlshift self-update`, for an install made without a package manager (P7's reading).

**Exit gate.** The second project adopts Owlshift with configuration only, no change to the core.

## P10 · Teams

Deciders, identity map across tracker and forge, per-developer runners, team-server mode, agent identities (Linear app user, GitHub App), claims with leases on git refs and the takeover of an expired one (S11's part, P7's reading). Scenario S14.

**Exit gate.** Two developers share one project for two weeks.

## P11 · Menu-bar app

A Tauri 2 shell around the P8 UI: tray icon, native notifications, login item, answering a gate from the app.

**Exit gate.** The app replaces the terminal for daily use on macOS and Windows.

## Scenario coverage

| Scenario | Step |
| --- | --- |
| S1 | P1 |
| S2, S7 | P2 (automatic resume and S7's correction run in P3) |
| S9 | P3 (red checks and conflicts; findings and review passes in P5) |
| S3, S4, S6, S12, S13 | P4 |
| S8 | P5 |
| S5, S10, S16, S17 | P6 (S17's fallback for the review roles only; S5's browser slot, S16's ports and priorities across projects later: P6's readings) |
| S11, S18 | P7 (S18 outside P7's exit gate; claims and leases in P10, a run cut by a crash checked later: P7's readings) |
| S15 | P9 |
| S14 | P10 |
