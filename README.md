# liminis-graph

A thin Rust knowledge-graph service over [LadybugDB](https://github.com/kuzudb/kuzu) — the logical successor to the Python [graphiti](https://github.com/getzep/graphiti) fork that currently backs Liminis.

**Status**: pre-implementation. See [`specs/001-rust-knowledge-graph/spec.md`](specs/001-rust-knowledge-graph/spec.md) for the v1 specification.

## Goals

- Preserve the IPC surface liminis-framework consumes from the current Python service (drop-in replacement behind a feature flag).
- Reuse existing on-disk artifacts (WAL JSONL, LadybugDB files) without migration.
- Ship as both a library crate and a standalone IPC binary so non-Liminis projects can adopt it.

## Non-goals (v1)

- Replacing the Node-side MCP servers or chat-agent retrieval glue.
- Drivers other than LadybugDB.
- In-process LLM / embedding models (kept as out-of-process adapters).
- Hosted / multi-tenant deployment.

## Next step

Run `/speckit-clarify` against the spec to surface ambiguities, then `/speckit-plan` to produce design artifacts.
