# ADR-0019: Workspace Migration Partial-Resume vs. Schism Marker

**Date**: 2026-05-26
**Status**: Accepted
**Issues**: #104 (Safe `.graphiti/` → `.lcg/` workspace migration)

## Context

When `liminis-context-graph` starts with a legacy `.graphiti/` workspace directory, it
automatically migrates to the new `.lcg/` layout. The migration is a multi-step file-rename
sequence that can be interrupted (power loss, SIGKILL, disk full). On the next start, the
binary must distinguish two ambiguous states that both result in *both* `.graphiti/` and
`.lcg/` existing simultaneously:

1. **Partial migration** — the migration ran partially and was interrupted; `.lcg/` was
   created by the migration logic and contains a subset of the files it should have. The
   binary should *resume* the migration by completing the remaining steps.

2. **Schism** — `.lcg/` was created independently by a user or tool (e.g. a fresh
   `liminis-app` session was started before migration ran, creating a competing `.lcg/`).
   Both directories may contain live data. The binary must *refuse* to proceed until the
   user manually resolves the conflict.

These two states are indistinguishable by directory existence alone, but require opposite
responses: partial migration should resume; schism should hard-fail with guidance.

## Decision

**Use `.lcg/db/liminis.db` as the partial-migration marker.**

Migration step 2 moves the lbug database file from `.graphiti/db` to `.lcg/db/liminis.db`.
This is the *first* step that creates any file under `.lcg/db/` — the directory itself
(`mkdir .lcg/db/`) happens in step 1, but directories are cheap and contain no data.

When the binary detects both `.graphiti/` and `.lcg/` on startup:

- **`.lcg/db/liminis.db` exists** → the DB file was successfully moved in a prior attempt.
  This can only happen via the migration code path (a fresh `liminis-app` session creates
  `.lcg/db/` as a directory, not `.lcg/db/liminis.db` as a file). Classify as *partial
  migration*; resume by completing the remaining steps.

- **`.lcg/db/liminis.db` absent** → `.lcg/` was not populated by this migration code.
  Classify as *schism*; refuse to start with a clear error and preserve both directories.

## Consequences

### Correct resumption

A migration interrupted after step 2 (DB file moved) but before later steps (WAL dir, ontology,
hash file) will be correctly identified as a partial migration and resumed. Each step is
idempotent: it checks "does the destination exist?" before moving, so already-moved files
are skipped. After all remaining steps complete, step 8 re-validates the DB and step 9
removes the empty `.graphiti/`.

### False-negative schism: edge case

If a migration is interrupted *before step 2* (i.e., the DB file was never moved), the
marker is absent. On the next start, both directories exist but the marker is missing, so
the binary classifies this as a schism — even though `.lcg/` was created by the migration
(step 1: `mkdir .lcg/db/`). The user sees a spurious schism error.

**Mitigation**: Migration step 1 only creates `.lcg/db/` (an empty directory). A user-created
`.lcg/` would also lack `.lcg/db/liminis.db` in any scenario where no DB migration has
happened. In both cases, the conservative response (refuse-with-guidance) is the safe
default — the user can manually resolve by removing the empty `.lcg/` if that's what's
there, and restarting.

**Why not use a `.migration-in-progress` sentinel file instead?**  Creating a sentinel
at migration start and removing it at completion is the classic solution, but it introduces
a new failure mode: if the process crashes between creating `.lcg/` and creating the
sentinel, the sentinel is absent and we're back to the same ambiguity. The DB file itself
is a better sentinel because it's a load-bearing artifact — its presence proves the most
expensive migration step (moving the live DB) completed successfully.

### Future layout changes

If the new `.lcg/` layout changes such that the DB file is no longer at `.lcg/db/liminis.db`
(e.g., renamed or moved to a different subdirectory), this ADR's invariant breaks. Any
change to the DB file path **must** update the partial-migration marker check in
`crates/service/src/migration.rs` (the `partial_marker` variable in `migrate_workspace`).

### Implications for operators

Operators who manually create `.lcg/` (e.g., to pre-populate a workspace) should be aware
that the binary treats `.lcg/db/liminis.db` as evidence of a prior migration attempt. To
avoid triggering the partial-resume path unexpectedly, operators should either:
- Place the DB at `.lcg/db/liminis.db` and let the binary treat it as already-migrated, or
- Not create `.lcg/` at all until after the binary has run its first migration.
