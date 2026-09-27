# Instructions for coding agents

Owlshift is in its design phase. Before changing anything, read the design in `docs/design/`, starting with `architecture.md`, `roadmap.md` and `build-plan.md`.

- The design documents are the reference. A change that contradicts them starts with a change to the document, reviewed like code.
- Work items live in the Owlshift Linear workspace, team `OWL`, one project per roadmap step. Put the issue ID in the branch name (`owl-12-short-title`) so the pull request attaches to its issue. Blockers are recorded as issue relations: do not start an issue whose blockers are not done.
- Everything in this repository is written in English: code, comments, docs, commit messages.
- P0 starts with the live checks listed in `docs/design/build-plan.md`. An assumption about an external tool (a CLI flag, an API behaviour) is verified with a real call before code depends on it, and the dated result is written next to the decision it settles.
- The core crate stays pure: no I/O in `owlshift-core`.
- Guardrails are enforced by the runner, never requested of a model: agents launched by Owlshift get no tracker or forge credentials and never merge.
