# What Owlshift does with a ticket

The [adoption pages](../README.md#adopt-a-project) say what to type; the [design](design/architecture.md) records why each rule is what it is. This page sits between the two: it follows one ticket from the moment you hand it over to the merge, so that you can predict what Owlshift will do with yours. Each part ends with the section of the design that decides it.

## The words

- **The runner** is Owlshift itself: deterministic code, no model, the only process that writes to your tracker and your forge.
- **A role** is what one stage asks of an agent: a prompt, the tools it may use, and whether it may write.
- **A run** is one execution of one role, by a harness, in the ticket's own worktree. It ends with a result the runner checks; an exit code is never taken as proof.
- **A harness** is a vendor's coding-agent CLI, run on your own subscription: Claude Code, and Codex for one optional reviewer.
- **The decider** is the one person whose answers unblock a ticket.
- **The ticket ref** is where Owlshift keeps a ticket's progress in git: its stage, plan, rounds, findings and spend.

Decided in: [architecture, section 3](design/architecture.md#3-core-model).

## Getting in: admission

Owlshift never takes a ticket on its own. A ticket enters its queue only through a person's gesture, the project's `[tracker].admit` in `owlshift.toml`: a label (such as `agent`), or the move to the ready state. The gesture counts only when its author is one of `[tracker].admitters`, read from the project file of your default branch; anyone else's gesture admits nothing, and nothing is posted on that ticket. A ticket carrying the `[tracker].exclude` label is never taken in.

`owlshift do TICKET` skips all of this: the person typing it is the gesture. It runs one ticket in the foreground, admitted or not.

Decided in: [architecture, section 5](design/architecture.md#5-architecture-overview), "The drain", "Admission".

## Waiting for its turn: the drain

The background service (or `owlshift watch --drain` in a terminal) reads the ready column about once a minute and works through it.

**Intake comes first.** Each pass runs one intake, for the admitted ticket of highest priority that has none yet, whatever its blockers. Intake asks its questions at once, so that they reach you before the ticket can hold anything up.

**A ticket is dispatched** when all of these hold:

- it is admitted and not excluded;
- it has a decider;
- its intake ran on its current text (an edit of the title or description before then runs intake again);
- it is neither waiting for an answer nor parked;
- every blocker has released it;
- fewer pull requests are open than `policy.max_open_prs` (3 by default; at the cap, intake goes on but no new work starts);
- the project has room, `policy.max_in_flight` (1 by default): the tickets dispatched and not yet delivered, not counting those waiting for an answer or parked.

**Order** is by priority (urgent, high, medium, low, then none), then the oldest ticket first. Nothing is preempted: a more urgent ticket is simply first in line.

**Blockers** are the tracker's "blocked by" relations, plus any line of the description that starts with `Blocked by` and names ticket identifiers. A blocker releases the ticket once it is completed, or once its pull request is merged. A canceled blocker does not: the ticket waits until a person removes the relation or the line. Intake turns the prerequisites it finds into relations, and names them in its `INTAKE` comment.

`owlshift why` says, for each admitted ticket, why it is not running.

Decided in: [architecture, section 5](design/architecture.md#5-architecture-overview), "The drain", "Picking" and "Back-pressure"; "Parallel", "Three counts".

## The pipeline

Once dispatched, a ticket goes through stages, each a role run by an agent:

| Stage | What it does | What it leaves |
| --- | --- | --- |
| Intake | Reads the ticket, checks what it rests on against the repository, picks the variant and the routing, names the folders it will change and its prerequisites | An `INTAKE` comment, relations to its blockers, questions |
| Design | Writes the plan: approach, files, ordered steps with their tests, risks, criteria | A `PLAN` comment (or a document attached to the ticket) |
| Design review | Fresh reviewers read the plan against the ticket and the code | Findings, then plan approval when required |
| Build | Implements the plan step by step, one commit per step, running the checks its change needs; the runner then runs the project's whole gate on the result | Commits on the ticket's branch |
| Verify | Fresh reviewers read the diff against the plan and the ticket, before any pull request | Findings, then fix runs |
| Deliver | The runner pushes the branch and opens the pull request | The pull request, a `DELIVERY` report on the ticket |
| Watch | Follows the pull request until it is merged or closed, and repairs it | Fix and rebase runs, the merge if you turned it on |

**Variants.** Intake picks one per ticket; a `variant:<name>` label on the ticket overrides it.

| Variant | Design | Design review | Verify |
| --- | --- | --- | --- |
| `trivial` | none | none | one reviewer: the critic |
| `standard` | one run | two: the critic and conformance | one: the critic |
| `risky` | one run | three: the critic, conformance and debt | two: the critic and conformance |

The critic looks for what breaks the work, conformance checks it against the ticket, its answers and your rules, and debt looks at what the change does to the code around it.

**Reviews are bounded.** At most two review passes at Design review, and two at Verify; the second pass reviews only what changed. A finding goes back to its author, Design for the plan, Build for the code, which fixes it or says why it disagrees; no finding becomes a question for you by itself. A minor finding at Design review is a note for the Build. Whatever is left after the last pass goes to one last revision or fix with no review after it, and the `PLAN` comment or the `DELIVERY` report lists what the author answered and every point it disagreed with, so you can read and overrule it. A project can add a second judge, an arbiter run that confirms or rejects each finding, with `pipeline.findings = "arbiter"`.

**Plan approval** is `pipeline.plan_approval`: `always`, `on-fork` (only when the plan names a real choice between approaches; `owlshift init` writes this one) or `never`. `risky` always asks. Once a plan is approved, no Build starts on another.

**A false premise** stops the work: when intake or design finds that the repository contradicts what the ticket rests on, it asks whether to re-scope or close the ticket, closing being yours alone, and nothing is built.

Decided in: [architecture, section 4](design/architecture.md#4-default-pipeline--gates); [section 5](design/architecture.md#5-architecture-overview), "The quality pipeline".

## Who runs each stage: roles, harnesses, tiers

Claude Code runs every role. Agent runs log in with a token of your subscription that `owlshift init` stores, never with your own Claude Code login, and no role needs a model API key. Codex is optional: `pipeline.codex_reviewer = true` adds one Codex reviewer to every review pass, beside the Claude Code ones, and a Codex that cannot serve only drops that seat.

Roles ask for a tier, never a model: `deep`, `standard` or `fast`, which `[models]` in `owlshift.toml` maps to a model of each harness.

| Run | Tier |
| --- | --- |
| Design, Build, the fix runs | The ticket's: intake's verdict, which a `tier:` or `effort:` label overrides |
| Reviewers | Another model than the author's when `[models]` names one: `deep`'s, or `standard`'s when the author ran on `deep` |
| Intake, the answer check, the arbiter | `standard` |
| The resolver | `standard`, or `deep` when the project delegates everything to it (`all`, below) |
| A rebase run on a trivial conflict | `fast`; any other conflict takes the Build's routing |

When a harness reaches its subscription's usage limit, Owlshift records the reset time and waits for it; the interrupted ticket goes on afterwards. `owlshift config show` prints every key in force and the file it comes from.

Decided in: [architecture, section 9](design/architecture.md#9-usage--routing); [section 5](design/architecture.md#5-architecture-overview), "The quality pipeline", "Per-ticket routing".

## When a ticket stops: the decision loop

An agent that needs a person stops. It never guesses on a point the ticket leaves open: what the work delivers, its behaviour, interface or wording, is a question.

**The questions are posted on the ticket**, in a `QUESTIONS` comment: numbered, each with lettered options and a recommendation. The ticket moves to your needs-input state, and you are notified.

**One person decides.** The decider is the ticket's assignee; for a ticket without one, the owner of the code zones it touches (`[zones]` in `owlshift.toml`), when they all belong to one person. Without a decider, a ticket is not worked. Other people's comments are context, never instructions.

**The resolver comes first.** Before a question reaches you, a resolver run decides what it can settle from the ticket, the repository, the project's rules, earlier answers or a fact it checks, and posts each decision in a `DECISION` comment. How far it may go is `policy.resolver_delegation`:

- `none`: it decides nothing;
- `facts`: what it observes in the repository, or checks on the web with its source;
- `discoverable` (the default): facts, and what the ticket, your comments, earlier decisions and the rules settle;
- `ticket`: also a scope question the ticket's own text answers;
- `all`: everything it can weigh, on its own judgment where nothing settles a question.

A question on security or on an irreversible action always reaches you, at every level, and so do the categories you list in `policy.always_human`; at the default level, so do questions on data loss, money, legal wording and scope. Every decision is reversible: reply to its `DECISION` comment, naming `D2` to reverse that one alone.

**Answering.** Reply on the ticket: `Q1: B`, a bare letter when one question was asked, or `ok` to take every recommendation; a thumbs-up on a one-question ask also means `ok`. A small run checks that your reply answers each question before anything resumes: what is missing is asked again, and a question about a question gets a reply. Owlshift waits for your reply to rest (`policy.quiet_window_minutes`, 10 by default) so it never reads half an answer; end your reply with `go` to skip the wait. The stage then goes on, with your answers in its thread.

**Rounds have no limit**, at any stage. What does stop a ticket is a park: two failed runs at a stage, questions still not answered after three re-asks, the project's `policy.ticket_budget_usd` reached, or a cancel. A parked ticket shows a `PARKED` comment saying why, and only a person restarts it: `owlshift continue TICKET`, or a reply `continue` from its decider on the ticket.

Decided in: [architecture, section 4](design/architecture.md#4-default-pipeline--gates), "Gate policy".

## Two tickets at once: resources and zones

Several tickets can be worked at once (`policy.max_in_flight`), but two of them never build on the same code without Owlshift knowing it.

**What a ticket holds.** When its Build is admitted, a ticket holds the files its plan names; a ticket without a plan's files (a `trivial` one, for instance) holds the folders intake named, and the whole repository when intake named none. A `zone:<folder>` label adds a folder. Named resources, `stack.resources` in `owlshift.toml` (a migration chain, say), are held by any ticket whose files their patterns reach. The held set grows with every file the ticket's branch changes, until its pull request is merged or closed.

**What it is checked against**: Owlshift's other tickets from their Build on, the repository's other open pull requests by the files they change, and the tickets people work by hand in the tracker's working state, by their `zone:` labels. A person's ticket with neither a pull request nor a label is not counted.

**A collision makes the Build wait**, never a ticket in the ready column: it waits with a `WAITING` comment naming the other ticket and the resource, and goes on by itself once the other one's pull request is merged or closed. Owlshift's ticket always yields to a person's work; between two of its own, the first dispatched goes on.

**Tolerated overlap.** `policy.overlap = "rebase"` lets two of Owlshift's tickets change the same file at once, as people do, and repairs them at merge: once one is merged, the other is rebased and its checks run again on the combined code. Named resources, a person's work, and a file two tickets already hold still make a Build wait. The default, `exclusive`, makes it wait for any shared file.

Decided in: [architecture, section 5](design/architecture.md#5-architecture-overview), "Parallel", "What a ticket holds", "The collision rule", "Resources admitted at Build" and "Tolerated overlap"; [section 7](design/architecture.md#7-state-coordination--multiple-developers), "In flight".

## After the pull request: watch and merge

Delivery opens the pull request and posts a `DELIVERY` report on the ticket: what was built, the decisions taken without you (those on judgment listed first), the findings answered, and the cost. Follow-ups the agents propose go to your tracker's proposals inbox (Triage on Linear), at most three per ticket, and never into the queue: a person promotes them.

Then Owlshift follows the pull request:

- **A red check** gets a fix run. At most two fix passes since the checks were last green, and six repairs per delivery in all; past that, the ticket parks.
- **A merge conflict** gets a rebase run.
- **A reply to a `DECISION` comment** after delivery gets a correction run on the pull request.
- **A review on the pull request** sends the ticket back to Build when it comes from a forge login listed in `[forge].reviewers` that maps to the ticket's decider. Another listed reviewer's review is context for that Build; anyone else's is ignored.

**Humans merge**, unless the project turns on merge on green with `[forge] merge`: `never` (the default), `labeled` (tickets carrying the label `owlshift:merge`) or `all`. Owlshift then merges a pull request it follows once its checks are green, never one whose ticket asked a security question, nor one with another always-human question no person settled.

Decided in: [architecture, section 5](design/architecture.md#5-architecture-overview), "Watch after delivery", "Merge on green" and "The drain", "Follow-ups".

## What Owlshift never does

These rules are code, and no configuration loosens them:

- **No agent merges, deploys, or changes infrastructure.** The only merge is the runner's own under merge on green, which the floor checks against the project file of your default branch.
- **No agent holds a credential** for the tracker, the forge or a cloud. Each run starts from an empty environment, confined to its worktree; the runner is the only writer to the tracker and the forge.
- **No agent pushes.** The runner pushes the ticket's own branch, and force-pushes it only after a rebase run.
- **Nothing written by a stranger is an instruction.** Only admitters admit, only the decider answers, and follow-ups never enter the queue without a person.
- **No model API key** is asked for, stored or passed.
- **Security and irreversible actions** always reach a person.

[SECURITY.md](../SECURITY.md) says what the confinement of agent runs protects, and what it does not.

Decided in: [architecture, section 8](design/architecture.md#8-policy-security--trust), "The floor".

## Where it runs: the background service

`owlshift start` serves the project of the current repository with a background service, started at login by your system's service manager (a LaunchAgent on macOS, a systemd user unit on Linux and WSL2). One service per machine serves every project you start, each with its own drain, and runs at most `concurrent_runs` steps at once across them (2 by default, in your personal file). It reads `owlshift.toml` from your default branch, never from your checkout. It costs no tokens at rest, restarts what crashes, and stays quiet unless a person is the blocker: a question, a parked ticket, every harness at its limit, and a daily digest. `owlshift status` says what it does, `owlshift pause` and `owlshift resume` hold and release new work, and `owlshift stop` stops it once the steps under way finish.

Decided in: [architecture, section 5](design/architecture.md#5-architecture-overview), "The background service".

## The local web UI

While the service runs, `owlshift ui` opens a page in your browser, served on `127.0.0.1` only. It shows what each ticket is doing and what it waits for, as a graph, the spend and the configuration in force, all from local state. Its actions are the CLI's gestures: pause and resume, stop serving a project, restart a parked ticket, cancel one, answer a ticket's questions or approve its plan (when your tracker key's account is its decider), and edit your personal file.

Decided in: [architecture, section 5](design/architecture.md#5-architecture-overview), "The local web UI".
