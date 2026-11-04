// crates/audiolib-remote/src/lib.rs
//! Client side of AudioLib server mode.
//!
//! * [`RemoteClient`]: blocking HTTP client for `<server_url>/api/v1` (health, library,
//!   roots, consistent snapshot download).
//! * [`sync_library`] / [`RemoteCache`]: revision-checked local snapshot cache that the
//!   desktop opens with the ordinary read-only `SqliteProvider`.
//! * [`effective_roots`] / [`map_server_path`]: server scan-root path to WebDAV URL mapping
//!   (user override, then server suggestion, then a global base URL).
//! * [`MemoryStream`] / [`MemoryCache`]: in-memory audition streaming (server `/stream`,
//!   `/preview` or WebDAV) with a `Read + Seek` reader for the decoder, windowed for large
//!   files and kept in a bounded LRU keyed by record, version and route (no disk cache).
//! * [`choose_route`] / [`bandwidth`]: the original `/stream`, or (when the server
//!   profile transcodes auditions) a PCM/FLAC `/preview` chosen by the measured bandwidth.
//! * [`test_connection`]: structured per-step connectivity report for the GUI and CLI
//!   (DNS/TCP, TLS handshake, health, token, stream and preview samples, WebDAV).
//! * TLS (`tls` module): every `https://` request verifies the certificate against the OS
//!   trust store ([`TRUST_STORE`]); failures are classified as [`TlsFailure`].
//! * [`RevisionCheck`] / [`PollSchedule`]: background revision check decisions and backoff.
//! * [`RemoteSecrets`]: API token and WebDAV password, DPAPI-protected on Windows and
//!   never logged or written to settings.json.
//! * [`RemoteWriter`] / [`RemoteProvider`]: Metadata writes (§6.3) applied to
//!   the snapshot by the revision rule (`revision_before` match, else full sync).
//! * [`DownloadCache`]: complete local copies for drag and drop (`?download=1`, §8).
//! * Server library management (`manage` module): [`Capabilities`] (`manage_mode`),
//!   server scans ([`ScanRequest`] / [`ServerScan`]) and database-only removals
//!   ([`RemoveReport`]); typed `403`/`409`/`503` errors ([`manage_error_text`]).
//! * List thumbnail Waveforms (`list_waveforms` module, §9.1–§9.3): [`RemoteListWaveforms`]
//!   (`POST /waveforms` with `known` entity tags) over the [`ListWaveformStore`] disk cache,
//!   plugged into [`RemoteProvider`]; `GET`/`PATCH /settings` and `GET /waveforms/status`.
//! * Remote folder browsing ([`RemoteDirectory`]) and sound-library root registration,
//!   path changes and database-only root removal ([`RootRemoveReport`]), guarded by
//!   [`Capabilities::may_manage_roots`].
//! * Optional embedded covers ([`RemoteClient::artwork`]): bounded image responses with
//!   conditional ETags; gate requests with [`Capabilities::album_artwork_usable`].
//!   At most two cover requests are in flight. `503 BUSY` retries on `Retry-After`
//!   seconds (one second when the header is missing) a bounded number of times; HTTP 409
//!   is not retried. HTTP 405 is logged and still means no cover.
//!   Shared albums use [`RemoteClient::album_artwork_for_snapshot`], retaining legacy
//!   source provenance and checking server-authoritative album identities.
//! * Server profiles (`profiles` module): the data source dialog saved against the
//!   active profile, "use the external address" as a sibling profile, switching and
//!   removal, with per-profile tokens in [`RemoteSecrets`].
mod abort;
mod artwork;
mod bandwidth;
mod cache_cleanup;
mod client;
mod compatibility;
mod download;
mod error;
#[cfg(test)]
mod legacy_compatibility_tests;
mod list_waveforms;
mod manage;
mod mapping;
#[cfg(test)]
mod marker_matrix_tests;
mod poll;
mod probe;
mod profiles;
#[cfg(test)]
mod resume_regression_tests;
mod roots;
mod route;
mod secrets;
mod snapshot_transfer;
mod stream;
mod sync;
#[cfg(test)]
mod test_server;
mod tls;
mod util;
mod waveform_cache;
mod writes;
pub use waveform_cache::waveform_cache_error;

pub use abort::set_connection_warming;
pub use artwork::{ArtworkReply, MAX_ARTWORK_BYTES, cover_busy_delay};
pub use bandwidth::{Bandwidth, bandwidth};
pub use client::{
    BatchWrite, CONNECT_TIMEOUT, DownloadStage, FileWrite, HealthInfo, LIBRARY_ID_HEADER,
    MAX_WRITE_BODY_BYTES, RemoteClient, RemoteLibrary, RemoteRoot, SnapshotMeta, SnapshotProgress,
    WaveformQuery, WaveformReply, api_base_url,
};
pub use download::{
    DEFAULT_DOWNLOAD_CACHE_BYTES, DownloadCache, DownloadItem, DownloadProbe, DownloadProgress,
    MARKER_FILE, content_disposition_filename, etag_token, probe_download, sanitize_filename,
};
pub use error::{NetworkFailure, RemoteError, Result, TlsFailure};
pub use list_waveforms::{
    BatchItem, BatchReply, BatchStatus, BundleDownload, BundleImport, BundleMeta,
    DEFAULT_LIST_WAVEFORM_CACHE_BYTES, LIST_WAVEFORM_ENCODING, LIST_WAVEFORM_SHARD_IDS,
    ListWaveformStatus, ListWaveformStore, ListWaveformsCapability, RemoteListWaveforms,
    ServerSettings, StoredListWaveform, UNSUPPORTED_ENCODING, bundle_backoff, bundle_etag,
};
pub use manage::{
    Capabilities, MAX_HISTORY_LIMIT, MAX_REMOVE_IDS, MAX_SCAN_WORKERS, ManageMode, RemoveReport,
    RootScan, ScanControl, ScanMode, ScanRequest, ServerScan, manage_error_text,
};
pub use mapping::{
    EffectiveRoot, RootState, UrlSource, effective_roots, map_server_path, root_for_path,
    root_status_text,
};
pub use poll::{
    AUTO_SYNC_MAX_DEFER, AutoSyncGate, CachedIdentity, MAX_POLL_BACKOFF, MAX_SLOW_BACKOFF,
    PollSchedule, RevisionCheck, SLOW_RETRY_STEPS, STARTING_RETRY, UNAUTHORIZED_RETRY,
    check_revision, check_revision_with_roots,
};
pub use probe::{ConnectionReport, ProbeStep, StepReport, probe_webdav, test_connection};
pub use profiles::{
    ConnectionForm, ProfileEdit, activate_source, apply_connection_form, edit_transcode,
    remove_server_profile, save_profile_edit, untouched_transcode, use_external_address,
};
pub use roots::{
    RemoteDirectory, RemoteDirectoryEntry, RemotePlace, RootCreateRequest, RootRemoveReport,
};
pub use route::{
    AudioRoute, CONSERVATIVE_BANDWIDTH_BPS, FLAC_BELOW_BPS, MONO_BELOW_BPS, PreviewParams,
    RouteOptions, choose_route, is_wav, pcm_output_rate, preview_rate, raw_bitrate,
};
pub use secrets::{
    ProfileSecret, RemoteSecrets, load_profile_secrets, load_secrets, save_secrets, secrets_path,
};
pub use snapshot_transfer::{PendingSync, SyncControl, SyncOptions};
pub use stream::{
    DEFAULT_MEMORY_CACHE_BYTES, FIRST_BYTE_TIMEOUT, MIN_MEMORY_CACHE_BYTES, MemoryCache,
    MemoryReader, MemoryStream, PreviewFormat, READ_STALL_TIMEOUT, SEEK_JUMP_BYTES, STREAM_RETRIES,
    SourceAuth, StreamProbe, StreamSource, WAKING_HINT_AFTER, WebDavAuth, probe_stream,
};
pub use sync::{
    CacheFormat, CachePreparation, CacheState, RemoteCache, SyncOutcome, SyncProgress,
    SyncRunOutcome, refresh_cached_roots, sync_library, sync_library_with_options,
    validate_snapshot,
};
pub use tls::{TRUST_STORE, TlsInfo};
pub use util::encode_path_segment;
pub use writes::{
    CacheUpdate, RemoteProvider, RemoteWriter, WriteEvent, WriteListener, WriteOutcome,
    apply_server_write, core_error,
};
