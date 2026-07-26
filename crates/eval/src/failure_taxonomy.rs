//! Failure-taxonomy bucketing, ported verbatim (down to normalization details) from the
//! prior Python harness's `failure_taxonomy.py`
//! (`verveguy/liminis-framework@main:eval/extraction-quality/failure_taxonomy.py`), applied
//! to a judge's `unmatched_reference` (misses) and `unmatched_candidate` (extras) after
//! judging — not to raw string mismatches.
//!
//! Two normalization details matter for parity and are easy to over-generalize by
//! accident: the source's article check only strips a leading `"the "` (not `"a "`/`"an "`),
//! and its modifier/granularity token-subset check splits on whitespace only — it does
//! *not* strip punctuation or fold `-`/`_` to spaces (that folding is specific to the
//! separate `case_or_format` check).

use std::collections::HashSet;

fn normalize(s: &str) -> String {
    s.trim().to_lowercase()
}

/// Plain whitespace-split tokens of the trimmed/lowercased string — matches the Python
/// source's `set(n.split())` exactly (no punctuation stripping, no `-`/`_` folding; that
/// folding is scoped to `is_case_or_format_variant` only, per the source's own comment
/// that case/format variance is "very minor" and handled separately).
fn tokenize(s: &str) -> HashSet<String> {
    normalize(s)
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// Source only special-cases a leading `"the "` (not `"a "`/`"an "`).
fn strip_the(s: &str) -> &str {
    s.strip_prefix("the ").unwrap_or(s)
}

fn is_article_variant(a: &str, b: &str) -> bool {
    let na = normalize(a);
    let nb = normalize(b);
    na != nb && strip_the(&na) == strip_the(&nb)
}

/// True when `subset`'s tokens are a non-empty, proper subset of `superset`'s tokens.
fn is_token_subset(subset: &str, superset: &str) -> bool {
    let sub = tokenize(subset);
    let sup = tokenize(superset);
    !sub.is_empty() && sub.len() < sup.len() && sub.is_subset(&sup)
}

fn is_case_or_format_variant(a: &str, b: &str) -> bool {
    let na = normalize(a).replace(['-', '_'], " ");
    let nb = normalize(b).replace(['-', '_'], " ");
    a != b && na == nb
}

/// Buckets a missed reference entity (`unmatched_reference`) against the full candidate
/// name list: `article_dropped`, `modifier_dropped` (candidate name is a token subset of
/// the reference name), `granularity_merged` (reference name is a token subset of a
/// candidate's), `case_or_format`, else `missing_entity`.
pub fn classify_entity_miss(ref_name: &str, candidate_names: &[String]) -> &'static str {
    if candidate_names
        .iter()
        .any(|c| is_article_variant(ref_name, c))
    {
        return "article_dropped";
    }
    if candidate_names.iter().any(|c| is_token_subset(c, ref_name)) {
        return "modifier_dropped";
    }
    if candidate_names.iter().any(|c| is_token_subset(ref_name, c)) {
        return "granularity_merged";
    }
    if candidate_names
        .iter()
        .any(|c| is_case_or_format_variant(ref_name, c))
    {
        return "case_or_format";
    }
    "missing_entity"
}

/// Buckets an extra candidate entity (`unmatched_candidate`) against the full reference
/// name list: `article_dropped`, `modifier_added` (reference name is a token subset of the
/// candidate name), `granularity_split` (candidate name is a token subset of a
/// reference's), else `extra_entity`.
pub fn classify_entity_extra(cand_name: &str, reference_names: &[String]) -> &'static str {
    if reference_names
        .iter()
        .any(|r| is_article_variant(cand_name, r))
    {
        return "article_dropped";
    }
    if reference_names
        .iter()
        .any(|r| is_token_subset(r, cand_name))
    {
        return "modifier_added";
    }
    if reference_names
        .iter()
        .any(|r| is_token_subset(cand_name, r))
    {
        return "granularity_split";
    }
    "extra_entity"
}

#[derive(Debug, Clone, Copy)]
pub struct EdgeKey<'a> {
    pub source: &'a str,
    pub target: &'a str,
    pub relation_type: &'a str,
}

fn eq_ci(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

/// Buckets a missed reference edge against the full candidate edge list:
/// `inverted_edge` (same pair, reversed), `synonym_relation` (same pair, different
/// relation type — the judge treated it as non-equivalent but source/target matched),
/// else `missing_edge`.
pub fn classify_edge_miss(missed: &EdgeKey, candidates: &[EdgeKey]) -> &'static str {
    if candidates
        .iter()
        .any(|c| eq_ci(c.source, missed.target) && eq_ci(c.target, missed.source))
    {
        return "inverted_edge";
    }
    if candidates.iter().any(|c| {
        eq_ci(c.source, missed.source)
            && eq_ci(c.target, missed.target)
            && !eq_ci(c.relation_type, missed.relation_type)
    }) {
        return "synonym_relation";
    }
    "missing_edge"
}

/// Edge extras have no sub-bucketing on the extra side (ported taxonomy).
pub fn classify_edge_extra() -> &'static str {
    "extra_edge"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn entity_miss_article_dropped() {
        assert_eq!(
            classify_entity_miss("The White House", &names(&["White House"])),
            "article_dropped"
        );
    }

    #[test]
    fn entity_miss_article_scope_excludes_a_and_an() {
        // The ported source only special-cases "the ", not "a "/"an ". An "a X"/"X" pair
        // still falls through to the generic token-subset check (X's tokens are a strict
        // subset of "A X"'s), landing on modifier_dropped rather than being misclassified
        // as article_dropped the way an over-generalized "a"/"an"/"the" article check would.
        assert_eq!(
            classify_entity_miss("A White House", &names(&["White House"])),
            "modifier_dropped"
        );
    }

    #[test]
    fn entity_miss_modifier_dropped() {
        // Candidate "Alice" is a token subset of reference "Alice Smith".
        assert_eq!(
            classify_entity_miss("Alice Smith", &names(&["Alice"])),
            "modifier_dropped"
        );
    }

    #[test]
    fn entity_miss_granularity_merged() {
        // Reference "Alice" is a token subset of candidate "Alice Smith".
        assert_eq!(
            classify_entity_miss("Alice", &names(&["Alice Smith"])),
            "granularity_merged"
        );
    }

    #[test]
    fn entity_miss_case_or_format() {
        assert_eq!(
            classify_entity_miss("New-York", &names(&["new york"])),
            "case_or_format"
        );
    }

    #[test]
    fn entity_miss_modifier_check_does_not_fold_hyphens() {
        // The ported source's modifier/granularity check splits on whitespace only — it
        // must not treat "New-York" and "New York" as token-equal the way the dedicated
        // case_or_format check does. "New-York-City" has no whitespace-token subset
        // relationship with "New York", so this falls through to missing_entity rather
        // than modifier_dropped/granularity_merged.
        assert_eq!(
            classify_entity_miss("New-York-City", &names(&["New York"])),
            "missing_entity"
        );
    }

    #[test]
    fn entity_miss_falls_back_to_missing_entity() {
        assert_eq!(
            classify_entity_miss("Alice", &names(&["Bob", "Carol"])),
            "missing_entity"
        );
    }

    #[test]
    fn entity_miss_missing_entity_with_no_candidates() {
        assert_eq!(classify_entity_miss("Alice", &[]), "missing_entity");
    }

    #[test]
    fn entity_extra_article_dropped() {
        assert_eq!(
            classify_entity_extra("White House", &names(&["The White House"])),
            "article_dropped"
        );
    }

    #[test]
    fn entity_extra_modifier_added() {
        // Reference "Alice" is a token subset of candidate "Alice Smith".
        assert_eq!(
            classify_entity_extra("Alice Smith", &names(&["Alice"])),
            "modifier_added"
        );
    }

    #[test]
    fn entity_extra_granularity_split() {
        // Candidate "Alice" is a token subset of reference "Alice Smith".
        assert_eq!(
            classify_entity_extra("Alice", &names(&["Alice Smith"])),
            "granularity_split"
        );
    }

    #[test]
    fn entity_extra_falls_back_to_extra_entity() {
        assert_eq!(
            classify_entity_extra("Dave", &names(&["Bob", "Carol"])),
            "extra_entity"
        );
    }

    #[test]
    fn edge_miss_inverted_edge() {
        let missed = EdgeKey {
            source: "Alice",
            target: "Acme",
            relation_type: "WORKS_AT",
        };
        let candidates = [EdgeKey {
            source: "Acme",
            target: "Alice",
            relation_type: "EMPLOYS",
        }];
        assert_eq!(classify_edge_miss(&missed, &candidates), "inverted_edge");
    }

    #[test]
    fn edge_miss_synonym_relation() {
        let missed = EdgeKey {
            source: "Alice",
            target: "Acme",
            relation_type: "WORKS_AT",
        };
        let candidates = [EdgeKey {
            source: "Alice",
            target: "Acme",
            relation_type: "EMPLOYED_BY",
        }];
        assert_eq!(classify_edge_miss(&missed, &candidates), "synonym_relation");
    }

    #[test]
    fn edge_miss_falls_back_to_missing_edge() {
        let missed = EdgeKey {
            source: "Alice",
            target: "Acme",
            relation_type: "WORKS_AT",
        };
        let candidates = [EdgeKey {
            source: "Bob",
            target: "Other Corp",
            relation_type: "WORKS_AT",
        }];
        assert_eq!(classify_edge_miss(&missed, &candidates), "missing_edge");
    }

    #[test]
    fn edge_extra_is_always_extra_edge() {
        assert_eq!(classify_edge_extra(), "extra_edge");
    }
}
