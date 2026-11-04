// server/audiolib-server/src/lib.rs
//! AudioLib LAN server: exposes one server-side library over HTTP under `/api/v1`.
//!
//! The wire contract is drafted in `doc/api/README.md`. Responses reuse the owned
//! `audiolib-core` DTOs so a remote `DataProvider` can deserialize them directly. A
//! LAN-only admin page (`/admin`) manages the public URL, authentication and library
//! management modes, tokens and optional native TLS.
pub mod admin;
mod artwork;
pub mod auth;
pub mod browse;
mod compatibility;
pub mod config;
pub mod error;
pub mod list_waveform;
pub mod manage;
pub mod pcm;
pub mod preview;
pub mod revision;
pub mod roots;
pub mod routes;
pub mod scans;
pub mod server;
pub mod service;
mod snapshot_registry;
pub mod state;
pub mod stream;
pub mod tls;
pub mod update;
pub mod waveform;
pub mod wf_job;
pub mod wfdb;

pub use auth::{AuthMode, ConnectionInfo, Listener, ManageMode, Security};
pub use config::{BearerToken, RootConfig, ServerConfig};
pub use error::{ApiError, ErrorBody};
pub use manage::{
    CapabilitiesResponse, MAX_REMOVE_IDS, RemoveAudioRequest, RemoveAudioResponse,
    RemoveFolderResponse,
};
pub use revision::{RevisionInfo, WriteOutcome};
pub use roots::{
    OpenGuard, ProbeResult, RootMonitor, RootPolicy, RootProbe, RootState, RootStatus,
};
pub use routes::{API_BASE, HealthResponse, REQUEST_TIMEOUT, router, router_with_request_timeout};
pub use scans::{
    CurrentScanResponse, ScanAction, ScanManager, ScanMode, ScanRequest, ScanResponse,
};
pub use server::{ServeOptions, serve, serve_tls};
pub use service::{Readiness, RetryPolicy, Service, open_in_background};
pub use state::{
    AppState, AudioWriteResponse, BatchWriteResponse, INLINE_RECHECK, LibraryResponse, OpenError,
    OpenProgress, PreparedSnapshot, RootLastScan, RootResponse, ServedAudio, SnapshotEncoding,
    SnapshotFile, WaveformReply, spawn_revision_poller,
};

/// Crate version reported by `GET /api/v1/health`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// API version reported by `GET /api/v1/health` (the `/api/v1/` path prefix).
pub const API_VERSION: &str = "v1";
/// Short git commit the binary was built from (`build.rs`), or `unknown`.
pub const COMMIT: &str = match option_env!("AUDIOLIB_GIT_COMMIT") {
    Some(commit) => commit,
    None => "unknown",
};
