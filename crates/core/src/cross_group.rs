//! Cross-group edge creation and pointer resolution/re-binding for the hub/layer-graph
//! topology (issue #369).
//!
//! An edge whose endpoints are not all within its own `group_id` is created here, not through
//! `episode.rs`'s extraction-time Phase C (which is confirmed intra-batch/intra-group only —
//! cross-group edges are a separate mechanism, never a variant of extraction, and never reach
//! ADR-0051's commit-time drop). See `crate::pointer` for the persisted data model and
//! `docs/adr/0369-resolvable-cross-group-pointers.md` for the full design.

use uuid::Uuid;

use crate::{
    db::Conn,
    error::Error,
    pointer::{self, BindingState, CrossGroupPointer, CrossGroupPointers, EndpointSide},
    prompts::normalize_name,
    types::RelatesToEdge,
};

/// Empty-string sentinel for `RelatesToEdge.source_node_uuid`/`target_node_uuid` marking a
/// foreign endpoint that is currently `Unbound`/`Ambiguous` — never a valid entity UUID, so
/// `db::insert_cross_group_edge`'s `MATCH {uuid: ""}` naturally binds nothing.
const UNRESOLVED: &str = "";

/// One endpoint of a cross-group edge as supplied by the caller.
#[derive(Debug, Clone)]
pub enum EndpointSpec {
    /// An endpoint already known to live in the edge's own `group_id` — no pointer needed.
    Uuid(String),
    /// An endpoint foreign to the edge's own `group_id`, identified by name in its source
    /// group. Always resolved and given pointer fields (FR-001/FR-002).
    Foreign {
        source_group_id: String,
        endpoint_name: String,
    },
}

/// Inputs for [`create_cross_group_edge`].
pub struct CreateCrossGroupEdgeParams {
    pub name: String,
    pub source: EndpointSpec,
    pub target: EndpointSpec,
    /// The edge's own `group_id` — the "layer" group, which may differ from either endpoint's
    /// group for a genuinely cross-group edge.
    pub group_id: String,
    pub fact: String,
    pub fact_embedding: Vec<f32>,
    pub valid_at: Option<String>,
    pub relation_type: Option<String>,
}

/// Resolves a single foreign endpoint against `(source_group_id, endpoint_name)`, reusing the
/// exact ADR-0283 authority (`get_entity_by_name_ci_with_scan_fallback`) rather than a second
/// implementation (FR-003) — the entire feature depends on there being exactly one notion of
/// "what this name resolves to" per source group.
///
/// Ambiguity detection runs only after a winning candidate is found, via
/// `count_active_entities_by_name_ci` (Merged-tombstone-excluded, unlike the raw
/// `count_entities_by_name_ci` dedup-regression counter) — so a name shared by a canonical and
/// its own merged-away aliases resolves `Bound` to the canonical (User Story 2 AC 4), while a
/// name genuinely shared by two or more *active* entities is reported `Ambiguous` rather than
/// silently taking the name index's winner (FR-006).
///
/// If the winner is itself `Merged` (issue #371: a merge that also changed the canonical's name
/// leaves re-resolution landing back on the tombstoned alias, since name resolution deliberately
/// resolves *through* `Merged` rows), the winner's `merged_into` forwarding reference is followed
/// to a fixpoint — repeating while the target is itself `Merged` — rather than reporting the
/// tombstone as `Bound` (FR-007). A visited-UUID guard bounds the walk against a cycle (FR-008);
/// a cycle, a missing `merged_into`, or a dangling `merged_into` target all resolve `Unbound`
/// rather than ever reporting a dead end as `Bound` (FR-009).
///
/// Returns `(binding_state, resolved_uuid)`; `resolved_uuid` is `Some` only when
/// `binding_state == Bound`.
pub fn resolve_endpoint(
    conn: &Conn,
    source_group_id: &str,
    endpoint_name: &str,
) -> Result<(BindingState, Option<String>), Error> {
    let Some(winner) =
        conn.get_entity_by_name_ci_with_scan_fallback(endpoint_name, source_group_id)?
    else {
        return Ok((BindingState::Unbound, None));
    };
    if conn.count_active_entities_by_name_ci(endpoint_name, source_group_id)? > 1 {
        return Ok((BindingState::Ambiguous, None));
    }
    if !winner.labels.contains(&"Merged".to_string()) {
        return Ok((BindingState::Bound, Some(winner.uuid)));
    }

    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    visited.insert(winner.uuid.clone());
    let mut current = winner;
    loop {
        let Some(target_uuid) = pointer::read_merged_into(&current.attributes) else {
            return Ok((BindingState::Unbound, None)); // no forwarding reference recorded
        };
        if !visited.insert(target_uuid.clone()) {
            return Ok((BindingState::Unbound, None)); // cycle guard (FR-008)
        }
        let Some(next) = conn.get_entity_by_uuid(&target_uuid)? else {
            return Ok((BindingState::Unbound, None)); // dangling merged_into target
        };
        if !next.labels.contains(&"Merged".to_string()) {
            return Ok((BindingState::Bound, Some(next.uuid)));
        }
        current = next;
    }
}

/// Resolves one endpoint spec into `(uuid_for_the_hop, pointer_if_foreign)`. Read-only — no
/// writes happen here, so a caller can resolve both endpoints and bail (via `?`) before either
/// touches the database, keeping a validation failure a true no-op (FR-002/SC-005: "no partial
/// edge is written").
fn resolve_side(
    conn: &Conn,
    spec: &EndpointSpec,
    edge_group_id: &str,
    applied_seq: Option<u64>,
) -> Result<(Option<String>, Option<CrossGroupPointer>), Error> {
    match spec {
        EndpointSpec::Uuid(uuid) => {
            let entity = conn.get_entity_by_uuid(uuid)?.ok_or_else(|| {
                Error::Ipc(format!("cross-group edge endpoint {uuid} does not exist"))
            })?;
            if entity.group_id != edge_group_id {
                return Err(Error::Ipc(format!(
                    "endpoint {uuid} belongs to group '{}', foreign to this edge's own group \
                     '{}' — foreign endpoints must be supplied as EndpointSpec::Foreign with \
                     pointer fields, not a bare UUID (FR-002)",
                    entity.group_id, edge_group_id
                )));
            }
            Ok((Some(uuid.clone()), None))
        }
        EndpointSpec::Foreign {
            source_group_id,
            endpoint_name,
        } => {
            if source_group_id.trim().is_empty() || endpoint_name.trim().is_empty() {
                return Err(Error::Ipc(
                    "cross-group edge endpoint requires non-empty source_group_id and \
                     endpoint_name (FR-002)"
                        .to_string(),
                ));
            }
            let normalized_name = normalize_name(endpoint_name);
            let (state, resolved_uuid) = resolve_endpoint(conn, source_group_id, &normalized_name)?;
            let ptr = CrossGroupPointer {
                source_group_id: source_group_id.clone(),
                endpoint_name: normalized_name,
                resolved_uuid: resolved_uuid.clone(),
                bound_at_seq: applied_seq,
                binding_state: state,
            };
            Ok((resolved_uuid, Some(ptr)))
        }
    }
}

/// Creates a cross-group `RelatesToEdge`. Every `EndpointSpec::Foreign` endpoint is resolved
/// and given pointer fields; every `EndpointSpec::Uuid` endpoint must actually belong to the
/// edge's own `group_id` or the call is rejected before any write (FR-002/SC-005). A foreign
/// endpoint that doesn't currently resolve is retained as `Unbound` rather than dropped
/// (FR-004, diverging from ADR-0051) — only that side's hop is left absent
/// (`db::insert_cross_group_edge`).
pub fn create_cross_group_edge(
    conn: &Conn,
    params: CreateCrossGroupEdgeParams,
    ts: &str,
) -> Result<RelatesToEdge, Error> {
    let applied_seq = conn.get_applied_seq()?;

    let (source_uuid, src_pointer) =
        resolve_side(conn, &params.source, &params.group_id, applied_seq)?;
    let (target_uuid, dst_pointer) =
        resolve_side(conn, &params.target, &params.group_id, applied_seq)?;

    let mut pointers = CrossGroupPointers::default();
    if let Some(p) = src_pointer {
        pointers.set(EndpointSide::Src, p);
    }
    if let Some(p) = dst_pointer {
        pointers.set(EndpointSide::Dst, p);
    }
    let attributes = pointer::write_pointers("{}", &pointers);

    let edge = RelatesToEdge {
        uuid: Uuid::new_v4().to_string(),
        name: params.name,
        source_node_uuid: source_uuid.unwrap_or_else(|| UNRESOLVED.to_string()),
        target_node_uuid: target_uuid.unwrap_or_else(|| UNRESOLVED.to_string()),
        group_id: params.group_id,
        fact: params.fact,
        fact_embedding: params.fact_embedding,
        created_at: ts.to_string(),
        valid_at: params.valid_at,
        invalid_at: None,
        attributes,
        relation_type: params.relation_type,
        episode_uuids: vec![],
        source_descriptions: vec![],
    };

    conn.insert_cross_group_edge(&edge)?;
    Ok(edge)
}

/// Outcome counters for a [`rebind_pointers`] pass (FR-012-adjacent observability for the
/// operation itself, distinct from `knowledge_status`'s aggregate `PointerStateCounts`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RebindCounts {
    /// Pointers actually re-resolved (i.e. not skipped by the staleness gate).
    pub checked: u64,
    pub bound: u64,
    pub unbound: u64,
    pub ambiguous: u64,
    pub invalidated_self_loop: u64,
    pub invalidated_duplicate: u64,
}

/// Re-resolves every cross-group pointer whose `source_group_id == source_group_id`, for use
/// after a source group's purge → rehydrate refresh cycle (User Story 3). For each pointer:
///
/// - Skipped entirely if `bound_at_seq >= source.WalPosition.applied_seq` (FR-007's staleness
///   gate; per-database singleton until #360 adds per-source positions). This is also what
///   makes a second call with no intervening WAL activity a true no-op (FR-009): every pointer
///   this call touches has its `bound_at_seq` bumped to the current applied position, so the
///   next call's gate skips it. The gate only engages when *both* the pointer's `bound_at_seq`
///   and the current applied position are known (`Some`): if no `WalPosition` has been recorded
///   yet, `bound_at_seq` is written as `None` too, so every pointer is re-resolved on every pass
///   until a position exists to compare against — always-recheck, not always-skip, is the safe
///   default for "we don't yet know if this is stale."
/// - Otherwise re-resolved via [`resolve_endpoint`] (the same ADR-0283 authority, FR-003). Both
///   sides are fully resolved *before* either hop is written, and the self-loop/duplicate check
///   runs once against the fully-prospective `(src, dst)` pair — not per side — so a pass that
///   resolves both a `src` and a `dst` pointer at once can never commit one side's hop and then
///   discover the *other* side's resolution collapses the edge into a self-loop or duplicate,
///   which would otherwise leave the first side's hop orphaned on an invalidated node. A
///   resolution that does collapse reuses `merge_entities_inner`'s exact handling
///   (`has_directed_edge` scoped to the edge's own `group_id` per ADR-0368, then
///   `invalidate_edge`) rather than a new policy (User Story 3 AC 5): the whole edge is
///   invalidated with neither hop touched, rather than partially rebound.
/// - A transition into `Bound` creates the missing hop (`Db::create_relates_to_hop`); a
///   transition out of `Bound` (rename, purge, now-ambiguous) drops the stale hop
///   (`Db::delete_relates_to_hop`) — matching the two-hop model's "unbound = missing hop"
///   representation (FR-004/FR-005), so read paths need no change (User Story 4 AC 1). The
///   direct `Entity→Entity` compat rel (`Db::create_relates_to_direct`/`delete_relates_to_direct`)
///   is kept in sync the same way — present iff both sides are currently resolved — so a pointer
///   bound after creation (rather than at creation, like `insert_cross_group_edge`'s case) isn't
///   permanently missing the direct rel that raw-Cypher consumers of `knowledge_query_cypher`
///   may query.
///
/// Safe to call against a partially-hydrated source: an endpoint that hasn't arrived yet simply
/// resolves `Unbound` again and is left with no hop (FR-009's mid-hydration safety).
pub fn rebind_pointers(
    conn: &Conn,
    source_group_id: &str,
    ts: &str,
) -> Result<RebindCounts, Error> {
    let current_seq = conn.get_applied_seq()?;
    let mut counts = RebindCounts::default();

    for (rn_uuid, rn_name, rn_group_id, attrs) in conn.list_cross_group_pointer_candidates()? {
        let mut pointers = pointer::read_pointers(&attrs);

        // Phase 1: resolve every side that needs re-checking, without writing anything yet —
        // so the self-loop/duplicate check below always sees a fully-resolved prospective pair,
        // never a state where one side's hop has already been committed this pass.
        struct SideResolution {
            side: EndpointSide,
            existing: CrossGroupPointer,
            new_state: BindingState,
            new_uuid: Option<String>,
        }
        let mut resolutions = Vec::new();
        for side in [EndpointSide::Src, EndpointSide::Dst] {
            let Some(existing) = pointers.get(side).cloned() else {
                continue;
            };
            if existing.source_group_id != source_group_id {
                continue;
            }
            if let (Some(bound_at), Some(current)) = (existing.bound_at_seq, current_seq) {
                if bound_at >= current {
                    continue; // already checked as of this source position (FR-009)
                }
            }
            counts.checked += 1;
            let (new_state, new_uuid) =
                resolve_endpoint(conn, source_group_id, &existing.endpoint_name)?;
            resolutions.push(SideResolution {
                side,
                existing,
                new_state,
                new_uuid,
            });
        }
        if resolutions.is_empty() {
            continue; // nothing on this candidate needs re-checking this pass
        }

        // The actual current hop state, straight from the graph — `get_relates_to_by_uuids`
        // derives `source_node_uuid`/`target_node_uuid` via `OPTIONAL MATCH` against the real
        // `Entity<->RelatesToNode_` hops, not from the pointer's cached `resolved_uuid`. Phase 3
        // below diffs against *this*, not against `existing.resolved_uuid`, so a hop that was
        // externally detached while the cached `resolved_uuid` still matches what re-resolution
        // produces (e.g. a source entity deleted and recreated under the same UUID) is still
        // restored rather than silently left missing.
        let current_hop = conn
            .get_relates_to_by_uuids(std::slice::from_ref(&rn_uuid))?
            .into_iter()
            .next();
        let (actual_src, actual_dst) = current_hop
            .map(|e| (e.source_node_uuid, e.target_node_uuid))
            .unwrap_or_default();

        // The fully-prospective (src, dst) pair: each resolved side's new value applied on top
        // of the actual current hop state, with any side *not* re-checked this pass left as-is.
        let (mut prospective_src, mut prospective_dst) = (actual_src.clone(), actual_dst.clone());
        for r in &resolutions {
            let val = r.new_uuid.clone().unwrap_or_default();
            match r.side {
                EndpointSide::Src => prospective_src = val,
                EndpointSide::Dst => prospective_dst = val,
            }
        }

        // Phase 2: self-loop/duplicate check against the fully-prospective pair, before either
        // hop is touched — reuses merge_entities_inner's exact handling (`has_directed_edge`
        // scoped to the edge's own group_id per ADR-0368, then `invalidate_edge`), applied once
        // per candidate so a two-sided resolution can never partially commit one side's hop and
        // then discover the other side's resolution collapses the edge (User Story 3 AC 5).
        //
        // Gated on `any_changed`: if every resolved side reconfirms its existing resolved_uuid
        // unchanged, the prospective pair is identical to the edge's own current hops, and
        // `has_directed_edge` (which does not exclude `rn_uuid` from its match) would otherwise
        // report the edge as a "duplicate" of itself on every idempotent re-check.
        let any_changed = resolutions
            .iter()
            .any(|r| r.new_uuid != r.existing.resolved_uuid);
        let both_resolved = !prospective_src.is_empty() && !prospective_dst.is_empty();
        if any_changed && both_resolved && prospective_src == prospective_dst {
            conn.invalidate_edge(&rn_uuid, ts)?;
            counts.invalidated_self_loop += 1;
            continue;
        }
        if any_changed
            && both_resolved
            && conn.has_directed_edge(&prospective_src, &prospective_dst, &rn_name, &rn_group_id)?
        {
            conn.invalidate_edge(&rn_uuid, ts)?;
            counts.invalidated_duplicate += 1;
            continue;
        }

        // Phase 3: the edge survives — apply each resolved side's hop change (if any) and
        // record its new state. Whether a hop needs (re)writing is decided against the *actual*
        // graph state (`actual_src`/`actual_dst`, from Phase 1's `OPTIONAL MATCH`), not against
        // `existing.resolved_uuid` — the two can diverge (e.g. the hop was detached without the
        // cached pointer ever being told), and only comparing against the actual state catches
        // that case rather than trusting a resolved-but-possibly-stale cache entry.
        let mut hop_changed = false;
        for r in &resolutions {
            let actual_uuid = match r.side {
                EndpointSide::Src => &actual_src,
                EndpointSide::Dst => &actual_dst,
            };
            let hop_present = !actual_uuid.is_empty();
            let hop_matches_desired = match &r.new_uuid {
                Some(u) => hop_present && actual_uuid == u,
                None => !hop_present,
            };
            if !hop_matches_desired {
                if r.new_state == BindingState::Bound {
                    if hop_present {
                        conn.delete_relates_to_hop(&rn_uuid, r.side)?;
                    }
                    conn.create_relates_to_hop(&rn_uuid, r.side, r.new_uuid.as_ref().unwrap())?;
                } else if hop_present {
                    conn.delete_relates_to_hop(&rn_uuid, r.side)?;
                }
                hop_changed = true;
            }

            match r.new_state {
                BindingState::Bound => counts.bound += 1,
                BindingState::Unbound => counts.unbound += 1,
                BindingState::Ambiguous => counts.ambiguous += 1,
            }

            pointers.set(
                r.side,
                CrossGroupPointer {
                    source_group_id: r.existing.source_group_id.clone(),
                    endpoint_name: r.existing.endpoint_name.clone(),
                    resolved_uuid: r.new_uuid.clone(),
                    bound_at_seq: current_seq,
                    binding_state: r.new_state,
                },
            );
        }

        let new_attrs = pointer::write_pointers(&attrs, &pointers);
        conn.update_relates_to_attributes(&rn_uuid, &new_attrs)?;

        if hop_changed {
            // Re-sync the direct compat rel to the two-hop model's final state. Delete first
            // (not just MERGE) because a stale rel from before this pass may point at a
            // different src/dst than the now-current resolution.
            conn.delete_relates_to_direct(&rn_uuid)?;
            if let Some(final_edge) = conn
                .get_relates_to_by_uuids(std::slice::from_ref(&rn_uuid))?
                .into_iter()
                .next()
            {
                if !final_edge.source_node_uuid.is_empty()
                    && !final_edge.target_node_uuid.is_empty()
                {
                    conn.create_relates_to_direct(&final_edge)?;
                }
            }
        }
    }

    Ok(counts)
}
