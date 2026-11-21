# Records

One file per ticket: `OWL-123.md`, named by the ticket's identifier. A ticket's record holds what the design documents should not carry line by line: the dated result of a live check, a measure, and what was built otherwise than designed.

## Why a file per ticket

Until 2026-11-21 a ticket added its record to [the build plan](../build-plan.md), next to the part it built. 94 of the last 150 commits on `main` changed that file. Owlshift's collision rule lets two tickets at most hold a file, so the repository built two tickets at a time and the others waited for a document ([architecture](../architecture.md) section 5, "A stopped holder yields", the measures). A file only one ticket writes collides with nothing.

## Rules

- A ticket writes its own file and no other ticket's. A later ticket that corrects an earlier record says so in its own file and links the earlier one.
- The design documents change when the design changes: a rule, a decision, a rejected alternative, a step's plan or its exit gate. A record never states a rule that the architecture does not.
- A design document that needs a record's result links the file (`records/OWL-123.md`); a record links the paragraph or the part it settles.
- No index is kept here: a list every ticket adds a line to is the shared file again. The folder's listing and the ticket identifiers in the build plan's parts are the index.
- The records written in the build plan before 2026-11-21 stay there.

## Shape

```markdown
# OWL-123 · The ticket's title

Settles: [architecture](../architecture.md) section 5, "The paragraph's name"; [build plan](../build-plan.md), "P6 · Parallel", part 15.

## As built (2026-11-22)

What was built, where it differs from the design and why, the functions and tests that hold it.

## Live check R1 (2026-11-22)

The versions, the command, the result, and what it decides.
```

Dates are absolute. Account identifiers, tokens and private paths are left out, as in every live check.
