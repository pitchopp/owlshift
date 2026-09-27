# Instructions for coding agents

Owlshift is in its design phase. Before changing anything, read the design in `docs/design/`, starting with `architecture.md` and `v0-build-plan.md`.

- The design documents are the reference. A change that contradicts them starts with a change to the document, reviewed like code.
- Everything in this repository is written in English: code, comments, docs, commit messages.
- v0 starts with the live checks listed in `docs/design/v0-build-plan.md`. An assumption about an external tool (a CLI flag, an API behaviour) is verified with a real call before code depends on it, and the dated result is written next to the decision it settles.
- The core crate stays pure: no I/O in `owlshift-core`.
- Guardrails are enforced by the runner, never requested of a model: agents launched by Owlshift get no tracker or forge credentials and never merge.
