// crates/audiolib-remote/src/snapshot_transfer.rs
//! Owned, leased wire-byte checkpoints for streaming snapshot downloads.
use crate::client::{
    RemoteClient, RemoteLibrary, SnapshotMeta, cancellable_io_with_stall, snapshot_read_stall,
};
use crate::error::{RemoteError, Result};
use crate::sync::{CacheState, RemoteCache, SyncProgress};
use crate::util::{unix_now, write_atomic};
use ring::digest::{Context, SHA256};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const CHECKPOINT_BYTES: u64 = 8 * 1024 * 1024;
const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(2);

/// Options for one explicitly requested sync. Restart also forces a fresh download.
#[derive(Clone, Copy, Debug, Default)]
pub struct SyncOptions {
    pub force: bool,
    pub restart: bool,
}

#[derive(Default)]
struct ControlState {
    stop: AtomicBool,
    paused: AtomicBool,
}

/// Shared stop handle. Pausing persists user intent; cancellation only stops this run.
#[derive(Clone, Default)]
pub struct SyncControl(Arc<ControlState>);

impl SyncControl {
    /// Create an independent run's control handle.
    pub fn new() -> Self {
        Self::default()
    }
    /// Request a durable pause, including while waiting for response headers.
    pub fn pause(&self) {
        self.0.paused.store(true, Ordering::Release);
        self.0.stop.store(true, Ordering::Release);
    }
    /// Stop this run while keeping any committed wire prefix for the next run.
    pub fn cancel(&self) {
        self.0.stop.store(true, Ordering::Release);
    }
    /// Whether the stop was an explicit user pause.
    pub fn is_paused(&self) -> bool {
        self.0.paused.load(Ordering::Acquire)
    }
    /// Common stop flag used by network and owned SQLite operations.
    pub fn cancellation(&self) -> &AtomicBool {
        &self.0.stop
    }
}

/// Public, credential-free description of an unfinished transfer or pause intent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingSync {
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub manual_paused: bool,
    pub library_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Object {
    #[serde(default)]
    snapshot_format: Option<audiolib_core::compatibility::SnapshotFormat>,
    snapshot_id: Option<String>,
    etag: Option<String>,
    library_id: Option<String>,
    revision: Option<u64>,
    schema_version: Option<i64>,
    compressed: bool,
    wire_length: Option<u64>,
    output_length: Option<u64>,
    resumable: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Checkpoint {
    endpoint: String,
    manual_paused: bool,
    library_id: Option<String>,
    object: Option<Object>,
    committed: u64,
    sha256: String,
    updated_at: u64,
    /// A validated cache is published, but its consumer has not acknowledged opening it.
    #[serde(default)]
    published_database_file: Option<String>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn endpoint_directory(root: &Path, endpoint: &str) -> Result<PathBuf> {
    let endpoint = crate::client::api_base_url(endpoint)?;
    Ok(root.join(".transfers").join(hex(
        ring::digest::digest(&SHA256, endpoint.as_bytes()).as_ref()
    )))
}

fn plain_file(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_file() => Err(RemoteError::Validation(
            "Transfer file is not an owned regular file".into(),
        )),
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    }
}

fn load_checkpoint(path: &Path, endpoint: &str) -> Result<Checkpoint> {
    plain_file(path)?;
    match std::fs::read(path) {
        Ok(bytes) if bytes.len() <= 64 * 1024 => {
            let value: Checkpoint = serde_json::from_slice(&bytes).map_err(|_| {
                RemoteError::Protocol("Invalid snapshot checkpoint; use restart".into())
            })?;
            if value.endpoint != endpoint {
                return Err(RemoteError::Validation(
                    "Snapshot checkpoint belongs to another endpoint".into(),
                ));
            }
            Ok(value)
        }
        Ok(_) => Err(RemoteError::Protocol(
            "Snapshot checkpoint is too large".into(),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Checkpoint {
            endpoint: endpoint.to_owned(),
            ..Checkpoint::default()
        }),
        Err(error) => Err(error.into()),
    }
}

impl Checkpoint {
    fn published_matches(&self, opened: &CacheState) -> bool {
        !self.manual_paused
            && self.library_id.as_deref() == Some(opened.library_id.as_str())
            && self.published_database_file.as_deref() == Some(opened.database_file.as_str())
    }

    fn pending(&self) -> PendingSync {
        PendingSync {
            downloaded_bytes: self.committed,
            total_bytes: self.object.as_ref().and_then(|object| object.wire_length),
            manual_paused: self.manual_paused,
            library_id: self.library_id.clone(),
        }
    }
}

impl RemoteCache {
    /// Inspect an atomically published checkpoint without disturbing an active writer.
    pub fn pending_sync(&self, endpoint: &str) -> Result<Option<PendingSync>> {
        let endpoint = crate::client::api_base_url(endpoint)?;
        let path = endpoint_directory(self.root(), &endpoint)?.join("checkpoint.json");
        if !path.try_exists()? {
            return Ok(None);
        }
        let checkpoint = load_checkpoint(&path, &endpoint)?;
        if checkpoint
            .library_id
            .as_deref()
            .and_then(|id| self.state(id))
            .is_some_and(|state| checkpoint.published_matches(&state))
        {
            return Ok(None);
        }
        Ok(Some(checkpoint.pending()))
    }

    /// Acknowledge a successfully opened published snapshot and remove only its wire
    /// transfer. Exact file receipts and the endpoint OS lease protect newer transfers,
    /// including a newer pause. Does not prune any library or snapshot database.
    pub fn finish_sync_transfer(&self, opened: &CacheState) -> Result<()> {
        let endpoint = crate::client::api_base_url(&opened.server_url)?;
        let checkpoint = endpoint_directory(self.root(), &endpoint)?.join("checkpoint.json");
        if !checkpoint.try_exists()? {
            return Ok(());
        }
        let mut session = TransferSession::open(self, &endpoint, false)?;
        if session.published_matches(opened) {
            session.finish()?;
        }
        Ok(())
    }
}

/// OS lease spans verification, download, checkpoint publication and cache installation.
pub(crate) struct TransferSession {
    _lease: File,
    directory: PathBuf,
    state: Checkpoint,
}

impl Drop for TransferSession {
    fn drop(&mut self) {
        // A pause followed immediately by Continue must not wait for deferred handle
        // cleanup to release the endpoint lock on Windows.
        let _ = self._lease.unlock();
    }
}

impl TransferSession {
    pub(crate) fn open(cache: &RemoteCache, endpoint: &str, restart: bool) -> Result<Self> {
        let endpoint = crate::client::api_base_url(endpoint)?;
        let directory = endpoint_directory(cache.root(), &endpoint)?;
        for path in [cache.root().join(".transfers"), directory.clone()] {
            if std::fs::symlink_metadata(&path).is_ok_and(|metadata| !metadata.file_type().is_dir())
            {
                return Err(RemoteError::Validation(
                    "Transfer directory is not owned".into(),
                ));
            }
            std::fs::create_dir_all(path)?;
        }
        let lock_path = directory.join("sync.lock");
        plain_file(&lock_path)?;
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)?;
        lease.try_lock().map_err(|_| RemoteError::Busy {
            message: "Another process is syncing this server".into(),
            retry_after: None,
        })?;
        let checkpoint = directory.join("checkpoint.json");
        let state = if restart {
            plain_file(&checkpoint)?;
            Checkpoint {
                endpoint,
                ..Checkpoint::default()
            }
        } else {
            load_checkpoint(&checkpoint, &endpoint)?
        };
        let mut session = Self {
            _lease: lease,
            directory,
            state,
        };
        if restart {
            session.discard_payload()?;
        }
        Ok(session)
    }

    fn payload(&self) -> PathBuf {
        self.directory.join("payload.part")
    }

    fn save(&mut self) -> Result<()> {
        self.state.updated_at = unix_now();
        let bytes = serde_json::to_vec(&self.state)
            .map_err(|error| RemoteError::Protocol(error.to_string()))?;
        write_atomic(&self.directory.join("checkpoint.json"), &bytes)?;
        Ok(())
    }

    pub(crate) fn pending(&self) -> PendingSync {
        self.state.pending()
    }

    /// A fixed object with an already durable prefix can legitimately predate the
    /// latest library revision. Bind negotiation to that exact saved identity/format;
    /// fresh responses still use the current library's conservative admission rules.
    pub(crate) fn pinned_library(&self, current: &RemoteLibrary) -> Option<RemoteLibrary> {
        let object = self.state.object.as_ref()?;
        let revision = object.revision?;
        if !object.resumable
            || self.state.committed == 0
            || self.state.published_database_file.is_some()
            || object.library_id.as_deref() != Some(current.library_id.as_str())
            || revision >= current.revision
        {
            return None;
        }
        let mut pinned = current.clone();
        pinned.revision = revision;
        pinned.schema_version = object.schema_version?;
        pinned.snapshot_format = object.snapshot_format.clone();
        Some(pinned)
    }

    pub(crate) fn set_paused(&mut self, paused: bool) -> Result<()> {
        self.state.manual_paused = paused;
        if paused {
            // A new explicit pause is a new intent, even before another download starts.
            self.state.published_database_file = None;
        }
        self.save()
    }

    pub(crate) fn published_matches(&self, opened: &CacheState) -> bool {
        self.state.published_matches(opened)
    }

    pub(crate) fn record_publication(&mut self, opened: &CacheState) -> Result<()> {
        self.state.published_database_file = Some(opened.database_file.clone());
        self.save()
    }

    pub(crate) fn bind_library(&mut self, library_id: &str) -> Result<()> {
        if self
            .state
            .library_id
            .as_deref()
            .is_some_and(|id| id != library_id)
        {
            self.discard_payload()?;
        }
        self.state.library_id = Some(library_id.to_owned());
        self.save()
    }

    pub(crate) fn discard_payload(&mut self) -> Result<()> {
        let payload = self.payload();
        plain_file(&payload)?;
        match std::fs::remove_file(payload) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
        self.state.object = None;
        self.state.committed = 0;
        self.state.sha256.clear();
        self.state.published_database_file = None;
        self.save()
    }

    pub(crate) fn finish(&mut self) -> Result<()> {
        // Its exact published cache has been opened. Mark the transfer settled before
        // payload deletion, so a sharing violation cannot replay old data.
        self.state.object = None;
        self.state.committed = 0;
        self.state.sha256.clear();
        self.state.manual_paused = false;
        self.state.published_database_file = None;
        self.save()?;
        let payload = self.payload();
        plain_file(&payload)?;
        match std::fs::remove_file(payload) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
        std::fs::remove_file(self.directory.join("checkpoint.json"))?;
        Ok(())
    }

    /// Keep raw bytes and decode at the same time; each retry rebuilds only owned output.
    pub(crate) fn download(
        &mut self,
        client: &RemoteClient,
        destination: &Path,
        progress: &mut dyn FnMut(SyncProgress),
        cancel: &AtomicBool,
    ) -> Result<SnapshotMeta> {
        if self.state.published_database_file.is_some() {
            // A previous publication is complete. A genuinely new download must not
            // replay it, nor leave an old consumer's receipt able to clear this attempt.
            self.discard_payload()?;
        }
        let result = cancellable_io_with_stall(cancel, snapshot_read_stall(), || {
            self.download_io(client, destination, progress, cancel)
        });
        if matches!(
            &result,
            Err(RemoteError::Protocol(_) | RemoteError::Snapshot(_))
        ) {
            self.discard_payload()?;
        }
        if result.is_err() {
            let _ = std::fs::remove_file(destination);
        }
        result
    }

    fn download_io(
        &mut self,
        client: &RemoteClient,
        destination: &Path,
        progress: &mut dyn FnMut(SyncProgress),
        cancel: &AtomicBool,
    ) -> Result<SnapshotMeta> {
        if self.state.object.as_ref().is_some_and(|object| {
            !object.resumable && object.wire_length != Some(self.state.committed)
        }) {
            self.discard_payload()?;
        }
        let mut hash = Context::new(&SHA256);
        let payload = self.payload();
        plain_file(&payload)?;
        let mut raw = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&payload)?;
        let offset = self.state.committed;
        if let Some(object) = &self.state.object {
            progress(SyncProgress::Replaying {
                received: offset,
                total: object.wire_length,
            });
        }
        if offset > 0 && self.state.object.is_none() {
            return Err(RemoteError::Protocol(
                "Snapshot prefix has no object identity".into(),
            ));
        }
        if raw.metadata()?.len() < offset {
            return Err(RemoteError::Protocol(
                "Snapshot checkpoint prefix is truncated; restart required".into(),
            ));
        }
        raw.set_len(offset)?;
        if offset > 0 {
            let mut remaining = offset;
            let mut buffer = vec![0u8; 256 * 1024];
            while remaining > 0 {
                if cancel.load(Ordering::Acquire) {
                    return Err(RemoteError::Cancelled);
                }
                let amount = buffer.len().min(remaining as usize);
                raw.read_exact(&mut buffer[..amount])?;
                hash.update(&buffer[..amount]);
                remaining -= amount as u64;
            }
            if hex(hash.clone().finish().as_ref()) != self.state.sha256 {
                return Err(RemoteError::Protocol(
                    "Snapshot checkpoint checksum mismatch; restart required".into(),
                ));
            }
        }
        raw.seek(SeekFrom::Start(offset))?;
        let (object, network) = if let Some(object) = self.state.object.clone() {
            if object.wire_length.is_some_and(|length| offset > length) {
                return Err(RemoteError::Protocol(
                    "Snapshot checkpoint exceeds object length".into(),
                ));
            }
            progress(SyncProgress::Replaying {
                received: offset,
                total: object.wire_length,
            });
            (object, None)
        } else {
            let (response, object) = snapshot_response(client, None, 0)?;
            if object
                .library_id
                .as_deref()
                .zip(self.state.library_id.as_deref())
                .is_some_and(|(actual, expected)| actual != expected)
            {
                return Err(RemoteError::Snapshot(
                    "Server library changed during snapshot preparation".into(),
                ));
            }
            self.state.object = Some(object.clone());
            self.save()?;
            (object, Some(response.into_body().into_reader()))
        };
        progress(SyncProgress::Downloading {
            received: offset,
            total: object.wire_length,
        });
        if object.compressed {
            progress(SyncProgress::Decompressing {
                written: 0,
                total: object.output_length,
            });
        }
        let parent = destination.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent)?;
        if let (Some(wire), Some(output)) = (object.wire_length, object.output_length) {
            // The resumable wire payload stays under the cache root even when
            // the decoded snapshot is prepared on a different volume.
            audiolib_db::ensure_free_space(&self.directory, wire.saturating_sub(offset))?;
            audiolib_db::ensure_free_space(
                parent,
                wire.saturating_sub(offset).saturating_add(output),
            )?;
        }
        let mut output = File::create(destination)?;
        if let Some(length) = object.output_length.filter(|_| object.compressed) {
            output.set_len(length)?;
        }
        let failure = Rc::new(RefCell::new(None));
        let sink = Rc::new(RefCell::new(Sink {
            session: self,
            raw,
            hash,
            position: offset,
            committed: offset,
            last_commit: Instant::now(),
        }));
        let prefix = File::open(payload)?.take(offset);
        let input = Input {
            prefix,
            prefix_left: offset,
            network,
            client,
            object: object.clone(),
            sink: sink.clone(),
            failure: failure.clone(),
            cancel,
            throughput: None,
        };
        let decoding = (|| -> Result<u64> {
            let mut reader: Box<dyn Read + '_> = if object.compressed {
                Box::new(zstd::stream::read::Decoder::new(input).map_err(|error| {
                    input_error(
                        error,
                        &failure,
                        object.wire_length == Some(sink.borrow().position),
                    )
                })?)
            } else {
                Box::new(input)
            };
            let mut buffer = vec![0; 256 * 1024];
            let mut written = 0u64;
            loop {
                if cancel.load(Ordering::Acquire) {
                    return Err(RemoteError::Cancelled);
                }
                let amount = reader.read(&mut buffer).map_err(|error| {
                    input_error(
                        error,
                        &failure,
                        object.wire_length == Some(sink.borrow().position),
                    )
                })?;
                progress(SyncProgress::Downloading {
                    received: sink.borrow().position,
                    total: object.wire_length,
                });
                if amount == 0 {
                    break;
                }
                if object
                    .output_length
                    .is_some_and(|total| written.saturating_add(amount as u64) > total)
                {
                    return Err(RemoteError::Protocol(
                        "Snapshot exceeds advertised decompressed length".into(),
                    ));
                }
                output.write_all(&buffer[..amount])?;
                written += amount as u64;
                if object.compressed {
                    progress(SyncProgress::Decompressing {
                        written,
                        total: object.output_length,
                    });
                }
            }
            if written == 0 || object.output_length.is_some_and(|total| total != written) {
                return Err(RemoteError::Protocol(
                    "Snapshot decompressed length mismatch".into(),
                ));
            }
            if object
                .wire_length
                .is_some_and(|total| total != sink.borrow().position)
            {
                return Err(RemoteError::Unreachable(
                    "Snapshot body ended before its full length".into(),
                ));
            }
            progress(SyncProgress::Writing);
            crate::client::flush_abandonable(output, destination, cancel)?;
            if cancel.load(Ordering::Acquire) {
                return Err(RemoteError::Cancelled);
            }
            Ok(written)
        })();
        // Also commit after read cancellation/errors, before releasing the OS lease.
        sink.borrow_mut().commit()?;
        drop(sink);
        let bytes = decoding?;
        Ok(SnapshotMeta {
            snapshot_format: object.snapshot_format,
            bytes,
            library_id: object.library_id,
            revision: object.revision,
            schema_version: object.schema_version,
        })
    }
}

fn input_error(
    error: std::io::Error,
    failure: &RefCell<Option<RemoteError>>,
    complete_wire_body: bool,
) -> RemoteError {
    failure.borrow_mut().take().unwrap_or_else(|| {
        if error.kind() == std::io::ErrorKind::UnexpectedEof && !complete_wire_body {
            RemoteError::Unreachable("Snapshot compressed stream ended early".into())
        } else {
            RemoteError::Protocol(format!("snapshot: zstd: {error}"))
        }
    })
}

struct Sink<'a> {
    session: &'a mut TransferSession,
    raw: File,
    hash: Context,
    position: u64,
    committed: u64,
    last_commit: Instant,
}

impl Sink<'_> {
    fn append(&mut self, bytes: &[u8]) -> Result<()> {
        self.raw.write_all(bytes)?;
        self.hash.update(bytes);
        self.position += bytes.len() as u64;
        if self.position - self.committed >= CHECKPOINT_BYTES
            || self.last_commit.elapsed() >= CHECKPOINT_INTERVAL
        {
            self.commit()?;
        }
        Ok(())
    }
    fn commit(&mut self) -> Result<()> {
        self.raw.sync_data()?;
        self.session.state.committed = self.position;
        self.session.state.sha256 = hex(self.hash.clone().finish().as_ref());
        self.session.save()?;
        self.committed = self.position;
        self.last_commit = Instant::now();
        Ok(())
    }
}

struct Input<'a, 'b> {
    prefix: std::io::Take<File>,
    prefix_left: u64,
    network: Option<ureq::BodyReader<'static>>,
    client: &'a RemoteClient,
    object: Object,
    sink: Rc<RefCell<Sink<'b>>>,
    failure: Rc<RefCell<Option<RemoteError>>>,
    cancel: &'a AtomicBool,
    throughput: Option<crate::bandwidth::Throughput>,
}

impl Input<'_, '_> {
    fn read_inner(&mut self, buffer: &mut [u8]) -> Result<usize> {
        if self.cancel.load(Ordering::Acquire) {
            return Err(RemoteError::Cancelled);
        }
        if self.prefix_left > 0 {
            let amount = self.prefix.read(buffer)?;
            if amount == 0 {
                return Err(RemoteError::Protocol(
                    "Local snapshot prefix ended early".into(),
                ));
            }
            self.prefix_left -= amount as u64;
            return Ok(amount);
        }
        if self.network.is_none() {
            let position = self.sink.borrow().position;
            if self.object.wire_length == Some(position) {
                return Ok(0);
            }
            let (response, _) = snapshot_response(self.client, Some(&self.object), position)?;
            self.network = Some(response.into_body().into_reader());
        }
        let throughput = self
            .throughput
            .get_or_insert_with(crate::bandwidth::Throughput::new);
        let amount = self
            .network
            .as_mut()
            .expect("network initialized")
            .read(buffer)
            .map_err(|error| {
                if self.cancel.load(Ordering::Acquire) {
                    RemoteError::Cancelled
                } else if is_read_stall(&error) {
                    // The per-read stall limit fired: retryable, resumed with Range.
                    RemoteError::Timeout(format!(
                        "snapshot body: no data for {} s",
                        snapshot_read_stall().as_secs_f32()
                    ))
                } else {
                    RemoteError::from_io(&error, "snapshot body")
                }
            })?;
        throughput.add(amount as u64, crate::bandwidth::bandwidth());
        if amount == 0
            && self
                .object
                .wire_length
                .is_some_and(|length| self.sink.borrow().position < length)
        {
            return Err(RemoteError::Unreachable(
                "Snapshot connection closed before the end".into(),
            ));
        }
        if self.object.wire_length.is_some_and(|length| {
            self.sink.borrow().position.saturating_add(amount as u64) > length
        }) {
            return Err(RemoteError::Protocol(
                "Snapshot exceeds advertised wire length".into(),
            ));
        }
        self.sink.borrow_mut().append(&buffer[..amount])?;
        Ok(amount)
    }
}

impl Read for Input<'_, '_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        self.read_inner(buffer).map_err(|error| {
            *self.failure.borrow_mut() = Some(error);
            std::io::Error::other("snapshot input failed")
        })
    }
}

impl Drop for Input<'_, '_> {
    fn drop(&mut self) {
        if let Some(throughput) = &mut self.throughput {
            throughput.pause(crate::bandwidth::bandwidth());
        }
    }
}

fn snapshot_response(
    client: &RemoteClient,
    previous: Option<&Object>,
    offset: u64,
) -> Result<(ureq::http::Response<ureq::Body>, Object)> {
    let catalog = audiolib_core::compatibility::compatibility_manifest();
    let catalog_json = crate::compatibility::client_header()?;
    let accepted = catalog.snapshot_read_formats.join(", ");
    let url = match previous.and_then(|object| object.snapshot_id.as_deref()) {
        Some(id) => format!(
            "{}/library/snapshot?snapshot_id={}",
            client.api_url(),
            crate::util::encode_path_segment(id)
        ),
        None => format!("{}/library/snapshot", client.api_url()),
    };
    let response = crate::abort::call_fresh(|| {
        let mut request = client
            .bulk
            .get(&url)
            .header(
                "Accept-Encoding",
                if previous.is_some_and(|object| !object.compressed) {
                    "identity"
                } else {
                    "zstd"
                },
            )
            .header(
                audiolib_core::compatibility::CLIENT_COMPATIBILITY_HEADER,
                &catalog_json,
            )
            .header(
                audiolib_core::compatibility::ACCEPT_FORMATS_HEADER,
                &accepted,
            )
            .header(
                audiolib_core::compatibility::SNAPSHOT_SCHEMA_MAX_HEADER,
                &audiolib_core::compatibility::SNAPSHOT_SCHEMA_MAX.to_string(),
            )
            .header(
                audiolib_core::compatibility::SEARCH_STORAGE_MAX_HEADER,
                &audiolib_core::compatibility::SEARCH_STORAGE_MAX.to_string(),
            )
            .header(
                audiolib_core::compatibility::TOKENIZATION_MAX_HEADER,
                &audiolib_core::compatibility::TOKENIZATION_MAX.to_string(),
            );
        if !client.token.is_empty() {
            request = request.header("Authorization", &format!("Bearer {}", client.token));
        }
        if let Some(object) = previous {
            request = request
                .header("Range", &format!("bytes={offset}-"))
                .header("If-Range", object.etag.as_deref().unwrap_or_default());
        }
        request
    })
    .map_err(|error| RemoteError::from_transport(error, &url))?;
    let status = response.status().as_u16();
    if previous.is_some() && matches!(status, 200 | 410 | 416) {
        return Err(RemoteError::Changed(
            "Saved snapshot expired or no longer supports this byte range".into(),
        ));
    }
    let response = client.checked(response, "GET /library/snapshot")?;
    if status != if previous.is_some() { 206 } else { 200 } {
        return Err(RemoteError::Protocol(format!(
            "Unexpected snapshot status {status}"
        )));
    }
    let snapshot_format = crate::compatibility::response_format(response.headers())?;
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .map(str::to_owned)
    };
    let compressed = match header("content-encoding").as_deref() {
        None | Some("identity") => false,
        Some(value) if value.eq_ignore_ascii_case("zstd") => true,
        _ => {
            return Err(RemoteError::Protocol(
                "Unsupported snapshot content encoding".into(),
            ));
        }
    };
    let content_length = header("content-length").and_then(|value| value.parse::<u64>().ok());
    let wire_length = if previous.is_some() {
        let range = header("content-range")
            .ok_or_else(|| RemoteError::Changed("Missing snapshot Content-Range".into()))?;
        let (positions, total) = range
            .strip_prefix("bytes ")
            .and_then(|range| range.split_once('/'))
            .ok_or_else(|| RemoteError::Changed("Invalid snapshot Content-Range".into()))?;
        let (start, end) = positions
            .split_once('-')
            .ok_or_else(|| RemoteError::Changed("Invalid snapshot range positions".into()))?;
        let (start, end, total) = (
            start.parse::<u64>().ok(),
            end.parse::<u64>().ok(),
            total.parse::<u64>().ok(),
        );
        match (start, end, total) {
            (Some(start), Some(end), Some(total))
                if start == offset
                    && end >= start
                    && end.checked_add(1) == Some(total)
                    && content_length == Some(end - start + 1) =>
            {
                Some(total)
            }
            _ => {
                return Err(RemoteError::Changed(
                    "Snapshot range does not match saved prefix".into(),
                ));
            }
        }
    } else {
        content_length
    };
    let object = Object {
        snapshot_format,
        snapshot_id: header("x-snapshot-id"),
        etag: header("etag"),
        library_id: header("x-library-id"),
        revision: header("x-library-revision").and_then(|value| value.parse().ok()),
        schema_version: header("x-schema-version").and_then(|value| value.parse().ok()),
        compressed,
        wire_length,
        output_length: if compressed {
            header("x-uncompressed-length").and_then(|value| value.parse().ok())
        } else {
            wire_length
        },
        resumable: header("accept-ranges").is_some_and(|value| value.eq_ignore_ascii_case("bytes"))
            && header("etag").is_some_and(|value| value.starts_with('"') && value.ends_with('"'))
            && header("x-snapshot-id").is_some_and(|id| !id.is_empty())
            && wire_length.is_some(),
    };
    if let Some(previous) = previous
        && (object.snapshot_id != previous.snapshot_id
            || object.snapshot_format != previous.snapshot_format
            || object.etag != previous.etag
            || object.library_id != previous.library_id
            || object.revision != previous.revision
            || object.schema_version != previous.schema_version
            || object.compressed != previous.compressed
            || object.wire_length != previous.wire_length
            || object.output_length != previous.output_length)
    {
        return Err(RemoteError::Changed(
            "Snapshot response identity differs from saved prefix".into(),
        ));
    }
    Ok((response, object))
}

/// ureq reports a transport read timeout inside the body reader as an `io::Error`
/// wrapping `ureq::Error::Timeout`; `from_io` alone would see only its outer kind.
fn is_read_stall(error: &std::io::Error) -> bool {
    matches!(error.kind(), std::io::ErrorKind::TimedOut)
        || error
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<ureq::Error>())
            .is_some_and(|inner| matches!(inner, ureq::Error::Timeout(_)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_server::{TestRequest, TestResponse, TestServer};
    use std::sync::{Mutex, atomic::AtomicUsize};

    fn bytes() -> Vec<u8> {
        let mut seed = 0x1234_5678u32;
        (0..400_000)
            .map(|index| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                if index % 4 == 0 { seed as u8 } else { 0 }
            })
            .collect()
    }

    fn response(
        body: Vec<u8>,
        offset: Option<usize>,
        wire_length: usize,
        output_length: usize,
        compressed: bool,
    ) -> TestResponse {
        let mut response = TestResponse::bytes(if offset.is_some() { 206 } else { 200 }, body)
            .with_header("X-Snapshot-Id", "fixed-object")
            .with_header("ETag", "\"snapshot-fixed-object\"")
            .with_header("Accept-Ranges", "bytes")
            .with_header("X-Library-Id", "library-a")
            .with_header("X-Library-Revision", "7")
            .with_header("X-Schema-Version", "5");
        if compressed {
            response = response
                .with_header("Content-Encoding", "zstd")
                .with_header("X-Uncompressed-Length", &output_length.to_string());
        }
        if let Some(offset) = offset {
            response = response.with_header(
                "Content-Range",
                &format!("bytes {offset}-{}/{wire_length}", wire_length - 1),
            );
        }
        response
    }

    fn request_offset(request: &TestRequest) -> Option<usize> {
        request
            .header("range")
            .and_then(|range| range.strip_prefix("bytes="))
            .and_then(|range| range.strip_suffix('-'))
            .and_then(|range| range.parse().ok())
    }

    #[test]
    fn a_stalled_snapshot_body_times_out_and_resumes_from_the_kept_prefix() {
        let original = bytes();
        let cut = original.len() / 3;
        let requests = Arc::new(Mutex::new(Vec::new()));
        let server = {
            let requests = requests.clone();
            let original = original.clone();
            TestServer::start(move |request| {
                let offset = request_offset(request);
                requests.lock().unwrap().push(offset);
                match offset {
                    Some(offset) => response(
                        original[offset..].to_vec(),
                        Some(offset),
                        original.len(),
                        original.len(),
                        false,
                    ),
                    // Send a prefix, then go silent far longer than the stall limit.
                    None => response(
                        original.clone(),
                        None,
                        original.len(),
                        original.len(),
                        false,
                    )
                    .slow(cut, Duration::from_secs(5)),
                }
            })
        };
        let directory = tempfile::tempdir().unwrap();
        let cache = RemoteCache::new(directory.path());
        let client = RemoteClient::new(&server.url, "").unwrap();
        let output = directory.path().join("output.db");
        let cancel = AtomicBool::new(false);
        crate::client::override_snapshot_read_stall(Some(Duration::from_millis(400)));
        {
            let mut session = TransferSession::open(&cache, client.api_url(), false).unwrap();
            session.bind_library("library-a").unwrap();
            let started = Instant::now();
            let error = session
                .download(&client, &output, &mut |_| {}, &cancel)
                .unwrap_err();
            assert!(
                matches!(error, RemoteError::Timeout(_)) && error.is_slow(),
                "{error:?}"
            );
            assert!(
                started.elapsed() < Duration::from_secs(4),
                "{:?}",
                started.elapsed()
            );
            assert_eq!(session.pending().downloaded_bytes, cut as u64);
            assert!(!output.exists());
        }
        let mut session = TransferSession::open(&cache, client.api_url(), false).unwrap();
        session.bind_library("library-a").unwrap();
        session
            .download(&client, &output, &mut |_| {}, &cancel)
            .unwrap();
        crate::client::override_snapshot_read_stall(None);
        assert_eq!(std::fs::read(&output).unwrap(), original);
        assert_eq!(*requests.lock().unwrap(), vec![None, Some(cut)]);
    }

    #[test]
    fn snapshot_read_stall_defaults_to_ninety_seconds_without_a_total_limit() {
        assert_eq!(
            crate::client::SNAPSHOT_READ_STALL_TIMEOUT,
            Duration::from_secs(90)
        );
        assert_eq!(
            crate::client::snapshot_read_stall(),
            Duration::from_secs(90)
        );
    }

    #[test]
    fn zstd_header_blocks_and_trailer_resume_after_a_new_session() {
        let original = bytes();
        let packed = zstd::encode_all(original.as_slice(), 3).unwrap();
        for cut in [1, 4, 20, packed.len() / 2, packed.len() - 1] {
            let requests = Arc::new(Mutex::new(Vec::new()));
            let server = {
                let requests = requests.clone();
                let packed = packed.clone();
                let output_length = original.len();
                TestServer::start(move |request| {
                    let offset = request_offset(request);
                    requests.lock().unwrap().push(offset);
                    if let Some(offset) = offset {
                        assert_eq!(
                            request.header("if-range"),
                            Some("\"snapshot-fixed-object\"")
                        );
                        assert!(request.path.contains("snapshot_id=fixed-object"));
                        response(
                            packed[offset..].to_vec(),
                            Some(offset),
                            packed.len(),
                            output_length,
                            true,
                        )
                    } else {
                        response(packed.clone(), None, packed.len(), output_length, true)
                            .cut_after(cut)
                    }
                })
            };
            let directory = tempfile::tempdir().unwrap();
            let cache = RemoteCache::new(directory.path());
            let client = RemoteClient::new(&server.url, "").unwrap();
            let output = directory.path().join("output.db");
            let cancel = AtomicBool::new(false);
            {
                let mut session = TransferSession::open(&cache, client.api_url(), false).unwrap();
                session.bind_library("library-a").unwrap();
                let error = session
                    .download(&client, &output, &mut |_| {}, &cancel)
                    .unwrap_err();
                assert!(error.is_unreachable(), "{error:?}");
                assert_eq!(session.pending().downloaded_bytes, cut as u64);
                assert!(!output.exists());
                // An uncommitted crash tail must never become the next Range offset.
                OpenOptions::new()
                    .append(true)
                    .open(session.payload())
                    .unwrap()
                    .write_all(b"uncommitted crash tail")
                    .unwrap();
            }
            let mut session = TransferSession::open(&cache, client.api_url(), false).unwrap();
            let mut events = Vec::new();
            let meta = session
                .download(&client, &output, &mut |event| events.push(event), &cancel)
                .unwrap();
            assert_eq!(meta.bytes, original.len() as u64);
            assert_eq!(std::fs::read(output).unwrap(), original);
            assert_eq!(*requests.lock().unwrap(), vec![None, Some(cut)]);
            assert!(events.contains(&SyncProgress::Replaying {
                received: cut as u64,
                total: Some(packed.len() as u64)
            }));
        }
    }

    #[test]
    fn complete_wire_prefix_rebuilds_locally_without_an_eof_range_request() {
        let original = bytes();
        let packed = zstd::encode_all(original.as_slice(), 3).unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let server = {
            let requests = requests.clone();
            let output_length = original.len();
            TestServer::start(move |_| {
                requests.fetch_add(1, Ordering::SeqCst);
                response(packed.clone(), None, packed.len(), output_length, true)
            })
        };
        let directory = tempfile::tempdir().unwrap();
        let cache = RemoteCache::new(directory.path());
        let client = RemoteClient::new(&server.url, "").unwrap();
        let output = directory.path().join("output.db");
        let cancel = AtomicBool::new(false);
        {
            let mut session = TransferSession::open(&cache, client.api_url(), false).unwrap();
            session
                .download(&client, &output, &mut |_| {}, &cancel)
                .unwrap();
        }
        let mut session = TransferSession::open(&cache, client.api_url(), false).unwrap();
        session
            .download(&client, &output, &mut |_| {}, &cancel)
            .unwrap();
        assert_eq!(std::fs::read(output).unwrap(), original);
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn pause_before_headers_persists_and_endpoint_lease_excludes_other_writers() {
        let directory = tempfile::tempdir().unwrap();
        let cache = RemoteCache::new(directory.path());
        let endpoint = "https://example.invalid/api/v1";
        let mut session = TransferSession::open(&cache, endpoint, false).unwrap();
        assert!(matches!(
            TransferSession::open(&cache, endpoint, false),
            Err(RemoteError::Busy { .. })
        ));
        session.set_paused(true).unwrap();
        drop(session);
        let pending = cache.pending_sync(endpoint).unwrap().unwrap();
        assert!(pending.manual_paused);
        assert_eq!(pending.downloaded_bytes, 0);
        let session = TransferSession::open(&cache, endpoint, false).unwrap();
        assert!(session.pending().manual_paused);
        assert!(session.directory.join("sync.lock").is_file());
    }

    #[test]
    fn bad_prefix_is_rejected_without_network_and_restart_recovers_corrupt_manifest() {
        let directory = tempfile::tempdir().unwrap();
        let cache = RemoteCache::new(directory.path());
        let server =
            TestServer::start(|_| panic!("Bad local prefix must not make a network request"));
        let client = RemoteClient::new(&server.url, "").unwrap();
        let mut session = TransferSession::open(&cache, client.api_url(), false).unwrap();
        std::fs::write(session.payload(), b"changed").unwrap();
        session.state.committed = 7;
        session.state.sha256 = "wrong".into();
        session.save().unwrap();
        let error = session
            .download(
                &client,
                &directory.path().join("output.db"),
                &mut |_| {},
                &AtomicBool::new(false),
            )
            .unwrap_err();
        assert!(matches!(error, RemoteError::Protocol(_)));
        assert_eq!(session.state.committed, 0);
        let path = session.directory.join("checkpoint.json");
        drop(session);
        std::fs::write(path, b"bad JSON").unwrap();
        assert!(TransferSession::open(&cache, client.api_url(), false).is_err());
        assert!(TransferSession::open(&cache, client.api_url(), true).is_ok());
    }

    #[test]
    fn full_200_to_a_range_is_never_appended() {
        let body = bytes();
        let directory = tempfile::tempdir().unwrap();
        let cache = RemoteCache::new(directory.path());
        let requests = Arc::new(AtomicUsize::new(0));
        let server = {
            let requests = requests.clone();
            TestServer::start(move |_| {
                let first = requests.fetch_add(1, Ordering::SeqCst) == 0;
                let reply = response(body.clone(), None, body.len(), body.len(), false);
                if first { reply.cut_after(31) } else { reply }
            })
        };
        let client = RemoteClient::new(&server.url, "").unwrap();
        let mut session = TransferSession::open(&cache, client.api_url(), false).unwrap();
        let output = directory.path().join("output.db");
        let cancel = AtomicBool::new(false);
        assert!(
            session
                .download(&client, &output, &mut |_| {}, &cancel)
                .is_err()
        );
        assert_eq!(session.pending().downloaded_bytes, 31);
        assert!(matches!(
            session.download(&client, &output, &mut |_| {}, &cancel),
            Err(RemoteError::Changed(_))
        ));
        assert_eq!(session.pending().downloaded_bytes, 31);
        assert!(!output.exists());
    }
}
