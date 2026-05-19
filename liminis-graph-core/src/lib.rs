pub mod app_state;
pub mod db;
pub mod dedup_adapter;
pub mod embedder;
pub mod episode;
pub mod error;
pub mod extractor;
pub mod handlers;
pub mod ipc;
pub mod llm_router;
pub mod replay;
pub mod schema;
pub mod search;
pub mod telemetry;
pub mod types;
pub mod wal;

pub use app_state::AppState;
pub use db::{Conn, Db};
pub use dedup_adapter::{DedupAdapter, LocalDedupAdapter, PassthroughDedupAdapter};
pub use embedder::Embedder;
pub use error::Error;
pub use extractor::{AnthropicExtractor, Extractor, MockExtractor};
pub use ipc::{IpcRequest, IpcResponse};
pub use llm_router::LlmRouter;
pub use replay::{ReplayStats, WalReplayer};
pub use schema::init as init_schema;
pub use telemetry::{CaptureSink, NoopSink, TelemetryEvent, TelemetrySink};
pub use types::{
    EntityRow, EpisodicRow, ExtractionResult, MentionsEdge, RelatesToEdge,
};
pub use wal::{WalLine, WalWriter};
