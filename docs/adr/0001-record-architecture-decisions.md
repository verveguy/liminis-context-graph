# ADR 0001: Record Architecture Decisions

**Date**: 2026-05-19
**Status**: Accepted

## Context

As liminis-graph evolves, significant architectural choices will be made — database selection, serialization formats, IPC protocols, async strategy. Without a record of these decisions and their reasoning, future contributors (and future-us) will encounter constraints with no explanation, or worse, revisit settled debates.

## Decision

We will use Architecture Decision Records (ADRs) to document every significant architectural choice. ADRs live in `docs/adr/` and are numbered sequentially (`NNNN-short-title.md`). Once accepted, an ADR is immutable except for a status change to Superseded (pointing at the new ADR).

The format for each ADR is:
- **Date** — when the decision was made
- **Status** — Proposed / Accepted / Superseded
- **Context** — what problem the decision addresses
- **Decision** — what we chose
- **Consequences** — trade-offs, downstream effects, risks

## Consequences

Developers must write an ADR before merging any change that alters:
- The storage engine or schema
- The IPC protocol or buffer format
- The async/sync boundary in the library API
- The embedding model or vector dimension

ADRs are linked from the relevant spec and README sections so the constitution and the decision record stay in sync.

## References

- [Documenting Architecture Decisions — Michael Nygard](https://cognitect.com/blog/2011/11/15/documenting-architecture-decisions)
- Project constitution: `.specify/memory/constitution.md`
