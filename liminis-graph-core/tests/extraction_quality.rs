// Extraction quality regression tests for issue #82.
//
// Two categories of tests:
//   1. Prompt content assertions — verify that the ported prompt files contain the
//      key rules that address each defect from the demo-notebook audit (no LLM calls).
//   2. ScriptedExtractor behavioral tests — verify that ScriptedExtractor returns
//      pre-scripted ExtractionResult values in order (zero-latency, no API calls).
//
// Note: filter-pipeline tests (SCREAMING_SNAKE_CASE normalization, self-ref rejection,
// entity-name validation) are covered by the inline unit tests in extractor.rs
// (apply_edge_filters_* and normalize_relation_type_* tests).

use liminis_graph_core::{
    prompts,
    types::{ExtractedEdge, ExtractedEntity, ExtractionResult, SourceType},
};

// ── Prompt content tests ───────────────────────────────────────────────────────

#[test]
fn source_type_dispatch_returns_distinct_prompts() {
    let text = prompts::entity_system_prompt(SourceType::Text);
    let message = prompts::entity_system_prompt(SourceType::Message);
    let json = prompts::entity_system_prompt(SourceType::Json);

    assert_ne!(text, message, "text and message prompts must differ");
    assert_ne!(text, json, "text and json prompts must differ");
    assert_ne!(message, json, "message and json prompts must differ");
}

#[test]
fn text_prompt_contains_key_extraction_rules() {
    let prompt = prompts::entity_system_prompt(SourceType::Text);
    assert!(
        prompt.contains("NEVER extract"),
        "text prompt must include NEVER extract directive"
    );
    assert!(
        prompt.contains("Wikipedia"),
        "text prompt must include Wikipedia specificity heuristic"
    );
    assert!(
        prompt.contains("Generic common nouns") || prompt.contains("generic common nouns"),
        "text prompt must forbid generic common nouns"
    );
    assert!(
        prompt.contains("temporal"),
        "text prompt must forbid temporal entities"
    );
}

#[test]
fn edge_prompt_contains_entity_validation_rule() {
    let prompt = prompts::edge_system_prompt();
    assert!(
        prompt.contains("ENTITIES"),
        "edge prompt must reference ENTITIES list"
    );
    assert!(
        prompt.contains("SCREAMING_SNAKE_CASE"),
        "edge prompt must require SCREAMING_SNAKE_CASE relation types"
    );
    assert!(
        prompt.contains("distinct"),
        "edge prompt must require distinct source and target"
    );
}

#[test]
fn edge_user_prompt_contains_entities_and_reference_time() {
    let entity_names = vec!["Alice".to_string(), "Acme Corp".to_string()];
    let prompt = prompts::edge_user_prompt(
        &entity_names,
        "2026-01-01T00:00:00Z",
        "Alice works at Acme Corp.",
        None,
    );
    assert!(
        prompt.contains("Alice"),
        "edge user prompt must list entity names"
    );
    assert!(
        prompt.contains("Acme Corp"),
        "edge user prompt must list entity names"
    );
    assert!(
        prompt.contains("REFERENCE_TIME"),
        "edge user prompt must include REFERENCE_TIME"
    );
    assert!(
        prompt.contains("2026-01-01T00:00:00Z"),
        "edge user prompt must embed the reference time value"
    );
}

#[test]
fn edge_user_prompt_appends_custom_instructions() {
    let names = vec!["Alice".to_string()];
    let prompt = prompts::edge_user_prompt(
        &names,
        "2026-01-01T00:00:00Z",
        "body",
        Some("Focus on financial relationships only."),
    );
    assert!(
        prompt.contains("Focus on financial relationships only."),
        "custom instructions must appear in edge user prompt"
    );
}

#[test]
fn entity_user_prompt_wraps_body_in_text_tags() {
    let prompt = prompts::entity_user_prompt("Hello world", None);
    assert!(
        prompt.contains("<TEXT>"),
        "entity user prompt must use TEXT tags"
    );
    assert!(
        prompt.contains("Hello world"),
        "entity user prompt must include episode body"
    );
}

#[test]
fn message_user_prompt_uses_current_message_tags() {
    let prompt = prompts::message_user_prompt("Alice: Hi there", None);
    assert!(
        prompt.contains("CURRENT MESSAGE"),
        "message user prompt must use CURRENT MESSAGE tags"
    );
}

#[test]
fn json_user_prompt_uses_json_tags() {
    let prompt = prompts::json_user_prompt("{\"name\": \"Alice\"}", None);
    assert!(
        prompt.contains("<JSON>"),
        "json user prompt must use JSON tags"
    );
}

// ── SCREAMING_SNAKE_CASE normalization test ────────────────────────────────────
//
// These tests exercise the `normalize_relation_type` function indirectly through
// the edge filter applied by ScriptedExtractor. Since ScriptedExtractor bypasses
// the HTTP layer and returns pre-scripted results, post-extraction filters don't
// apply through ScriptedExtractor (the filters are in AnthropicExtractor).
// We test the normalization logic through its observable effects on the
// `ExtractionResult` that would come from a live AnthropicExtractor.
//
// For now we test normalization at the prompt/unit level. Integration-level tests
// for the full filter pipeline are covered by the in-module unit tests in
// extractor.rs (apply_edge_filters_* and normalize_relation_type_* tests).

#[test]
fn scripted_extractor_returns_results_in_order() {
    use futures::executor::block_on;
    use liminis_graph_core::{ExtractOptions, Extractor, ScriptedExtractor};

    let results = vec![
        ExtractionResult {
            entities: vec![ExtractedEntity {
                name: "Alice".to_string(),
                entity_type: "Person".to_string(),
                summary: "A person".to_string(),
            }],
            edges: vec![],
        },
        ExtractionResult {
            entities: vec![ExtractedEntity {
                name: "Bob".to_string(),
                entity_type: "Person".to_string(),
                summary: "Another person".to_string(),
            }],
            edges: vec![],
        },
    ];

    let extractor = ScriptedExtractor::new(results);

    let opts1 = ExtractOptions {
        episode_body: "Alice is here",
        group_id: "g",
        source_type: SourceType::Text,
        custom_instructions: None,
        reference_time: "2026-01-01T00:00:00Z",
    };
    let opts2 = ExtractOptions {
        episode_body: "Bob is here",
        group_id: "g",
        source_type: SourceType::Text,
        custom_instructions: None,
        reference_time: "2026-01-01T00:00:00Z",
    };

    let r1 = block_on(extractor.extract(opts1)).unwrap();
    let r2 = block_on(extractor.extract(opts2)).unwrap();

    assert_eq!(
        r1.entities[0].name, "Alice",
        "first call should return Alice"
    );
    assert_eq!(r2.entities[0].name, "Bob", "second call should return Bob");
}

#[test]
#[should_panic(expected = "ScriptedExtractor script exhausted")]
fn scripted_extractor_panics_when_exhausted() {
    use futures::executor::block_on;
    use liminis_graph_core::{ExtractOptions, Extractor, ScriptedExtractor};

    let extractor = ScriptedExtractor::new(vec![]);
    let opts = ExtractOptions {
        episode_body: "body",
        group_id: "g",
        source_type: SourceType::Text,
        custom_instructions: None,
        reference_time: "2026-01-01T00:00:00Z",
    };
    let _ = block_on(extractor.extract(opts));
}

// ── Quality defect pattern tests ───────────────────────────────────────────────
//
// Each test documents a specific defect from the audit evidence table and
// verifies that the extraction prompts address it.

#[test]
fn generic_noun_exclusions_are_in_text_prompt() {
    let prompt = prompts::entity_system_prompt(SourceType::Text);
    // Verify the NEVER-extract list covers the audit defect categories.
    let must_include = ["meeting", "supplies", "clothes", "keys", "gear"];
    for item in must_include {
        assert!(
            prompt.contains(item),
            "text prompt must exclude generic noun {:?}",
            item
        );
    }
}

#[test]
fn date_entity_exclusion_in_text_prompt() {
    let prompt = prompts::entity_system_prompt(SourceType::Text);
    assert!(
        prompt.contains("dates") || prompt.contains("temporal information"),
        "text prompt must exclude date entities"
    );
}

#[test]
fn edge_prompt_forbids_self_referential_edges() {
    let prompt = prompts::edge_system_prompt();
    assert!(
        prompt.contains("same entity") || prompt.contains("self"),
        "edge prompt must forbid edges where source == target"
    );
}

#[test]
fn edge_prompt_requires_paraphrase_not_verbatim() {
    let prompt = prompts::edge_system_prompt();
    assert!(
        prompt.contains("paraphrase"),
        "edge prompt must require fact to be a paraphrase, not verbatim quote"
    );
}

#[test]
fn edge_prompt_has_iso8601_temporal_rules() {
    let prompt = prompts::edge_system_prompt();
    assert!(
        prompt.contains("ISO 8601") || prompt.contains("YYYY"),
        "edge prompt must include ISO 8601 temporal rules"
    );
    assert!(
        prompt.contains("valid_at"),
        "edge prompt must reference valid_at field"
    );
}

#[test]
fn extracted_edge_has_relation_type_field() {
    // Verify the ExtractedEdge type has the new fields we need.
    let edge = ExtractedEdge {
        source_name: "Alice".to_string(),
        target_name: "Acme Corp".to_string(),
        fact: "Alice works at Acme Corp".to_string(),
        relation_type: "WORKS_AT".to_string(),
        valid_at: Some("2026-01-01T00:00:00Z".to_string()),
        invalid_at: None,
    };
    assert_eq!(edge.relation_type, "WORKS_AT");
    assert_eq!(edge.valid_at.as_deref(), Some("2026-01-01T00:00:00Z"));
    assert!(edge.invalid_at.is_none());
}
