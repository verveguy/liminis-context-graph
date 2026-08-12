//! Entity resolution for the direct assertion API (issue #379): `knowledge_assert_entity` and
//! `knowledge_assert_relationship`. Both tools resolve an existing entity by exact
//! (case-insensitive, whitespace-normalized) name — or, for `knowledge_assert_entity`, by an
//! explicit `entity_uuid` — strictly within a single `group_id`, following forward through any
//! `Merged` tombstone to its canonical entity.
//!
//! This module deliberately does not reuse `cross_group::resolve_endpoint` directly: that
//! function's `Unbound` state conflates "no entity with this name exists" (assert should create
//! one, FR-010) with "a `Merged` tombstone was found but its forward walk dead-ends" (assert
//! must hard-error, FR-009) — a distinction `resolve_endpoint`'s own caller (`rebind_pointers`,
//! which safely retries later) doesn't need to make but assert does, since an assert call has no
//! later retry. The functions here compose the same underlying primitives
//! (`get_entity_by_name_ci_with_scan_fallback`, `get_entity_by_uuid`, and
//! `cross_group::follow_merged_into_chain` for the forward walk itself) with assert's own
//! miss-handling policy — there is exactly one implementation of the cycle-guarded walk, shared
//! via `follow_merged_into_chain`, but two call sites with different reactions to a dead end.

use crate::{cross_group, db::Conn, error::Error, types::EntityRow};

/// Outcome of resolving a name to an entity within a `group_id` (FR-006/008/009/013).
pub enum Resolved {
    /// No entity with this name exists in the group at all — `knowledge_assert_entity` creates
    /// a new one (FR-010); `knowledge_assert_relationship` fails naming
    /// `knowledge_add_cross_group_edge` (FR-014/FR-015).
    NotFound,
    /// A canonical (non-`Merged`) entity resolved, either directly or forwarded through a
    /// `Merged` tombstone chain to its canonical (FR-008). Boxed — `EntityRow` is large enough
    /// (`name_embedding: Vec<f32>` plus several `String`s) that an unboxed `NotFound` variant
    /// would otherwise pay its full size for no data.
    Existing(Box<EntityRow>),
}

/// Resolves `name` within `group_id` by exact (case-insensitive, whitespace-normalized) match —
/// the same lookup `cross_group::resolve_endpoint` uses (FR-006) — with no embedding-similarity
/// fallback. `get_entity_by_name_ci_with_scan_fallback` already returns a single deterministic
/// winner (`ORDER BY created_at ASC, uuid ASC`), so there is no separate ambiguity check here —
/// FR-006 asks only for exact-match determinism, not ambiguity detection.
///
/// If the winner carries the `Merged` label, forwards through
/// `cross_group::follow_merged_into_chain` to the canonical entity (FR-008). A dead-end forward
/// walk (no forwarding reference, a cycle, a dangling target, or a target that escaped
/// `group_id`) is a hard error (FR-009) — never `NotFound`, since a name that *does* resolve to
/// something (a tombstone) is not "absent"; treating it as absent would silently create a
/// duplicate entity under the same name rather than surfacing the dead end.
pub fn resolve_entity_by_name(conn: &Conn, group_id: &str, name: &str) -> Result<Resolved, Error> {
    let Some(winner) = conn.get_entity_by_name_ci_with_scan_fallback(name, group_id)? else {
        return Ok(Resolved::NotFound);
    };
    if !winner.labels.contains(&"Merged".to_string()) {
        return Ok(Resolved::Existing(Box::new(winner)));
    }
    match cross_group::follow_merged_into_chain(conn, group_id, winner)? {
        Some(canonical) => Ok(Resolved::Existing(Box::new(canonical))),
        None => Err(Error::Ipc(format!(
            "name '{name}' resolves to a Merged tombstone in group '{group_id}' whose \
             merged_into forwarding does not reach a canonical entity within the group \
             (dangling target, a cycle, or a target outside the group) — refusing to guess"
        ))),
    }
}

/// Resolves `entity_uuid` within `group_id` by strict lookup (FR-007) — never a create
/// fallback, never a search of other groups. Absence is a hard error, not "create a new entity
/// under this UUID" — the caller-chosen-mint path #179 specified and this spec deliberately
/// narrows away (see FR-007's rationale: idempotency for `knowledge_assert_entity` rests
/// entirely on `(name, group_id)` per FR-011, not on `entity_uuid`).
///
/// If the resolved entity carries the `Merged` label, forwards through
/// `cross_group::follow_merged_into_chain` to the canonical (FR-008), with the same hard-error
/// dead-end policy as [`resolve_entity_by_name`] (FR-009).
pub fn resolve_entity_by_uuid(
    conn: &Conn,
    group_id: &str,
    entity_uuid: &str,
) -> Result<EntityRow, Error> {
    let entity = conn
        .get_entity_by_uuid(entity_uuid)?
        .filter(|e| e.group_id == group_id)
        .ok_or_else(|| {
            Error::Ipc(format!(
                "entity_uuid '{entity_uuid}' does not exist in group '{group_id}'"
            ))
        })?;
    if !entity.labels.contains(&"Merged".to_string()) {
        return Ok(entity);
    }
    cross_group::follow_merged_into_chain(conn, group_id, entity)?.ok_or_else(|| {
        Error::Ipc(format!(
            "entity_uuid '{entity_uuid}' resolves to a Merged tombstone in group \
             '{group_id}' whose merged_into forwarding does not reach a canonical entity \
             within the group — refusing to guess"
        ))
    })
}
