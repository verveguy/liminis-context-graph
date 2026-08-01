//! Shared `max_tokens` policy for extraction calls (#307 FR-001/FR-002/FR-003/FR-006).
//!
//! Replaces the four `INITIAL_MAX_TOKENS: u32 = 8192` constants that used to be hardcoded,
//! identically, at each Anthropic/OAI-compatible entity/edge call site. The output budget now
//! scales with input size (FR-002) instead of being a single uniform default that binds
//! hardest exactly where content is densest, and it uses one ceiling shared across providers
//! and call types (FR-006) — the ceiling's job is to stop genuine non-termination, not to
//! optimize spend, so it is deliberately generous rather than tight (see ADR-0307).

/// Which half of a chunk's extraction this call is for. Edge prompts are larger (the ontology
/// alone adds +1,734 chars per the issue) and edge output is typically denser than entity
/// output for the same input, so edges get a higher per-byte ratio — the ceiling itself stays
/// uniform across both (FR-006 governs providers, not call types).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractionCallType {
    Entities,
    Edges,
}

/// Floor: the minimum initial budget regardless of how small the input is. Compiled-in, not
/// configurable — FR-003 only requires the ceiling to be tunable.
pub const MAX_TOKENS_FLOOR: u32 = 4_096;

/// Default ceiling, overridable via `LCG_EXTRACTION_MAX_TOKENS_CEILING` (FR-003). Sized well
/// above plausible legitimate need (the corpus max chunk needs ~25K tokens for edges) so it
/// catches genuine non-termination — e.g. the observed gemma-4 case, a full 8,192 tokens
/// generated from a 1,500-char input — rather than trimming well-behaved responses.
pub const MAX_TOKENS_CEILING_DEFAULT: u32 = 32_768;

const ENTITY_TOKENS_PER_INPUT_BYTE: f64 = 1.0;
const EDGE_TOKENS_PER_INPUT_BYTE: f64 = 1.5;

/// Resolves the uniform ceiling from `LCG_EXTRACTION_MAX_TOKENS_CEILING`, falling back to
/// [`MAX_TOKENS_CEILING_DEFAULT`] on an absent or invalid value (soft fallback + warn, matching
/// the WAL-knobs precedent in `app_state.rs` — env is re-read per call rather than cached, since
/// the cost is negligible next to the network round-trip it precedes).
pub fn resolve_max_tokens_ceiling() -> u32 {
    std::env::var("LCG_EXTRACTION_MAX_TOKENS_CEILING")
        .ok()
        .and_then(|v| {
            v.parse::<u32>()
                .map_err(|_| {
                    eprintln!(
                        "liminis-context-graph: LCG_EXTRACTION_MAX_TOKENS_CEILING={v:?} is not a \
                         valid u32; using default {MAX_TOKENS_CEILING_DEFAULT}"
                    );
                })
                .ok()
        })
        .unwrap_or(MAX_TOKENS_CEILING_DEFAULT)
}

/// Computes the initial `max_tokens` budget for a call: `clamp(chunk_len_bytes * ratio, FLOOR,
/// ceiling)`, where `ratio` depends on `call_type` (FR-002).
pub fn compute_initial_max_tokens(
    chunk_len_bytes: usize,
    call_type: ExtractionCallType,
    ceiling: u32,
) -> u32 {
    let ratio = match call_type {
        ExtractionCallType::Entities => ENTITY_TOKENS_PER_INPUT_BYTE,
        ExtractionCallType::Edges => EDGE_TOKENS_PER_INPUT_BYTE,
    };
    let scaled = (chunk_len_bytes as f64 * ratio).round();
    let scaled = if scaled.is_finite() {
        scaled.clamp(0.0, u32::MAX as f64) as u32
    } else {
        u32::MAX
    };
    scaled.clamp(MAX_TOKENS_FLOOR, ceiling)
}

/// Computes the next `max_tokens` value for the existing one-shot doubling retry, clamped to
/// `ceiling`. Returns `None` when `current` has already reached `ceiling` — doubling from there
/// would just resend an identical request and fail identically, wasting a round-trip.
pub fn next_retry_max_tokens(current: u32, ceiling: u32) -> Option<u32> {
    if current >= ceiling {
        return None;
    }
    Some(current.saturating_mul(2).min(ceiling))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_initial_max_tokens_applies_floor_for_small_chunks() {
        let got = compute_initial_max_tokens(10, ExtractionCallType::Entities, 32_768);
        assert_eq!(got, MAX_TOKENS_FLOOR);
    }

    #[test]
    fn compute_initial_max_tokens_scales_with_input_size() {
        let small = compute_initial_max_tokens(10_000, ExtractionCallType::Entities, 32_768);
        let large = compute_initial_max_tokens(20_000, ExtractionCallType::Entities, 32_768);
        assert!(large > small, "budget must grow with input size");
    }

    #[test]
    fn compute_initial_max_tokens_clamps_to_ceiling() {
        let got = compute_initial_max_tokens(1_000_000, ExtractionCallType::Edges, 32_768);
        assert_eq!(got, 32_768);
    }

    #[test]
    fn compute_initial_max_tokens_edges_ratio_exceeds_entities_ratio() {
        let entities = compute_initial_max_tokens(10_000, ExtractionCallType::Entities, 32_768);
        let edges = compute_initial_max_tokens(10_000, ExtractionCallType::Edges, 32_768);
        assert!(
            edges > entities,
            "edge calls must get a higher per-byte budget than entity calls for the same input"
        );
    }

    #[test]
    fn compute_initial_max_tokens_corpus_max_chunk_stays_under_ceiling() {
        // SC-003: the corpus's largest chunk (16,670 chars) must not saturate the ceiling on
        // either call type, leaving headroom for a retry-doubling if ever needed.
        let ceiling = MAX_TOKENS_CEILING_DEFAULT;
        let entities = compute_initial_max_tokens(16_670, ExtractionCallType::Entities, ceiling);
        let edges = compute_initial_max_tokens(16_670, ExtractionCallType::Edges, ceiling);
        assert!(entities < ceiling, "entities budget must have headroom");
        assert!(edges < ceiling, "edges budget must have headroom");
    }

    #[test]
    fn next_retry_max_tokens_doubles_and_clamps_to_ceiling() {
        assert_eq!(next_retry_max_tokens(8_192, 32_768), Some(16_384));
        assert_eq!(next_retry_max_tokens(20_000, 32_768), Some(32_768));
    }

    #[test]
    fn next_retry_max_tokens_returns_none_once_already_at_ceiling() {
        assert_eq!(next_retry_max_tokens(32_768, 32_768), None);
        assert_eq!(next_retry_max_tokens(40_000, 32_768), None);
    }

    #[test]
    fn resolve_max_tokens_ceiling_defaults_when_unset() {
        // Avoid mutating global env state in a parallel test run; only assert the pure default
        // path holds when the var happens to be absent, which is the common case in CI.
        if std::env::var("LCG_EXTRACTION_MAX_TOKENS_CEILING").is_err() {
            assert_eq!(resolve_max_tokens_ceiling(), MAX_TOKENS_CEILING_DEFAULT);
        }
    }
}
