use std::collections::{BTreeSet, HashMap};
use std::sync::RwLock;

/// In-process accelerator for `Conn::get_entity_by_name_ci`, restoring an indexed access
/// path for a lookup that lbug cannot serve from an index (`lower(e.name) = $x` is a
/// scalar-function predicate; see ADR-0029 and the new NameIndex ADR).
///
/// Keyed by `(group_id, lower(name))`, each key maps to the full set of matching
/// entities' `(created_at, uuid)` pairs. `String` tuple ordering is lexicographic, so the
/// set's minimum element reproduces the DB's `ORDER BY created_at ASC, uuid ASC LIMIT 1`
/// winner-selection rule exactly — every `created_at` is normalized to the canonical
/// "YYYY-MM-DD HH:MM:SS" form via `db::canonical_created_at_for_index` before it reaches
/// this struct, since callers pass mixed formats (RFC-3339 from freshly-extracted entities,
/// space-format read back from the database) that would otherwise sort inconsistently.
///
/// This structure is an accelerator only: a hit is always re-verified against the database
/// (`Conn::get_entity_by_uuid`) before being trusted, so a stale or missing entry can only
/// ever degrade a lookup to a miss, never return a wrong entity.
#[derive(Default)]
pub(crate) struct NameIndex {
    state: RwLock<NameIndexState>,
}

#[derive(Default)]
struct NameIndexState {
    by_key: HashMap<(String, String), BTreeSet<(String, String)>>,
    by_uuid: HashMap<String, (String, String, String)>,
}

impl NameIndex {
    /// Records a newly-inserted entity. Idempotent under retries with the same values.
    pub(crate) fn insert(&self, uuid: &str, name: &str, group_id: &str, created_at: &str) {
        let lower_name = name.trim().to_lowercase();
        let created_at = crate::db::canonical_created_at_for_index(created_at);
        let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
        state
            .by_key
            .entry((group_id.to_string(), lower_name.clone()))
            .or_default()
            .insert((created_at.clone(), uuid.to_string()));
        state.by_uuid.insert(
            uuid.to_string(),
            (group_id.to_string(), lower_name, created_at),
        );
    }

    /// Relocates `uuid` within its `(group_id, lower_name)` key's ordered set after its
    /// `created_at` changes (the one mutation `merge_entities` performs that reorders the
    /// winner without adding/removing a row). A no-op if `uuid` isn't tracked yet.
    pub(crate) fn update_created_at(&self, uuid: &str, created_at: &str) {
        let created_at = crate::db::canonical_created_at_for_index(created_at);
        let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
        let Some((group_id, lower_name, old_created_at)) = state.by_uuid.get(uuid).cloned() else {
            return;
        };
        if let Some(set) = state
            .by_key
            .get_mut(&(group_id.clone(), lower_name.clone()))
        {
            set.remove(&(old_created_at, uuid.to_string()));
            set.insert((created_at.clone(), uuid.to_string()));
        }
        state
            .by_uuid
            .insert(uuid.to_string(), (group_id, lower_name, created_at));
    }

    /// Returns the UUID of the deterministic winner for `(name, group_id)`, if the index
    /// knows of any matching entity. Callers MUST verify this against the database before
    /// trusting it (FR-006) — this method alone does not guarantee the entity still exists
    /// or still matches.
    pub(crate) fn lookup(&self, name: &str, group_id: &str) -> Option<String> {
        let lower_name = name.trim().to_lowercase();
        let state = self.state.read().unwrap_or_else(|e| e.into_inner());
        state
            .by_key
            .get(&(group_id.to_string(), lower_name))
            .and_then(|set| set.iter().next())
            .map(|(_, uuid)| uuid.clone())
    }

    /// Replaces the entire index with `rows` (`uuid, name, group_id, created_at`), the
    /// FR-004 single-scan mechanism used at startup, after recovery, and after WAL rebuild
    /// — the only paths that populate `Entity` rows without going through `insert`/
    /// `update_created_at`.
    pub(crate) fn rebuild(&self, rows: Vec<(String, String, String, String)>) {
        let mut new_state = NameIndexState::default();
        for (uuid, name, group_id, created_at) in rows {
            let lower_name = name.trim().to_lowercase();
            let created_at = crate::db::canonical_created_at_for_index(&created_at);
            new_state
                .by_key
                .entry((group_id.clone(), lower_name.clone()))
                .or_default()
                .insert((created_at.clone(), uuid.clone()));
            new_state
                .by_uuid
                .insert(uuid, (group_id, lower_name, created_at));
        }
        let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
        *state = new_state;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_returns_none_when_empty() {
        let idx = NameIndex::default();
        assert_eq!(idx.lookup("Alice", "g1"), None);
    }

    #[test]
    fn insert_then_lookup_is_case_and_whitespace_insensitive() {
        let idx = NameIndex::default();
        idx.insert("u1", "Alice", "g1", "2026-01-01 00:00:00");
        assert_eq!(idx.lookup(" alice ", "g1"), Some("u1".to_string()));
        assert_eq!(idx.lookup("alice", "g2"), None);
    }

    #[test]
    fn earliest_created_at_wins_with_uuid_tiebreak() {
        let idx = NameIndex::default();
        idx.insert("z-later", "Alice", "g1", "2026-02-01 00:00:00");
        idx.insert("a-earliest", "Alice", "g1", "2026-01-01 00:00:00");
        idx.insert("b-tie", "Alice", "g1", "2026-01-01 00:00:00");
        // "a-earliest" wins on created_at; among ties, the smallest uuid wins.
        assert_eq!(idx.lookup("Alice", "g1"), Some("a-earliest".to_string()));
    }

    #[test]
    fn update_created_at_reorders_winner() {
        let idx = NameIndex::default();
        idx.insert("early", "Alice", "g1", "2026-01-01 00:00:00");
        idx.insert("late", "Alice", "g1", "2026-02-01 00:00:00");
        assert_eq!(idx.lookup("Alice", "g1"), Some("early".to_string()));
        idx.update_created_at("late", "2025-01-01 00:00:00");
        assert_eq!(idx.lookup("Alice", "g1"), Some("late".to_string()));
    }

    #[test]
    fn update_created_at_on_unknown_uuid_is_a_noop() {
        let idx = NameIndex::default();
        idx.update_created_at("ghost", "2026-01-01 00:00:00");
        assert_eq!(idx.lookup("Alice", "g1"), None);
    }

    #[test]
    fn mixed_rfc3339_and_space_format_created_at_still_orders_correctly() {
        // Reproduces a real production mismatch: episode.rs's `insert_entity` call passes
        // `created_at` as the raw RFC-3339 `reference_time` string, while `rebuild_name_index`
        // (and `update_entity_created_at` callers, which read EntityRow back from the DB) use
        // the space form. Without normalization, the 'T'/' ' separator byte would dominate the
        // lexicographic comparison ahead of the actual time-of-day, picking the wrong winner.
        let idx = NameIndex::default();
        // Chronologically later (Feb) but RFC-3339-formatted — 'T' > ' ' would wrongly sort
        // this ahead of everything if formats weren't normalized to the same form first.
        idx.insert("later-rfc3339", "Alice", "g1", "2026-02-01T00:00:00Z");
        // Chronologically earlier (Jan), space-formatted.
        idx.insert("earlier-space", "Alice", "g1", "2026-01-01 00:00:00");
        assert_eq!(
            idx.lookup("Alice", "g1"),
            Some("earlier-space".to_string()),
            "the chronologically earliest entry must win regardless of its created_at string format"
        );
    }

    #[test]
    fn rebuild_replaces_prior_state() {
        let idx = NameIndex::default();
        idx.insert("stale", "Alice", "g1", "2026-01-01 00:00:00");
        idx.rebuild(vec![(
            "fresh".to_string(),
            "Bob".to_string(),
            "g1".to_string(),
            "2026-01-01 00:00:00".to_string(),
        )]);
        assert_eq!(idx.lookup("Alice", "g1"), None);
        assert_eq!(idx.lookup("Bob", "g1"), Some("fresh".to_string()));
    }
}
