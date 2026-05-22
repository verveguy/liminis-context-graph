use chrono::{DateTime, Utc};
use serde_json::Value;

/// Status of a background `rebuild_from_wal` job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobStatus {
    Running,
    Completed,
    Failed,
}

impl JobStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            JobStatus::Running => "running",
            JobStatus::Completed => "completed",
            JobStatus::Failed => "failed",
        }
    }
}

/// In-memory record of a background WAL rebuild job (not persisted across restarts).
#[derive(Debug, Clone)]
pub struct RebuildJob {
    pub job_id: String,
    pub status: JobStatus,
    pub mutations_replayed: u64,
    pub wal_files_processed: u64,
    pub start_time: DateTime<Utc>,
    pub error: Option<String>,
    pub result: Option<Value>,
}

impl RebuildJob {
    pub fn new(job_id: String) -> Self {
        RebuildJob {
            job_id,
            status: JobStatus::Running,
            mutations_replayed: 0,
            wal_files_processed: 0,
            start_time: Utc::now(),
            error: None,
            result: None,
        }
    }

    pub fn elapsed_seconds(&self) -> f64 {
        (Utc::now() - self.start_time).num_milliseconds() as f64 / 1000.0
    }
}
