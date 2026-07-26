//! Strict string-match scoring (ported from the prior Python harness's `metrics.py`) —
//! the SC-004 comparator that the judge must beat: strict matching puts a sonnet-vs-itself
//! comparison at a wording-variance floor well below the judge's near-1.0 score, which is
//! exactly the property that motivates using a judge at all.

use std::collections::HashSet;

use lcg_core::{ExtractedEdge, ExtractedEntity};

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Prf1 {
    pub precision: f64,
    pub recall: f64,
    pub f1: f64,
}

fn prf1_from_sets<T: Eq + std::hash::Hash>(reference: &HashSet<T>, candidate: &HashSet<T>) -> Prf1 {
    let tp = reference.intersection(candidate).count() as f64;
    let precision = if candidate.is_empty() {
        1.0
    } else {
        tp / candidate.len() as f64
    };
    let recall = if reference.is_empty() {
        1.0
    } else {
        tp / reference.len() as f64
    };
    let f1 = if precision + recall == 0.0 {
        0.0
    } else {
        2.0 * precision * recall / (precision + recall)
    };
    Prf1 {
        precision,
        recall,
        f1,
    }
}

fn normalize(s: &str) -> String {
    s.trim().to_lowercase()
}

/// Entities: `name_set` — case-folded, trimmed name string set (ported `metrics.py`).
pub fn entity_strict_prf1(reference: &[ExtractedEntity], candidate: &[ExtractedEntity]) -> Prf1 {
    let ref_set: HashSet<String> = reference.iter().map(|e| normalize(&e.name)).collect();
    let cand_set: HashSet<String> = candidate.iter().map(|e| normalize(&e.name)).collect();
    prf1_from_sets(&ref_set, &cand_set)
}

/// Edges: `edge_set` — `(source_entity_name, target_entity_name, fact_type)` tuples,
/// case-folded (ported `metrics.py`). This repo's edges carry `relation_type` as the
/// `fact_type` equivalent.
pub fn edge_strict_prf1(reference: &[ExtractedEdge], candidate: &[ExtractedEdge]) -> Prf1 {
    let key = |e: &ExtractedEdge| {
        (
            normalize(&e.source_name),
            normalize(&e.target_name),
            normalize(e.relation_type.as_deref().unwrap_or("")),
        )
    };
    let ref_set: HashSet<_> = reference.iter().map(key).collect();
    let cand_set: HashSet<_> = candidate.iter().map(key).collect();
    prf1_from_sets(&ref_set, &cand_set)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entity(name: &str) -> ExtractedEntity {
        ExtractedEntity {
            name: name.to_string(),
            entity_type: "Thing".to_string(),
            summary: String::new(),
        }
    }

    fn edge(source: &str, target: &str, relation_type: &str) -> ExtractedEdge {
        ExtractedEdge {
            source_name: source.to_string(),
            target_name: target.to_string(),
            fact: String::new(),
            relation_type: Some(relation_type.to_string()),
            valid_at: None,
            invalid_at: None,
        }
    }

    #[test]
    fn entity_exact_match_is_perfect() {
        let refs = vec![entity("Alice"), entity("Bob")];
        let cands = vec![entity("Alice"), entity("Bob")];
        let prf1 = entity_strict_prf1(&refs, &cands);
        assert_eq!(
            prf1,
            Prf1 {
                precision: 1.0,
                recall: 1.0,
                f1: 1.0
            }
        );
    }

    #[test]
    fn entity_case_and_whitespace_insensitive() {
        let refs = vec![entity("Alice")];
        let cands = vec![entity("  alice  ")];
        let prf1 = entity_strict_prf1(&refs, &cands);
        assert_eq!(prf1.f1, 1.0);
    }

    #[test]
    fn entity_wording_variance_is_penalized_strictly() {
        // "Alice Smith" vs "Alice" — strict string matching does NOT treat these as
        // equivalent, unlike a semantic judge. This is exactly SC-004's point.
        let refs = vec![entity("Alice Smith")];
        let cands = vec![entity("Alice")];
        let prf1 = entity_strict_prf1(&refs, &cands);
        assert_eq!(prf1.f1, 0.0);
    }

    #[test]
    fn entity_both_empty_is_perfect() {
        let prf1 = entity_strict_prf1(&[], &[]);
        assert_eq!(
            prf1,
            Prf1 {
                precision: 1.0,
                recall: 1.0,
                f1: 1.0
            }
        );
    }

    #[test]
    fn entity_partial_overlap() {
        let refs = vec![entity("Alice"), entity("Bob")];
        let cands = vec![entity("Alice"), entity("Carol")];
        let prf1 = entity_strict_prf1(&refs, &cands);
        assert!((prf1.precision - 0.5).abs() < 1e-9);
        assert!((prf1.recall - 0.5).abs() < 1e-9);
    }

    #[test]
    fn edge_exact_match_is_perfect() {
        let refs = vec![edge("Alice", "Acme", "WORKS_AT")];
        let cands = vec![edge("Alice", "Acme", "WORKS_AT")];
        let prf1 = edge_strict_prf1(&refs, &cands);
        assert_eq!(prf1.f1, 1.0);
    }

    #[test]
    fn edge_relation_type_synonym_is_penalized_strictly() {
        // "WORKS_AT" vs "EMPLOYED_BY" is exactly the kind of judge-equivalent-but-
        // strictly-different pair SC-004 needs to distinguish.
        let refs = vec![edge("Alice", "Acme", "WORKS_AT")];
        let cands = vec![edge("Alice", "Acme", "EMPLOYED_BY")];
        let prf1 = edge_strict_prf1(&refs, &cands);
        assert_eq!(prf1.f1, 0.0);
    }

    #[test]
    fn edge_missing_relation_type_normalizes_to_empty_string() {
        let refs = vec![ExtractedEdge {
            source_name: "Alice".to_string(),
            target_name: "Acme".to_string(),
            fact: String::new(),
            relation_type: None,
            valid_at: None,
            invalid_at: None,
        }];
        let cands = refs.clone();
        let prf1 = edge_strict_prf1(&refs, &cands);
        assert_eq!(prf1.f1, 1.0);
    }
}
