pub mod db;
pub mod embedder;
pub mod episode;
pub mod error;
pub mod extractor;
pub mod handlers;
pub mod ipc;
pub mod replay;
pub mod schema;
pub mod search;
pub mod telemetry;
pub mod types;
pub mod wal;

pub use db::{Conn, Db};
pub use embedder::Embedder;
pub use error::Error;
pub use extractor::Extractor;
pub use ipc::{IpcRequest, IpcResponse};
pub use replay::{ReplayStats, WalReplayer};
pub use schema::init as init_schema;
pub use telemetry::{NoopSink, TelemetryEvent, TelemetrySink};
pub use types::{
    EntityRow, EpisodicRow, ExtractionResult, MentionsEdge, RelatesToEdge,
};
pub use wal::{WalLine, WalWriter};
