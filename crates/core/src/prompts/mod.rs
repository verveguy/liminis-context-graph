use crate::ontology::{Ontology, OntologyMode};
use crate::types::SourceType;

static EXTRACT_TEXT: &str = include_str!("extract_text.txt");
static EXTRACT_MESSAGE: &str = include_str!("extract_message.txt");
static EXTRACT_JSON: &str = include_str!("extract_json.txt");
static EXTRACT_EDGES: &str = include_str!("extract_edges.txt");
static CLASSIFY_NODES: &str = include_str!("classify_nodes.txt");

/// The original closed entity-type list used when no workspace ontology is configured.
const DEFAULT_ENTITY_TYPES_SECTION: &str = "\
For each entity extracted, assign a single entity_type label from this closed ontology:
- Person: An individual human being
- Organization: A company, institution, group, or governing body
- Software: A software application, library, or framework
- Service: A deployed service, platform, or SaaS product
- System: An integrated technical system or infrastructure component
- Technology: A protocol, standard, methodology, or technical approach
- Concept: An abstract idea, principle, or theoretical framework
- Location: A physical or virtual place, region, or address
- Event: A dated or scheduled occurrence or incident
- Process: A workflow, procedure, or repeatable operation
- Requirement: A constraint, specification, or policy requirement
- Document: A document, report, specification, or publication
- Product: A physical or digital product or deliverable
- Project: A project, initiative, or program
- Award: A prize, honor, or recognition
- Book: A book, novel, or long-form publication
Choose the single most appropriate type. If none fit well, use the closest match.";

fn build_entity_types_section(ontology: Option<&Ontology>) -> String {
    let onto = match ontology {
        Some(o) if o.has_entity_types() => o,
        _ => return DEFAULT_ENTITY_TYPES_SECTION.to_string(),
    };

    let mut section = String::from(
        "<ENTITY_TYPES>\nThe following entity types are defined for this workspace:\n",
    );
    for et in &onto.entity_types {
        if let Some(desc) = &et.description {
            section.push_str(&format!("- {}: {}\n", et.name, desc));
        } else {
            section.push_str(&format!("- {}\n", et.name));
        }
    }
    match onto.mode {
        OntologyMode::Strict => section.push_str(
            "Only extract entities whose type is exactly one of the listed types; \
             do not invent or use types not in this list.\n",
        ),
        OntologyMode::Open => section.push_str(
            "Prefer the listed entity types when they apply; \
             you may use other types for entities that clearly don't fit any listed type.\n",
        ),
    }
    section.push_str("</ENTITY_TYPES>");
    section
}

// FR-002/FR-003 (issue #310): under `strict` mode, this section also lists each relation
// type's declared aliases/keywords and appends an instruction restricting the model to the
// declared vocabulary — mirroring `build_entity_types_section`'s mode-aware pattern. Under
// `open` mode, output is deliberately unchanged from before this issue: Acceptance Scenario 2
// requires the open-mode rendering stay byte-identical, so the `Open` arm below is a no-op
// rather than adding both-modes text like the entity-type section does (see ADR-0310).
fn build_fact_types_section(ontology: Option<&Ontology>) -> String {
    let onto = match ontology {
        Some(o) if o.has_relation_types() => o,
        _ => return String::new(),
    };

    let mut section = String::from(
        "<FACT_TYPES>\nThe following relation types are defined for this workspace:\n",
    );
    for rt in &onto.relation_types {
        let sig = match (&rt.source_type, &rt.target_type) {
            (Some(s), Some(t)) => format!(" ({} → {})", s, t),
            _ => String::new(),
        };
        if let Some(desc) = &rt.description {
            section.push_str(&format!("- {}{}: {}\n", rt.name, sig, desc));
        } else {
            section.push_str(&format!("- {}{}\n", rt.name, sig));
        }
        if onto.mode == OntologyMode::Strict {
            if !rt.aliases.is_empty() {
                section.push_str(&format!("  Aliases: {}\n", rt.aliases.join(", ")));
            }
            if !rt.keywords.is_empty() {
                section.push_str(&format!("  Keywords: {}\n", rt.keywords.join(", ")));
            }
        }
    }
    section.push_str("</FACT_TYPES>\n");
    match onto.mode {
        OntologyMode::Strict => section.push_str(
            "Only use relation types from the list above, identified by their canonical name. \
             Aliases and keywords are listed to help you recognize a matching fact — always \
             output the canonical name, never the alias or keyword itself. Do not invent \
             relation types outside this list.\n",
        ),
        OntologyMode::Open => {}
    }
    section
}

/// Returns the entity extraction system prompt for the given source type, with optional ontology injection.
pub fn entity_system_prompt(source_type: SourceType, ontology: Option<&Ontology>) -> String {
    let template = match source_type {
        SourceType::Text => EXTRACT_TEXT,
        SourceType::Message => EXTRACT_MESSAGE,
        SourceType::Json => EXTRACT_JSON,
    };
    let section = build_entity_types_section(ontology);
    template.replace("{{ENTITY_TYPES_SECTION}}", &section)
}

/// Returns the edge extraction system prompt with optional ontology injection.
pub fn edge_system_prompt(ontology: Option<&Ontology>) -> String {
    let section = build_fact_types_section(ontology);
    EXTRACT_EDGES.replace("{{FACT_TYPES_SECTION}}", &section)
}

/// Returns the entity classification system prompt.
pub fn classify_system_prompt() -> &'static str {
    CLASSIFY_NODES
}

/// Builds the entity extraction user message for a single episode.
pub fn entity_user_prompt(body: &str, custom_instructions: Option<&str>) -> String {
    let custom = custom_instructions.unwrap_or("").trim();
    if custom.is_empty() {
        format!("<TEXT>\n{body}\n</TEXT>\n")
    } else {
        format!("<TEXT>\n{body}\n</TEXT>\n\n{custom}\n")
    }
}

/// Builds the entity extraction user message for a message-type episode.
pub fn message_user_prompt(body: &str, custom_instructions: Option<&str>) -> String {
    let custom = custom_instructions.unwrap_or("").trim();
    if custom.is_empty() {
        format!("<CURRENT MESSAGE>\n{body}\n</CURRENT MESSAGE>\n")
    } else {
        format!("<CURRENT MESSAGE>\n{body}\n</CURRENT MESSAGE>\n\n{custom}\n")
    }
}

/// Builds the entity extraction user message for a JSON-type episode.
pub fn json_user_prompt(body: &str, custom_instructions: Option<&str>) -> String {
    let custom = custom_instructions.unwrap_or("").trim();
    if custom.is_empty() {
        format!("<JSON>\n{body}\n</JSON>\n\nExtract relevant entities from the provided JSON.\n")
    } else {
        format!(
            "<JSON>\n{body}\n</JSON>\n\nExtract relevant entities from the provided JSON.\n{custom}\n"
        )
    }
}

/// Builds the entity user prompt dispatch — selects the appropriate format by source type.
pub fn entity_user_prompt_for(
    source_type: SourceType,
    body: &str,
    custom_instructions: Option<&str>,
) -> String {
    match source_type {
        SourceType::Text => entity_user_prompt(body, custom_instructions),
        SourceType::Message => message_user_prompt(body, custom_instructions),
        SourceType::Json => json_user_prompt(body, custom_instructions),
    }
}

fn strip_control_chars(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// Sanitizes a list of entity names for use in the edge-extraction prompt and tool schema:
/// strips control characters (including newlines, which would break the bullet-list structure
/// or a JSON schema `enum` entry), trims whitespace, drops entries that become empty, and
/// deduplicates while preserving first-seen order (FR-001, FR-006).
pub fn sanitize_entity_names(entity_names: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    entity_names
        .iter()
        .filter_map(|n| {
            let sanitized = strip_control_chars(n).trim().to_string();
            if sanitized.is_empty() || !seen.insert(sanitized.clone()) {
                None
            } else {
                Some(sanitized)
            }
        })
        .collect()
}

/// Normalizes a name to the key used to match an entity against a model-echoed edge endpoint
/// name: strips control characters (the same stripping `sanitize_entity_names` applies before a
/// name ever reaches the model), trims whitespace, and lowercases. An entity name containing a
/// control character is control-stripped before the model ever sees it (both in the edge
/// prompt's entity list and the tool schema's `enum`), so any endpoint-resolution key derived
/// from the *original* entity name must use the same normalization or a genuine batch-local
/// match will spuriously fail and fall through to salvage or drop.
pub fn normalize_name(name: &str) -> String {
    strip_control_chars(name).trim().to_lowercase()
}

/// Builds the edge extraction user message.
///
/// `entity_names` is the list of entity names extracted in the entity pass.
/// `reference_time` is an ISO 8601 timestamp used for temporal grounding.
/// `body` is the episode text.
pub fn edge_user_prompt(
    entity_names: &[String],
    reference_time: &str,
    body: &str,
    custom_instructions: Option<&str>,
) -> String {
    let entities_section = sanitize_entity_names(entity_names)
        .into_iter()
        .map(|n| format!("- {n}"))
        .collect::<Vec<_>>()
        .join("\n");

    let custom = custom_instructions.unwrap_or("").trim();
    if custom.is_empty() {
        format!(
            "<CURRENT_MESSAGE>\n{body}\n</CURRENT_MESSAGE>\n\n\
             <ENTITIES>\n{entities_section}\n</ENTITIES>\n\n\
             <REFERENCE_TIME>\n{reference_time}\n</REFERENCE_TIME>\n"
        )
    } else {
        format!(
            "<CURRENT_MESSAGE>\n{body}\n</CURRENT_MESSAGE>\n\n\
             <ENTITIES>\n{entities_section}\n</ENTITIES>\n\n\
             <REFERENCE_TIME>\n{reference_time}\n</REFERENCE_TIME>\n\n\
             {custom}\n"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_files_are_non_empty() {
        assert!(
            !EXTRACT_TEXT.is_empty(),
            "extract_text.txt must not be empty"
        );
        assert!(
            !EXTRACT_MESSAGE.is_empty(),
            "extract_message.txt must not be empty"
        );
        assert!(
            !EXTRACT_JSON.is_empty(),
            "extract_json.txt must not be empty"
        );
        assert!(
            !EXTRACT_EDGES.is_empty(),
            "extract_edges.txt must not be empty"
        );
        assert!(
            !CLASSIFY_NODES.is_empty(),
            "classify_nodes.txt must not be empty"
        );
    }

    #[test]
    fn source_type_dispatch_returns_distinct_prompts() {
        let text = entity_system_prompt(SourceType::Text, None);
        let message = entity_system_prompt(SourceType::Message, None);
        let json = entity_system_prompt(SourceType::Json, None);
        assert_ne!(text, message, "text and message prompts must differ");
        assert_ne!(text, json, "text and json prompts must differ");
        assert_ne!(message, json, "message and json prompts must differ");
    }

    #[test]
    fn no_ontology_uses_default_entity_types() {
        let prompt = entity_system_prompt(SourceType::Text, None);
        assert!(
            prompt.contains("Person: An individual human being"),
            "default entity types must appear when no ontology is set"
        );
        assert!(
            !prompt.contains("{{ENTITY_TYPES_SECTION}}"),
            "placeholder must not appear in output"
        );
    }

    // FR-002: the entity prompt must not forbid the entity types its own ontology defines —
    // the old unqualified "NEVER extract...abstract concepts" ban contradicted the Concept
    // entity type and must not reappear in any of the three source-type prompts.
    #[test]
    fn concept_ban_contradiction_is_resolved_by_specificity_test() {
        for (label, prompt) in [
            ("extract_text.txt", EXTRACT_TEXT),
            ("extract_message.txt", EXTRACT_MESSAGE),
            ("extract_json.txt", EXTRACT_JSON),
        ] {
            assert!(
                !prompt.contains("NEVER extract vague or standalone abstract concepts"),
                "{label}: the old unqualified concept-ban phrasing must not reappear"
            );
            assert!(
                prompt.contains("Concept entity"),
                "{label}: must instruct that a specific, named concept is extracted as a Concept entity"
            );
            assert!(
                prompt.contains("Wikipedia-article test") || prompt.contains("Wikipedia article"),
                "{label}: must carry the specificity/Wikipedia-article test for concepts"
            );
        }
    }

    fn ontology_with_relation_types(mode: OntologyMode) -> Ontology {
        Ontology {
            mode,
            entity_types: vec![],
            relation_types: vec![
                crate::ontology::RelationTypeDef {
                    name: "LAUNCHED".to_string(),
                    description: Some("One entity launched another".to_string()),
                    source_type: None,
                    target_type: None,
                    aliases: vec!["LAUNCHED_BY".to_string(), "LAUNCHED_FROM".to_string()],
                    keywords: vec!["launch".to_string()],
                },
                crate::ontology::RelationTypeDef {
                    name: "USES".to_string(),
                    description: None,
                    source_type: None,
                    target_type: None,
                    aliases: vec![],
                    keywords: vec![],
                },
            ],
            ancestor_map: std::collections::HashMap::new(),
        }
    }

    // SC-002/Acceptance Scenario 2: open-mode rendering is byte-identical to before this issue
    // — no aliases/keywords, no mode-instruction sentence.
    #[test]
    fn open_mode_fact_types_section_unchanged() {
        let onto = ontology_with_relation_types(OntologyMode::Open);
        let prompt = edge_system_prompt(Some(&onto));
        assert!(
            !prompt.contains("Aliases:"),
            "open mode must not list aliases: {prompt}"
        );
        assert!(
            !prompt.contains("Keywords:"),
            "open mode must not list keywords: {prompt}"
        );
        assert!(
            !prompt.contains("Only use relation types"),
            "open mode must not add a vocabulary-restriction instruction: {prompt}"
        );
    }

    // SC-002/Acceptance Scenario 1: strict-mode rendering differs from open-mode for the same
    // ontology, and contains an explicit vocabulary-restriction instruction.
    #[test]
    fn strict_and_open_fact_types_sections_differ() {
        let open_prompt =
            edge_system_prompt(Some(&ontology_with_relation_types(OntologyMode::Open)));
        let strict_prompt =
            edge_system_prompt(Some(&ontology_with_relation_types(OntologyMode::Strict)));
        assert_ne!(
            open_prompt, strict_prompt,
            "strict and open renderings must differ for the same ontology"
        );
        assert!(
            strict_prompt.contains("Only use relation types"),
            "strict mode must state the vocabulary-restriction instruction: {strict_prompt}"
        );
    }

    // Acceptance Scenario 3: declared aliases/keywords are visible in the strict-mode rendering.
    #[test]
    fn strict_mode_exposes_aliases_and_keywords() {
        let onto = ontology_with_relation_types(OntologyMode::Strict);
        let prompt = edge_system_prompt(Some(&onto));
        assert!(
            prompt.contains("Aliases: LAUNCHED_BY, LAUNCHED_FROM"),
            "strict mode must list LAUNCHED's aliases: {prompt}"
        );
        assert!(
            prompt.contains("Keywords: launch"),
            "strict mode must list LAUNCHED's keywords: {prompt}"
        );
    }

    // Edge Case: a relation type with no declared aliases/keywords must not print empty
    // "Aliases:"/"Keywords:" lines.
    #[test]
    fn strict_mode_omits_alias_keyword_lines_when_absent() {
        let onto = ontology_with_relation_types(OntologyMode::Strict);
        let prompt = edge_system_prompt(Some(&onto));
        let uses_line_idx = prompt.find("- USES").expect("USES type must be listed");
        let after_uses = &prompt[uses_line_idx..];
        let next_line_end = after_uses.find('\n').unwrap_or(after_uses.len());
        let following = &after_uses[next_line_end..];
        assert!(
            !following.trim_start().starts_with("Aliases:")
                && !following.trim_start().starts_with("Keywords:"),
            "USES has no declared aliases/keywords and must not print empty lines: {prompt}"
        );
    }

    #[test]
    fn sanitize_entity_names_strips_control_chars_dedupes_and_drops_empty() {
        let names = vec![
            "Alice".to_string(),
            "Alice".to_string(),               // exact duplicate, dropped
            "Bob\ncontrol\tchars".to_string(), // control chars stripped
            "   ".to_string(),                 // empty after trim, dropped
            "\u{0007}".to_string(),            // empty after control-char strip, dropped
            "  Carol  ".to_string(),           // trimmed
        ];
        let sanitized = sanitize_entity_names(&names);
        assert_eq!(
            sanitized,
            vec![
                "Alice".to_string(),
                "Bobcontrolchars".to_string(),
                "Carol".to_string(),
            ]
        );
    }

    #[test]
    fn sanitize_entity_names_empty_input_yields_empty_output() {
        assert!(sanitize_entity_names(&[]).is_empty());
        assert!(sanitize_entity_names(&["   ".to_string(), "\n".to_string()]).is_empty());
    }

    #[test]
    fn edge_user_prompt_contains_entities() {
        let names = vec!["Alice".to_string(), "Acme Corp".to_string()];
        let prompt = edge_user_prompt(
            &names,
            "2026-01-01T00:00:00Z",
            "Alice works at Acme Corp.",
            None,
        );
        assert!(
            prompt.contains("Alice"),
            "edge prompt must contain entity name"
        );
        assert!(
            prompt.contains("Acme Corp"),
            "edge prompt must contain entity name"
        );
        assert!(
            prompt.contains("REFERENCE_TIME"),
            "edge prompt must contain REFERENCE_TIME"
        );
    }
}
