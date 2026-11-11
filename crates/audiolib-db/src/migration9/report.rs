// crates/audiolib-db/src/migration9/report.rs
//! Wire types shared by the migration command and administrator HTTP API.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Durable migration state, independent of the server's startup readiness.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    #[default]
    Idle,
    Deferred,
    Preflight,
    BackingUp,
    Running,
    Paused,
    Switching,
    Done,
    Failed,
    Cancelled,
    RolledBack,
}
impl State {
    /// Canonical value of the `x-audiolib-migration` response header.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Deferred => "deferred",
            Self::Preflight => "preflight",
            Self::BackingUp => "backing_up",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Switching => "switching",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::RolledBack => "rolled_back",
        }
    }
}

/// Machine-readable errors from migration-report.schema.json.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Failure {
    InsufficientSpace,
    Locked,
    AlreadyRunning,
    BackupFailed,
    IntegrityFailed,
    DiskFull,
    ReadOnly,
    IoError,
    NotMigrated,
}
/// Committed directory count (never a file count).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Progress {
    pub done: u64,
    pub total: u64,
}
/// SQLite writer-lock time and actual row count for one committed directory batch.
/// Admission waits are excluded here and retained in the path-build phase time.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Batch {
    pub rows: u64,
    pub ms: f64,
}
/// The currently committed publication markers.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Switch {
    pub source_search_rev: u32,
    pub user_version: i64,
    pub features: Vec<String>,
}
/// Exact CLI last-line / HTTP response body; contains no local paths or secrets.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Report {
    pub state: State,
    pub error_code: Option<Failure>,
    pub progress: Progress,
    pub phases: BTreeMap<String, f64>,
    pub switch: Option<Switch>,
    pub batches: Vec<Batch>,
    pub eta_s: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ids: Option<Vec<i64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Option<String>>,
}
impl Default for Report {
    fn default() -> Self {
        Self {
            state: State::Idle,
            error_code: None,
            progress: Progress::default(),
            phases: ["backup", "path_build", "optimize", "switch"]
                .into_iter()
                .map(|s| (s.into(), 0.0))
                .collect(),
            switch: None,
            batches: Vec::new(),
            eta_s: None,
            ids: None,
            next_cursor: None,
        }
    }
}
impl Report {
    /// Exit status is stable across the CLI and HTTP adapter's interpretation.
    pub fn exit_code(&self) -> i32 {
        match self.error_code {
            None => 0,
            Some(Failure::AlreadyRunning | Failure::Locked) => 2,
            Some(Failure::InsufficientSpace) => 3,
            Some(Failure::ReadOnly) => 4,
            Some(Failure::DiskFull) => 5,
            Some(_) => 1,
        }
    }
}
