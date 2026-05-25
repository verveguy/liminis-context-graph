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

fn build_fact_types_section(ontology: Option<&Ontology>) -> String {
    let onto = match ontology {
        Some(o) if o.has_relation_types() => o,
        _ => return String::new(),
    };

    let mut section =
        String::from("<FACT_TYPES>\nThe following relation types are defined for this workspace:\n");
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
    }
    section.push_str("</FACT_TYPES>\n");
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
    let entities_section = entity_names
        .iter()
        .filter_map(|n| {
            // Strip control chars (including newlines) that would break the bullet-list structure.
            let sanitized: String = n.chars().filter(|c| !c.is_control()).collect();
            let sanitized = sanitized.trim().to_string();
            if sanitized.is_empty() {
                None
            } else {
                Some(format!("- {sanitized}"))
            }
        })
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
