//! Group-scoped complete purge: remove a group's `Entity`, `Episodic`, and same-group
//! `RelatesToNode_` (RELATES_TO edge) data in one atomic operation, without orphaning another
//! group's cross-group pointers (issue #361).
//!
//! See `docs/adr/0361-group-scoped-purge.md` for the design rationale — in particular why
//! `applied_seq` (FR-005) is left untouched pending #378, and why the same pre-mutation
//! counting query can serve as both the `dry_run` prediction and the real result (SC-009).

use std::collections::{BTreeMap, HashSet};

use crate::{
    cross_group,
    db::{Conn, GroupedMutations},
    error::Error,
    pointer,
};

/// Per-group counts of what a purge removes (or, under `dry_run`, would remove).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupPurgeCounts {
    pub group_id: String,
    pub entities: u64,
    pub episodes: u64,
    pub edges: u64,
}

/// The count of cross-group pointers owned by `owning_group_id` (the layer group whose
/// `RelatesToNode_` carries the pointer, per FR-008/FR-009) that are left `unbound` as a result
/// of purging the groups the pointer's `source_group_id` names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnboundImpact {
    pub owning_group_id: String,
    pub pointer_count: u64,
}

/// The full result of a purge call — identical shape whether `dry_run` is `true` (a preview,
/// FR-012) or `false` (what was actually removed/unbound). Because both branches compute these
/// counts from the same pre-mutation queries, and a purge always empties the source group(s)
/// before any rebind resolution runs (so every affected pointer is guaranteed to resolve
/// `Unbound`, never `Bound` — see the module doc), the two calls are guaranteed to agree
/// (SC-009) with no separate reconciliation logic.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PurgeCounts {
    pub dry_run: bool,
    pub groups: Vec<GroupPurgeCounts>,
    pub unbound_impacts: Vec<UnboundImpact>,
}

/// Purges all `Entity`, `Episodic`, and same-group `RelatesToNode_` data for `group_ids`.
///
/// `group_ids` naming a group with no data is a no-op success for that group (FR-006) — every
/// query here is a `WHERE x.group_id IN $gids` match, which simply matches nothing for an
/// absent group_id, not an error.
///
/// When `dry_run` is `true`, only the counting queries run; nothing is mutated (FR-012/FR-013).
///
/// When `dry_run` is `false`, the delete + forced-rebind sequence runs inside one
/// `BEGIN TRANSACTION`/`COMMIT` (FR-002: multiple `group_ids` are purged atomically — a failure
/// partway through rolls back the whole call, never leaving a partial purge). No index
/// invalidation step is needed after commit (issue #221): `Entity.lookup_key`'s secondary ART
/// index is a database-native structure lbug maintains automatically across this call's
/// `DETACH DELETE`s, unlike the in-process `NameIndex` this purge previously had to rebuild by
/// hand.
///
/// Returns, alongside [`PurgeCounts`], a [`GroupedMutations`] bucketing every mutation this call
/// issued by the `group_id` it actually modified (issue #385 FR-002): each purged group's own
/// deletions under its own `group_id`, and any forced-rebind write under the *owning* group of
/// the `RelatesToNode_` row it touched — never a shared default-group bucket. Draining happens at
/// per-group boundaries *inside* this transaction, but that only moves data from `Conn`'s
/// in-memory buffer into this in-memory map; nothing is flushed to disk here. The caller must
/// flush each bucket to its own group's WAL stream only after this function returns `Ok` (i.e.
/// strictly after `COMMIT`) — on `Err`, the accumulated map is simply dropped, mirroring the
/// pre-#385 behavior where a rolled-back transaction's `executed_mutations` were never drained.
pub fn purge_groups(
    conn: &Conn,
    group_ids: &[&str],
    ts: &str,
    dry_run: bool,
) -> Result<(PurgeCounts, GroupedMutations), Error> {
    let groups = group_ids
        .iter()
        .map(|gid| -> Result<GroupPurgeCounts, Error> {
            let single = [*gid];
            Ok(GroupPurgeCounts {
                group_id: (*gid).to_string(),
                entities: conn.count_entities_by_group_ids(&single)?,
                episodes: conn.count_episodics_by_group_ids(&single)?,
                edges: conn.count_relates_to_by_group_ids(&single)?,
            })
        })
        .collect::<Result<Vec<_>, Error>>()?;

    let unbound_impacts = compute_unbound_impacts(conn, group_ids)?;

    if dry_run {
        return Ok((
            PurgeCounts {
                dry_run: true,
                groups,
                unbound_impacts,
            },
            GroupedMutations::new(),
        ));
    }

    conn.exec_transaction_control("BEGIN TRANSACTION")?;
    let mut grouped: GroupedMutations = GroupedMutations::new();
    let mutate_result = (|| -> Result<(), Error> {
        // Per-group delete loop (issue #385): each group_id gets its own singleton-slice calls,
        // drained immediately after, so every DETACH DELETE line is attributable to exactly one
        // group's stream (FR-001) — a single `WHERE group_id IN [...]` call spanning several
        // purge targets can't be split back apart after the fact. Within one group's turn, the
        // ordering is unchanged from the pre-#385 batched form and for the same reason: same-group
        // edges first (deletes only RelatesToNode_ owned by this group — FR-008 never touches a
        // foreign group's RelatesToNode_, by construction of the WHERE clause); entities next
        // (DETACH DELETE destroys any hop into a *surviving* foreign RelatesToNode_, i.e. another
        // group's cross-group edge pointing into this group, which is exactly what leaves that
        // edge in need of rebinding below); episodics last.
        for gid in group_ids {
            let single = [*gid];
            conn.delete_relates_to_by_group_ids(&single)?;
            conn.delete_entities_by_group_ids(&single)?;
            conn.delete_episodics_by_group_ids(&single)?;
            conn.drain_mutations_into(&mut grouped, gid);
        }
        // Forced rebind per purged group, as a separate pass (not interleaved with the delete
        // loop above): every pointer whose source_group_id matches is guaranteed to resolve
        // Unbound now (the group is empty), which both drops the stale hop and records
        // binding_state: Unbound so knowledge_status reflects it immediately (FR-009/FR-010/
        // SC-008). Each call's own GroupedMutations is merged in, keyed by the *owning* group of
        // the RelatesToNode_ row it touched (issue #385 FR-002) — not necessarily `gid` itself.
        for gid in group_ids {
            let (_, rebind_grouped) = cross_group::rebind_pointers_forced(conn, gid, ts)?;
            for (owning_gid, mutations) in rebind_grouped {
                grouped.entry(owning_gid).or_default().extend(mutations);
            }
        }
        Ok(())
    })();

    match mutate_result {
        Ok(()) => conn.exec_transaction_control("COMMIT")?,
        Err(e) => {
            let _ = conn.exec_transaction_control("ROLLBACK");
            return Err(e);
        }
    }

    Ok((
        PurgeCounts {
            dry_run: false,
            groups,
            unbound_impacts,
        },
        grouped,
    ))
}

/// Counts, per owning `group_id` (the `RelatesToNode_`'s own `rn.group_id`, i.e. the layer
/// group — FR-012's "broken out by the group_id that owns each affected pointer"), how many
/// live cross-group pointers have a `source_group_id` in `purged_group_ids`.
///
/// Excludes any `RelatesToNode_` whose own owning group (`rn_group_id`) is itself among
/// `purged_group_ids`: FR-002 allows purging several `group_ids` in one atomic call, and a
/// `RelatesToNode_` owned by one of *those* groups is `DETACH DELETE`d outright by
/// `delete_relates_to_by_group_ids` — it never reaches the `unbound` state, so counting it here
/// would misreport a deletion as a survival. Only a `RelatesToNode_` owned by a group *outside*
/// the call can be left `unbound` (FR-008/FR-009); its own count is already covered by that
/// group's `edges` count in `GroupPurgeCounts`.
///
/// Every remaining candidate is guaranteed to resolve `Unbound` once the purge runs, regardless
/// of its binding_state right now (even an already-`Unbound` pointer is re-checked and stays
/// `Unbound`) — because the purge empties the entire source group before any rebind pass runs,
/// so there is nothing left for `resolve_endpoint` to bind to. That's what makes this
/// pre-mutation query usable, unmodified, as both the dry-run prediction and the actual
/// post-purge outcome (SC-009).
fn compute_unbound_impacts(
    conn: &Conn,
    purged_group_ids: &[&str],
) -> Result<Vec<UnboundImpact>, Error> {
    let purged: HashSet<&str> = purged_group_ids.iter().copied().collect();
    let mut tally: BTreeMap<String, u64> = BTreeMap::new();
    for (_, _, rn_group_id, attrs) in conn.list_cross_group_pointer_candidates()? {
        if purged.contains(rn_group_id.as_str()) {
            continue;
        }
        for (_, ptr) in pointer::read_pointers(&attrs).iter() {
            if purged.contains(ptr.source_group_id.as_str()) {
                *tally.entry(rn_group_id.clone()).or_insert(0) += 1;
            }
        }
    }
    Ok(tally
        .into_iter()
        .map(|(owning_group_id, pointer_count)| UnboundImpact {
            owning_group_id,
            pointer_count,
        })
        .collect())
}
