# ADR-0007: lbug Cypher Escaping Convention — Backslash, Not SQL Doubling

**Status**: Accepted
**Date**: 2026-06-12
**Issue**: #128

## Context

`liminis-graph-core/src/replay.rs` reconstructs Cypher statements at WAL replay time by
string-interpolating JSON param values into the template using `json_to_cypher_literal`.
For single-quoted string literals in Cypher, two escaping conventions exist:

- **SQL-style**: `'` → `''` (double the apostrophe)
- **Backslash-style**: `'` → `\'` (prefix with backslash)

The original implementation used SQL-style doubling, which was wrong for lbug. A production
WAL recovery of 43,821 files produced `failed_lines: 2,156,181` out of `2,552,224` total
mutations — an 84.5% failure rate — entirely caused by this escaping bug. Every string
param containing an apostrophe (names with possessives, contractions, natural-language
content) produced a parse error.

## Decision

lbug's Cypher parser uses **backslash escaping** for single quotes inside single-quoted
string literals: `'` must be written as `\'`.

Additionally, backslashes themselves must be doubled: `\` → `\\` (to prevent a literal
backslash before a quote from being misread as an escape sequence).

The canonical escape sequence is therefore:

```rust
s.replace('\\', "\\\\").replace('\'', "\\'")
```

This is identical to the `escape()` function in `liminis-graph-core/src/db.rs` (line ~1326),
which serves as the authoritative reference implementation.

## Rationale

Evidence for backslash escaping (not SQL doubling):

1. **`db.rs::escape()`** (line ~1326): `s.replace('\\', "\\\\").replace('\'', "\\'")` —
   explicitly uses backslash escaping for all user-supplied string values written to the DB.
2. **`wal.rs::strip_quoted_literals()`** (line ~303): the WAL parser handles `\'` as an
   escape sequence inside single-quoted literals, confirming lbug's lexer recognises `\'`.
3. **`db.rs` comment** (line ~1299): `"use only trusted input until lbug exposes a
   parameterised-query API"` — confirms no parameterized API exists; escaping is required.
4. **ADR-001** (`001-wal-drain-and-flush-pattern.md`): confirms lbug has no parameterized
   query API and that WAL params are always `{}` for Rust-produced mutations.
5. **Production failure**: SQL-style doubling (`''`) caused the lbug Cypher parser to stop
   at the first lone quote, treating the remainder of the string as a syntax error.

## Consequences

- `json_to_cypher_literal` in `replay.rs` uses backslash escaping from issue #128 onward.
- Any future code that interpolates user-supplied strings into lbug Cypher MUST use the
  same pattern (`s.replace('\\', "\\\\").replace('\'', "\\'")`) or call `db::escape()`.
- SQL-style `''` doubling MUST NOT be introduced for lbug, even if it is the default for
  other databases (PostgreSQL, SQLite, etc.).
- This ADR becomes moot if lbug exposes a parameterized-query API in a future version.
  At that point, ADR-001 should be revisited and the `interpolate_params` + `raw_query`
  path replaced with parameterized execution.

## Legacy-Pattern Error Detection

The `LEGACY_SCHEMA_ERROR_PATTERNS` constant in `replay.rs` matches lbug 0.17.x error text
for missing legacy tables/properties. If lbug changes its error message format in a future
version, these patterns will silently stop matching. When upgrading lbug, verify that the
error text for `CREATE (:Community …)`, `CREATE (:HAS …)`, and a property access on a
non-existent `episodes` field still contains the expected substrings. See `replay.rs` for
the current patterns.
