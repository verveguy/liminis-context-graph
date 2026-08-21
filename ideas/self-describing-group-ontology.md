# Self-describing group graphs: the ontology as graph content

**Status**: pre-spec sketch. Not a plan, not scoped, do not implement from this file.
**Raised**: 2026-08-20, during triage of #446 (per-group ontology).
**Related**: #446, #447, #83 (workspace ontology), ADR-0378 (per-group streams), ADR-0387
(stream dot-namespace).

## The idea

Represent a group's ontology **inside the group graph itself** — as nodes and relationships
carrying that `group_id` — rather than as a configuration file beside the workspace or a sidecar
beside the stream.

A group graph then describes its own vocabulary. An agent handed a hydrated graph can ask what
entity and relation types it contains, and what they mean, using the same queries it uses for
everything else, with no second channel and no out-of-band file to locate, parse, or version.

## Why it is attractive

- **It travels for free.** Ontology-as-graph-content is written to the group's WAL like any other
  mutation, so it replicates with the stream automatically. No new dot-namespace entry, no fourth
  category in the publish contract, no risk of a `*.jsonl` glob dropping it — which is exactly how
  `.wal-generation.json` went missing in #414.
- **It is group-scoped by construction.** Nodes carry `group_id`, so a per-group ontology needs no
  new resolution rule, no per-group file layout, and no `group_id`-as-path-component encoding.
- **It survives the round trip.** A rebuild from WAL reconstructs the ontology along with the data
  it describes, because they are the same kind of thing.
- **Agentic use cases get a real answer.** "What is in this graph and what do the types mean?"
  becomes a query rather than a documentation lookup. This is the motivating benefit.

## Why it is not obviously right

- **It mixes vocabulary with instance data.** Every existing query, count, search and export would
  need to exclude ontology nodes, or start reporting them. `entity_count` is the obvious first
  casualty. That is a wide blast radius across a surface that is currently uniform.
- **Operative vs documentary is unresolved.** #446 settled that a *published* ontology is
  provenance and must never govern the consumer's extraction or validation. If the ontology is graph
  content, that boundary has to be re-drawn inside the graph rather than at the file layer, where it
  is currently easy to state.
- **Extraction needs it before the graph exists.** Extraction is guided by the ontology; a
  freshly-created group has no content yet. Bootstrapping is not circular in principle — the
  ontology can be written first — but it means graph content is now a startup input, which the
  current `Option<Arc<Ontology>>`-at-startup design is not shaped for.
- **Editing gets harder, not easier.** A YAML file is diffable, reviewable and version-controllable.
  Graph nodes are none of those without tooling built for it. The documented file format
  (`https://v3rv.com/liminis-context-graph/ontology`) is a real asset.
- **Schema cost.** Ontology nodes need labels and relationship types that cannot collide with user
  vocabulary, in a schema that must stay in parity with the Kuzu driver (see CLAUDE.md).

## Probably not either/or

The file stays the authoring surface — diffable, reviewable, the thing a human edits. Loading it
*projects* it into the group graph as content, so the graph is self-describing and the projection
travels with the stream. That keeps YAML's ergonomics and gains queryability, at the cost of a
projection step and a well-defined answer to what happens when the two disagree.

That conflict question is the crux, and is not answered here.

## What would make this worth specifying

An actual agentic use case that is blocked today — an agent that receives a hydrated graph and
cannot proceed because it has no way to discover the vocabulary. #446's driver is close to this but
not the same: it asks for per-group ontology *configuration*, which per-group files satisfy without
any of the above.

Until then this is a better long-term shape, not a current need, and #446 should not wait for it.
