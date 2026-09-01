pub mod app_state;
pub mod assert;
pub mod backfill;
pub mod backfill_summary_embeddings;
pub mod canonicalize;
pub mod cassette;
pub(crate) mod checkpoint;
pub mod corrections;
pub mod cross_group;
pub mod db;
pub mod dedup_adapter;
pub(crate) mod dump;
pub mod embedder;
pub mod embedding_cache;
pub mod env;
pub mod episode;
pub mod error;
pub mod extraction_failures;
pub mod extractor;
pub mod group_purge;
pub mod handlers;
pub mod ipc;
pub(crate) mod legacy_wal;
pub mod llm_router;
pub mod ontology;
pub mod ontology_sidecar;
pub mod pointer;
pub mod prompts;
pub mod rebuild_job;
pub mod recovery;
pub mod replay;
pub mod reprocess_relations;
pub mod schema;
pub mod search;
pub mod telemetry;
pub mod token_budget;
pub mod types;
pub mod wal;
pub(crate) mod wal_exec;
pub mod wal_generation;
pub mod wal_group;

pub use app_state::AppState;
pub use cassette::{CassetteRecord, CassetteWriter, RecordingExtractor, ReplayingExtractor};
pub use db::{Conn, Db, WalPositionRecord};
pub use dedup_adapter::{DedupAdapter, LocalDedupAdapter, PassthroughDedupAdapter};
pub use embedder::{Embedder, MockEmbedder, NameMapEmbedder, OaiEmbedder};
pub use embedding_cache::{EmbedderContext, EmbeddingCache};
pub use error::Error;
pub use extraction_failures::{
    ExtractionFailureRecord, ExtractionFailureSink, ExtractionFailureWriter,
    DEFAULT_MAX_BYTES_PER_FILE,
};
pub use extractor::{
    AnthropicExtractor, ConfigurableExtractor, ExtractOptions, Extractor, MockExtractor,
    OaiExtractor,
};
pub use ipc::{IpcRequest, IpcResponse};
pub use llm_router::LlmRouter;
pub use ontology::{Ontology, OntologyMode};
pub use rebuild_job::{JobStatus, RebuildJob};
pub use replay::{FailureSample, ReplayOptions, ReplayProgress, ReplayStats, WalReplayer};
pub use schema::init as init_schema;
pub use telemetry::{CaptureSink, NoopSink, TeeSink, TelemetryEvent, TelemetrySink};
pub use types::{
    EntityRow, EpisodicRow, ExtractedEdge, ExtractedEntity, ExtractionOutcome, ExtractionResult,
    MentionsEdge, RelatesToEdge, SourceType,
};
pub use wal::{WalLine, WalRotationInfo, WalWriter};
pub use wal_group::DEFAULT_GROUP_ID;
