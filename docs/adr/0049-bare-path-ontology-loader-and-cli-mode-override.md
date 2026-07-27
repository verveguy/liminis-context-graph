# ADR-0049: Bare-Path Ontology Loader and CLI Mode-Override Precedence

**Status**: Accepted
**Date**: 2026-07-27
**Issues**: #266

## Context

`lcg-eval`'s eval harness (#228/ADR-0048) has always hardcoded `ExtractOptions.ontology: None`
(`crates/eval/src/runner.rs:113`) — every report it has ever produced describes freeform
extraction only. `crates/core/src/prompts/mod.rs` injects a materially different system-prompt
section depending on whether an ontology is applied and, if so, whether it's `Open` (prefer
declared types, may invent others) or `Strict` (only declared types are ever accepted) — these
are different tasks for the model, not a minor parameter, so the harness needed a way to
exercise all three regimes.

The obvious place to load an ontology, `crates/core::ontology::load_ontology`, doesn't fit a CLI
harness's needs on two axes:

1. **It's workspace-rooted.** `load_ontology(workspace_root: Option<&Path>)` only resolves
   `{workspace_root}/.lcg/ontology.yaml` or `.../.graphiti/ontology.yaml`. A standalone eval
   fixture (`crates/core/tests/fixtures/real_corpus_wal/ontology.yaml`) isn't inside any
   `.lcg`-rooted workspace, so there's no workspace root to pass in.
2. **It swallows every failure to `None`.** A missing file, malformed YAML, or an ontology
   declaring zero entity/relation types all silently degrade to "no ontology" (`eprintln!` a
   warning, return `None`). That's the right behavior for production ingestion — a workspace
   without an ontology file is normal, not an error — but it's exactly wrong for a CLI flag: if a
   maintainer passes `--ontology /typo/path.yaml`, silently falling back to freeform would look
   like a `Strict` run that quietly measured freeform instead, which is the one failure mode this
   feature exists to make impossible.

Separately, FR-002 requires `--ontology-mode` (when given) to always override any `mode:` the
loaded file itself declares — this lets one committed fixture file drive all three regimes in a
mode-matrix run (freeform via no `--ontology` flag at all, `Open`, `Strict`) instead of shipping
three near-duplicate fixtures that would need to be kept in sync by hand.

## Decision

**Add `pub fn load_ontology_from_path(path: &Path, mode: OntologyMode) -> Result<Ontology, String>`
to `crates/core::ontology`, sharing `load_ontology`'s parse/normalize/validate internals via a
new private `build_ontology(file: OntologyFile, mode_override: Option<OntologyMode>) ->
Option<Ontology>` helper.** `load_ontology`'s body was refactored to call
`build_ontology(file, None)` — its own signature, error-swallowing behavior, and ~20 existing
tests are unchanged. `load_ontology_from_path` reads the file and parses YAML itself
(propagating `std::io::Error`/`serde_yaml::Error` as a formatted `Err(String)` rather than
logging and returning `None`), then calls `build_ontology(file, Some(mode))` and turns a `None`
result (empty ontology) into an `Err` too — every failure mode a CLI harness needs to reject
loudly is a `Result::Err`, not a silent `None`.

**Mode resolution happens at construction, inside `build_ontology`, not by mutating a returned
`Ontology` afterward.** `mode: OntologyMode` is a required parameter to
`load_ontology_from_path` — the caller (the CLI, having already resolved `--ontology-mode`'s
explicit value or its `strict` default) always supplies it, and it unconditionally wins over
whatever `file.mode` parsed to. This keeps "one `Ontology`, one authoritative mode" true from the
moment the struct exists, rather than requiring every future call site to remember to overwrite
`.mode` after loading and risking a bug where some code path reads the file's mode before the
override runs.

`OntologyMode` was added to `crates/core/src/lib.rs`'s crate-root `pub use` (alongside
`Ontology`, which was already there) so `crates/eval` can reference it without a fully-qualified
`lcg_core::ontology::OntologyMode` path.

## Consequences

- `crates/eval` needed no new dependency and no duplicated YAML schema. Adding `serde_yaml`
  directly to `crates/eval` and re-implementing `OntologyFile`/`EntityTypeRaw`/`RelationTypeRaw`
  there would have violated this repo's schema-single-sourcing discipline (the same discipline
  that governs `crates/core::schema` staying the single source of truth against graphiti's Kuzu
  driver) for no benefit — `build_ontology`'s extraction keeps the schema defined exactly once.
- A future contributor adding another ontology-loading call site (a different CLI tool, a test
  harness, anything outside `add_episode`'s workspace-rooted path) has a documented, intentional
  precedent to follow: `load_ontology_from_path` exists specifically because `load_ontology` is
  workspace-rooted and error-swallowing *by design* for production ingestion, not because nobody
  got around to making it more general. Don't loosen `load_ontology` itself to accept a bare path
  or to return `Result` — that would change production `add_episode`'s error-handling contract,
  which is Out of Scope for the issue this ADR documents and not something either call site
  needs.
- "CLI mode always overrides the file's declared mode" is a deliberate, documented precedence
  rule (both in this ADR and in `load_ontology_from_path`'s own doc comment), not an oversight —
  a fixture file's `mode: strict` declaration is a sensible default for hand-invoking it directly,
  but the CLI flag is the actual source of truth whenever both are present. This is what lets
  `crates/core/tests/fixtures/real_corpus_wal/ontology.yaml` (committed with `mode: strict`)
  drive all three regimes of #266's freeform/`Open`/`Strict` mode matrix without needing a
  second, near-duplicate `ontology-open.yaml` fixture kept in sync by hand.
- `load_ontology`'s existing ~20 unit tests plus the 2 integration tests in
  `crates/core/tests/ontology_integration.rs` continued passing unmodified after the refactor —
  the mechanical extraction (move shared logic into `build_ontology`, call it from both entry
  points) touched no observable behavior on the production path, satisfying the issue's
  constraint that `Ontology`/`OntologyMode`/prompt-injection behavior itself is unchanged.

## Related

- ADR-0014: `ExtractOptions.ontology: Option<&'a Ontology>` as the per-call injection point this
  loader ultimately feeds; the prompt-injection design itself is unchanged by this ADR.
- ADR-0018 / ADR-0032: `content_hash()`'s canonical form, which `build_ontology`'s refactor does
  not touch or otherwise risk (no change to `Ontology`'s field shape or `content_hash`'s inputs).
- ADR-0041: local/OpenAI-compatible extraction's `response_format: json_object` guarantee, weaker
  than the hosted path's schema-enforced tool use — the reason `Strict` mode is where FR-004's
  structured-output-reliability gap between local and hosted backends is expected to narrow most,
  which is part of why measuring it needed this loader to exist.
- ADR-0044: the cassette record/replay seam; `ontology` participates in the cassette matching key
  via the rendered `entity_system_prompt`/`edge_system_prompt`
  (`crates/core/src/cassette.rs::extract_request_value`), verified in
  `crates/eval/tests/harness_integration.rs::cassette_recorded_freeform_misses_against_strict_ontology_replay`
  once `--ontology` actually reached `ExtractOptions` end to end from the CLI.
- ADR-0048: the eval harness this loader's consumer (`lcg-eval`) is built on.
- `crates/core/src/ontology.rs`: `load_ontology`, `load_ontology_from_path`, `build_ontology`.
- `crates/eval/src/cli.rs`: `--ontology`/`--ontology-mode` flag parsing, including the FR-002
  usage-error rejection of `--ontology-mode` given without `--ontology`.
- `crates/core/tests/fixtures/real_corpus_wal/ontology.yaml`: the FR-005 fixture this loader
  reads for #266's mode-matrix runs.
