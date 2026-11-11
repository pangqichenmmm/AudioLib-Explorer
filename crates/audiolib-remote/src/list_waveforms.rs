// crates/audiolib-remote/src/list_waveforms.rs
//! Server-mode list thumbnail Waveforms (doc/api/README.md §9.1–§9.3), client side.
//!
//! * [`ListWaveformsCapability`]: `GET /capabilities` `list_waveforms`. Only a server that
//!   reports version 1, `buckets` 128 and the `i16le-minmax-base64` encoding is used;
//!   otherwise the list hides its Waveform column. There is **no** fallback to per-item
//!   `GET /audio/{id}/waveform`.
//! * [`RemoteClient::list_waveform_batch`]: one `POST /waveforms` (up to 200 ids with the
//!   `known` entity tags, `Accept-Encoding: zstd`) over the shared keep-alive pool with the
//!   same authentication as every other request.
//! * [`ListWaveformStore`]: the client disk cache
//!   `<cache>/waveforms/server/<library_id hash>/list/`, one file per 1024 ids
//!   (client-private format, checksummed), replaced atomically, keyed by entity tag and
//!   bounded (640 MiB by default) by evicting least recently used shards.
//! * [`RemoteListWaveforms`]: the lookup a [`crate::RemoteProvider`] answers
//!   `DataProvider::list_waveforms` with: at most one `POST /waveforms` in flight per
//!   client, `503 BUSY` turned into a deferral by `Retry-After`, replies for another
//!   `library_id` discarded, `ok` stored, `not_modified` answered from the store, stale
//!   copies offered with `pending` and `root_offline`.
//! * [`RemoteClient::server_settings`] / [`RemoteClient::set_pregenerate_list_waveforms`] /
//!   [`RemoteClient::list_waveform_status`]: `GET`/`PATCH /settings` and
//!   `GET /waveforms/status` for the 服务器音效库 page.
//!
//! * Compact `i8-minmax-base64` items: requested (field `encoding`) only when
//!   `list_waveforms.encodings` of `GET /capabilities` lists that encoding and a
//!   `waveforms.db` store was attached ([`RemoteListWaveforms::with_i8_store`]). The reply
//!   must echo the encoding (top level and any per-item value), otherwise it is a protocol
//!   error: nothing of that reply is cached and every id backs off like `pending`.
//!   An error body with `sub_code` `UNSUPPORTED_ENCODING` (doc/api §9.4) switches this
//!   client back to the int16 path. Entity tags are compared in full, including the
//!   `-l1i8` suffix. Peaks are expanded to the int16
//!   scale at true amplitude for display.
//!
//! * Bundle import (§9.4.2, [`RemoteListWaveforms::import_bundle`]): only when the
//!   capability advertises `bundle` and i8. The bundle is streamed to a temporary file
//!   beside `waveforms.db` (never held in memory), its `ETag` `"wfb1-<32 hex>"` must equal
//!   the first 16 bytes of the file's SHA-256, `X-Library-Id` must be the open library,
//!   and [`audiolib_db::validate_bundle`] must pass before the store is atomically
//!   replaced. Lookups then continue incrementally with the bundle's per-entry etags.
//!   `503 BUSY` is retried by [`bundle_backoff`] (5 s doubling, at most 5 minutes).
//!
//! Retry policy (only still-visible `pending` ids, at most 5 times; `root_offline` after
//! `/roots` reports the root online; no automatic retry of `missing` or `error`) and the
//! audition yield (no POST during the first 3 s after a stream or preview starts) belong to
//! the desktop's list Waveform worker, which knows what is visible and playing.
use crate::client::{RemoteClient, call_cancellable, retry_after};
use crate::error::{RemoteError, Result};
use crate::manage::Capabilities;
use audiolib_core::{
    LIST_WAVEFORM_COLUMNS, LIST_WAVEFORM_ENCODING_I8, LIST_WAVEFORM_ENCODING_VERSION,
    LIST_WAVEFORM_I8_ETAG_SUFFIX, ListWaveform, ListWaveformBatch, ListWaveformI8,
    ListWaveformItem, ListWaveformSource, ListWaveformState, MAX_LIST_WAVEFORM_IDS,
};
use audiolib_db::{WaveformsDb, WaveformsDbRow};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// `encoding` of a usable `list_waveforms` capability.
pub const LIST_WAVEFORM_ENCODING: &str = "i16le-minmax-base64";
/// Default size limit of one library's list Waveform disk cache (640 MiB).
pub const DEFAULT_LIST_WAVEFORM_CACHE_BYTES: u64 = 640 * 1024 * 1024;
/// Ids per cache shard file.
pub const LIST_WAVEFORM_SHARD_IDS: i64 = 1024;
/// `POST /waveforms` answers within the server's 2 s `stat` budget; its general limit
/// is 30 s.
const BATCH_TIMEOUT: Duration = Duration::from_secs(30);
/// Reply body limit after decompression (200 items are about 150 KB).
const MAX_BATCH_BYTES: u64 = 8 * 1024 * 1024;
/// Parsed shards kept in memory (each at most 1024 entries, about 0.6 MB).
const MEMORY_SHARDS: usize = 16;
/// A failed capability probe is repeated after this long.
const CAPABILITY_RETRY: Duration = Duration::from_secs(30);
/// Deferral of a `503 BUSY` without `Retry-After`.
const BUSY_DEFAULT: Duration = Duration::from_secs(1);
const SHARD_MAGIC: &[u8; 8] = b"ALXLWS1\n";
const SHARD_EXTENSION: &str = "lws";
/// Error code of a server that rejects the requested `encoding`.
pub const UNSUPPORTED_ENCODING: &str = "UNSUPPORTED_ENCODING";

/// `list_waveforms` of `GET /capabilities` (§9.2).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ListWaveformsCapability {
    /// List encoding version (`-l<version>` of the entity tags).
    pub version: u32,
    /// Accepted `buckets` values.
    pub buckets: Vec<u32>,
    /// Most ids per request.
    pub max_ids: usize,
    /// `peaks` encoding.
    pub encoding: String,
    /// Additional encodings the server accepts in a request's `encoding` field.
    pub encodings: Vec<String>,
    /// `GET /waveforms/bundle` is available (§9.4.2).
    pub bundle: bool,
}

impl ListWaveformsCapability {
    /// Whether this client understands the server's list Waveforms.
    pub fn usable(&self) -> bool {
        self.version == LIST_WAVEFORM_ENCODING_VERSION
            && self.buckets.contains(&LIST_WAVEFORM_COLUMNS)
            && self.encoding == LIST_WAVEFORM_ENCODING
    }

    /// Whether a request may ask for `i8-minmax-base64` (listed in `encodings`).
    pub fn offers_i8(&self) -> bool {
        self.encodings
            .iter()
            .any(|encoding| encoding == LIST_WAVEFORM_ENCODING_I8)
    }

    /// Ids per request: the server's `max_ids`, at most the UI page size.
    pub fn batch_limit(&self) -> usize {
        match self.max_ids {
            0 => MAX_LIST_WAVEFORM_IDS,
            limit => limit.min(MAX_LIST_WAVEFORM_IDS),
        }
    }
}

/// Per-item `status` of a `POST /waveforms` reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchStatus {
    Ok,
    NotModified,
    Pending,
    Missing,
    RootOffline,
    Error,
    /// A status this client does not know (treated like `pending`).
    #[serde(other)]
    Unknown,
}

/// One item of a `POST /waveforms` reply.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchItem {
    pub id: i64,
    pub status: BatchStatus,
    #[serde(default)]
    pub etag: Option<String>,
    #[serde(default)]
    pub n: Option<usize>,
    #[serde(default)]
    pub peaks: Option<String>,
    #[serde(default)]
    pub root: Option<String>,
    #[serde(default)]
    pub error_code: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    /// Per-item encoding echo, when the server sends one.
    #[serde(default)]
    pub encoding: Option<String>,
}

/// `POST /waveforms` reply.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchReply {
    /// Library that answered; a different one than the open snapshot's is discarded.
    pub library_id: String,
    #[serde(default)]
    pub buckets: u32,
    /// Suggested retry interval of `pending` items (seconds).
    #[serde(default)]
    pub retry_after: Option<u64>,
    #[serde(default)]
    pub items: Vec<BatchItem>,
    /// Echo of the requested `encoding` (absent on the int16 path of older servers).
    #[serde(default)]
    pub encoding: Option<String>,
}

/// `GET /settings` / `PATCH /settings` reply (§9.2, §9.5.4): the core contract.
pub type ServerSettings = audiolib_core::waveform_cache::WaveformCacheSettings;

/// `GET /waveforms/status` (§9.2, §9.5.2): the core contract.
pub type ListWaveformStatus = audiolib_core::waveform_cache::WaveformCacheStatus;

fn decompress(context: &str, encoding: Option<&str>, body: Vec<u8>) -> Result<Vec<u8>> {
    match encoding {
        None => Ok(body),
        Some(value) if value.eq_ignore_ascii_case("identity") => Ok(body),
        Some(value) if value.eq_ignore_ascii_case("zstd") => {
            let mut plain = Vec::new();
            zstd::stream::read::Decoder::new(body.as_slice())
                .and_then(|decoder| decoder.take(MAX_BATCH_BYTES).read_to_end(&mut plain))
                .map_err(|error| RemoteError::Protocol(format!("{context}: zstd: {error}")))?;
            Ok(plain)
        }
        Some(other) => Err(RemoteError::Protocol(format!(
            "{context}: unexpected Content-Encoding {other}"
        ))),
    }
}

impl RemoteClient {
    /// `POST /waveforms` (§9.1) for 1..=200 ids with the entity tags of cached copies in
    /// `known`. `cancel` closes the connection at once. Errors keep their class:
    /// `503 BUSY` is [`RemoteError::Busy`] with the `Retry-After` seconds, a server without
    /// the endpoint is [`RemoteError::Unsupported`].
    pub fn list_waveform_batch(
        &self,
        ids: &[i64],
        known: &BTreeMap<i64, String>,
        cancel: &AtomicBool,
    ) -> Result<BatchReply> {
        self.list_waveform_batch_encoded(ids, known, None, cancel)
    }

    /// [`RemoteClient::list_waveform_batch`] with an optional request `encoding` (sent only
    /// when `Some`). A `UNSUPPORTED_ENCODING` error body is reported as
    /// [`RemoteError::Server`] with that code.
    pub fn list_waveform_batch_encoded(
        &self,
        ids: &[i64],
        known: &BTreeMap<i64, String>,
        encoding: Option<&str>,
        cancel: &AtomicBool,
    ) -> Result<BatchReply> {
        if ids.is_empty() || ids.len() > MAX_LIST_WAVEFORM_IDS || ids.iter().any(|id| *id <= 0) {
            return Err(RemoteError::Validation(format!(
                "POST /waveforms takes 1..={MAX_LIST_WAVEFORM_IDS} positive ids"
            )));
        }
        let known: BTreeMap<String, &String> = known
            .iter()
            .filter(|(id, _)| ids.contains(id))
            .map(|(id, etag)| (id.to_string(), etag))
            .collect();
        let mut body = serde_json::json!({ "ids": ids, "buckets": LIST_WAVEFORM_COLUMNS });
        if !known.is_empty() {
            body["known"] = serde_json::json!(known);
        }
        if let Some(encoding) = encoding {
            body["encoding"] = serde_json::json!(encoding);
        }
        let body = serde_json::to_vec(&body)
            .map_err(|error| RemoteError::Validation(format!("POST /waveforms: {error}")))?;
        let url = format!("{}/waveforms", self.api);
        let token = (!self.token.is_empty()).then(|| format!("Bearer {}", self.token));
        let context = "POST /waveforms";
        let (status, encoding, reply, retry) = call_cancellable(
            move |abort| {
                let agent = crate::abort::abortable_agent(BATCH_TIMEOUT, BATCH_TIMEOUT, abort);
                let send = || {
                    let mut request = agent
                        .post(&url)
                        .header("Accept-Encoding", "zstd")
                        .header("Content-Type", "application/json");
                    if let Some(token) = &token {
                        request = request.header("Authorization", token);
                    }
                    request.send(body.as_slice())
                };
                // Read-only despite the method: resent once on a stale pooled connection.
                let mut response = match send() {
                    Err(error) if crate::abort::stale_connection(&error) => send(),
                    result => result,
                }
                .map_err(|error| RemoteError::from_transport(error, &url))?;
                let encoding = response
                    .headers()
                    .get("content-encoding")
                    .and_then(|value| value.to_str().ok())
                    .map(|value| value.trim().to_owned());
                let retry = retry_after(response.headers());
                let reply = response
                    .body_mut()
                    .with_config()
                    .limit(MAX_BATCH_BYTES)
                    .read_to_vec()
                    .map_err(|error| RemoteError::from_transport(error, &url))?;
                Ok((response.status().as_u16(), encoding, reply, retry))
            },
            cancel,
            &mut |_| {},
        )?;
        if status == 200 {
            let json = decompress(context, encoding.as_deref(), reply)?;
            return serde_json::from_slice(&json)
                .map_err(|error| RemoteError::Protocol(format!("{context}: {error}")));
        }
        let text = decompress(context, encoding.as_deref(), reply)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
        if let Some(message) = unsupported_encoding(&text) {
            return Err(RemoteError::Server {
                status,
                code: UNSUPPORTED_ENCODING.into(),
                message: format!("{context}: {message}"),
            });
        }
        let error = match RemoteError::from_response(status, &text, context, retry) {
            RemoteError::NotFound(_) | RemoteError::Server { status: 405, .. } => {
                RemoteError::Unsupported(format!("{context}: the server has no list Waveforms"))
            }
            other => other,
        };
        Err(if self.token.is_empty() {
            error.without_token()
        } else {
            error
        })
    }

    /// Stream `GET /waveforms/bundle?encoding=i8-minmax-base64` (§9.4.2) into
    /// `destination` (created or truncated) without holding it in memory, checking
    /// `Content-Length` and the `ETag` (`"wfb1-"` + first 16 bytes of the SHA-256 as hex).
    /// `503 BUSY` is [`RemoteError::Busy`] with `Retry-After`. The caller validates and
    /// removes the file.
    /// With `if_none_match` (the ETag of the bundle already installed) the request carries
    /// `If-None-Match`; a `304` (whose `ETag`, if any, must equal `if_none_match`), or a `200`
    /// whose `ETag` equals `if_none_match` (a server ignoring the condition; the body is not
    /// read), is [`BundleDownload::NotModified`] and `destination` is not touched. A `304` to a
    /// request without `If-None-Match` is a protocol error.
    pub fn download_list_waveform_bundle(
        &self,
        destination: &Path,
        if_none_match: Option<&str>,
        cancel: &AtomicBool,
    ) -> Result<BundleDownload> {
        crate::client::cancellable_io(cancel, || {
            self.download_list_waveform_bundle_io(destination, if_none_match, cancel)
        })
    }

    fn download_list_waveform_bundle_io(
        &self,
        destination: &Path,
        if_none_match: Option<&str>,
        cancel: &AtomicBool,
    ) -> Result<BundleDownload> {
        use std::io::Write;
        let context = "GET /waveforms/bundle";
        let url = format!(
            "{}/waveforms/bundle?encoding={LIST_WAVEFORM_ENCODING_I8}",
            self.api
        );
        let response = crate::abort::call_fresh(|| {
            let mut request = self.bulk.get(&url);
            if !self.token.is_empty() {
                request = request.header("Authorization", &format!("Bearer {}", self.token));
            }
            if let Some(etag) = if_none_match {
                request = request.header("If-None-Match", etag);
            }
            request
        })
        .map_err(|error| RemoteError::from_transport(error, &url))?;
        let raw_header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(|value| value.trim().to_owned())
        };
        let (reply_etag, reply_library) = (raw_header("etag"), raw_header("x-library-id"));
        let Some(response) =
            self.checked_conditional(response, context, if_none_match.is_some())?
        else {
            // A 304 names the representation it confirms: it must be the installed one.
            if let (Some(known), Some(reply)) = (if_none_match, reply_etag.as_deref())
                && reply != known
            {
                return Err(RemoteError::Protocol(format!(
                    "{context}: 304 Not Modified with ETag {reply}, expected {known}"
                )));
            }
            return Ok(BundleDownload::NotModified {
                library_id: reply_library,
            });
        };
        // An older server ignores If-None-Match and answers 200 with the whole bundle (about
        // 300 MB for a large library). When its ETag is the installed one, the body is not
        // read: re-downloading would only replace the store and drop rows written since.
        if if_none_match.is_some() && reply_etag.as_deref() == if_none_match {
            drop(response);
            return Ok(BundleDownload::NotModified {
                library_id: reply_library,
            });
        }
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(|value| value.trim().to_owned())
        };
        if let Some(encoding) = header("content-encoding")
            && !encoding.eq_ignore_ascii_case("identity")
        {
            return Err(RemoteError::Protocol(format!(
                "{context}: unexpected Content-Encoding {encoding}"
            )));
        }
        let etag =
            header("etag").ok_or_else(|| RemoteError::Protocol(format!("{context}: no ETag")))?;
        let length = header("content-length").and_then(|value| value.parse::<u64>().ok());
        let meta = BundleMeta {
            etag,
            library_id: header("x-library-id").unwrap_or_default(),
            revision: header("x-library-revision").and_then(|value| value.parse().ok()),
            bytes: 0,
        };
        let mut file = std::fs::File::create(destination)?;
        let mut reader = response.into_body().into_reader();
        let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
        let mut buffer = vec![0u8; 256 * 1024];
        let mut received = 0u64;
        loop {
            if cancel.load(Ordering::Acquire) {
                return Err(RemoteError::Cancelled);
            }
            let read = reader.read(&mut buffer).map_err(|error| {
                if cancel.load(Ordering::Acquire) {
                    RemoteError::Cancelled
                } else {
                    RemoteError::Unreachable(format!("{context}: {error}"))
                }
            })?;
            if read == 0 {
                break;
            }
            digest.update(&buffer[..read]);
            file.write_all(&buffer[..read])?;
            received += read as u64;
        }
        file.sync_all()?;
        drop(file);
        if length.is_some_and(|length| length != received) {
            return Err(RemoteError::Protocol(format!(
                "{context}: truncated, received {received} of {} bytes",
                length.unwrap_or_default()
            )));
        }
        let expected = bundle_etag(digest.finish().as_ref());
        if meta.etag != expected {
            return Err(RemoteError::Protocol(format!(
                "{context}: ETag {} does not match the content ({expected})",
                meta.etag
            )));
        }
        Ok(BundleDownload::Downloaded(BundleMeta {
            bytes: received,
            ..meta
        }))
    }

    /// `GET /settings` (§9.2): not gated by `manage_mode`.
    pub fn server_settings(&self) -> Result<ServerSettings> {
        self.manage_json("GET", "/settings", None)
    }

    /// `PATCH /settings` with `pregenerate_list_waveforms` (gated by `manage_mode`:
    /// `403 MANAGE_FORBIDDEN`; `409 BUSY` while the settings lock is taken).
    pub fn set_pregenerate_list_waveforms(&self, enabled: bool) -> Result<ServerSettings> {
        self.manage_json(
            "PATCH",
            "/settings",
            Some(&serde_json::json!({ "pregenerate_list_waveforms": enabled })),
        )
    }

    /// `GET /waveforms/status` (§9.2): pre-generation progress (kept in server memory).
    pub fn list_waveform_status(&self) -> Result<ListWaveformStatus> {
        self.manage_json("GET", "/waveforms/status", None)
    }
}

fn checksum(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// One stored list Waveform and its server entity tag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredListWaveform {
    pub etag: String,
    pub waveform: ListWaveform,
}

type Shard = BTreeMap<i64, StoredListWaveform>;

fn encode_shard(shard: &Shard) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(16 + shard.len() * 560);
    bytes.extend_from_slice(SHARD_MAGIC);
    bytes.extend_from_slice(&(shard.len() as u32).to_le_bytes());
    for (id, entry) in shard {
        let etag = entry.etag.as_bytes();
        bytes.extend_from_slice(&id.to_le_bytes());
        bytes.extend_from_slice(&(etag.len() as u16).to_le_bytes());
        bytes.extend_from_slice(etag);
        bytes.extend_from_slice(&(entry.waveform.columns() as u16).to_le_bytes());
        bytes.extend_from_slice(&entry.waveform.to_interleaved_le());
    }
    let sum = checksum(&bytes);
    bytes.extend_from_slice(&sum.to_le_bytes());
    bytes
}

fn decode_shard(bytes: &[u8]) -> Option<Shard> {
    let (body, sum) = bytes.split_at_checked(bytes.len().checked_sub(8)?)?;
    if checksum(body).to_le_bytes() != sum || !body.starts_with(SHARD_MAGIC) {
        return None;
    }
    fn take<'a>(rest: &mut &'a [u8], count: usize) -> Option<&'a [u8]> {
        let (head, tail) = rest.split_at_checked(count)?;
        *rest = tail;
        Some(head)
    }
    let mut rest = &body[SHARD_MAGIC.len()..];
    let count = u32::from_le_bytes(take(&mut rest, 4)?.try_into().ok()?);
    let mut shard = Shard::new();
    for _ in 0..count {
        let id = i64::from_le_bytes(take(&mut rest, 8)?.try_into().ok()?);
        let etag_len = u16::from_le_bytes(take(&mut rest, 2)?.try_into().ok()?) as usize;
        let etag = String::from_utf8(take(&mut rest, etag_len)?.to_vec()).ok()?;
        let columns = u16::from_le_bytes(take(&mut rest, 2)?.try_into().ok()?) as usize;
        let waveform = ListWaveform::from_interleaved_le(1, take(&mut rest, columns * 4)?).ok()?;
        shard.insert(id, StoredListWaveform { etag, waveform });
    }
    rest.is_empty().then_some(shard)
}

fn now_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos() as u64)
}

#[derive(Clone, Copy, Debug)]
struct ShardFile {
    bytes: u64,
    /// Last use (Unix nanoseconds): the file time at startup, then reads and writes.
    used: u64,
}

#[derive(Default)]
struct StoreInner {
    scanned: bool,
    files: HashMap<i64, ShardFile>,
    total: u64,
    memory: HashMap<i64, Shard>,
    memory_order: VecDeque<i64>,
}

/// Disk cache of one server library's list Waveforms (§9.3 client cache): shard
/// `id / 1024` per file, client-private format with a checksum, written to a temporary
/// file and renamed over the old one. Damaged shards are dropped. When the total exceeds
/// the limit, whole shards are evicted, least recently used first.
pub struct ListWaveformStore {
    directory: PathBuf,
    limit: u64,
    inner: Mutex<StoreInner>,
}

static TEMPORARY: AtomicU64 = AtomicU64::new(0);

impl ListWaveformStore {
    /// `<cache_root>/waveforms/server/<library_id hash>/list/` with the default limit.
    pub fn for_library(cache_root: &Path, library_id: &str) -> Self {
        Self::new(
            Self::library_directory(cache_root, library_id),
            DEFAULT_LIST_WAVEFORM_CACHE_BYTES,
        )
    }

    pub fn library_directory(cache_root: &Path, library_id: &str) -> PathBuf {
        cache_root
            .join("waveforms")
            .join("server")
            .join(format!("{:016x}", crate::util::fnv64(library_id)))
            .join("list")
    }

    pub fn new(directory: PathBuf, limit: u64) -> Self {
        Self {
            directory,
            limit,
            inner: Mutex::new(StoreInner::default()),
        }
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    fn lock(&self) -> MutexGuard<'_, StoreInner> {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if !inner.scanned {
            inner.scanned = true;
            self.scan(&mut inner);
        }
        inner
    }

    fn shard_path(&self, shard: i64) -> PathBuf {
        self.directory.join(format!("{shard}.{SHARD_EXTENSION}"))
    }

    /// Sizes and times of the existing shards; leftover temporary files are removed.
    fn scan(&self, inner: &mut StoreInner) {
        let Ok(entries) = std::fs::read_dir(&self.directory) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if name.contains(".tmp") {
                let _ = std::fs::remove_file(&path);
                continue;
            }
            let Some(shard) = name
                .strip_suffix(&format!(".{SHARD_EXTENSION}"))
                .and_then(|stem| stem.parse::<i64>().ok())
            else {
                continue;
            };
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let used = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |elapsed| elapsed.as_nanos() as u64);
            inner.files.insert(
                shard,
                ShardFile {
                    bytes: metadata.len(),
                    used,
                },
            );
            inner.total += metadata.len();
        }
    }

    fn load<'a>(&self, inner: &'a mut StoreInner, shard: i64) -> &'a mut Shard {
        if !inner.memory.contains_key(&shard) {
            let loaded = match inner.files.contains_key(&shard) {
                true => match std::fs::read(self.shard_path(shard))
                    .ok()
                    .and_then(|bytes| decode_shard(&bytes))
                {
                    Some(loaded) => loaded,
                    None => {
                        tracing::warn!(shard, "damaged list Waveform cache shard dropped");
                        let _ = std::fs::remove_file(self.shard_path(shard));
                        if let Some(file) = inner.files.remove(&shard) {
                            inner.total -= file.bytes;
                        }
                        Shard::new()
                    }
                },
                false => Shard::new(),
            };
            inner.memory.insert(shard, loaded);
            inner.memory_order.push_back(shard);
            while inner.memory_order.len() > MEMORY_SHARDS {
                if let Some(oldest) = inner.memory_order.pop_front() {
                    inner.memory.remove(&oldest);
                }
            }
        }
        if let Some(file) = inner.files.get_mut(&shard) {
            file.used = now_nanos();
        }
        inner.memory.get_mut(&shard).expect("loaded shard")
    }

    /// Stored copies of `ids` (absent ones are left out).
    pub fn get_many(&self, ids: &[i64]) -> HashMap<i64, StoredListWaveform> {
        let mut inner = self.lock();
        let mut found = HashMap::new();
        for id in ids {
            let shard = id.div_euclid(LIST_WAVEFORM_SHARD_IDS);
            if let Some(entry) = self.load(&mut inner, shard).get(id) {
                found.insert(*id, entry.clone());
            }
        }
        found
    }

    pub fn get(&self, id: i64) -> Option<StoredListWaveform> {
        self.get_many(&[id]).remove(&id)
    }

    /// Store new copies (one atomic rewrite per touched shard), then evict old shards
    /// beyond the limit.
    pub fn put_many(&self, entries: Vec<(i64, StoredListWaveform)>) -> std::io::Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let mut inner = self.lock();
        let mut touched = Vec::new();
        for (id, entry) in entries {
            let shard = id.div_euclid(LIST_WAVEFORM_SHARD_IDS);
            if self.load(&mut inner, shard).get(&id) != Some(&entry) {
                self.load(&mut inner, shard).insert(id, entry);
                if !touched.contains(&shard) {
                    touched.push(shard);
                }
            }
        }
        std::fs::create_dir_all(&self.directory)?;
        let mut result = Ok(());
        for shard in &touched {
            let bytes = encode_shard(self.load(&mut inner, *shard));
            match self.write_atomic(*shard, &bytes) {
                Ok(()) => {
                    let size = bytes.len() as u64;
                    let previous = inner.files.insert(
                        *shard,
                        ShardFile {
                            bytes: size,
                            used: now_nanos(),
                        },
                    );
                    inner.total = inner.total - previous.map_or(0, |file| file.bytes) + size;
                }
                Err(error) => result = Err(error),
            }
        }
        self.evict(&mut inner, &touched);
        result
    }

    fn write_atomic(&self, shard: i64, bytes: &[u8]) -> std::io::Result<()> {
        let temporary = self.directory.join(format!(
            "{shard}.tmp{}-{}",
            std::process::id(),
            TEMPORARY.fetch_add(1, Ordering::Relaxed)
        ));
        let written = std::fs::write(&temporary, bytes)
            .and_then(|()| std::fs::rename(&temporary, self.shard_path(shard)));
        if written.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        written
    }

    fn evict(&self, inner: &mut StoreInner, keep: &[i64]) {
        while inner.total > self.limit {
            let Some(oldest) = inner
                .files
                .iter()
                .filter(|(shard, _)| !keep.contains(shard))
                .min_by_key(|(_, file)| file.used)
                .map(|(shard, _)| *shard)
            else {
                break;
            };
            let _ = std::fs::remove_file(self.shard_path(oldest));
            if let Some(file) = inner.files.remove(&oldest) {
                inner.total -= file.bytes;
            }
            inner.memory.remove(&oldest);
            inner.memory_order.retain(|shard| *shard != oldest);
            tracing::info!(shard = oldest, "list Waveform cache shard evicted");
        }
    }

    /// Bytes on disk.
    pub fn bytes(&self) -> u64 {
        self.lock().total
    }
}

enum Probe {
    Unknown,
    Known(Option<ListWaveformsCapability>),
    Failed(Instant),
}

/// List Waveform lookups of one open server snapshot (see the module documentation).
pub struct RemoteListWaveforms {
    client: RemoteClient,
    library_id: String,
    store: ListWaveformStore,
    capability: Mutex<Probe>,
    in_flight: Mutex<()>,
    /// `waveforms.db` for compact i8 items; `None` keeps the int16 path only.
    i8_store: Option<WaveformsDb>,
    /// Set after `UNSUPPORTED_ENCODING`: this client stays on the int16 path.
    i8_rejected: AtomicBool,
}

impl RemoteListWaveforms {
    pub fn new(
        client: RemoteClient,
        library_id: impl Into<String>,
        store: ListWaveformStore,
    ) -> Self {
        Self {
            client,
            library_id: library_id.into(),
            store,
            capability: Mutex::new(Probe::Unknown),
            in_flight: Mutex::new(()),
            i8_store: None,
            i8_rejected: AtomicBool::new(false),
        }
    }

    /// Attach a `waveforms.db` store, enabling `i8-minmax-base64` requests when the
    /// server's capability lists that encoding.
    pub fn with_i8_store(mut self, store: WaveformsDb) -> Self {
        self.i8_store = Some(store);
        self
    }

    /// `waveforms.db` beside a server library's list cache directory
    /// (`<cache>/waveforms/server/<library hash>/waveforms.db`).
    pub fn i8_store_path(cache_root: &Path, library_id: &str) -> PathBuf {
        ListWaveformStore::library_directory(cache_root, library_id)
            .with_file_name(audiolib_db::WAVEFORMS_DB_FILE)
    }

    /// Whether a bundle import is possible (`bundle` and i8 advertised, store attached,
    /// i8 not rejected).
    pub fn offers_bundle(&self) -> bool {
        self.requests_i8()
            && self
                .capability()
                .is_some_and(|capability| capability.bundle)
    }

    /// Download, verify, validate and install the server's `waveforms.db` bundle
    /// (§9.4.2). On any failure the temporary file is removed and the existing store is
    /// kept. `503 BUSY` is returned as [`RemoteError::Busy`]; retry after
    /// [`bundle_backoff`].
    ///
    /// The ETag of an installed bundle is kept in `waveforms.db.etag` beside the store
    /// ([`Self::installed_bundle_etag`]). While the store is not empty the request sends it
    /// as `If-None-Match`; `304 Not Modified` returns [`BundleImport::NotModified`] and the
    /// store (the earlier bundle plus rows written since) is reused unchanged.
    pub fn import_bundle(&self, cancel: &AtomicBool) -> Result<BundleImport> {
        let Some(store) = self.i8_store.as_ref().filter(|_| self.offers_bundle()) else {
            return Err(RemoteError::Unsupported(
                "The server offers no list Waveform bundle".into(),
            ));
        };
        let etag_path = bundle_etag_path(store.path());
        let known = self.installed_bundle_etag();
        let staged = store.path().with_file_name(format!(
            "{}.bundle-{}-{}.tmp",
            audiolib_db::WAVEFORMS_DB_FILE,
            std::process::id(),
            BUNDLE_TEMPORARY.fetch_add(1, Ordering::Relaxed)
        ));
        let result = (|| {
            let meta = match self.client.download_list_waveform_bundle(
                &staged,
                known.as_deref(),
                cancel,
            )? {
                BundleDownload::NotModified { library_id } => {
                    if let Some(library_id) = library_id
                        && library_id != self.library_id
                    {
                        return Err(RemoteError::LibraryChanged(format!(
                            "waveform bundle check is for library {library_id:?}, not the open one"
                        )));
                    }
                    tracing::info!(etag = ?known, "waveform bundle not modified; store reused");
                    return Ok(BundleImport::NotModified);
                }
                BundleDownload::Downloaded(meta) => meta,
            };
            if meta.library_id != self.library_id {
                return Err(RemoteError::LibraryChanged(format!(
                    "waveform bundle is for library {:?}, not the open snapshot's; discarded",
                    meta.library_id
                )));
            }
            if cancel.load(Ordering::Acquire) {
                return Err(RemoteError::Cancelled);
            }
            let rows =
                audiolib_db::validate_bundle(&staged, LIST_WAVEFORM_ENCODING_I8, &self.library_id)
                    .map_err(|error| RemoteError::Protocol(error.to_string()))?;
            // Drop the old tag first: a store replaced without a recorded tag only costs a
            // full download next time, while a stale tag could answer for the wrong bundle.
            remove_bundle_etag(&etag_path);
            store
                .replace_with(&staged)
                .map_err(|error| RemoteError::Io(std::io::Error::other(error.to_string())))?;
            if let Err(error) = write_bundle_etag(&etag_path, &meta.etag) {
                tracing::warn!(%error, "waveform bundle ETag not saved; next check downloads");
            }
            tracing::info!(rows, bytes = meta.bytes, etag = %meta.etag, "waveform bundle installed");
            Ok(BundleImport::Installed { rows })
        })();
        if !matches!(result, Ok(BundleImport::NotModified)) {
            let _ = std::fs::remove_file(&staged);
        }
        result
    }

    /// ETag of the bundle last installed into the attached `waveforms.db`, when one was
    /// recorded, it is well formed (`"wfb1-<32 hex>"`) and the store is not empty. An empty
    /// store (new, or emptied since) never sends `If-None-Match`, so a `304` cannot leave
    /// it without rows.
    pub fn installed_bundle_etag(&self) -> Option<String> {
        let store = self.i8_store.as_ref()?;
        if store.is_empty().unwrap_or(true) {
            return None;
        }
        let text = std::fs::read_to_string(bundle_etag_path(store.path())).ok()?;
        let etag = text.trim();
        is_bundle_etag(etag).then(|| etag.to_owned())
    }

    /// Whether the next lookup sends `encoding: i8-minmax-base64`.
    pub fn requests_i8(&self) -> bool {
        self.i8_store.is_some()
            && !self.i8_rejected.load(Ordering::Acquire)
            && self
                .capability()
                .is_some_and(|capability| capability.offers_i8())
    }

    /// Use already fetched capabilities instead of probing `GET /capabilities`.
    pub fn with_capabilities(self, capabilities: &Capabilities) -> Self {
        *self
            .capability
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Probe::Known(usable(capabilities));
        self
    }

    pub fn library_id(&self) -> &str {
        &self.library_id
    }

    pub fn store(&self) -> &ListWaveformStore {
        &self.store
    }

    /// The attached `waveforms.db`, if any.
    pub fn i8_store(&self) -> Option<&WaveformsDb> {
        self.i8_store.as_ref()
    }

    /// Blocking on first use (`GET /capabilities`, run it off the UI thread):
    /// [`ListWaveformSource::Server`] when the server offers usable list Waveforms,
    /// otherwise [`ListWaveformSource::Unavailable`]. A failed probe is repeated after 30 s.
    pub fn source(&self) -> ListWaveformSource {
        match self.capability() {
            Some(_) => ListWaveformSource::Server,
            None => ListWaveformSource::Unavailable,
        }
    }

    fn capability(&self) -> Option<ListWaveformsCapability> {
        {
            let probe = self
                .capability
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            match &*probe {
                Probe::Known(capability) => return capability.clone(),
                Probe::Failed(at) if at.elapsed() < CAPABILITY_RETRY => return None,
                _ => {}
            }
        }
        let probed = self.client.capabilities();
        let mut probe = self
            .capability
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match probed {
            Ok(capabilities) => {
                let capability = usable(&capabilities);
                *probe = Probe::Known(capability.clone());
                capability
            }
            Err(error) => {
                tracing::debug!(%error, "list Waveform capability probe failed");
                *probe = Probe::Failed(Instant::now());
                None
            }
        }
    }

    /// One `POST /waveforms` for up to the server's `max_ids` of `ids` (deduplicated,
    /// in order), resolved against the disk cache. Never more than one request of this
    /// client is in flight.
    pub fn lookup(&self, ids: &[i64], cancel: &AtomicBool) -> Result<ListWaveformBatch> {
        let Some(capability) = self.capability() else {
            return Err(RemoteError::Unsupported(
                "The server offers no list Waveforms".into(),
            ));
        };
        let mut seen = HashSet::new();
        let ids: Vec<i64> = ids
            .iter()
            .copied()
            .filter(|id| *id > 0 && seen.insert(*id))
            .take(capability.batch_limit())
            .collect();
        if ids.is_empty() {
            return Ok(ListWaveformBatch::default());
        }
        if capability.offers_i8()
            && !self.i8_rejected.load(Ordering::Acquire)
            && let Some(i8_store) = &self.i8_store
        {
            match self.lookup_i8(i8_store, &ids, cancel) {
                Err(RemoteError::Server { code, .. }) if code == UNSUPPORTED_ENCODING => {
                    tracing::info!("server rejected i8 list Waveforms; using int16");
                    self.i8_rejected.store(true, Ordering::Release);
                }
                other => return other,
            }
        }
        let stored = self.store.get_many(&ids);
        let known: BTreeMap<i64, String> = stored
            .iter()
            .map(|(id, entry)| (*id, entry.etag.clone()))
            .collect();
        let reply = {
            let _one = self
                .in_flight
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if cancel.load(Ordering::Acquire) {
                return Err(RemoteError::Cancelled);
            }
            self.client.list_waveform_batch(&ids, &known, cancel)
        };
        let reply = match reply {
            Ok(reply) => reply,
            Err(RemoteError::Busy { retry_after, .. }) => return Ok(busy(retry_after)),
            Err(error) => return Err(error),
        };
        self.check_reply(&reply, None)?;
        Ok(self.resolve(&ids, reply, &stored))
    }

    /// Library, bucket and encoding echo checks shared by both encodings.
    fn check_reply(&self, reply: &BatchReply, requested: Option<&str>) -> Result<()> {
        if reply.library_id != self.library_id {
            return Err(RemoteError::LibraryChanged(format!(
                "POST /waveforms answered for library {} instead of the open snapshot's; reply discarded",
                reply.library_id
            )));
        }
        if reply.buckets != 0 && reply.buckets != LIST_WAVEFORM_COLUMNS {
            return Err(RemoteError::Protocol(format!(
                "POST /waveforms answered with {} buckets",
                reply.buckets
            )));
        }
        if let Some(requested) = requested {
            if reply.encoding.as_deref() != Some(requested) {
                return Err(RemoteError::Protocol(format!(
                    "POST /waveforms answered with encoding {:?} instead of {requested}",
                    reply.encoding
                )));
            }
            if reply.items.iter().any(|item| {
                item.encoding
                    .as_deref()
                    .is_some_and(|value| value != requested)
            }) {
                return Err(RemoteError::Protocol(
                    "POST /waveforms mixed encodings in one reply".into(),
                ));
            }
        }
        Ok(())
    }

    /// The `i8-minmax-base64` lookup, cached in `waveforms.db`.
    fn lookup_i8(
        &self,
        i8_store: &WaveformsDb,
        ids: &[i64],
        cancel: &AtomicBool,
    ) -> Result<ListWaveformBatch> {
        let stored: HashMap<i64, WaveformsDbRow> = i8_store
            .get_many(ids)
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "waveforms.db read failed");
                Vec::new()
            })
            .into_iter()
            .filter(|row| row.encoding == LIST_WAVEFORM_ENCODING_I8)
            .map(|row| (row.file_id, row))
            .collect();
        let known: BTreeMap<i64, String> = stored
            .iter()
            .map(|(id, row)| (*id, row.etag.clone()))
            .collect();
        let reply = {
            let _one = self
                .in_flight
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if cancel.load(Ordering::Acquire) {
                return Err(RemoteError::Cancelled);
            }
            self.client.list_waveform_batch_encoded(
                ids,
                &known,
                Some(LIST_WAVEFORM_ENCODING_I8),
                cancel,
            )
        };
        let reply = match reply {
            Ok(reply) => reply,
            Err(RemoteError::Busy { retry_after, .. }) => return Ok(busy(retry_after)),
            Err(error) => return Err(error),
        };
        match self.check_reply(&reply, Some(LIST_WAVEFORM_ENCODING_I8)) {
            Ok(()) => Ok(resolve_i8(i8_store, ids, reply, &stored)),
            // §9.4: a wrong echo is a protocol error; nothing of the reply is cached and
            // every id backs off like `pending`, keeping any older copy on screen.
            Err(RemoteError::Protocol(message)) => {
                tracing::warn!(%message, "i8 list Waveform reply discarded");
                Ok(ListWaveformBatch {
                    items: ids
                        .iter()
                        .map(|id| ListWaveformItem {
                            id: *id,
                            state: ListWaveformState::Pending {
                                previous: stored.get(id).map(|row| row.peaks.to_list_waveform()),
                            },
                        })
                        .collect(),
                    retry_after: Some(ECHO_MISMATCH_RETRY),
                    deferred: None,
                })
            }
            Err(error) => Err(error),
        }
    }

    fn resolve(
        &self,
        ids: &[i64],
        reply: BatchReply,
        stored: &HashMap<i64, StoredListWaveform>,
    ) -> ListWaveformBatch {
        let previous = |id: i64| stored.get(&id).map(|entry| entry.waveform.clone());
        let mut answered = HashSet::new();
        let mut items = Vec::new();
        let mut store = Vec::new();
        for item in reply.items {
            if !ids.contains(&item.id) || !answered.insert(item.id) {
                continue;
            }
            let id = item.id;
            let state = match item.status {
                BatchStatus::Ok => {
                    let parsed = match (&item.etag, &item.peaks, item.n) {
                        (Some(etag), Some(peaks), Some(n)) => {
                            ListWaveform::from_base64_peaks(peaks, n)
                                .map(|waveform| (etag.clone(), waveform))
                                .map_err(|error| error.to_string())
                        }
                        _ => Err("incomplete ok item".into()),
                    };
                    match parsed {
                        Ok((etag, waveform)) => {
                            store.push((
                                id,
                                StoredListWaveform {
                                    etag,
                                    waveform: waveform.clone(),
                                },
                            ));
                            ListWaveformState::Ready { waveform }
                        }
                        Err(message) => {
                            tracing::warn!(id, %message, "malformed list Waveform item");
                            ListWaveformState::Failed { message }
                        }
                    }
                }
                BatchStatus::NotModified => match stored.get(&id) {
                    Some(entry) if item.etag.as_ref().is_none_or(|etag| *etag == entry.etag) => {
                        ListWaveformState::Ready {
                            waveform: entry.waveform.clone(),
                        }
                    }
                    // Not what was sent in `known`: ask again without it later.
                    _ => ListWaveformState::Pending { previous: None },
                },
                BatchStatus::Pending | BatchStatus::Unknown => ListWaveformState::Pending {
                    previous: previous(id),
                },
                BatchStatus::Missing => ListWaveformState::Missing,
                BatchStatus::RootOffline => ListWaveformState::RootOffline {
                    root: item.root.unwrap_or_default(),
                    previous: previous(id),
                },
                BatchStatus::Error => ListWaveformState::Failed {
                    message: item
                        .message
                        .or(item.error_code)
                        .unwrap_or_else(|| "error".into()),
                },
            };
            items.push(ListWaveformItem { id, state });
        }
        if let Err(error) = self.store.put_many(store) {
            tracing::warn!(%error, "list Waveform cache write failed");
        }
        ListWaveformBatch {
            items,
            retry_after: reply
                .retry_after
                .map(|seconds| Duration::from_secs(seconds.clamp(1, 30))),
            deferred: None,
        }
    }
}

/// The `message` of an error body whose `sub_code` (doc/api §9.4: `400 VALIDATION` with
/// `"sub_code": "UNSUPPORTED_ENCODING"`) or `error_code` is `UNSUPPORTED_ENCODING`.
pub(crate) fn unsupported_encoding(body: &str) -> Option<String> {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Body {
        error_code: String,
        sub_code: Option<String>,
        message: String,
    }
    let parsed: Body = serde_json::from_str(body).ok()?;
    (parsed.sub_code.as_deref() == Some(UNSUPPORTED_ENCODING)
        || parsed.error_code == UNSUPPORTED_ENCODING)
        .then_some(parsed.message)
}

/// Pending wait after an i8 reply with a wrong encoding echo (§9.4: nothing is cached and
/// the ids back off like `pending`).
const ECHO_MISMATCH_RETRY: Duration = Duration::from_secs(2);

/// Headers of a downloaded `waveforms.db` bundle (§9.4.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundleMeta {
    /// `"wfb1-<32 hex>"`, verified against the content.
    pub etag: String,
    /// `X-Library-Id`.
    pub library_id: String,
    /// `X-Library-Revision` (diagnostic only; never used to judge entry freshness).
    pub revision: Option<i64>,
    pub bytes: u64,
}

/// Outcome of [`RemoteClient::download_list_waveform_bundle`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BundleDownload {
    /// The installed bundle is current (`304`, or a `200` carrying the same `ETag` whose body
    /// was not read); nothing was written. `library_id` is the reply's `X-Library-Id`.
    NotModified { library_id: Option<String> },
    /// `200`: the bundle was written to the destination and its ETag verified.
    Downloaded(BundleMeta),
}

/// Outcome of [`RemoteListWaveforms::import_bundle`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BundleImport {
    /// A new bundle replaced the store; `rows` is the bundle's row count.
    Installed { rows: u64 },
    /// `304 Not Modified`: the cached store is reused unchanged.
    NotModified,
}

/// `waveforms.db.etag` beside the store: the ETag of the bundle installed there. The name
/// does not start with `waveforms.db.bundle-`, which stale-download cleanup removes.
fn bundle_etag_path(store: &Path) -> PathBuf {
    store.with_file_name(format!("{}.etag", audiolib_db::WAVEFORMS_DB_FILE))
}

fn is_bundle_etag(text: &str) -> bool {
    text.strip_prefix("\"wfb1-")
        .and_then(|rest| rest.strip_suffix('"'))
        .is_some_and(|hex| {
            hex.len() == 32
                && hex
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
}

/// Best effort: a tag that cannot be removed (for example denied by another process) is
/// logged and the install continues; [`write_bundle_etag`] then replaces it, and a tag left
/// stale only makes the next check download the bundle in full.
fn remove_bundle_etag(path: &Path) {
    if let Err(error) = std::fs::remove_file(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(%error, "old waveform bundle ETag not removed; continuing");
    }
}

/// Write the tag through a temporary file and rename it, so a crash never leaves a
/// partial tag.
fn write_bundle_etag(path: &Path, etag: &str) -> std::io::Result<()> {
    let temporary = path.with_extension(format!("etag-{}.tmp", std::process::id()));
    std::fs::write(&temporary, format!("{etag}\n"))?;
    std::fs::rename(&temporary, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })
}

/// `"wfb1-<first 16 bytes of SHA-256 as lowercase hex>"`.
pub fn bundle_etag(sha256: &[u8]) -> String {
    let hex: String = sha256
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("\"wfb1-{hex}\"")
}

/// Wait before bundle attempt `attempt + 1` after a `503 BUSY` (§9.4.2): the larger of the
/// server's `Retry-After` and 5 s doubling per consecutive failure, at most 5 minutes.
pub fn bundle_backoff(attempt: u32, retry_after: Option<u64>) -> Duration {
    const CAP: Duration = Duration::from_secs(300);
    let doubling = Duration::from_secs(5).saturating_mul(1u32 << attempt.min(16));
    doubling
        .max(Duration::from_secs(retry_after.unwrap_or(0)))
        .min(CAP)
}

static BUNDLE_TEMPORARY: AtomicU64 = AtomicU64::new(0);

fn busy(retry_after: Option<u64>) -> ListWaveformBatch {
    ListWaveformBatch {
        deferred: Some(
            retry_after.map_or(BUSY_DEFAULT, |seconds| Duration::from_secs(seconds.max(1))),
        ),
        ..ListWaveformBatch::default()
    }
}

/// Parse one `ok` i8 item: its entity tag must carry the `-l1i8` suffix.
pub(crate) fn parse_i8_item(
    item: &BatchItem,
) -> std::result::Result<(String, ListWaveformI8), String> {
    match (&item.etag, &item.peaks, item.n) {
        (Some(etag), Some(peaks), Some(n)) => {
            if !etag
                .trim_end_matches('"')
                .ends_with(LIST_WAVEFORM_I8_ETAG_SUFFIX)
            {
                return Err(format!(
                    "i8 item entity tag {etag} lacks {LIST_WAVEFORM_I8_ETAG_SUFFIX}"
                ));
            }
            ListWaveformI8::from_base64_peaks(peaks, n)
                .map(|waveform| (etag.clone(), waveform))
                .map_err(|error| error.to_string())
        }
        _ => Err("incomplete ok item".into()),
    }
}

fn resolve_i8(
    i8_store: &WaveformsDb,
    ids: &[i64],
    reply: BatchReply,
    stored: &HashMap<i64, WaveformsDbRow>,
) -> ListWaveformBatch {
    let previous = |id: i64| stored.get(&id).map(|row| row.peaks.to_list_waveform());
    let mut written = Vec::new();
    let mut answered = HashSet::new();
    let mut items = Vec::new();
    for item in reply.items {
        if !ids.contains(&item.id) || !answered.insert(item.id) {
            continue;
        }
        let id = item.id;
        let state = match item.status {
            BatchStatus::Ok => match parse_i8_item(&item) {
                Ok((etag, peaks)) => {
                    let waveform = peaks.to_list_waveform();
                    let row = WaveformsDbRow {
                        file_id: id,
                        etag,
                        encoding: LIST_WAVEFORM_ENCODING_I8.into(),
                        src_mtime: 0,
                        src_size: 0,
                        peaks,
                    };
                    written.push(row);
                    ListWaveformState::Ready { waveform }
                }
                Err(message) => {
                    tracing::warn!(id, %message, "malformed i8 list Waveform item");
                    ListWaveformState::Failed { message }
                }
            },
            // Entity tags are compared in full, suffix included.
            BatchStatus::NotModified => match stored.get(&id) {
                Some(row) if item.etag.as_ref().is_none_or(|etag| *etag == row.etag) => {
                    ListWaveformState::Ready {
                        waveform: row.peaks.to_list_waveform(),
                    }
                }
                _ => ListWaveformState::Pending { previous: None },
            },
            BatchStatus::Pending | BatchStatus::Unknown => ListWaveformState::Pending {
                previous: previous(id),
            },
            BatchStatus::Missing => ListWaveformState::Missing,
            BatchStatus::RootOffline => ListWaveformState::RootOffline {
                root: item.root.unwrap_or_default(),
                previous: previous(id),
            },
            BatchStatus::Error => ListWaveformState::Failed {
                message: item
                    .message
                    .or(item.error_code)
                    .unwrap_or_else(|| "error".into()),
            },
        };
        items.push(ListWaveformItem { id, state });
    }
    if !written.is_empty()
        && let Err(error) = i8_store.upsert_many(&written)
    {
        tracing::warn!(%error, "waveforms.db write failed");
    }
    ListWaveformBatch {
        items,
        retry_after: reply
            .retry_after
            .map(|seconds| Duration::from_secs(seconds.clamp(1, 30))),
        deferred: None,
    }
}

fn usable(capabilities: &Capabilities) -> Option<ListWaveformsCapability> {
    let mut capability = capabilities.list_waveforms.clone().filter(|capability| {
        capabilities.supported
            && capabilities.feature_enabled("list_waveforms")
            && capability.usable()
    })?;
    // Bundle import relies on the strong bundle ETag; degrade to incremental lookups.
    capability.bundle &= capabilities.feature_enabled("waveform_bundle_strong_etag");
    Some(capability)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_server::{TestRequest, TestResponse, TestServer};
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    const LIBRARY: &str = "lib-lw";
    const CAPABILITIES: &str = r#"{"manage_mode":"all","can_manage":true,"root_management":true,
        "list_waveforms":{"version":1,"buckets":[128],"max_ids":200,"encoding":"i16le-minmax-base64"},
        "pregenerate_list_waveforms":false}"#;

    fn waveform(values: &[i16]) -> ListWaveform {
        ListWaveform::new(1, values.to_vec()).unwrap()
    }

    fn base64(bytes: &[u8]) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut text = String::new();
        for chunk in bytes.chunks(3) {
            let word = (u32::from(chunk[0]) << 16)
                | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
                | u32::from(*chunk.get(2).unwrap_or(&0));
            for index in 0..4 {
                text.push(if index <= chunk.len() {
                    TABLE[(word >> (18 - 6 * index)) as usize & 63] as char
                } else {
                    '='
                });
            }
        }
        text
    }

    fn body_json(request: &TestRequest) -> serde_json::Value {
        serde_json::from_slice(&request.body).unwrap()
    }

    fn service(server: &TestServer, directory: &Path) -> RemoteListWaveforms {
        RemoteListWaveforms::new(
            RemoteClient::new(&server.url, "").unwrap(),
            LIBRARY,
            ListWaveformStore::new(directory.join("list"), DEFAULT_LIST_WAVEFORM_CACHE_BYTES),
        )
    }

    fn stored(etag: &str, values: &[i16]) -> StoredListWaveform {
        StoredListWaveform {
            etag: etag.into(),
            waveform: waveform(values),
        }
    }

    #[test]
    fn every_item_status_maps_to_a_state_and_ok_items_are_cached_by_etag() {
        let directory = tempfile::tempdir().unwrap();
        let posts = Arc::new(Mutex::new(Vec::new()));
        let server = {
            let posts = posts.clone();
            TestServer::start(move |request| match request.path.as_str() {
                "/api/v1/capabilities" => TestResponse::json(200, CAPABILITIES),
                "/api/v1/waveforms" => {
                    assert_eq!(request.method, "POST");
                    assert_eq!(request.header("accept-encoding"), Some("zstd"));
                    assert_eq!(request.header("content-type"), Some("application/json"));
                    let body = body_json(request);
                    posts.lock().unwrap().push(body.clone());
                    let ok = base64(&waveform(&[-100, 200, 0, 0]).to_interleaved_le());
                    let reply = serde_json::json!({
                        "library_id": LIBRARY, "buckets": 128, "retry_after": 4,
                        "items": [
                            {"id": 1, "status": "ok", "etag": "W/\"e1-l1\"", "n": 2, "peaks": ok},
                            {"id": 2, "status": "not_modified", "etag": "W/\"e2-l1\""},
                            {"id": 3, "status": "pending"},
                            {"id": 4, "status": "root_offline", "root": "SFX_Main"},
                            {"id": 5, "status": "missing"},
                            {"id": 6, "status": "error", "error_code": "AUDIO", "message": "Cannot decode"},
                            {"id": 7, "status": "ok", "etag": "W/\"e7-l1\"", "n": 0, "peaks": ""},
                            {"id": 99, "status": "ok", "etag": "x", "n": 0, "peaks": ""}
                        ]
                    });
                    let json = serde_json::to_vec(&reply).unwrap();
                    TestResponse::bytes(200, zstd::encode_all(json.as_slice(), 3).unwrap())
                        .with_header("Content-Encoding", "zstd")
                }
                other => panic!("unexpected {other}"),
            })
        };
        let service = service(&server, directory.path());
        service
            .store()
            .put_many(vec![
                (2, stored("W/\"e2-l1\"", &[-1, 1])),
                (3, stored("W/\"old3\"", &[-3, 3])),
                (4, stored("W/\"old4\"", &[-4, 4])),
            ])
            .unwrap();
        assert_eq!(service.source(), ListWaveformSource::Server);
        let cancel = AtomicBool::new(false);
        let batch = service.lookup(&[1, 2, 2, 3, 4, 5, 6, 7], &cancel).unwrap();
        let body = posts.lock().unwrap()[0].clone();
        assert_eq!(body["ids"], serde_json::json!([1, 2, 3, 4, 5, 6, 7]));
        assert_eq!(body["buckets"], 128);
        assert_eq!(
            body["known"],
            serde_json::json!({"2": "W/\"e2-l1\"", "3": "W/\"old3\"", "4": "W/\"old4\""})
        );
        assert_eq!(batch.retry_after, Some(Duration::from_secs(4)));
        assert_eq!(batch.deferred, None);
        let states: Vec<_> = batch
            .items
            .iter()
            .map(|item| (item.id, &item.state))
            .collect();
        assert_eq!(
            states,
            vec![
                (
                    1,
                    &ListWaveformState::Ready {
                        waveform: waveform(&[-100, 200, 0, 0])
                    }
                ),
                (
                    2,
                    &ListWaveformState::Ready {
                        waveform: waveform(&[-1, 1])
                    }
                ),
                (
                    3,
                    &ListWaveformState::Pending {
                        previous: Some(waveform(&[-3, 3]))
                    }
                ),
                (
                    4,
                    &ListWaveformState::RootOffline {
                        root: "SFX_Main".into(),
                        previous: Some(waveform(&[-4, 4]))
                    }
                ),
                (5, &ListWaveformState::Missing),
                (
                    6,
                    &ListWaveformState::Failed {
                        message: "Cannot decode".into()
                    }
                ),
                (
                    7,
                    &ListWaveformState::Ready {
                        waveform: waveform(&[])
                    }
                ),
            ],
            "unrequested id 99 is ignored"
        );
        // Stored by entity tag: a fresh store over the same directory sees them.
        let reopened = ListWaveformStore::new(
            directory.path().join("list"),
            DEFAULT_LIST_WAVEFORM_CACHE_BYTES,
        );
        assert_eq!(
            reopened.get(1),
            Some(stored("W/\"e1-l1\"", &[-100, 200, 0, 0]))
        );
        assert_eq!(reopened.get(7), Some(stored("W/\"e7-l1\"", &[])));
        assert_eq!(reopened.get(99), None);
        // The next lookup sends the new tag in `known`.
        service.lookup(&[1], &cancel).unwrap();
        assert_eq!(
            posts.lock().unwrap()[1]["known"],
            serde_json::json!({"1": "W/\"e1-l1\""})
        );
    }

    #[test]
    fn busy_defers_by_retry_after_and_another_library_is_discarded() {
        let directory = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let server = {
            let calls = calls.clone();
            TestServer::start(move |request| match request.path.as_str() {
                "/api/v1/capabilities" => TestResponse::json(200, CAPABILITIES),
                "/api/v1/waveforms" => match calls.fetch_add(1, Ordering::SeqCst) {
                    0 => TestResponse::json(
                        503,
                        r#"{"error_code":"BUSY","message":"Too many list Waveform requests"}"#,
                    )
                    .with_header("Retry-After", "3"),
                    1 => TestResponse::json(503, r#"{"error_code":"BUSY","message":"busy"}"#),
                    _ => TestResponse::json(
                        200,
                        r#"{"library_id":"replacement","buckets":128,"retry_after":null,
                            "items":[{"id":1,"status":"ok","etag":"e","n":0,"peaks":""}]}"#,
                    ),
                },
                other => panic!("unexpected {other}"),
            })
        };
        let service = service(&server, directory.path());
        let cancel = AtomicBool::new(false);
        let busy = service.lookup(&[1, 2], &cancel).unwrap();
        assert!(busy.items.is_empty());
        assert_eq!(busy.deferred, Some(Duration::from_secs(3)));
        let busy = service.lookup(&[1, 2], &cancel).unwrap();
        assert_eq!(busy.deferred, Some(BUSY_DEFAULT));
        assert!(matches!(
            service.lookup(&[1], &cancel),
            Err(RemoteError::LibraryChanged(_))
        ));
        assert_eq!(
            service.store().get(1),
            None,
            "a foreign reply is not stored"
        );
        assert_eq!(service.store().bytes(), 0);
    }

    #[test]
    fn without_a_usable_capability_nothing_is_posted() {
        let directory = tempfile::tempdir().unwrap();
        for capabilities in [
            None,
            Some(r#"{"manage_mode":"all"}"#),
            Some(
                r#"{"list_waveforms":{"version":2,"buckets":[128],"max_ids":200,"encoding":"i16le-minmax-base64"}}"#,
            ),
            Some(
                r#"{"list_waveforms":{"version":1,"buckets":[256],"max_ids":200,"encoding":"i16le-minmax-base64"}}"#,
            ),
            Some(
                r#"{"list_waveforms":{"version":1,"buckets":[128],"max_ids":200,"encoding":"f32"}}"#,
            ),
        ] {
            let posts = Arc::new(AtomicUsize::new(0));
            let server = {
                let posts = posts.clone();
                TestServer::start(move |request| match (request.path.as_str(), capabilities) {
                    ("/api/v1/capabilities", Some(body)) => TestResponse::json(200, body),
                    ("/api/v1/capabilities", None) => TestResponse::json(
                        404,
                        r#"{"error_code":"NOT_FOUND","message":"No such endpoint"}"#,
                    ),
                    _ => {
                        posts.fetch_add(1, Ordering::SeqCst);
                        TestResponse::json(500, "{}")
                    }
                })
            };
            let service = service(&server, directory.path());
            assert_eq!(
                service.source(),
                ListWaveformSource::Unavailable,
                "{capabilities:?}"
            );
            assert!(matches!(
                service.lookup(&[1], &AtomicBool::new(false)),
                Err(RemoteError::Unsupported(_))
            ));
            assert_eq!(
                posts.load(Ordering::SeqCst),
                0,
                "no per-item or batch fallback"
            );
        }
        // Known capabilities skip the probe.
        let server = TestServer::start(|request| panic!("unexpected {}", request.path));
        let caps: Capabilities = serde_json::from_str(CAPABILITIES).unwrap();
        assert!(caps.list_waveforms_usable());
        let known = service(&server, directory.path()).with_capabilities(&caps);
        assert_eq!(known.source(), ListWaveformSource::Server);
        let none = service(&server, directory.path()).with_capabilities(&Capabilities::unknown());
        assert_eq!(none.source(), ListWaveformSource::Unavailable);
    }

    #[test]
    fn the_disk_cache_evicts_least_recently_used_shards_and_drops_damaged_ones() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("list");
        let entry = |id: i64| (id, stored(&format!("W/\"{id}\""), &[-7, 7].repeat(128)));
        // One 128-column entry is about 530 bytes (shard header and checksum 20 more):
        // shards 0 (two entries), 1 and 2 fit in 2300 bytes, a fourth shard does not.
        let store = ListWaveformStore::new(root.clone(), 2300);
        store.put_many(vec![entry(1), entry(2)]).unwrap();
        assert_eq!(store.get(1).unwrap().etag, "W/\"1\"");
        let pause = || std::thread::sleep(Duration::from_millis(5));
        pause();
        store.put_many(vec![entry(1024)]).unwrap();
        pause();
        store.put_many(vec![entry(2048)]).unwrap();
        // Reading shard 0 makes shard 1 the least recently used one.
        pause();
        assert!(store.get(2).is_some());
        pause();
        store.put_many(vec![entry(3072)]).unwrap();
        assert!(store.bytes() <= 2300, "{}", store.bytes());
        assert!(
            store.get(1024).is_none(),
            "the least recently used shard is evicted"
        );
        assert!(store.get(1).is_some() && store.get(3072).is_some());
        let mut names: Vec<String> = std::fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert!(names.iter().all(|name| name.ends_with(".lws")), "{names:?}");
        assert!(!names.contains(&"1.lws".to_owned()));
        // A damaged shard and a leftover temporary file are dropped on the next start.
        std::fs::write(root.join("3.lws"), b"ALXLWS1\ngarbage").unwrap();
        std::fs::write(root.join("0.tmp1-1"), b"partial").unwrap();
        let reopened = ListWaveformStore::new(root.clone(), 2300);
        assert!(reopened.get(3072).is_none());
        assert!(reopened.get(1).is_some());
        assert!(!root.join("3.lws").exists() && !root.join("0.tmp1-1").exists());
        // Shards round trip exactly.
        let mut shard = Shard::new();
        shard.insert(5, stored("tag", &[1, -1, 32767, -32767]));
        shard.insert(6, stored("", &[]));
        let bytes = encode_shard(&shard);
        assert_eq!(decode_shard(&bytes), Some(shard));
        let mut flipped = bytes.clone();
        flipped[12] ^= 1;
        assert_eq!(decode_shard(&flipped), None);
        assert_eq!(
            ListWaveformStore::library_directory(Path::new("c"), LIBRARY),
            Path::new("c")
                .join("waveforms")
                .join("server")
                .join(format!("{:016x}", crate::util::fnv64(LIBRARY)))
                .join("list")
        );
    }

    #[test]
    fn settings_and_status_use_the_settings_endpoints() {
        let patches = Arc::new(Mutex::new(Vec::new()));
        let server = {
            let patches = patches.clone();
            TestServer::start(move |request| {
                if let Some(response) = crate::test_server::marker_handshake(&request.path) {
                    return response;
                }
                match (request.method.as_str(), request.path.as_str()) {
                    ("GET", "/api/v1/library") => TestResponse::json(
                        200,
                        r#"{"library_id":"lib-lw","schema_version":5,"revision":1}"#,
                    ),
                    ("GET", "/api/v1/settings") => TestResponse::json(
                        200,
                        r#"{"pregenerate_list_waveforms":false,"pregenerate_list_waveforms_source":"default","can_change":true}"#,
                    ),
                    ("PATCH", "/api/v1/settings") => {
                        patches.lock().unwrap().push(body_json(request));
                        TestResponse::json(
                            200,
                            r#"{"pregenerate_list_waveforms":true,"pregenerate_list_waveforms_source":"admin","can_change":true}"#,
                        )
                    }
                    ("GET", "/api/v1/waveforms/status") => TestResponse::json(
                        200,
                        r#"{"enabled":true,"state":"yielding","yield_reason":"audio","total":10,"cached":4,
                        "remaining":6,"queued_on_demand":0,"failed":1,"skipped_offline":0,"rate":10,
                        "cache_bytes":2048,"cache_limit_bytes":671088640,"over_budget":false,
                        "pass_started_at":1700000000,"pass_finished_at":null}"#,
                    ),
                    (method, path) => panic!("unexpected {method} {path}"),
                }
            })
        };
        let client = RemoteClient::new(&server.url, "").unwrap();
        let settings = client.server_settings().unwrap();
        assert!(!settings.pregenerate_list_waveforms && settings.can_change);
        let changed = client.set_pregenerate_list_waveforms(true).unwrap();
        assert!(changed.pregenerate_list_waveforms);
        assert_eq!(changed.pregenerate_list_waveforms_source, "admin");
        assert_eq!(
            patches.lock().unwrap()[0],
            serde_json::json!({"pregenerate_list_waveforms": true})
        );
        let status = client.list_waveform_status().unwrap();
        assert_eq!(
            (status.state.as_str(), status.cached, status.remaining),
            ("yielding", 4, Some(6))
        );
        assert_eq!(status.yield_reason.as_deref(), Some("audio"));
    }
}

#[cfg(test)]
mod i8_tests {
    use super::*;
    use crate::test_server::{TestResponse, TestServer};
    use std::sync::Arc;

    const LIBRARY: &str = "lib-i8";
    const CAPS_I16: &str = r#"{"manage_mode":"all","list_waveforms":{"version":1,"buckets":[128],"max_ids":200,"encoding":"i16le-minmax-base64"}}"#;
    const CAPS_I8: &str = r#"{"manage_mode":"all","list_waveforms":{"version":1,"buckets":[128],"max_ids":200,"encoding":"i16le-minmax-base64","encodings":["i16le-minmax-base64","i8-minmax-base64"]}}"#;

    fn b64(bytes: &[u8]) -> String {
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let w = (u32::from(chunk[0]) << 16)
                | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
                | u32::from(*chunk.get(2).unwrap_or(&0));
            for i in 0..4 {
                out.push(if i <= chunk.len() {
                    T[(w >> (18 - 6 * i)) as usize & 63] as char
                } else {
                    '='
                });
            }
        }
        out
    }

    type Posts = Arc<Mutex<Vec<serde_json::Value>>>;

    /// A server answering `/capabilities` with `caps` and every POST with `reply(body)`.
    fn server(
        caps: &'static str,
        reply: impl Fn(&serde_json::Value) -> TestResponse + Send + Sync + 'static,
    ) -> (TestServer, Posts) {
        let posts: Posts = Arc::new(Mutex::new(Vec::new()));
        let seen = posts.clone();
        let server = TestServer::start(move |request| match request.path.as_str() {
            "/api/v1/capabilities" => TestResponse::json(200, caps),
            "/api/v1/waveforms" => {
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                seen.lock().unwrap().push(body.clone());
                reply(&body)
            }
            other => panic!("unexpected {other}"),
        });
        (server, posts)
    }

    fn service(server: &TestServer, directory: &Path, with_store: bool) -> RemoteListWaveforms {
        let service = RemoteListWaveforms::new(
            RemoteClient::new(&server.url, "").unwrap(),
            LIBRARY,
            ListWaveformStore::new(directory.join("list"), DEFAULT_LIST_WAVEFORM_CACHE_BYTES),
        );
        if with_store {
            service.with_i8_store(WaveformsDb::open(&directory.join("waveforms.db")).unwrap())
        } else {
            service
        }
    }

    fn i8_reply(items: serde_json::Value) -> TestResponse {
        TestResponse::json(
            200,
            &serde_json::json!({"library_id": LIBRARY, "buckets": 128,
                "encoding": "i8-minmax-base64", "items": items})
            .to_string(),
        )
    }

    fn i16_reply() -> TestResponse {
        TestResponse::json(
            200,
            &serde_json::json!({"library_id": LIBRARY, "buckets": 128,
                "items": [{"id": 1, "status": "missing"}]})
            .to_string(),
        )
    }

    #[test]
    fn encoding_is_sent_only_when_the_capability_lists_it_and_a_store_exists() {
        let cancel = AtomicBool::new(false);
        for (caps, with_store, expect_i8) in [
            (CAPS_I16, true, false),
            (CAPS_I8, false, false),
            (CAPS_I8, true, true),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let (server, posts) = server(caps, move |body| {
                if body.get("encoding").is_some() {
                    i8_reply(serde_json::json!([{"id": 1, "status": "missing"}]))
                } else {
                    i16_reply()
                }
            });
            let service = service(&server, directory.path(), with_store);
            assert_eq!(service.requests_i8(), expect_i8);
            let batch = service.lookup(&[1], &cancel).unwrap();
            assert_eq!(batch.items[0].state, ListWaveformState::Missing);
            let body = posts.lock().unwrap()[0].clone();
            assert_eq!(
                body.get("encoding").and_then(|value| value.as_str()),
                expect_i8.then_some(LIST_WAVEFORM_ENCODING_I8),
                "{caps} {with_store}"
            );
        }
    }

    #[test]
    fn ok_items_are_stored_in_waveforms_db_and_etags_compare_in_full() {
        let directory = tempfile::tempdir().unwrap();
        let peaks = b64(&[0x80, 0x7f, 0xf6, 0x0a]); // (-128 → -127, 127), (-10, 10)
        let (server, posts) = server(CAPS_I8, move |body| {
            if body.get("known").is_none() {
                i8_reply(serde_json::json!([
                    {"id": 1, "status": "ok", "etag": "W/\"e1-l1i8\"", "n": 2, "peaks": peaks},
                    {"id": 2, "status": "ok", "etag": "W/\"e2-l1\"", "n": 0, "peaks": ""},
                    {"id": 3, "status": "ok", "etag": "W/\"e3-l1i8\"", "n": 1, "peaks": b64(&[5, 4])},
                    {"id": 4, "status": "ok", "etag": "W/\"e4-l1i8\"", "n": 2, "peaks": b64(&[1, 2])}
                ]))
            } else {
                i8_reply(serde_json::json!([
                    {"id": 1, "status": "not_modified", "etag": "W/\"e1-l1i8\""},
                    {"id": 2, "status": "not_modified", "etag": "W/\"e1-l1\""}
                ]))
            }
        });
        let cancel = AtomicBool::new(false);
        {
            let service = service(&server, directory.path(), true);
            let batch = service.lookup(&[1, 2, 3, 4], &cancel).unwrap();
            assert_eq!(
                batch.items[0].state,
                ListWaveformState::Ready {
                    waveform: ListWaveform::new(1, vec![-32767, 32767, -2580, 2580]).unwrap()
                }
            );
            // Wrong suffix, min > max and a length mismatch are rejected per item.
            for item in &batch.items[1..] {
                assert!(
                    matches!(item.state, ListWaveformState::Failed { .. }),
                    "{item:?}"
                );
            }
            assert!(service.store().get(1).is_none(), "int16 cache untouched");
        }
        // Reopen: the row is still there, with its encoding; `known` carries the full tag.
        let reopened = WaveformsDb::open(&directory.path().join("waveforms.db")).unwrap();
        let row = reopened.get(1).unwrap().unwrap();
        assert_eq!(row.etag, "W/\"e1-l1i8\"");
        assert_eq!(row.encoding, LIST_WAVEFORM_ENCODING_I8);
        assert_eq!(row.peaks.values, vec![-127, 127, -10, 10]);
        assert!(reopened.get(2).unwrap().is_none());
        let service = service(&server, directory.path(), true);
        reopened
            .upsert(&WaveformsDbRow {
                file_id: 2,
                etag: "W/\"e1-l1i8\"".into(),
                ..row.clone()
            })
            .unwrap();
        let batch = service.lookup(&[1, 2], &cancel).unwrap();
        assert_eq!(
            posts.lock().unwrap()[1]["known"],
            serde_json::json!({"1": "W/\"e1-l1i8\"", "2": "W/\"e1-l1i8\""})
        );
        assert!(matches!(
            batch.items[0].state,
            ListWaveformState::Ready { .. }
        ));
        // `W/"e1-l1"` differs from the stored `W/"e1-l1i8"` only by its suffix: not a match.
        assert_eq!(
            batch.items[1].state,
            ListWaveformState::Pending { previous: None }
        );
    }

    #[test]
    fn encoding_mismatch_and_mixed_encodings_cache_nothing_and_back_off() {
        let cancel = AtomicBool::new(false);
        let ok = r#"{"id": 1, "status": "ok", "etag": "W/\"n-l1i8\"", "n": 1, "peaks": "AAA="}"#;
        for reply in [
            format!(r#"{{"library_id": "{LIBRARY}", "items": [{ok}]}}"#),
            format!(
                r#"{{"library_id": "{LIBRARY}", "encoding": "i16le-minmax-base64", "items": [{ok}]}}"#
            ),
            format!(
                r#"{{"library_id": "{LIBRARY}", "encoding": "i8-minmax-base64", "items": [{ok},
                {{"id": 2, "status": "missing", "encoding": "i16le-minmax-base64"}}]}}"#
            ),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let text = reply.clone();
            let (server, _) = server(CAPS_I8, move |_| TestResponse::json(200, &text));
            let service = service(&server, directory.path(), true);
            let old = ListWaveformI8::new(vec![-3, 3]).unwrap();
            service
                .i8_store
                .as_ref()
                .unwrap()
                .upsert(&WaveformsDbRow {
                    file_id: 2,
                    etag: "W/\"old-l1i8\"".into(),
                    encoding: LIST_WAVEFORM_ENCODING_I8.into(),
                    src_mtime: 0,
                    src_size: 0,
                    peaks: old.clone(),
                })
                .unwrap();
            let batch = service.lookup(&[1, 2], &cancel).unwrap();
            assert_eq!(batch.retry_after, Some(ECHO_MISMATCH_RETRY), "{reply}");
            assert_eq!(
                batch.items[0].state,
                ListWaveformState::Pending { previous: None }
            );
            assert_eq!(
                batch.items[1].state,
                ListWaveformState::Pending {
                    previous: Some(old.to_list_waveform())
                }
            );
            let store = service.i8_store.as_ref().unwrap();
            assert!(store.get(1).unwrap().is_none(), "nothing cached: {reply}");
            assert_eq!(store.get(2).unwrap().unwrap().etag, "W/\"old-l1i8\"");
        }
    }

    #[test]
    fn unsupported_encoding_sub_code_is_recognised() {
        assert_eq!(
            unsupported_encoding(
                r#"{"error_code":"VALIDATION","sub_code":"UNSUPPORTED_ENCODING","message":"unsupported list waveform encoding"}"#
            )
            .as_deref(),
            Some("unsupported list waveform encoding")
        );
        assert!(unsupported_encoding(r#"{"error_code":"VALIDATION","message":"x"}"#).is_none());
        assert!(unsupported_encoding("not json").is_none());
    }

    #[test]
    fn unsupported_encoding_falls_back_to_int16() {
        let directory = tempfile::tempdir().unwrap();
        let (server, posts) = server(CAPS_I8, |body| {
            if body.get("encoding").is_some() {
                TestResponse::json(
                    400,
                    r#"{"error_code":"VALIDATION","sub_code":"UNSUPPORTED_ENCODING","message":"unsupported list waveform encoding"}"#,
                )
            } else {
                i16_reply()
            }
        });
        let service = service(&server, directory.path(), true);
        let cancel = AtomicBool::new(false);
        let batch = service.lookup(&[1], &cancel).unwrap();
        assert_eq!(batch.items[0].state, ListWaveformState::Missing);
        assert!(!service.requests_i8());
        service.lookup(&[1], &cancel).unwrap();
        let posts = posts.lock().unwrap();
        assert_eq!(posts.len(), 3);
        assert!(posts[0].get("encoding").is_some());
        assert!(posts[1].get("encoding").is_none());
        assert!(posts[2].get("encoding").is_none());
    }

    #[test]
    fn pending_previous_follows_the_requested_encoding_and_bad_lengths_are_rejected() {
        let cancel = AtomicBool::new(false);
        let i16_copy = ListWaveform::new(1, vec![-1000, 1000]).unwrap();
        let i8_copy = ListWaveformI8::new(vec![-64, 64]).unwrap();
        for (caps, with_store, expected) in [
            (CAPS_I8, true, i8_copy.to_list_waveform()),
            (CAPS_I16, true, i16_copy.clone()),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let (server, _) = server(caps, move |body| {
                let items = serde_json::json!([
                    {"id": 1, "status": "pending"},
                    // n = 2 needs 4 bytes (8 base64 characters); 2 bytes is a length mismatch.
                    {"id": 2, "status": "ok", "etag": "W/\"x-l1i8\"", "n": 2, "peaks": "AAA="}
                ]);
                if body.get("encoding").is_some() {
                    i8_reply(items)
                } else {
                    TestResponse::json(
                        200,
                        &serde_json::json!({"library_id": LIBRARY, "buckets": 128,
                            "items": [{"id": 1, "status": "pending"},
                                {"id": 2, "status": "ok", "etag": "W/\"x-l1\"", "n": 2, "peaks": "AAAAAA=="}]})
                        .to_string(),
                    )
                }
            });
            let service = service(&server, directory.path(), with_store);
            // Both caches hold an older copy of id 1; only the requested encoding's is used.
            service
                .store()
                .put_many(vec![(
                    1,
                    StoredListWaveform {
                        etag: "W/\"old-l1\"".into(),
                        waveform: i16_copy.clone(),
                    },
                )])
                .unwrap();
            service
                .i8_store
                .as_ref()
                .unwrap()
                .upsert(&WaveformsDbRow {
                    file_id: 1,
                    etag: "W/\"old-l1i8\"".into(),
                    encoding: LIST_WAVEFORM_ENCODING_I8.into(),
                    src_mtime: 0,
                    src_size: 0,
                    peaks: i8_copy.clone(),
                })
                .unwrap();
            let batch = service.lookup(&[1, 2], &cancel).unwrap();
            assert_eq!(
                batch.items[0].state,
                ListWaveformState::Pending {
                    previous: Some(expected)
                },
                "{caps}"
            );
            assert!(
                matches!(batch.items[1].state, ListWaveformState::Failed { .. }),
                "{caps}: {:?}",
                batch.items[1]
            );
            assert!(service.store().get(2).is_none());
            assert!(service.i8_store.as_ref().unwrap().get(2).unwrap().is_none());
        }
    }

    #[test]
    fn store_path_sits_beside_the_list_cache() {
        let root = Path::new("cache");
        let path = RemoteListWaveforms::i8_store_path(root, LIBRARY);
        assert_eq!(path.file_name().unwrap(), "waveforms.db");
        assert_eq!(
            path.parent(),
            ListWaveformStore::library_directory(root, LIBRARY).parent()
        );
    }
}

#[cfg(test)]
mod bundle_tests {
    use super::*;
    use crate::test_server::{TestResponse, TestServer};
    use std::sync::Arc;

    const LIBRARY: &str = "lib-bundle";
    const CAPS: &str = r#"{"manage_mode":"all","list_waveforms":{"version":1,"buckets":[128],"max_ids":200,"encoding":"i16le-minmax-base64","encodings":["i16le-minmax-base64","i8-minmax-base64"],"bundle":true}}"#;
    const CAPS_NO_BUNDLE: &str = r#"{"manage_mode":"all","list_waveforms":{"version":1,"buckets":[128],"max_ids":200,"encoding":"i16le-minmax-base64","encodings":["i16le-minmax-base64","i8-minmax-base64"]}}"#;

    /// Bytes of a server-style bundle; `tweak` is SQL run before closing.
    fn bundle_bytes(library: &str, tweak: &str) -> Vec<u8> {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("b.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE list_waveforms (file_id INTEGER PRIMARY KEY, etag TEXT NOT NULL,
               src_mtime INTEGER NOT NULL, src_size INTEGER NOT NULL, encoding TEXT NOT NULL,
               n INTEGER NOT NULL, blob BLOB NOT NULL);
             CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO list_waveforms VALUES(1,'W/\"b1-l1i8\"',0,0,'i8-minmax-base64',1,
               CAST(X'F60A' || zeroblob(254) AS BLOB));
             INSERT INTO meta VALUES('library_id','{library}'),('revision','3'),
               ('encoding','i8-minmax-base64'),('schema_version','1'),('generated_at','1');
             {tweak}"
        ))
        .unwrap();
        drop(conn);
        std::fs::read(path).unwrap()
    }

    fn etag_of(bytes: &[u8]) -> String {
        bundle_etag(ring::digest::digest(&ring::digest::SHA256, bytes).as_ref())
    }

    struct Reply {
        status: u16,
        bytes: Vec<u8>,
        etag: Option<String>,
        library: String,
        retry_after: Option<&'static str>,
    }

    fn ok_reply(bytes: Vec<u8>) -> Reply {
        Reply {
            status: 200,
            etag: Some(etag_of(&bytes)),
            bytes,
            library: LIBRARY.into(),
            retry_after: None,
        }
    }

    type Posts = Arc<Mutex<Vec<serde_json::Value>>>;
    type Check = fn(&RemoteError) -> bool;

    fn server(caps: &'static str, reply: Reply) -> (TestServer, Posts) {
        let posts: Posts = Arc::new(Mutex::new(Vec::new()));
        let seen = posts.clone();
        let server = TestServer::start(move |request| {
            if request.path.starts_with("/api/v1/waveforms/bundle") {
                assert!(request.path.ends_with("?encoding=i8-minmax-base64"));
                if reply.status != 200 {
                    let mut response = TestResponse::json(
                        reply.status,
                        r#"{"error_code":"BUSY","message":"one bundle at a time"}"#,
                    );
                    if let Some(seconds) = reply.retry_after {
                        response = response.with_header("Retry-After", seconds);
                    }
                    return response;
                }
                let mut response = TestResponse::bytes(200, reply.bytes.clone())
                    .with_header("Content-Type", "application/vnd.sqlite3")
                    .with_header("X-Library-Id", &reply.library)
                    .with_header("X-Library-Revision", "3");
                if let Some(etag) = &reply.etag {
                    response = response.with_header("ETag", etag);
                }
                return response;
            }
            match request.path.as_str() {
                "/api/v1/capabilities" => TestResponse::json(200, caps),
                "/api/v1/waveforms" => {
                    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                    seen.lock().unwrap().push(body);
                    TestResponse::json(
                        200,
                        &serde_json::json!({"library_id": LIBRARY, "buckets": 128,
                            "encoding": "i8-minmax-base64",
                            "items": [{"id": 1, "status": "not_modified", "etag": "W/\"b1-l1i8\""}]})
                        .to_string(),
                    )
                }
                other => panic!("unexpected {other}"),
            }
        });
        (server, posts)
    }

    fn service(server: &TestServer, directory: &Path) -> RemoteListWaveforms {
        RemoteListWaveforms::new(
            RemoteClient::new(&server.url, "").unwrap(),
            LIBRARY,
            ListWaveformStore::new(directory.join("list"), DEFAULT_LIST_WAVEFORM_CACHE_BYTES),
        )
        .with_i8_store(WaveformsDb::open(&directory.join("waveforms.db")).unwrap())
    }

    fn seed(service: &RemoteListWaveforms) {
        service
            .i8_store
            .as_ref()
            .unwrap()
            .upsert(&WaveformsDbRow {
                file_id: 9,
                etag: "W/\"keep-l1i8\"".into(),
                encoding: LIST_WAVEFORM_ENCODING_I8.into(),
                src_mtime: 0,
                src_size: 0,
                peaks: ListWaveformI8::new(vec![-1, 1]).unwrap(),
            })
            .unwrap();
    }

    fn leftovers(directory: &Path) -> Vec<String> {
        std::fs::read_dir(directory)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".bundle-"))
            .collect()
    }

    #[test]
    fn etag_and_backoff_helpers() {
        let digest = ring::digest::digest(&ring::digest::SHA256, b"abc");
        assert_eq!(
            bundle_etag(digest.as_ref()),
            "\"wfb1-ba7816bf8f01cfea414140de5dae2223\""
        );
        let secs = |attempt, retry| bundle_backoff(attempt, retry).as_secs();
        assert_eq!(secs(0, Some(5)), 5);
        assert_eq!(secs(1, Some(5)), 10);
        assert_eq!(secs(2, None), 20);
        assert_eq!(secs(0, Some(60)), 60);
        assert_eq!(secs(6, Some(5)), 300);
        assert_eq!(secs(40, None), 300);
        assert_eq!(secs(0, Some(10_000)), 300);
    }

    #[test]
    fn a_verified_bundle_replaces_the_store_and_lookups_resume_by_etag() {
        let directory = tempfile::tempdir().unwrap();
        let (server, posts) = server(CAPS, ok_reply(bundle_bytes(LIBRARY, "")));
        let service = service(&server, directory.path());
        seed(&service);
        assert!(service.offers_bundle());
        let cancel = AtomicBool::new(false);
        assert_eq!(
            service.import_bundle(&cancel).unwrap(),
            BundleImport::Installed { rows: 1 }
        );
        let store = service.i8_store.as_ref().unwrap();
        assert!(
            store.get(9).unwrap().is_some(),
            "local rows kept over the bundle"
        );
        assert_eq!(store.get(1).unwrap().unwrap().peaks.values, vec![-10, 10]);
        assert!(leftovers(directory.path()).is_empty());
        let batch = service.lookup(&[1], &cancel).unwrap();
        assert_eq!(
            posts.lock().unwrap()[0]["known"],
            serde_json::json!({"1": "W/\"b1-l1i8\""})
        );
        assert_eq!(
            batch.items[0].state,
            ListWaveformState::Ready {
                waveform: ListWaveformI8::new(vec![-10, 10])
                    .unwrap()
                    .to_list_waveform()
            }
        );
    }

    #[test]
    fn a_corrupt_bundle_preserves_both_surviving_store_handles() {
        let directory = tempfile::tempdir().unwrap();
        // The response checksum and library header are valid, but SQLite bytes
        // are corrupt: rejection must happen before any live store is replaced.
        let (server, _) = server(CAPS, ok_reply(b"not a SQLite database".to_vec()));
        let old = service(&server, directory.path());
        seed(&old);
        let current = service(&server, directory.path());
        assert!(matches!(
            current.import_bundle(&AtomicBool::new(false)),
            Err(RemoteError::Protocol(_))
        ));
        for service in [&old, &current] {
            let store = service.i8_store().unwrap();
            assert_eq!(store.len().unwrap(), 1);
            assert_eq!(store.get(9).unwrap().unwrap().etag, "W/\"keep-l1i8\"");
            assert!(store.get(1).unwrap().is_none());
        }
        assert!(leftovers(directory.path()).is_empty());
    }

    #[test]
    fn every_rejected_bundle_keeps_the_store_and_leaves_no_temporary_file() {
        let good = bundle_bytes(LIBRARY, "");
        let mut truncated_etag = ok_reply(good[..good.len() / 2].to_vec());
        truncated_etag.etag = Some(etag_of(&good));
        let cases: Vec<(&str, Reply, Check)> = vec![
            (
                "etag mismatch",
                Reply {
                    etag: Some("\"wfb1-00000000000000000000000000000000\"".into()),
                    ..ok_reply(good.clone())
                },
                |e| matches!(e, RemoteError::Protocol(m) if m.contains("ETag")),
            ),
            (
                "missing etag",
                Reply {
                    etag: None,
                    ..ok_reply(good.clone())
                },
                |e| matches!(e, RemoteError::Protocol(m) if m.contains("ETag")),
            ),
            ("content differs from etag", truncated_etag, |e| {
                matches!(e, RemoteError::Protocol(_))
            }),
            (
                "other library header",
                Reply {
                    library: "other".into(),
                    ..ok_reply(good.clone())
                },
                |e| matches!(e, RemoteError::LibraryChanged(_)),
            ),
            (
                "meta library",
                ok_reply(bundle_bytes("other", "")),
                |e| matches!(e, RemoteError::Protocol(m) if m.contains("library_id")),
            ),
            (
                "extra table",
                ok_reply(bundle_bytes(LIBRARY, "CREATE TABLE x(y);")),
                |e| matches!(e, RemoteError::Protocol(m) if m.contains("tables")),
            ),
            (
                "extra column",
                ok_reply(bundle_bytes(LIBRARY, "ALTER TABLE meta ADD COLUMN z;")),
                |e| matches!(e, RemoteError::Protocol(m) if m.contains("columns")),
            ),
            (
                "blob length",
                ok_reply(bundle_bytes(
                    LIBRARY,
                    "UPDATE list_waveforms SET blob=zeroblob(128);",
                )),
                |e| matches!(e, RemoteError::Protocol(m) if m.contains("bad blob")),
            ),
            (
                "encoding",
                ok_reply(bundle_bytes(
                    LIBRARY,
                    "UPDATE list_waveforms SET encoding='i16le-minmax-base64';",
                )),
                |e| matches!(e, RemoteError::Protocol(m) if m.contains("bad blob")),
            ),
            (
                "n out of range",
                ok_reply(bundle_bytes(LIBRARY, "UPDATE list_waveforms SET n=200;")),
                |e| matches!(e, RemoteError::Protocol(m) if m.contains("bad blob")),
            ),
            (
                "not sqlite",
                ok_reply(b"definitely not a sqlite file".to_vec()),
                |e| matches!(e, RemoteError::Protocol(_)),
            ),
            (
                "busy",
                Reply {
                    status: 503,
                    retry_after: Some("5"),
                    ..ok_reply(Vec::new())
                },
                |e| {
                    matches!(
                        e,
                        RemoteError::Busy {
                            retry_after: Some(5),
                            ..
                        }
                    )
                },
            ),
        ];
        for (name, reply, expected) in cases {
            let directory = tempfile::tempdir().unwrap();
            let (server, _) = server(CAPS, reply);
            let service = service(&server, directory.path());
            seed(&service);
            let error = service
                .import_bundle(&AtomicBool::new(false))
                .expect_err(name);
            assert!(expected(&error), "{name}: {error:?}");
            let store = service.i8_store.as_ref().unwrap();
            assert_eq!(store.len().unwrap(), 1, "{name}: store kept");
            assert!(store.get(9).unwrap().is_some(), "{name}");
            assert!(leftovers(directory.path()).is_empty(), "{name}");
        }
    }

    /// How [`conditional_server`] treats `If-None-Match`.
    #[derive(Clone, Copy)]
    enum Condition {
        /// `304` when it matches the bundle's ETag.
        Honour,
        /// `304` to every request, with this ETag and `X-Library-Id`.
        Always304(Option<&'static str>, &'static str),
        /// An older server: always `200` with the whole bundle.
        Ignore,
    }

    /// A bundle server treating `If-None-Match` per `mode`. Returns the `If-None-Match`
    /// value of each bundle request.
    fn conditional_server(
        bytes: Vec<u8>,
        mode: Condition,
    ) -> (TestServer, Arc<Mutex<Vec<Option<String>>>>) {
        let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let etag = etag_of(&bytes);
        let server = TestServer::start(move |request| {
            if request.path.starts_with("/api/v1/waveforms/bundle") {
                let condition = request.header("If-None-Match").map(str::to_owned);
                log.lock().unwrap().push(condition.clone());
                let not_modified = |tag: Option<&str>, library: &str| {
                    let response =
                        TestResponse::bytes(304, Vec::new()).with_header("X-Library-Id", library);
                    match tag {
                        Some(tag) => response.with_header("ETag", tag),
                        None => response,
                    }
                };
                match mode {
                    Condition::Always304(tag, library) => return not_modified(tag, library),
                    Condition::Honour if condition.as_deref() == Some(etag.as_str()) => {
                        return not_modified(Some(&etag), LIBRARY);
                    }
                    _ => {}
                }
                return TestResponse::bytes(200, bytes.clone())
                    .with_header("Content-Type", "application/vnd.sqlite3")
                    .with_header("X-Library-Id", LIBRARY)
                    .with_header("ETag", &etag);
            }
            match request.path.as_str() {
                "/api/v1/capabilities" => TestResponse::json(200, CAPS),
                other => panic!("unexpected {other}"),
            }
        });
        (server, seen)
    }

    #[test]
    fn a_second_import_sends_the_stored_etag_and_reuses_the_store_on_304() {
        let directory = tempfile::tempdir().unwrap();
        let bytes = bundle_bytes(LIBRARY, "");
        let etag = etag_of(&bytes);
        let (server, seen) = conditional_server(bytes, Condition::Honour);
        let service = service(&server, directory.path());
        let cancel = AtomicBool::new(false);
        assert_eq!(service.installed_bundle_etag(), None);
        assert_eq!(
            service.import_bundle(&cancel).unwrap(),
            BundleImport::Installed { rows: 1 }
        );
        assert_eq!(
            service.installed_bundle_etag().as_deref(),
            Some(etag.as_str())
        );
        assert_eq!(
            std::fs::read_to_string(directory.path().join("waveforms.db.etag")).unwrap(),
            format!("{etag}\n")
        );
        // A page lookup wrote a newer row after the install: a 304 must keep it.
        seed(&service);
        assert_eq!(
            service.import_bundle(&cancel).unwrap(),
            BundleImport::NotModified
        );
        assert_eq!(*seen.lock().unwrap(), vec![None, Some(etag.clone())]);
        let store = service.i8_store.as_ref().unwrap();
        assert_eq!(store.len().unwrap(), 2, "cached store reused unchanged");
        assert!(store.get(9).unwrap().is_some());
        assert_eq!(store.get(1).unwrap().unwrap().peaks.values, vec![-10, 10]);
        assert!(leftovers(directory.path()).is_empty());
        assert_eq!(
            service.installed_bundle_etag().as_deref(),
            Some(etag.as_str())
        );
    }

    #[test]
    fn an_empty_store_or_a_malformed_tag_sends_no_if_none_match() {
        let directory = tempfile::tempdir().unwrap();
        let bytes = bundle_bytes(LIBRARY, "");
        let etag = etag_of(&bytes);
        let (server, seen) = conditional_server(bytes, Condition::Honour);
        let service = service(&server, directory.path());
        // A tag left beside an empty store (cache emptied) is ignored.
        std::fs::write(directory.path().join("waveforms.db.etag"), &etag).unwrap();
        assert_eq!(service.installed_bundle_etag(), None);
        assert_eq!(
            service.import_bundle(&AtomicBool::new(false)).unwrap(),
            BundleImport::Installed { rows: 1 }
        );
        std::fs::write(directory.path().join("waveforms.db.etag"), "W/\"weak\"").unwrap();
        assert_eq!(service.installed_bundle_etag(), None);
        assert_eq!(
            service.import_bundle(&AtomicBool::new(false)).unwrap(),
            BundleImport::Installed { rows: 1 }
        );
        assert_eq!(*seen.lock().unwrap(), vec![None, None]);
        assert!(leftovers(directory.path()).is_empty());
    }

    #[test]
    fn a_304_to_an_unconditional_request_is_a_protocol_error() {
        let directory = tempfile::tempdir().unwrap();
        let (server, seen) = conditional_server(
            bundle_bytes(LIBRARY, ""),
            Condition::Always304(None, LIBRARY),
        );
        let service = service(&server, directory.path());
        seed(&service);
        let error = service
            .import_bundle(&AtomicBool::new(false))
            .expect_err("304 without If-None-Match");
        assert!(
            matches!(&error, RemoteError::Protocol(m) if m.contains("304")),
            "{error:?}"
        );
        assert_eq!(*seen.lock().unwrap(), vec![None]);
        let store = service.i8_store.as_ref().unwrap();
        assert_eq!(store.len().unwrap(), 1, "store kept");
        assert!(leftovers(directory.path()).is_empty());
        assert!(!directory.path().join("waveforms.db.etag").exists());
    }

    /// Install the bundle into `directory` from an `If-None-Match`-honouring server, then
    /// add a newer page-lookup row (id 9). Returns the installed ETag.
    fn install_then_seed(directory: &Path) -> String {
        let bytes = bundle_bytes(LIBRARY, "");
        let etag = etag_of(&bytes);
        let (server, _) = conditional_server(bytes, Condition::Honour);
        let service = service(&server, directory);
        assert_eq!(
            service.import_bundle(&AtomicBool::new(false)).unwrap(),
            BundleImport::Installed { rows: 1 }
        );
        seed(&service);
        etag
    }

    fn assert_store_kept(service: &RemoteListWaveforms, directory: &Path, etag: &str) {
        let store = service.i8_store.as_ref().unwrap();
        assert_eq!(store.len().unwrap(), 2, "store not replaced");
        assert!(
            store.get(9).unwrap().is_some(),
            "row written after install kept"
        );
        assert!(leftovers(directory).is_empty());
        assert_eq!(service.installed_bundle_etag().as_deref(), Some(etag));
    }

    #[test]
    fn a_server_ignoring_if_none_match_with_the_same_etag_is_not_modified() {
        let directory = tempfile::tempdir().unwrap();
        let etag = install_then_seed(directory.path());
        let (server, seen) = conditional_server(bundle_bytes(LIBRARY, ""), Condition::Ignore);
        let service = service(&server, directory.path());
        for _ in 0..2 {
            assert_eq!(
                service.import_bundle(&AtomicBool::new(false)).unwrap(),
                BundleImport::NotModified
            );
        }
        assert_eq!(
            *seen.lock().unwrap(),
            vec![Some(etag.clone()), Some(etag.clone())]
        );
        assert_store_kept(&service, directory.path(), &etag);
    }

    #[test]
    fn a_server_ignoring_if_none_match_with_a_new_bundle_installs_it() {
        let directory = tempfile::tempdir().unwrap();
        let etag = install_then_seed(directory.path());
        let newer = bundle_bytes(LIBRARY, "UPDATE meta SET value='4' WHERE key='revision';");
        let newer_etag = etag_of(&newer);
        assert_ne!(newer_etag, etag);
        let (server, seen) = conditional_server(newer, Condition::Ignore);
        let service = service(&server, directory.path());
        assert_eq!(
            service.import_bundle(&AtomicBool::new(false)).unwrap(),
            BundleImport::Installed { rows: 1 }
        );
        assert_eq!(*seen.lock().unwrap(), vec![Some(etag)]);
        assert_eq!(service.installed_bundle_etag(), Some(newer_etag));
    }

    #[test]
    fn a_304_for_another_etag_or_library_is_rejected_and_the_store_kept() {
        let cases: [(Condition, Check); 2] = [
            (
                Condition::Always304(Some("\"wfb1-00000000000000000000000000000000\""), LIBRARY),
                |e| matches!(e, RemoteError::Protocol(m) if m.contains("304")),
            ),
            (Condition::Always304(None, "other"), |e| {
                matches!(e, RemoteError::LibraryChanged(_))
            }),
        ];
        for (mode, expected) in cases {
            let directory = tempfile::tempdir().unwrap();
            let etag = install_then_seed(directory.path());
            let (server, _) = conditional_server(bundle_bytes(LIBRARY, ""), mode);
            let service = service(&server, directory.path());
            let error = service
                .import_bundle(&AtomicBool::new(false))
                .expect_err("rejected 304");
            assert!(expected(&error), "{error:?}");
            assert_store_kept(&service, directory.path(), &etag);
        }
    }

    #[test]
    fn bundle_etag_format_check() {
        assert!(is_bundle_etag("\"wfb1-ba7816bf8f01cfea414140de5dae2223\""));
        assert!(!is_bundle_etag("\"wfb1-BA7816BF8F01CFEA414140DE5DAE2223\""));
        assert!(!is_bundle_etag(
            "W/\"wfb1-ba7816bf8f01cfea414140de5dae2223\""
        ));
        assert!(!is_bundle_etag("\"wfb1-ba78\""));
        assert!(!is_bundle_etag(""));
    }

    #[test]
    fn no_bundle_request_without_the_capability() {
        let directory = tempfile::tempdir().unwrap();
        let (server, _) = server(CAPS_NO_BUNDLE, ok_reply(bundle_bytes(LIBRARY, "")));
        let service = service(&server, directory.path());
        assert!(!service.offers_bundle());
        assert!(matches!(
            service.import_bundle(&AtomicBool::new(false)),
            Err(RemoteError::Unsupported(_))
        ));
    }
}
