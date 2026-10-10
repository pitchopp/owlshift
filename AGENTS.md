# Instructions for coding agents

Owlshift is in its design phase. Before changing anything, read the design in `docs/design/`, starting with `architecture.md`, `roadmap.md` and `build-plan.md`.

- The design documents are the reference. A change that contradicts them starts with a change to the document, reviewed like code.
- Work items live in the Owlshift Linear workspace, team `OWL`, one project per roadmap step. Put the issue ID in the branch name (`owl-12-short-title`) so the pull request attaches to its issue. Blockers are recorded as issue relations: do not start an issue whose blockers are not done.
- Everything in this repository is written in English: code, comments, docs, commit messages.
- P0 starts with the live checks listed in `docs/design/build-plan.md`. An assumption about an external tool (a CLI flag, an API behaviour) is verified with a real call before code depends on it, and the dated result is written next to the decision it settles.
- The core crate stays pure: no I/O in `owlshift-core`.
- Guardrails are enforced by the runner, never requested of a model: agents launched by Owlshift get no tracker or forge credentials and never merge.
- Every commit is signed off (`git commit -s`), per the Developer Certificate of Origin in `CONTRIBUTING.md`.

## Tracker

The backlog lives in Linear, workspace **Owlshift** (URL key `owlshift`), team `OWL` (id `e3e05b95-d8f6-4a45-a545-bf44bf5d2467`).

**Access.** Do not use a Linear MCP connector for this project: the one available in Claude sessions is bound to another workspace, and its writes would land there. Use the GraphQL API directly:

```bash
KEY=${LINEAR_API_KEY:-$(grep -sE '^LINEAR_API_KEY=' ~/Projects/owlshift/.env | cut -d= -f2-)}
curl -s https://api.linear.app/graphql -H "Authorization: $KEY" -H "Content-Type: application/json" \
  -d '{"query":"{ organization { urlKey } }"}'
```

- The key comes from the `LINEAR_API_KEY` environment variable when it is set, otherwise from the `.env` of the main checkout (`~/Projects/owlshift/.env`); a worktree has no `.env`. Never print it, copy it or commit it.
- In a Claude Code cloud session the container has no `~/Projects/owlshift/.env`: the key is a secret of the cloud environment, exposed as `LINEAR_API_KEY`. Checked on 2026-10-10 in a cloud session: the command above returned `owlshift`. If the variable is missing there, ask the maintainer to add it to the environment; do not look for the key elsewhere.
- Before the first write of a session, check that `organization { urlKey }` returns `owlshift`.
- The API caps query complexity at 10,000: keep nested lists at `first: 50` or less.
- An issue accepts its identifier (`"OWL-12"`) wherever an issue id is expected.

**States.**

| State | Id | Meaning |
| --- | --- | --- |
| Triage | `ad6dab17-64c9-4d38-86e7-4f01a419a85f` | Proposed follow-ups, waiting for a human to accept or decline |
| Backlog | `6139ffa1-cc94-4e29-838d-e061835bcef4` | Accepted, not ready |
| Todo | `50be5363-4cce-4989-9ba9-970c23db5093` | Ready to work, once its blockers are done |
| In Progress | `23e8c0a5-dc85-451f-9488-d6227ae8ffb6` | Being worked |
| Needs Input | `9a51b9b2-2efd-4f14-a965-62e1c22a7585` | Waiting on the maintainer's answer to questions posted on the issue |
| In Review | `c00d199b-cd25-4ffc-a3cb-cf03f3bb8c5e` | A pull request is open |
| Done | `76f46edb-f965-4373-a668-b4a9c1eb1bae` | Merged into `main` |
| Canceled | `50b565a1-0340-4678-9a34-6c5d44ff6be2` | Dropped |

**Working an issue.**

- Pick from Todo only. Blockers are relations of type `blocks`; read them with `inverseRelations { nodes { type issue { identifier state { type } } } }` and do not start while one is not `completed`.
- Move the issue to In Progress when work starts. Until the Linear GitHub integration is connected, also move it to In Review when the pull request opens (with the PR link as a comment), and to Done after the merge.
- Open pull requests against `main`; never merge them.

**Creating issues.**

- Title in English, imperative mood. One type label (Feature, Bug, Improvement) and one area label (Core, Adapters, CLI, Docs, Infra). A priority, a project (the roadmap step), and a t-shirt estimate: XS = 1, S = 2, M = 3, L = 5, XL = 8.
- A description that stands alone: why, scope, acceptance, links to the design documents.
- A follow-up an agent proposes goes to Triage, never to Todo; a human promotes it.
- The free plan caps the workspace at 250 issues: search for an existing issue before creating one.
