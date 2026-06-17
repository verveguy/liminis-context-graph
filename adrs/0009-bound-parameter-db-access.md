# ADR-0009: Bound-Parameter DB Access — Retire Cypher String Interpolation

**Status**: Accepted
**Date**: 2026-06-15
**Issue**: WAL replay corruption investigation (follow-on to #128 / #130 / #133 / #139)

## Context

Every DB statement in `liminis-graph-core` was built by **string-interpolating values into
Cypher text** — `format!("... uuid: '{}' ...", escape(uuid))` for writes and reads alike, and
`interpolate_params` + `json_to_cypher_literal` for WAL replay. This single anti-pattern was the
root cause of a recurring class of production failures:

- **#128** — apostrophes in string values broke parsing (wrong escaping convention).
- **#130** — RFC-3339 timestamps needed a `timestamp('...')` literal wrapper in `SET`.
- **#133** — FalkorDB-era `vecf32(...)` constructors and missing schema columns.
- **#139** — a throughput optimization batched same-template mutations into a single
  `UNWIND [ {...}, {...}, ... ]` query by **inlining every row** — including 768-float
  embeddings — as Cypher literal text. The resulting multi-MB query strings corrupted lbug's
  internal `db.wal` on the first large batch (`lbug_wal_corrupt` on next open), wiping the
  workspace on recovery.

Each fix patched a symptom of interpolation; #139 showed the pattern actively corrupts data.

A spike confirmed lbug 0.17 (Kuzu) supports the correct idiom: `Connection::prepare()` +
`Connection::execute(prepared, params)` with typed `Value` parameters, and **coerces each bound
value to its destination column type** (an RFC-3339 `String` into `TIMESTAMP`, a numeric list
into `FLOAT[N]`, a string list into `STRING[]`), including inside `CALL` table-function args,
`LIMIT`, and `SKIP`.

## Decision

Convert **all** DB access — every write and every read — to lbug prepared-statement bound
parameters, and **delete the entire interpolation/escaping layer**.

- `Conn::exec_params(cypher, json_params)` — bound write; records `(template, params)` to the
  application WAL so Rust-era WAL is now itself parameterized.
- `Conn::query_params(cypher, json_params)` — bound read; materializes rows, no WAL recording.
- `Conn::prepare` + `Conn::execute_prepared` — prepare a template once and execute many rows
  (WAL replay); this is #139's intended throughput win (plan reuse) without inlined strings.
- `json_to_value` maps JSON → `lbug::Value` type-agnostically; lbug coerces per column. The one
  exception is RFC-3339 strings, bound as typed `Value::Timestamp` because lbug does **not**
  implicitly cast `STRING`→`TIMESTAMP` in a `SET col = $x` assignment (it does in a CREATE
  property map) — so #130's detection moves into the binding rather than being deleted outright.
- Deleted: `escape`, `escape_fts`, `escape_pub`, `format_str_list`, `format_float_array`,
  `interpolate_params`, `json_to_cypher_literal`, `build_unwind_query`,
  `rewrite_params_for_unwind`, `map_to_cypher_literal`, `execute_single`.
- `raw_query` survives only for DDL (CREATE TABLE / INDEX), which carries no user values.

## Consequences

- **#139 corruption is structurally impossible** — there are no inlined value strings; a
  large-batch replay regression test re-opens the DB from disk to guard it.
- **#128 and #130 are obsoleted** — apostrophes and timestamps are handled by typed binding;
  their escaping/literal logic is gone. ADR-0007 is superseded.
- **ADR-0008 still applies** — `legacy_wal` (`strip_vecf32`, `expand_bulk_property_set`) remains.
  Those are *Cypher-text dialect* transforms on recorded FalkorDB-era WAL (the `vecf32(` token is
  in the query text, not a param), which binding cannot address. They run before `prepare`.
- Net code reduction (~200 lines): a whole defect-prone layer removed, not added.
- Injection surface eliminated as a side effect (values are never concatenated into query text).

## Alternatives considered

- **Keep interpolation, fix #139 by shrinking batch size** — leaves the anti-pattern (and the
  next dialect/escaping bug) in place; does not address the root cause.
- **Convert only the replay path** — fixes the corruption but leaves `escape`/`format_str_list`
  alive for writes and reads, so the recurring class persists. Rejected in favor of the full
  conversion.
