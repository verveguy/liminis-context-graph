//! Extraction-quality eval harness (#228): drives this engine's real `Extractor`
//! implementations (`AnthropicExtractor`, `OaiExtractor`) over the public Wikipedia
//! corpus (#217), scores the results against a reference backend with both a strict
//! string-match metric and an LLM-as-judge, and emits a comparison report.
//!
//! See `README.md`'s "Extraction-quality eval harness" section for usage, and
//! `docs/adr/` for the architecture rationale.

pub mod backend;
pub mod cli;
pub mod corpus;
pub mod failure_taxonomy;
pub mod judge;
pub mod judge_cache;
pub mod metrics;
pub mod report;
pub mod runner;
