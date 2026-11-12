# The Markdown tracker

The Markdown tracker is the way to use Owlshift without a tracker account:
tickets are files in your repository, one folder per ticket. Choose it with
`owlshift init --tracker markdown`, which also writes a first ticket as an
example. Answering a question is adding a file, and everything stays diffable
in git.

## Limits

Read these before you choose it.

- **One machine.** In 0.1 the Markdown tracker serves one machine: you run
  `owlshift do TICKET` and `owlshift continue TICKET` in a checkout of the
  project, and they read and write the `tickets/` folder of that checkout. A
  shared mode, where several people or machines work the same tickets, waits
  for a real demand.
- **Nothing committed by Owlshift.** Owlshift writes the files (a comment, a
  stage, a proposal, a document) and never commits or pushes them. Commit
  them yourself when you want them in the repository's history.
- **Not served by the daemon.** The background service reads a project from
  its own clone, which no person reads or writes, so a Markdown project
  cannot be served: `owlshift start` refuses it and says why.

## The ticket folder

A ticket is a folder `tickets/<ID>/`, where `<ID>` is the ticket's
identifier, such as `DEMO-1`:

```text
tickets/DEMO-1/
  ticket.md        the front matter and the description
  comments/        one file per comment (absent until the first)
  documents/       one file per document (absent until the first)
```

## ticket.md

`ticket.md` opens with a TOML front matter between two `+++` lines, then the
description. The front matter is TOML, like every configuration file of the
project.

```markdown ticket tickets/DEMO-1/ticket.md
+++
title = "Add a greeting"
author = "reporter"
stage = "Todo"
priority = "high"
assignee = "maintainer"
labels = ["agent", "docs"]
blocked_by = ["DEMO-0"]
related = ["DEMO-0"]
+++

Print `hello` when the program starts.
```

| Key          | Meaning                                                                                    |
| ------------ | ------------------------------------------------------------------------------------------ |
| `title`      | The title. Required.                                                                       |
| `author`     | Who wrote the ticket. Required.                                                            |
| `stage`      | The visible stage: a state name as mapped under `[tracker].states` in `owlshift.toml`. Required. |
| `priority`   | `urgent`, `high`, `medium`, `low` or `unset`. Defaults to `unset`.                         |
| `assignee`   | The ticket's decider, the person Owlshift asks when it needs a decision. Optional.         |
| `labels`     | A list of labels. Empty by default.                                                        |
| `blocked_by` | A list of ticket identifiers that must be `Done` first. Empty by default.                  |
| `related`    | A list of ticket identifiers, such as a proposal's parent. Empty by default.               |

An unknown key is refused, and so is a front matter that is not closed by a
second `+++` line; the error names the file. Changing the stage rewrites the
`stage` line and nothing else, a one-line diff.

### Who decides a ticket

The `assignee` is the ticket's decider. Without one, the decider is the
owner of the zone a `zone:<folder>` label names. `owlshift do` refuses a
ticket that has neither, saying that it has no decider.

## Comments and replies

A comment is a file `comments/<YYYYMMDDTHHMMSSZ>-<author>.md`: the UTC time
in ISO 8601 basic format, which has no colon (Windows refuses one in a file
name), then the author, made of 1 to 64 ASCII letters, digits, `_` or `-`.
The body is the file's content. The comment's id is its file name without
`.md`.

```markdown comment tickets/DEMO-1/comments/20261003T101500Z-owlshift.md
Which greeting do you want: `hello` or `hello, world`?
```

Several comments can share a second. The first has no suffix; the n-th
carries `.<n>` right after the time, with n from 2 to 1,000,000 and no leading
zero: `20261003T101500Z.2-maintainer.md`. Comments read in order of time, then
n, then file name. A file is never overwritten, and a file in `comments/`
whose name is not a comment's is an error.

A reply opens with a TOML front matter holding `reply_to`, the id of the
comment it answers, then the body. Any other comment file is its body alone.

```markdown reply tickets/DEMO-1/comments/20261003T101500Z.2-maintainer.md
+++
reply_to = "20261003T101500Z-owlshift"
+++

`hello, world`, please.
```

## Documents

A document is a file `documents/<n>.md`, `n` one past the highest number
there. It holds its title on a `# ` line, a blank line, then its content.
Owlshift writes one when a run publishes a text, such as a plan.

```markdown document tickets/DEMO-1/documents/1.md
# Plan

1. Add the greeting.
2. Test it.
```

## Proposals

A follow-up Owlshift proposes is a new ticket folder named after its
parent's key, one past the highest number among that key's folders: a
proposal of `DEMO-1`, with `DEMO-1` alone, is `DEMO-2`. Its stage is
`Triage`, the proposals inbox, and it is `blocked_by` its parent when it
needs the parent's code. It has `author = "owlshift"` and `related` set to
its parent; it has no priority, assignee or label. A ticket at the stage
`Done` counts as completed for the tickets that are `blocked_by` it.

```markdown proposal tickets/DEMO-2/ticket.md
+++
title = "Greet by name"
author = "owlshift"
stage = "Triage"
blocked_by = ["DEMO-1"]
related = ["DEMO-1"]
+++

The greeting could take a name.
```

To accept a proposal, give it the labels and the assignee it needs and move
its `stage` to your ready state.
