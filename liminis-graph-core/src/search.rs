use std::collections::HashMap;
use std::sync::Arc;

use crate::{
    db::Db,
    embedder::Embedder,
    error::Error,
    types::{EntityRow, RelatesToEdge},
};

/// Reciprocal Rank Fusion (AD-7).
///
/// `rrf_score(rank) = 1.0 / (rank + 60.0)`
/// Returns UUIDs sorted by descending fused score; UUID tie-breaking for determinism.
pub fn rrf_fuse(bm25: &[(String, f64)], vector: &[(String, f64)]) -> Vec<String> {
    let mut scores: HashMap<String, f64> = HashMap::new();

    for (rank, (uuid, _)) in bm25.iter().enumerate() {
        *scores.entry(uuid.clone()).or_default() += 1.0 / (rank as f64 + 60.0);
    }
    for (rank, (uuid, _)) in vector.iter().enumerate() {
        *scores.entry(uuid.clone()).or_default() += 1.0 / (rank as f64 + 60.0);
    }

    let mut ranked: Vec<(String, f64)> = scores.into_iter().collect();
    // Descending score, UUID tie-break for determinism
    ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    ranked.into_iter().map(|(uuid, _)| uuid).collect()
}

/// Hybrid BM25 + HNSW entity search with RRF fusion (HOT path).
pub async fn hybrid_entity_search(
    db: Arc<Db>,
    embedder: Arc<Embedder>,
    query: &str,
    group_ids: Vec<String>,
    limit: usize,
) -> Result<Vec<EntityRow>, Error> {
    // Async: embed the query
    let embedding = embedder.embed(query).await?;

    // Sync: DB operations in spawn_blocking
    let query_owned = query.to_string();
    let results = tokio::task::spawn_blocking(move || -> Result<Vec<EntityRow>, Error> {
        let conn = db.connect()?;
        let gid_refs: Vec<&str> = group_ids.iter().map(String::as_str).collect();
        let candidate_limit = limit * 3;

        let bm25 = conn
            .fts_search_entities(&query_owned, &gid_refs, candidate_limit)?;
        let vector = conn
            .vector_search_entities(&embedding, &gid_refs, candidate_limit)?;

        let fused_uuids = rrf_fuse(&bm25, &vector);
        let top_uuids: Vec<String> = fused_uuids.into_iter().take(limit).collect();
        conn.get_entities_by_uuids(&top_uuids)
    })
    .await??;

    Ok(results)
}

/// Hybrid BM25 + HNSW edge (fact) search with RRF fusion (HOT path).
pub async fn hybrid_edge_search(
    db: Arc<Db>,
    embedder: Arc<Embedder>,
    query: &str,
    group_ids: Vec<String>,
    limit: usize,
) -> Result<Vec<RelatesToEdge>, Error> {
    let embedding = embedder.embed(query).await?;

    let query_owned = query.to_string();
    let results = tokio::task::spawn_blocking(move || -> Result<Vec<RelatesToEdge>, Error> {
        let conn = db.connect()?;
        let gid_refs: Vec<&str> = group_ids.iter().map(String::as_str).collect();
        let candidate_limit = limit * 3;

        let bm25 = conn
            .fts_search_edges(&query_owned, &gid_refs, candidate_limit)?;
        let vector = conn
            .vector_search_edges(&embedding, &gid_refs, candidate_limit)?;

        let fused_uuids = rrf_fuse(&bm25, &vector);
        let top_uuids: Vec<String> = fused_uuids.into_iter().take(limit).collect();
        conn.get_relates_to_by_uuids(&top_uuids)
    })
    .await??;

    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rrf_fuse_empty() {
        let result = rrf_fuse(&[], &[]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_rrf_fuse_single_list() {
        let bm25 = vec![
            ("a".to_string(), 1.0),
            ("b".to_string(), 0.8),
            ("c".to_string(), 0.5),
        ];
        let result = rrf_fuse(&bm25, &[]);
        assert_eq!(result, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_rrf_fuse_overlap_boosts() {
        // 'b' appears in both lists at rank 0 → should score highest
        let bm25 = vec![("a".to_string(), 1.0), ("b".to_string(), 0.9)];
        let vector = vec![("b".to_string(), 0.1), ("c".to_string(), 0.05)];
        let result = rrf_fuse(&bm25, &vector);
        assert_eq!(result[0], "b", "overlapping entry should rank first");
    }

    #[test]
    fn test_rrf_fuse_deterministic_tie_break() {
        // Two entries with identical scores → sorted by UUID alphabetically
        let bm25 = vec![("z".to_string(), 1.0), ("a".to_string(), 1.0)];
        let vector = vec![("a".to_string(), 1.0), ("z".to_string(), 1.0)];
        let result = rrf_fuse(&bm25, &vector);
        // Both have same rrf score; UUID tie-break gives "a" < "z"
        assert_eq!(result[0], "a");
        assert_eq!(result[1], "z");
    }
}
