# Glossary

Three Owlshift terms, each defined in one sentence taken from
[`docs/design/architecture.md`](../../docs/design/architecture.md).

- **Decider**: the one person per ticket whose answers unblock it, while other
  people's comments are context.
  Source: [section 4, "Who decides"](../../docs/design/architecture.md#4-default-pipeline--gates).
- **Intake**: the first stage, a read-only run without network that ends `done`
  or `questions`, both carrying its record of the variant, the routing verdict
  (harness, tier, effort) and the dependencies it found.
  Source: [section 5, "Intake"](../../docs/design/architecture.md#5-architecture-overview).
- **Drain**: what `owlshift watch --drain` adds to watch, picking its own work
  one ticket at a time by taking in the admitted tickets of the ready column,
  intake first, then a dispatch to Build when a ticket is ready and nothing else
  of the project is being built.
  Source: [section 5, "The drain"](../../docs/design/architecture.md#5-architecture-overview).
