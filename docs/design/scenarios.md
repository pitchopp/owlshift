# Owlshift — scenarios

Status: draft, 2026-09-27. Seventeen user stories fix the product's behaviour from P1 to P10 ([roadmap](roadmap.md) maps each one to its step); each acceptance line is meant to become an end-to-end test on the Markdown tracker with a fake harness.

## Nominal flow

### S1. A ticket with no questions

*As a delegator, I admit a ticket and later find a verified pull request to review, without opening a session.*

The scanner sees the admitted ticket; intake finds no question and records resources and routing; the scheduler claims it once blockers and resources allow; design, review, build and verify run; the writer opens the PR and posts the delivery report.

- No human interaction between admission and the review request.
- A delivery report on the ticket: decisions taken, state of the complete check set, what remains for a human.
- The PR is announced green only when every required check passed.
- Owlshift never merges.

### S2. The needs-input loop

*When an agent needs my decision, I find it on the ticket, answer there, and work resumes by itself, as many times as needed.*

1. The run pushes its branch and checkpoint and returns `questions`. The writer posts numbered questions (Q1 to Qn) with context, options and a recommendation, moves the ticket to needs-input, records the stage to return to, and notifies the decider.
2. The decider replies in one or several comments.
3. The reply counts once the decider's last comment is older than the quiet window (an edit restarts it), or immediately if it ends with `go`.
4. The answer check classifies each question, and the round has one outcome, the first that applies. A counter-question, even next to missing answers: an answer in the thread, the ticket stays, nothing is re-asked and no re-ask is counted; after the decider's next reply, the answer check classifies every question of the same ask again. Something missing: a *re-ask* comment lists only the open questions and why. All answered: a *resume* comment restates what was understood, the ticket returns to its stage, a fresh run resumes from the checkpoint.
5. Before delivery the run re-reads the thread. A comment posted after the resume is integrated, or sends the ticket back to needs-input if it contradicts the work.

- Every question is understandable without any transcript; all questions of a round sit in one comment.
- A bare "ok" means the recommendation when there is one, and is ambiguous otherwise.
- No comment from the decider is ever ignored, even one posted after the resume.
- An incomplete answer never starts a run.
- A resumed run never redoes a step its ledger marks done.
- Rounds are unlimited; from the third, the question comment suggests re-scoping the ticket.

### S3. Questions at intake

*As soon as a ticket is admitted, the predictable questions reach me before the ticket can block anything.*

- Intake runs once per ticket, and again only when the description changes (content hash).
- A ticket without a valid intake record is never dispatched.
- Intake questions send the ticket to needs-input with "ready" as the stage to return to.
- Intake turns dependencies written in plain text into tracker relations (or the parsed convention), and records routing and resources.
- After intake, readiness is computed without any model.

## Scheduling

### S4. Unblocked by a merge

*When a blocking ticket is merged, the ticket it blocked starts by itself.*

- The blocked ticket is dispatched at the first scan after the merge.
- Its branch starts from the updated default branch.
- Chains of blockers unroll with no batch boundary.

### S5. Collision with work in flight

*A ready ticket that would touch what another ticket holds waits for its turn.*

- In flight means claimed by any runner, in progress by a human, or an open PR not yet merged.
- Two tickets never hold overlapping code zones, the same exclusive resource (a migration chain, for instance) or the single browser slot at once.
- The waiting ticket shows why, for example "waits for PROJ-118, zone `leases/`", in `owlshift why` and in the digest.

### S6. The human is the bottleneck

*Owlshift does not pile up pull requests nobody will review.*

- Configurable caps on concurrent runs and on open PRs awaiting review; above either, nothing new starts.
- A daily digest: tickets waiting for an answer and what they block, PRs to review, follow-ups to triage.
- A notification goes out only when a human becomes the blocker (question, re-ask, parked ticket, every harness at its usage limit), never for progress or a green check.

## Exceptions

### S7. The resolver decides

*I am not asked what the ticket, the repository or the docs already settle.*

- The resolver is a run separate from the one that raised the question.
- Its decision is posted as a *decision* comment stating what settled it.
- Replying to that comment reverses it and starts a correction run.
- Always-human categories (security, data loss, money, legal wording, irreversible external actions, scope changes) always reach a human.

### S8. A false premise

*If a live check shows the ticket rests on something untrue, the agent stops and proposes a re-scope or a closure.*

- The ticket goes to needs-input with the evidence.
- Nothing is implemented anyway.
- The dated finding is recorded where the project keeps external facts, if it declares such a place.

### S9. After the pull request

*A PR that degrades is repaired without me, unless a decision is at stake.*

- A red check or a confirmed finding starts a fix run; at most two review passes, the residue becomes a proposed follow-up.
- A merge conflict caused by another merge starts a rebase run on a fast tier, escalated on a non-trivial conflict.
- A prerequisite (environment variable, flag, infrastructure) or a finding to arbitrate sends the ticket to needs-input, returning to review.

### S10. Circuit breakers and usage limits

*A ticket that goes in circles does not burn a night of usage, and a subscription limit pauses work instead of breaking it.*

- Three re-asks on the same round, or two failed runs, park the ticket with a *parked* comment giving the reason and what would restart it.
- A harness that reports its usage limit is paused until the reset time it reports; its roles switch to their declared fallback meanwhile. With no fallback left, dispatch waits for the earliest reset and the operator is notified once.
- A run interrupted by a usage limit resumes from its checkpoint after the reset; it is not counted as a failure.
- `budget_usd` is a dollar cap on each run of a harness, not on a day's total, whatever the billing; Claude Code enforces it, Codex has no such option. A run stopped by it alone counts as a failed run.
- Re-asks for an incomplete answer do not count as question rounds.

## Operations

### S11. Recovery after an interruption

*Sleep, reboot or crash: nothing is lost and nothing starts twice.*

- State is rebuilt from the tracker, the forge and the git refs alone.
- A claim whose lease expired is taken over; a live claim is never.
- A ticket in progress with no live run resumes from its checkpoint.

### S12. The human keeps control

*I can pause everything, exclude a ticket, work one myself, or run other tools beside Owlshift.*

- `owlshift pause` stops dispatch; running roles finish.
- An exclusion label keeps a ticket out for good.
- Owlshift only touches tickets it claimed.
- A ticket a human is working on counts as in flight for collisions.

### S13. Proposed follow-ups

*When an agent finds work outside its ticket, I get a clean proposal I accept or decline in one gesture; it never delays the original ticket and never starts without me.*

- The proposal lands in the tracker's triage inbox, or carries a proposal label where there is none.
- It follows the project's ticket conventions and stands on its own: why, evidence (`file:line`), source (agent, reviewer, CI), definition of done; related to its parent, blocked by it when it needs the parent's code.
- Accepting puts it in the backlog, never in the ready queue; a human promotes it.
- No duplicate of an open ticket or of a declined proposal; a recurring theme becomes one ticket, not one per occurrence; at most three per parent, the rest noted in the delivery report.
- An agent cannot defer part of its own acceptance criteria to a follow-up: that is a question.
- No intake before a human promotes the proposal.

## Teams, projects and harnesses

### S14. Several developers

*On a shared project, each developer's runner works the tickets that developer delegated, on that developer's subscription, and only the ticket's decider can unblock it.*

- A runner never takes a ticket its operator did not delegate; a team-server runner takes every admitted ticket.
- Only the decider's answers (assignee by default, or the zone owner) count; other comments are context, and the answer check says so.
- A PR review comment and a ticket comment from the same person count as one decider, through the identity map.
- Two runners on two machines never claim the same ticket.

### S15. Adopting a project

*I adopt Owlshift on a repository in minutes, without reading the code of Owlshift.*

- `owlshift init` detects the stack, connects the tracker, stores tracker and forge secrets in the system keychain and writes a commented project file. It never asks for a model API key.
- `owlshift doctor` checks every capability the config relies on, including that each configured harness CLI is installed, that Codex is logged in (a warning until the review roles exist in P5, since `owlshift do` does not run Codex before), that agent runs have their Claude Code token (made with `claude setup-token`, stored by `owlshift init`), and names what is missing and how to fix it.
- A missing required capability stops `init`, never a run.

### S16. Several projects on one machine

*One runner serves all my repositories fairly.*

- Machine-wide caps apply across projects: concurrent runs, the single browser slot, port ranges, and the usage of each subscription.
- Priorities are compared across projects; an idle project costs nothing.

### S17. Mixing harnesses and usage-limit fallback

*I choose Claude Code, Codex or both, per project, per role or per ticket, and a subscription reaching its limit does not stop the flow.*

- The effective harness is resolved in order: project default, role, ticket verdict, what the machine has.
- A reviewer runs on another model family than the author whenever one is available.
- A harness at its usage limit switches the role to its declared fallback and logs it on the ticket; the original harness is used again after its reset.
