---
layout: default
title: liminis-context-graph
---

# liminis-context-graph

A local-first context graph engine. One Rust binary that turns a stream of text into a
queryable graph of entities, relationships, and episodes — combining property-graph storage,
HNSW vector search, and full-text search in a single embedded service, built on
[LadybugDB](https://github.com/lbugdb/lbug). No database server, no separate vector store, no
search cluster: everything runs in one process, on your machine, against files in your
workspace.

This site documents **v{{ site.version }}** (see [`Cargo.toml`](https://github.com/{{ site.repository }}/blob/main/Cargo.toml)).
It describes the `main` branch as of that release; unreleased changes on `main` since the
last tag may not yet be reflected here.

Source: [github.com/{{ site.repository }}](https://github.com/{{ site.repository }}). The
[`README`](https://github.com/{{ site.repository }}/blob/main/README.md) has a short overview
and a standalone quickstart; this site is the full reference.

## Reference pages

- **[Getting Started](getting-started.md)** — install, run, build from source, bundle in downstream apps.
- **[Configuration](configuration.md)** — every environment variable and CLI flag.
- **[IPC & MCP Reference](ipc-mcp-reference.md)** — the JSON-RPC and Model Context Protocol method surface.
- **[Telemetry](telemetry.md)** — structured JSONL events emitted on stderr.
- **[Ontology](ontology.md)** — the optional entity/relation type vocabulary.
- **[Operations](operations.md)** — WAL administration, degraded mode, and self-healing recovery.
- **[Testing & Evaluation](testing-and-evaluation.md)** — LLM cassettes and the extraction-quality eval harness.
- **[ADR Index](adr/index.md)** — architecture decision records (historical, not current-state, documentation — see the index for framing).

## llms.txt

[`llms.txt`](llms.txt) and [`llms-full.txt`](llms-full.txt) provide this site's content in a
form suited to LLM ingestion. `CLAUDE.md` (agent guidance for contributors working in this
repository) is referenced from `llms.txt` but is not itself published here.
