# Questions asked before installing

Short answers to what people ask before they install Owlshift. Each one links to where the answer is decided, which says more.

## What does it cost?

Owlshift is free and open source, and costs zero tokens at rest: no model runs while nothing is being worked ([design](design/architecture.md#2-design-principles), principle 2). Its runs are Claude Code runs on your own subscription, which draw on that subscription's usage limits; the figures below are the cost Claude Code reported for them, a measure of usage rather than a bill. Measured at the exit gates ([build plan](design/build-plan.md)): ten small Owlshift tickets, 2.76 USD in all ([P4](design/build-plan.md#p4-tasks--exit-gate), 2026-10-15); five small tickets of a Next.js site, about 3.3 USD in all ([P9](design/build-plan.md#p9-tasks--exit-gate), 2026-11-18); tickets of a Django and Next.js product, about 15.7 USD a merged pull request ([P6](design/build-plan.md#p6-tasks--exit-gate), 2026-10-23) and about 33 USD a pull request opened ([P7](design/build-plan.md#p7-tasks--exit-gate), 2026-10-31). `policy.ticket_budget_usd` in `owlshift.toml` caps what one ticket's runs may cost before it parks.

## Do I need an API key?

No. Owlshift never asks for, stores or passes a model API key ([design](design/architecture.md#2-design-principles), principle 9). Claude Code agent runs log in with a long-lived token of your own subscription, which you make with `claude setup-token` and `owlshift init` keeps in the system keychain, handed to the Claude Code process alone ([SECURITY.md](../SECURITY.md#what-confinement-protects)). The optional Codex reviewer keeps whatever login you configured in Codex itself.

## Is unattended use within my Claude subscription's terms?

The terms are ambiguous, leaning permitted: that is the build plan's reading of Anthropic's Consumer Terms and Claude Code's legal page on 2026-09-28, a reading and not a legal conclusion ([check C7](design/build-plan.md#results)). Owlshift drives the unmodified `claude` binary with your own subscription's login, which the legal page does not prevent, and headless use on a subscription is documented as drawing on its usage limits. Left open: whether an unattended runner working through a queue still counts as the "ordinary, individual usage" the advertised limits assume, a change to how headless usage is counted that Anthropic announced and then paused, and whether the terms read the same now that Owlshift holds a `claude setup-token` token, which has not been checked since that change (noted on 2026-10-02). Read the current terms yourself before you rely on Owlshift.

## Does Owlshift merge?

Not unless you turn it on: by default a person merges every pull request, and no agent ever merges ([design](design/architecture.md#1-purpose--positioning), section 1). A project can opt into merge on green with `[forge] merge` in `owlshift.toml` (decision D14): the runner, never an agent, then merges a pull request it follows once its checks are green, without bypassing a branch protection ([design](design/architecture.md#8-policy-security--trust), section 8, "The floor"). Questions on security, or on an irreversible action, always go to a person.

## What does the sandbox protect, and what does it not?

Every agent run, and every gate and setup command, runs in a sandbox Owlshift builds: the home folder is unreadable but for what the run needs, nothing is written outside its worktree, its git folder and its own temporary folder, and the system keychain is closed ([SECURITY.md](../SECURITY.md#what-confinement-protects)). Agents hold no tracker or forge credentials, and a run that breaks isolation quarantines the project until a person clears it. The network stays open, so a run can send anywhere what it can read, its Claude Code login included; secrets on shared mounts outside the home are not hidden; and on macOS a process that leaves its group with `setsid` survives the run ([what it does not protect](../SECURITY.md#what-it-does-not-protect)).

## Which platforms does it run on?

macOS and Linux, on Apple silicon, Intel, x86_64 or arm64, and Windows through WSL2, running as Linux ([README](../README.md#requirements)). Native Windows is refused, since Owlshift cannot confine an agent there. On Linux every run is confined with bubblewrap (`bwrap`) and secrets are kept in a Secret Service; `owlshift doctor` says whether this machine can confine a run, and how to fix it when it cannot.

## Can I use Codex?

As an extra reviewer, not as the agent that does the work. Claude Code runs every role, reviewers included; a project that sets `pipeline.codex_reviewer = true` in `owlshift.toml` adds one Codex reviewer to each design review and verify pass, beside the Claude Code ones, and a Codex that cannot serve is simply left out of that pass ([design](design/architecture.md#5-architecture-overview), section 5, "The opt-in Codex reviewer"). On Codex's terms the build plan read, on 2026-10-15, that automation on a ChatGPT login is documented while an API key is preferred, and the terms for users in the EEA were not read ([check C7](design/build-plan.md#results)).

## What leaves my machine?

What a run sends to the model provider through its CLI (the ticket, the prompts and the code it reads), what Owlshift posts on your tracker (questions, reports), and the branches and pull requests it pushes to GitHub. A ticket's plan, questions and checkpoints are kept in git refs in the machine's own checkout and not pushed ([design](design/architecture.md#7-state-coordination--multiple-developers), section 7; a public repository with a private tracker is its "Privacy" note), and no telemetry is sent anywhere ([runtime and operations](design/runtime-and-operations.md#observability--the-local-web-ui)). An agent run's network stays open, so what it can read it could send ([SECURITY.md](../SECURITY.md#what-it-does-not-protect)).

## Why is my ticket not running?

`owlshift why` says why the project's admitted tickets wait: a blocker, a ticket or pull request holding the files it would touch, the caps, a harness at its usage limit, or the quarantine; `owlshift why ENG-12` gives one ticket's reasons over time. A ticket is admitted only by an account listed under `[tracker].admitters` in the project file of your default branch ([SECURITY.md](../SECURITY.md#what-confinement-protects)). `owlshift status` says what the background service does and which tickets wait for you, and `owlshift doctor` checks the machine and the project ([README](../README.md#adopt-a-project)).

## Can a team use it?

Not yet as a supported setup: release 0.1 serves a project from one operator's runner. Several developers on one project, each with a runner on their own machine and their own subscription, is roadmap step P10, being built on `main` and not finished, its exit gate not yet run ([roadmap](design/roadmap.md#p10--teams)).
