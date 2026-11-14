# Security

## Reporting a vulnerability

Please do not open a public issue for a vulnerability. Report it privately through GitHub's private vulnerability reporting: [Report a vulnerability](https://github.com/pitchopp/owlshift/security/advisories/new), also reachable from the repository's **Security** tab. The report and the discussion that follows stay private between you and the maintainer until a fix is published.

A useful report says:

- the version (`owlshift --version`) and the operating system;
- what an attacker needs: a ticket they can write, a comment on a public issue, a repository an agent works on, a file on the machine;
- the steps that reproduce it, and what it lets the attacker do.

In scope: anything that gets past one of the boundaries in [What confinement protects](#what-confinement-protects), such as an agent run that writes outside its worktree, a secret that reaches an agent, a log, a file or a comment, a person outside `admitters` who gets a ticket worked, or a release file accepted that Owlshift's key did not sign. The [limits](#what-it-does-not-protect) listed below are known and are not vulnerabilities by themselves; a way to make one of them worse is.

## What confinement protects

Owlshift runs coding agents on your machine, on your repositories. These are the boundaries it enforces in code, which no project file can loosen. The full rules are in the [design](docs/design/architecture.md#8-policy-security--trust), section 8, "Policy, security & trust"; the live checks behind them are recorded in the [build plan](docs/design/build-plan.md).

**Every agent run is sandboxed.** Each agent, the harness and everything it starts, and every gate and setup command of the project, run inside a sandbox Owlshift builds itself: `sandbox-exec` on macOS, `bwrap` on Linux. Inside it:

- the home folder is unreadable, except the folders a run needs: the tool chains on its `PATH` and the caches the project declares (`stack.caches`) that you allow in your personal configuration (`allow_caches`), read-only, and never a folder that holds a credential;
- nothing is written outside the run's worktree, the repository's git folder (not its hooks, its configuration or the other worktrees' entries) and the run's own temporary folder;
- the temporary folders your other processes share are closed;
- the system credential store (the macOS Keychain, the Secret Service on Linux) is closed.

A machine that cannot confine a run runs none: native Windows is refused (use WSL2), and so is a Linux machine without a usable `bwrap`. `owlshift doctor` says which, and how to fix it.

**Agents hold no tracker or forge credentials.** Owlshift's own credentials for your tracker and for GitHub stay in the system keychain, read by the runner alone, which makes every write itself: comments, stage moves, pushes, pull requests. An agent never pushes and never merges. A run starts from an empty environment: a known credential variable is refused even when a project or you list it, git and gh run without a credential, and the harness loads no settings file and no MCP server. Merging is a person's, unless you turn on merge on green for a project (`[forge] merge`), in which case the runner merges, never an agent, and never by bypassing a branch protection.

**Owlshift never asks for a model API key.** The one model login it holds is the token agent runs log in to Claude Code with, made by you with `claude setup-token` and stored by `owlshift init` in the system keychain. It is handed to the Claude Code process alone, on a pipe: never in an environment variable, a file Owlshift writes, an event, a comment or a message, and replaced by `<redacted>` in the run's logs. Codex, when a project adds the optional Codex reviewer, keeps its own login in its own configuration.

**Every run is checked after it ends.** After each run and each gate, Owlshift checks that your checkout is untouched, that the shared git files and the other worktrees are untouched, that the changes are inside the run's worktree, and that the run's branch was only added to, never rewritten. A run that fails the check quarantines the project: every command refuses it on that machine, and `owlshift doctor` says so, until a person runs `owlshift unquarantine` in the project's repository and types the project's name (`owner/repo`) at a terminal. Owlshift notifies you of a quarantine once, and tells each ticket it holds up.

**Only the people you list start work.** A ticket enters Owlshift's queue only through an admission gesture (a label or a state) made by an account listed under `[tracker].admitters` in `owlshift.toml`. On a public repository, anyone can open an issue or comment, but only those accounts admit. Text from anyone other than the ticket's decider reaches an agent as quoted data, never as instructions, and cannot answer a question. Follow-ups agents propose never reach the ready queue without a person. Questions whose category is security or an irreversible action always go to a person, whatever the project's delegation settings.

**Releases are signed.** Each release's `SHA256SUMS` is signed with Owlshift's own key, whose public half is [`minisign.pub`](minisign.pub) at the repository's root and is built into the binary. `owlshift self-update` verifies that signature, then the archive's checksum, before it replaces the binary. The Homebrew formula is written on `main` only from a `SHA256SUMS` that signature verifies. The Linux install script, `install.sh`, checks the archive's checksum only, which catches a damaged download, not someone able to replace the release's files; to check the signature yourself, run `minisign -Vm SHA256SUMS -p minisign.pub` in a folder holding the release's `SHA256SUMS`, `SHA256SUMS.minisig` and the key. The release workflow and its formats are in the [build plan](docs/design/build-plan.md), "P9 · Open-source readiness", parts 10 and 11.

## What it does not protect

These limits are known and accepted. They are recorded, with the live checks that found them, in the [design](docs/design/architecture.md#8-policy-security--trust) and the [build plan](docs/design/build-plan.md).

- **The network stays open.** A run can reach anything on the network, and send anywhere what it can read: the code of its worktree, and the Claude Code login it runs with, which its shell commands inherit. If you suspect a run misused that token, revoke it in your Claude account, make a new one with `claude setup-token`, then run `owlshift init --replace-agent-token` in any folder: it asks for the new token alone and stores it in place of the old one (system keychain, service `owlshift`, account `claude-agent`), with no keychain edit by hand. Every project of the machine uses the new token from its next run.
- **Secrets kept outside the home folder, on shared mounts, are not hidden** from a run: the sandbox closes the home folder and the credential store.
- **On macOS, a process that leaves the run's process group with `setsid` is not stopped with the run.** And `sandbox-exec`, which confines runs there, is deprecated by Apple and does not nest.
- **An agent may change any file it is allowed to write in its worktree**, the repository's agent configuration included (for example `.claude/` rules, settings and hooks). No later Owlshift run reads a settings file of the worktree, so such a change acts on no other run; but it reaches the pull request, and acts on whoever checks that branch out. Review an agent's pull request as you would any code, configuration included.
- **A web page a run fetches can carry instructions.** The planning and review roles (design, design review, verify, the arbiter and the resolver) have web search and web fetch, so that they check facts rather than guess them. Their prompts treat a fetched page as data, and what they produce is reviewed or reversible, but nothing guarantees a model ignores what a page says. `web_tools = false` in your personal configuration turns these tools off; a run's shell keeps the network either way.
- **The ticket text can change after admission.** On GitHub Issues, an issue's author can edit its text after an admitter labelled it, and a later step reads the edited text under the same admission.
- **A push made before a quarantine stays.** Once a quarantine stands nothing more is pushed or posted, but a push already sent is not taken back.
- **Typing the project's name is a speed bump, not a proof.** `owlshift unquarantine` asks for it at a terminal to stop scripts and an agent's non-interactive shell; an agent Owlshift launches cannot reach the quarantine marker at all, but any person or program with your shell can delete it.
- **The code is agents' code.** Owlshift's reviews, verification and gate reduce mistakes; they do not prove a change correct or safe. A person reviews and merges, unless a project opted into merge on green.
