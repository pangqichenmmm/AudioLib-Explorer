// crates/audiolib-remote/src/download.rs
//! Drag-out download cache for server mode (API §7, §8).
//!
//! Windows drag and drop needs real, complete local files, so each dragged record is
//! downloaded first with `GET /audio/{id}/stream?download=1` into
//! `<data dir>/cache/<library>/<id>-<etag token>/<original filename>`, where the file name
//! comes from `Content-Disposition` (`filename*` first, then `filename`) and is made safe
//! for Windows. A cached copy is revalidated with `HEAD ...?download=1` (same `ETag` as the
//! plain stream); a complete in-memory audition copy of the original (`/stream`, not a
//! preview) with the same `ETag` is written out without a download; an interrupted
//! download resumes with `Range` + `If-Range`.
//!
//! Size-capped LRU eviction only ever looks at library folders marked with
//! [`MARKER_FILE`] directly under the cache root, so other data in `cache/` (such as the
//! waveform cache) is never touched.
use crate::client::{agent, call_cancellable};
use crate::error::{RemoteError, Result};
use crate::stream::{MemoryCache, StreamSource};
use crate::sync::directory_name;
use crate::util::fnv64;
use audiolib_core::AudioFile;
use serde::{Deserialize, Serialize};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

fn fallback_audio_filename(file: &AudioFile) -> String {
    let extension = [file.filename.as_str(), file.file_path.as_str()]
        .into_iter()
        .filter_map(|name| Path::new(name).extension()?.to_str())
        .find(|extension| {
            audiolib_core::SUPPORTED_AUDIO_EXTENSIONS
                .iter()
                .any(|known| extension.eq_ignore_ascii_case(known))
        })
        .unwrap_or("audio");
    format!("audio-{}.{}", file.id, extension.to_ascii_lowercase())
}

/// How long revalidating an existing complete download may take before the cached copy
/// is used as is: a drag must not wait for a sleeping NAS disk when the file is at hand.
pub const CACHED_REVALIDATE_TIMEOUT: Duration = Duration::from_millis(2500);

/// Default cap of the drag-out cache: 4 GiB.
pub const DEFAULT_DOWNLOAD_CACHE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Marks a library folder under `cache/` as owned by the download cache.
pub const MARKER_FILE: &str = ".audiolib-downloads";
const STALE_PART_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_NAME_CHARS: usize = 180;
const SOURCE_VERSION_DIRECTORY: &str = ".source-versions";

/// A complete original pinned in a caller-owned staging directory, independently
/// of cache eviction. Keep this value while reading `path`; dropping it closes the
/// read-only guard but leaves directory/file cleanup to the caller's owner.
#[derive(Debug)]
pub struct OwnedOriginal {
    /// Complete original in the caller's independently owned staging directory.
    pub path: PathBuf,
    /// Server validator captured from the download/probe, never a cache-name hash.
    /// Older offline caches without a version receipt explicitly return `None`.
    pub etag: Option<String>,
    /// Validated complete byte length of the pinned original.
    pub length: u64,
    /// True when an existing original was used after server revalidation failed.
    pub offline: bool,
    _guard: std::fs::File,
}

/// Strong, original-byte identity suitable for binding a displayed Waveform.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OriginalVersion {
    /// Opaque, quoted strong validator of the original file bytes.
    pub etag: String,
    /// Complete original length checked alongside its validator.
    pub length: u64,
    /// Identity was recovered from an exact, verified original cache receipt.
    pub offline: bool,
}

fn strong_etag(etag: Option<String>) -> Result<String> {
    etag.filter(|etag| {
        etag.len() >= 2
            && etag.starts_with('"')
            && etag.ends_with('"')
            && !etag.chars().any(char::is_control)
    })
    .ok_or_else(|| {
        RemoteError::Protocol(
            "Original source has no verified strong ETag; reload when the server is available"
                .into(),
        )
    })
}

struct EnsuredOriginal {
    path: PathBuf,
    etag: Option<String>,
    length: u64,
    offline: bool,
}

#[derive(Serialize, Deserialize)]
struct SourceVersion {
    version: u8,
    filename: String,
    etag: Option<String>,
    length: u64,
}

/// `HEAD ...?download=1` result.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DownloadProbe {
    pub etag: Option<String>,
    pub length: Option<u64>,
    /// File name from `Content-Disposition` (`filename*` preferred), unsanitized.
    pub filename: Option<String>,
}

fn header(headers: &ureq::http::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn percent_decode(value: &str) -> Option<Vec<u8>> {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = std::str::from_utf8(bytes.get(index + 1..index + 3)?).ok()?;
            output.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    Some(output)
}

/// Split `a; b="x;y"; c` into parameters, honouring quoted strings.
fn split_parameters(value: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for character in value.chars() {
        if escaped {
            current.push(character);
            escaped = false;
            continue;
        }
        match character {
            '\\' if quoted => {
                current.push(character);
                escaped = true;
            }
            '"' => {
                quoted = !quoted;
                current.push(character);
            }
            ';' if !quoted => parts.push(std::mem::take(&mut current)),
            _ => current.push(character),
        }
    }
    parts.push(current);
    parts
}

/// File name from a `Content-Disposition` value: the RFC 5987 `filename*` (UTF-8 or
/// ISO-8859-1) when present and decodable, otherwise the plain `filename`.
pub fn content_disposition_filename(value: &str) -> Option<String> {
    let mut plain = None;
    let mut extended = None;
    for parameter in split_parameters(value).into_iter().skip(1) {
        let Some((name, raw)) = parameter.split_once('=') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let raw = raw.trim();
        if name == "filename*" {
            let mut pieces = raw.splitn(3, '\'');
            let (Some(charset), Some(_language), Some(encoded)) =
                (pieces.next(), pieces.next(), pieces.next())
            else {
                continue;
            };
            let Some(bytes) = percent_decode(encoded.trim_matches('"')) else {
                continue;
            };
            let decoded = if charset.eq_ignore_ascii_case("utf-8") {
                String::from_utf8(bytes).ok()
            } else if charset.eq_ignore_ascii_case("iso-8859-1") {
                Some(bytes.iter().map(|&byte| char::from(byte)).collect())
            } else {
                None
            };
            extended = decoded.filter(|name| !name.trim().is_empty());
        } else if name == "filename" {
            let unquoted = if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') {
                let mut text = String::new();
                let mut chars = raw[1..raw.len() - 1].chars();
                while let Some(character) = chars.next() {
                    if character == '\\' {
                        if let Some(next) = chars.next() {
                            text.push(next);
                        }
                    } else {
                        text.push(character);
                    }
                }
                text
            } else {
                raw.to_owned()
            };
            plain = Some(unquoted).filter(|name| !name.trim().is_empty());
        }
    }
    extended.or(plain)
}

const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// A Windows-safe single file name: path separators, `<>:"|?*` and control characters
/// become `_`, trailing dots/spaces are removed, reserved device names (`CON`, `NUL.wav`,
/// `com1.txt`, ...) get a `_` prefix, and long names are shortened keeping the extension.
/// Falls back to `fallback` when nothing usable remains.
pub fn sanitize_filename(name: &str, fallback: &str) -> String {
    let mut cleaned: String = name
        .chars()
        .map(|character| {
            if character.is_control() || "<>:\"/\\|?*".contains(character) {
                '_'
            } else {
                character
            }
        })
        .collect();
    cleaned = cleaned.trim().trim_end_matches(['.', ' ']).to_owned();
    if cleaned
        .chars()
        .all(|character| character == '_' || character == '.')
    {
        cleaned = fallback.to_owned();
    }
    let stem = cleaned
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end()
        .to_ascii_uppercase();
    if RESERVED.contains(&stem.as_str()) {
        cleaned.insert(0, '_');
    }
    if cleaned.chars().count() > MAX_NAME_CHARS {
        let extension: String = match cleaned.rfind('.') {
            Some(dot) if cleaned.len() - dot <= 16 => cleaned[dot..].to_owned(),
            _ => String::new(),
        };
        let keep = MAX_NAME_CHARS - extension.chars().count();
        cleaned = cleaned.chars().take(keep).collect::<String>() + &extension;
    }
    cleaned
}

/// Short, path-safe token for an `ETag` (or another version string).
pub fn etag_token(etag: &str) -> String {
    format!("{:016x}", fnv64(etag))
}

/// `HEAD` a download source: `ETag`, `Content-Length`, `Content-Disposition` file name.
/// Waits up to the source's first-byte timeout (the server may be waking a NAS disk).
pub fn probe_download(source: &StreamSource) -> Result<DownloadProbe> {
    probe_download_with(source, &AtomicBool::new(false), &mut |_| {})
}

/// Cancellable HEAD retaining a representation's actual server ETag.
pub fn probe_download_with_cancel(
    source: &StreamSource,
    cancel: &AtomicBool,
) -> Result<DownloadProbe> {
    probe_download_with(source, cancel, &mut |_| {})
}

/// [`probe_download`] that stops waiting when `cancel` is set and reports the wait.
fn probe_download_with(
    source: &StreamSource,
    cancel: &AtomicBool,
    waiting: &mut dyn FnMut(Duration),
) -> Result<DownloadProbe> {
    let owned = source.clone();
    call_cancellable(move |abort| head(&owned, abort), cancel, waiting)
}

fn head(source: &StreamSource, abort: &crate::abort::Abort) -> Result<DownloadProbe> {
    let agent =
        crate::abort::abortable_agent(source.first_byte_timeout, source.first_byte_timeout, abort);
    let response = crate::abort::call_fresh(|| {
        let mut request = agent.head(&source.url);
        if let Some(value) = source.auth.header() {
            request = request.header("Authorization", &value);
        }
        request
    })
    .map_err(|error| first_byte_error(error, source))?;
    let status = response.status().as_u16();
    let unauthenticated = matches!(source.auth, crate::stream::SourceAuth::None);
    if status >= 400 {
        // A HEAD reply has no body; for a 503 (e.g. an offline root) the reason is only in
        // the GET body, which the server answers at once without touching the disk.
        let body = if status == 503 {
            error_body(source)
        } else {
            String::new()
        };
        let error = RemoteError::from_audio_response(
            status,
            &body,
            &format!("HEAD {}", source.url),
            crate::client::retry_after(response.headers()),
        );
        return Err(if unauthenticated {
            error.without_token()
        } else {
            error
        });
    }
    if status != 200 {
        return Err(RemoteError::Protocol(format!(
            "HEAD {}: HTTP {status}",
            source.url
        )));
    }
    let headers = response.headers();
    Ok(DownloadProbe {
        etag: header(headers, "etag"),
        length: header(headers, "content-length").and_then(|value| value.parse().ok()),
        filename: header(headers, "content-disposition")
            .as_deref()
            .and_then(content_disposition_filename),
    })
}

/// The JSON error body of a `503` answer to `GET` (one byte requested), or empty.
fn error_body(source: &StreamSource) -> String {
    let agent = agent(Duration::from_secs(5), Duration::from_secs(5));
    let mut request = agent.get(&source.url).header("Range", "bytes=0-0");
    if let Some(value) = source.auth.header() {
        request = request.header("Authorization", &value);
    }
    match request.call() {
        Ok(mut response) if response.status().as_u16() == 503 => response
            .body_mut()
            .with_config()
            .limit(64 * 1024)
            .read_to_string()
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// One record to make available locally.
#[derive(Clone, Debug)]
pub struct DownloadItem {
    pub file: AudioFile,
    /// `GET /audio/{id}/stream?download=1` with the configured authentication.
    pub source: StreamSource,
}

/// Progress of [`DownloadCache::ensure_all`]: item index (0-based), item count, bytes of
/// the current item received so far and its `Content-Length`. `waiting` is set while the
/// server has not answered yet (reported about every 100 ms; a NAS disk may need 20-30 s
/// to spin up), with the time waited so far.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DownloadProgress {
    pub index: usize,
    pub count: usize,
    pub received: u64,
    pub total: Option<u64>,
    pub waiting: Option<Duration>,
}

/// What [`DownloadCache::ensure`] and its helpers report.
enum Event {
    Bytes(u64, Option<u64>),
    Waiting(Duration),
}

/// A request that got no response within the source's first-byte timeout.
fn first_byte_error(error: ureq::Error, source: &StreamSource) -> RemoteError {
    match RemoteError::from_transport(error, &source.url) {
        RemoteError::Timeout(_) => RemoteError::Timeout(format!(
            "{}: no response within {} s",
            source.url,
            source.first_byte_timeout.as_secs()
        )),
        other => other,
    }
}

/// Size-capped cache of complete server files for drag and drop.
#[derive(Clone, Debug)]
pub struct DownloadCache {
    root: PathBuf,
    cap_bytes: u64,
}

impl DownloadCache {
    /// Cache rooted at `<data dir>/cache` holding at most `cap_bytes` of downloads.
    pub fn new(root: PathBuf, cap_bytes: u64) -> Self {
        Self { root, cap_bytes }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<root>/<library>` for one server library.
    pub fn library_dir(&self, library_id: &str) -> PathBuf {
        self.root.join(directory_name(library_id))
    }

    /// Probe current original bytes without downloading them. Offline fallback is
    /// allowed only for a complete original with an exact saved version receipt;
    /// legacy/unknown cache names are never interpreted as server validators.
    pub fn original_version(
        &self,
        library_id: &str,
        id: i64,
        source: &StreamSource,
        cancel: &AtomicBool,
    ) -> Result<OriginalVersion> {
        self.original_version_checked(library_id, id, source, cancel, None)
    }

    /// As `original_version`, retaining an exact verified cached version offline
    /// instead of substituting the most recently used version of the same record.
    pub fn original_version_expected(
        &self,
        library_id: &str,
        id: i64,
        source: &StreamSource,
        cancel: &AtomicBool,
        expected: &OriginalVersion,
    ) -> Result<OriginalVersion> {
        self.original_version_checked(library_id, id, source, cancel, Some(expected))
    }

    fn original_version_checked(
        &self,
        library_id: &str,
        id: i64,
        source: &StreamSource,
        cancel: &AtomicBool,
        expected: Option<&OriginalVersion>,
    ) -> Result<OriginalVersion> {
        let cached = self.newest_complete(&self.library_dir(library_id), id);
        let source = if cached.is_some() {
            source
                .clone()
                .with_first_byte_timeout(source.first_byte_timeout.min(CACHED_REVALIDATE_TIMEOUT))
        } else {
            source.clone()
        };
        match probe_download_with(&source, cancel, &mut |_| {}) {
            Ok(probe) => Ok(OriginalVersion {
                etag: strong_etag(probe.etag)?,
                length: probe.length.ok_or_else(|| {
                    RemoteError::Protocol("Original source has no complete byte length".into())
                })?,
                offline: false,
            }),
            Err(error)
                if error.is_offline()
                    || matches!(
                        error,
                        RemoteError::RootOffline { .. }
                            | RemoteError::Starting { .. }
                            | RemoteError::Busy { .. }
                    ) =>
            {
                let library = self.library_dir(library_id);
                let path = match expected {
                    Some(expected) => self.exact_complete(&library, id, expected),
                    None => self.newest_complete(&library, id),
                }
                .ok_or(error)?;
                let length = std::fs::metadata(&path)?.len();
                Ok(OriginalVersion {
                    etag: strong_etag(self.read_source_version(&path, length))?,
                    length,
                    offline: true,
                })
            }
            Err(error) => Err(error),
        }
    }

    /// Make every item available as a complete local file (in order) and return the
    /// paths, then evict least-recently-used downloads above the cap (never these).
    pub fn ensure_all(
        &self,
        library_id: &str,
        items: &[DownloadItem],
        playback: Option<&MemoryCache>,
        progress: &mut dyn FnMut(DownloadProgress),
        cancel: &AtomicBool,
    ) -> Result<Vec<PathBuf>> {
        let mut paths = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            if cancel.load(Ordering::Acquire) {
                return Err(RemoteError::Cancelled);
            }
            let count = items.len();
            let path = self.ensure_reporting(
                library_id,
                item,
                playback,
                &mut |event| {
                    let (received, total, waiting) = match event {
                        Event::Bytes(received, total) => (received, total, None),
                        Event::Waiting(waited) => (0, None, Some(waited)),
                    };
                    progress(DownloadProgress {
                        index,
                        count,
                        received,
                        total,
                        waiting,
                    })
                },
                cancel,
            )?;
            paths.push(path);
        }
        self.evict(&paths);
        Ok(paths)
    }

    /// Make one record available locally (see the module docs). While the server is
    /// unreachable the newest complete cached copy of the record is used. A file that
    /// changes on the server during the download is fetched once more from scratch.
    pub fn ensure(
        &self,
        library_id: &str,
        item: &DownloadItem,
        playback: Option<&MemoryCache>,
        progress: &mut dyn FnMut(u64, Option<u64>),
        cancel: &AtomicBool,
    ) -> Result<PathBuf> {
        self.ensure_reporting(
            library_id,
            item,
            playback,
            &mut |event| {
                if let Event::Bytes(received, total) = event {
                    progress(received, total);
                }
            },
            cancel,
        )
    }

    fn ensure_reporting(
        &self,
        library_id: &str,
        item: &DownloadItem,
        playback: Option<&MemoryCache>,
        report: &mut dyn FnMut(Event),
        cancel: &AtomicBool,
    ) -> Result<PathBuf> {
        self.ensure_versioned(library_id, item, playback, report, cancel)
            .map(|original| original.path)
    }

    fn ensure_versioned(
        &self,
        library_id: &str,
        item: &DownloadItem,
        playback: Option<&MemoryCache>,
        report: &mut dyn FnMut(Event),
        cancel: &AtomicBool,
    ) -> Result<EnsuredOriginal> {
        match self.ensure_once(library_id, item, playback, report, cancel) {
            Err(RemoteError::Changed(detail)) => {
                tracing::info!(%detail, "download changed on the server; retrying");
                self.ensure_once(library_id, item, playback, report, cancel)
            }
            other => other,
        }
    }

    fn ensure_once(
        &self,
        library_id: &str,
        item: &DownloadItem,
        playback: Option<&MemoryCache>,
        report: &mut dyn FnMut(Event),
        cancel: &AtomicBool,
    ) -> Result<EnsuredOriginal> {
        let library = self.library_dir(library_id);
        let file = &item.file;
        // With a complete earlier download at hand, revalidate briefly and fall back to it
        // rather than wait for a waking disk; without one, wait the full first-byte timeout.
        let existing = self.newest_complete(&library, file.id).is_some();
        let source = if existing {
            item.source.clone().with_first_byte_timeout(
                item.source
                    .first_byte_timeout
                    .min(CACHED_REVALIDATE_TIMEOUT),
            )
        } else {
            item.source.clone()
        };
        let probe = match probe_download_with(&source, cancel, &mut |waited| {
            report(Event::Waiting(waited))
        }) {
            Ok(probe) => probe,
            // Server or root offline, starting or busy: a complete earlier download works.
            Err(error)
                if error.is_offline()
                    || matches!(
                        error,
                        RemoteError::RootOffline { .. }
                            | RemoteError::Starting { .. }
                            | RemoteError::Busy { .. }
                    ) =>
            {
                let path = self.newest_entry(&library, file.id).ok_or(error)?;
                let length = std::fs::metadata(&path)?.len();
                let etag = self.read_source_version(&path, length);
                return Ok(EnsuredOriginal {
                    path,
                    etag,
                    length,
                    offline: true,
                });
            }
            Err(error) => return Err(error),
        };
        let version = probe.etag.clone().unwrap_or_else(|| {
            format!(
                "size-{}-{}",
                probe.length.unwrap_or(file.file_size),
                file.file_path
            )
        });
        let fallback = fallback_audio_filename(file);
        let name = sanitize_filename(
            probe.filename.as_deref().unwrap_or(&file.filename),
            &fallback,
        );
        let entry = library.join(format!("{}-{}", file.id, etag_token(&version)));
        let target = entry.join(&name);
        if let Ok(metadata) = std::fs::metadata(&target)
            && metadata.is_file()
            && probe.length.is_none_or(|length| metadata.len() == length)
        {
            touch(&target);
            self.remove_other_versions(&library, file.id, &entry);
            report(Event::Bytes(metadata.len(), Some(metadata.len())));
            return self.completed_original(target, &probe);
        }
        std::fs::create_dir_all(&entry)?;
        let marker = library.join(MARKER_FILE);
        if !marker.exists() {
            std::fs::write(&marker, b"AudioLib Explorer drag-out download cache\n")?;
        }
        let partial = entry.join(format!("{name}.part"));
        if let (Some(etag), Some(playback)) = (&probe.etag, playback)
            && copy_from_memory(playback, &item.source, etag, probe.length, &partial)?
        {
            std::fs::rename(&partial, &target)?;
            self.remove_other_versions(&library, file.id, &entry);
            let length = std::fs::metadata(&target)?.len();
            report(Event::Bytes(length, Some(length)));
            return self.completed_original(target, &probe);
        }
        if let Err(error) = download(&item.source, &partial, &probe, report, cancel) {
            if matches!(error, RemoteError::Changed(_)) {
                let _ = std::fs::remove_file(&partial);
            }
            return Err(error);
        }
        std::fs::rename(&partial, &target)?;
        self.remove_other_versions(&library, file.id, &entry);
        self.completed_original(target, &probe)
    }

    fn source_version_path(&self, path: &Path) -> Option<PathBuf> {
        let entry = path.parent()?;
        let library = entry.parent()?;
        let name = entry.file_name()?.to_str()?;
        if !library.starts_with(&self.root) || !is_entry_name(name) {
            return None;
        }
        let directory = library.join(SOURCE_VERSION_DIRECTORY);
        if let Ok(metadata) = std::fs::symlink_metadata(&directory)
            && (!metadata.is_dir() || metadata.file_type().is_symlink())
        {
            return None;
        }
        Some(directory.join(format!("{name}.json")))
    }

    fn read_source_version(&self, path: &Path, length: u64) -> Option<String> {
        let receipt = std::fs::File::open(self.source_version_path(path)?).ok()?;
        if receipt.metadata().ok()?.len() > 8192 {
            return None;
        }
        let version: SourceVersion = serde_json::from_reader(receipt.take(8192)).ok()?;
        if version.version != 1
            || version.length != length
            || path.file_name()?.to_str()? != version.filename
        {
            return None;
        }
        version.etag
    }

    fn completed_original(&self, path: PathBuf, probe: &DownloadProbe) -> Result<EnsuredOriginal> {
        let length = std::fs::metadata(&path)?.len();
        // Receipts live outside the audio entry: newest_complete must never see
        // them as playable originals, and old download-cache layouts remain valid.
        if let Some(receipt) = self.source_version_path(&path) {
            let saved = (|| -> std::io::Result<()> {
                let directory = receipt
                    .parent()
                    .ok_or_else(|| std::io::Error::other("Missing source receipt directory"))?;
                std::fs::create_dir_all(directory)?;
                let file = match std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&receipt)
                {
                    Ok(file) => file,
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        return Ok(());
                    }
                    Err(error) => return Err(error),
                };
                let version = SourceVersion {
                    version: 1,
                    filename: path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                    etag: probe.etag.clone(),
                    length,
                };
                serde_json::to_writer(file, &version).map_err(std::io::Error::other)
            })();
            // The receipt enriches future offline use; inability to add one must
            // not invalidate an existing complete download used by older callers.
            if let Err(error) = saved {
                tracing::debug!(%error, "Could not save original source version receipt");
            }
        }
        Ok(EnsuredOriginal {
            path,
            etag: probe.etag.clone(),
            length,
            offline: false,
        })
    }

    fn remove_source_version(&self, entry: &Path) {
        if let Some(receipt) = self.source_version_path(&entry.join("audio")) {
            let _ = std::fs::remove_file(receipt);
        }
    }

    /// Prepare a real original in a new path inside an existing caller-owned staging
    /// directory. A hard link pins immutable cached bytes where supported; otherwise
    /// a bounded copy reads an already opened source handle. Concurrent LRU deletion
    /// can cause one retry before opening, but can never substitute another version.
    /// The destination is never overwritten, and incomplete copies are removed.
    pub fn prepare_owned_original(
        &self,
        library_id: &str,
        item: &DownloadItem,
        playback: Option<&MemoryCache>,
        destination: &Path,
        progress: &mut dyn FnMut(DownloadProgress),
        cancel: &AtomicBool,
    ) -> Result<OwnedOriginal> {
        self.prepare_owned_original_checked(
            library_id,
            item,
            playback,
            (destination, None),
            progress,
            cancel,
        )
    }

    /// Pin exactly the displayed source version. Online replacements are rejected;
    /// offline lookup can reuse any complete cache entry with the exact receipt.
    pub fn prepare_owned_original_version(
        &self,
        library_id: &str,
        item: &DownloadItem,
        playback: Option<&MemoryCache>,
        (destination, expected): (&Path, &OriginalVersion),
        progress: &mut dyn FnMut(DownloadProgress),
        cancel: &AtomicBool,
    ) -> Result<OwnedOriginal> {
        self.prepare_owned_original_checked(
            library_id,
            item,
            playback,
            (destination, Some(expected)),
            progress,
            cancel,
        )
    }

    fn prepare_owned_original_checked(
        &self,
        library_id: &str,
        item: &DownloadItem,
        playback: Option<&MemoryCache>,
        (destination, expected): (&Path, Option<&OriginalVersion>),
        progress: &mut dyn FnMut(DownloadProgress),
        cancel: &AtomicBool,
    ) -> Result<OwnedOriginal> {
        if let Some(expected) = expected {
            let current = self.original_version_expected(
                library_id,
                item.file.id,
                &item.source,
                cancel,
                expected,
            )?;
            if current.etag != expected.etag || current.length != expected.length {
                return Err(RemoteError::Changed(
                    "Original differs from the displayed Waveform version".into(),
                ));
            }
        }
        for attempt in 0..2 {
            if cancel.load(Ordering::Acquire) {
                return Err(RemoteError::Cancelled);
            }
            let mut original = self.ensure_versioned(
                library_id,
                item,
                playback,
                &mut |event| {
                    let (received, total, waiting) = match event {
                        Event::Bytes(received, total) => (received, total, None),
                        Event::Waiting(waited) => (0, None, Some(waited)),
                    };
                    progress(DownloadProgress {
                        index: 0,
                        count: 1,
                        received,
                        total,
                        waiting,
                    });
                },
                cancel,
            )?;
            if let Some(expected) = expected {
                if original.offline
                    && let Some(path) =
                        self.exact_complete(&self.library_dir(library_id), item.file.id, expected)
                {
                    original = EnsuredOriginal {
                        path,
                        etag: Some(expected.etag.clone()),
                        length: expected.length,
                        offline: true,
                    };
                }
                if original.etag.as_deref() != Some(expected.etag.as_str())
                    || original.length != expected.length
                {
                    return Err(RemoteError::Changed(
                        "Original differs from the displayed Waveform version".into(),
                    ));
                }
            }
            let mut input = match open_pinned(&original.path) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound && attempt == 0 => {
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            let before = input.metadata()?;
            if !before.is_file() || before.len() != original.length {
                return Err(RemoteError::Changed(
                    "Cached original changed before staging".into(),
                ));
            }
            let staged = stage_original(
                &mut input,
                &original.path,
                destination,
                original.length,
                cancel,
            )?;
            let after = input.metadata()?;
            if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
                drop(staged);
                let _ = std::fs::remove_file(destination);
                return Err(RemoteError::Changed(
                    "Cached original changed during staging".into(),
                ));
            }
            return Ok(OwnedOriginal {
                path: destination.to_owned(),
                etag: original.etag,
                length: original.length,
                offline: original.offline,
                _guard: staged,
            });
        }
        Err(RemoteError::Changed(
            "Original cache was evicted while staging".into(),
        ))
    }

    /// The newest complete download of `id` (offline use), marked as recently used.
    fn newest_entry(&self, library: &Path, id: i64) -> Option<PathBuf> {
        self.newest_complete(library, id)
            .inspect(|path| touch(path))
    }

    fn exact_complete(
        &self,
        library: &Path,
        id: i64,
        expected: &OriginalVersion,
    ) -> Option<PathBuf> {
        let prefix = format!("{id}-");
        for entry in std::fs::read_dir(library).ok()?.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with(&prefix) || !is_entry_name(&name) {
                continue;
            }
            let Ok(metadata) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                continue;
            }
            let Ok(files) = std::fs::read_dir(entry.path()) else {
                continue;
            };
            for file in files.flatten() {
                let path = file.path();
                let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                    continue;
                };
                if metadata.is_file()
                    && !metadata.file_type().is_symlink()
                    && metadata.len() == expected.length
                    && self.read_source_version(&path, expected.length).as_deref()
                        == Some(expected.etag.as_str())
                {
                    return Some(path);
                }
            }
        }
        None
    }

    /// The newest complete download of `id` in any version folder.
    fn newest_complete(&self, library: &Path, id: i64) -> Option<PathBuf> {
        let prefix = format!("{id}-");
        let mut best: Option<(SystemTime, PathBuf)> = None;
        for entry in std::fs::read_dir(library).ok()?.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with(&prefix) || !is_entry_name(&name) {
                continue;
            }
            for file in std::fs::read_dir(entry.path()).ok()?.flatten() {
                let path = file.path();
                let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                    continue;
                };
                if !metadata.is_file() || path.to_string_lossy().ends_with(".part") {
                    continue;
                }
                let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                if best.as_ref().is_none_or(|(time, _)| modified > *time) {
                    best = Some((modified, path));
                }
            }
        }
        best.map(|(_, path)| path)
    }

    /// Delete older version folders of record `id` (another `ETag`), keeping `keep`.
    fn remove_other_versions(&self, library: &Path, id: i64, keep: &Path) {
        let prefix = format!("{id}-");
        let Ok(entries) = std::fs::read_dir(library) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            if name.starts_with(&prefix)
                && is_entry_name(&name)
                && path != keep
                && remove_entry(&self.root, &path)
            {
                self.remove_source_version(&path);
            }
        }
    }

    /// Delete least-recently-used download folders until the marked library folders fit
    /// the cap. Folders holding a path in `keep` stay; abandoned `.part` files older than a
    /// day are removed. Nothing outside marked library folders under the root is touched.
    pub fn evict(&self, keep: &[PathBuf]) {
        let Ok(libraries) = std::fs::read_dir(&self.root) else {
            return;
        };
        let now = SystemTime::now();
        let mut entries: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
        for library in libraries.flatten() {
            let library = library.path();
            let Ok(metadata) = std::fs::symlink_metadata(&library) else {
                continue;
            };
            if !metadata.is_dir() || !library.join(MARKER_FILE).is_file() {
                continue;
            }
            let Ok(children) = std::fs::read_dir(&library) else {
                continue;
            };
            for child in children.flatten() {
                let path = child.path();
                let name = child.file_name().to_string_lossy().into_owned();
                let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                    continue;
                };
                if !metadata.is_dir() || !is_entry_name(&name) {
                    continue;
                }
                let mut size = 0;
                let mut newest = SystemTime::UNIX_EPOCH;
                let mut files = 0;
                if let Ok(contents) = std::fs::read_dir(&path) {
                    for item in contents.flatten() {
                        let Ok(metadata) = std::fs::symlink_metadata(item.path()) else {
                            continue;
                        };
                        if !metadata.is_file() {
                            continue;
                        }
                        let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                        if item.file_name().to_string_lossy().ends_with(".part") {
                            if now
                                .duration_since(modified)
                                .is_ok_and(|age| age > STALE_PART_AGE)
                            {
                                let _ = std::fs::remove_file(item.path());
                            }
                            continue;
                        }
                        files += 1;
                        size += metadata.len();
                        newest = newest.max(modified);
                    }
                }
                if files == 0 {
                    // Only stale or in-flight parts: an empty folder can go.
                    let _ = std::fs::remove_dir(&path);
                    continue;
                }
                entries.push((newest, size, path));
            }
        }
        let mut total: u64 = entries.iter().map(|(_, size, _)| size).sum();
        entries.sort_by_key(|(modified, _, _)| *modified);
        for (_, size, path) in entries {
            if total <= self.cap_bytes {
                break;
            }
            if keep.iter().any(|kept| kept.starts_with(&path)) {
                continue;
            }
            if remove_entry(&self.root, &path) {
                total = total.saturating_sub(size);
                self.remove_source_version(&path);
            }
        }
    }
}

fn open_pinned(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Keep cached/staged bytes stable while still allowing playback readers.
        options.share_mode(1);
    }
    options.open(path)
}

fn same_open_file(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    let same = left.is_file()
        && right.is_file()
        && left.len() == right.len()
        && left.modified().ok() == right.modified().ok();
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        same && left.dev() == right.dev() && left.ino() == right.ino()
    }
    #[cfg(not(unix))]
    {
        same
    }
}

fn stage_original(
    input: &mut std::fs::File,
    source: &Path,
    destination: &Path,
    length: u64,
    cancel: &AtomicBool,
) -> Result<std::fs::File> {
    // On Windows a conflicting writer/deleter is prevented by open_pinned.
    // Unix permits removing a cache path while a handle is held: compare the
    // hard-linked inode to that handle and fall back to copying the handle.
    let mut owns_destination = false;
    let result = (|| -> Result<std::fs::File> {
        match std::fs::hard_link(source, destination) {
            Ok(()) => {
                owns_destination = true;
                let staged = open_pinned(destination)?;
                if same_open_file(&input.metadata()?, &staged.metadata()?) {
                    if cancel.load(Ordering::Acquire) {
                        return Err(RemoteError::Cancelled);
                    }
                    return Ok(staged);
                }
                drop(staged);
                std::fs::remove_file(destination)?;
                owns_destination = false;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(error.into());
            }
            Err(_) => {}
        }
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(destination)?;
        owns_destination = true;
        input.seek(SeekFrom::Start(0))?;
        let mut copied = 0_u64;
        let mut bytes = [0; 256 * 1024];
        while copied < length {
            if cancel.load(Ordering::Acquire) {
                return Err(RemoteError::Cancelled);
            }
            let count = (length - copied).min(bytes.len() as u64) as usize;
            input.read_exact(&mut bytes[..count])?;
            output.write_all(&bytes[..count])?;
            copied += count as u64;
        }
        output.sync_all()?;
        drop(output);
        if cancel.load(Ordering::Acquire) {
            return Err(RemoteError::Cancelled);
        }
        Ok(open_pinned(destination)?)
    })();
    if result.is_err() && owns_destination {
        let _ = std::fs::remove_file(destination);
    }
    result
}

/// `<id>-<16 hex>` version folder names.
fn is_entry_name(name: &str) -> bool {
    let Some((id, token)) = name.split_once('-') else {
        return false;
    };
    !id.is_empty()
        && id.bytes().all(|byte| byte.is_ascii_digit())
        && token.len() == 16
        && token.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Remove one version folder (plain files only, never following links) if it is inside
/// `root`; true when it is gone.
fn remove_entry(root: &Path, entry: &Path) -> bool {
    if !entry.starts_with(root) || entry == root {
        return false;
    }
    let Ok(metadata) = std::fs::symlink_metadata(entry) else {
        return true;
    };
    if !metadata.is_dir() {
        return false;
    }
    if let Ok(contents) = std::fs::read_dir(entry) {
        for item in contents.flatten() {
            if std::fs::symlink_metadata(item.path()).is_ok_and(|metadata| metadata.is_file()) {
                let _ = std::fs::remove_file(item.path());
            }
        }
    }
    std::fs::remove_dir(entry).is_ok()
}

fn touch(path: &Path) {
    if let Ok(handle) = std::fs::OpenOptions::new().write(true).open(path) {
        let _ = handle.set_modified(SystemTime::now());
    }
}

/// Write the complete in-memory audition copy of the original file (the same URL without
/// `?download=1`) with exactly `etag` to `partial`; false (and no file) when there is none.
fn copy_from_memory(
    playback: &MemoryCache,
    source: &StreamSource,
    etag: &str,
    length: Option<u64>,
    partial: &Path,
) -> Result<bool> {
    let url = source.url.split('?').next().unwrap_or(&source.url);
    let mut buffer = Vec::new();
    if !playback.copy_complete(url, etag, length, &mut buffer)? {
        return Ok(false);
    }
    let mut file = std::fs::File::create(partial)?;
    file.write_all(&buffer)?;
    file.sync_all()?;
    Ok(true)
}

/// `bytes <start>-<end>/<total>` → (start, total).
fn content_range(value: &str) -> Option<(u64, Option<u64>)> {
    let rest = value.trim().strip_prefix("bytes ")?;
    let (range, total) = rest.split_once('/')?;
    let (start, _) = range.split_once('-')?;
    Some((start.trim().parse().ok()?, total.trim().parse().ok()))
}

/// GET `source` into `partial`, resuming an existing part with `Range` + `If-Range` when
/// the `ETag` is known. Verifies the `ETag` and the full length.
fn download(
    source: &StreamSource,
    partial: &Path,
    probe: &DownloadProbe,
    report: &mut dyn FnMut(Event),
    cancel: &AtomicBool,
) -> Result<()> {
    let existing = std::fs::metadata(partial).map(|m| m.len()).unwrap_or(0);
    let resume = existing > 0
        && probe.etag.is_some()
        && probe.length.is_some_and(|length| existing < length);
    let owned = source.clone();
    let range = resume
        .then(|| probe.etag.clone())
        .flatten()
        .map(|etag| (format!("bytes={existing}-"), etag));
    // Waiting for the first byte (a waking NAS disk) is cancellable and reported.
    let mut response = call_cancellable(
        move |abort| {
            let agent = crate::abort::abortable_stream_agent(
                owned.first_byte_timeout,
                crate::stream::READ_STALL_TIMEOUT,
                abort,
            );
            crate::abort::call_fresh(|| {
                let mut request = agent.get(&owned.url);
                if let Some(value) = owned.auth.header() {
                    request = request.header("Authorization", &value);
                }
                if let Some((range, etag)) = &range {
                    request = request.header("Range", range).header("If-Range", etag);
                }
                request
            })
            .map_err(|error| first_byte_error(error, &owned))
        },
        cancel,
        &mut |waited| report(Event::Waiting(waited)),
    )?;
    // The body is read on this thread: bound each read here too (the helper thread's
    // scope ended with the response head; `cancel` is checked between reads).
    let _reads = crate::abort::Abort::default().enter(Some(crate::stream::READ_STALL_TIMEOUT));
    let status = response.status().as_u16();
    if status >= 400 {
        let retry = crate::client::retry_after(response.headers());
        // The JSON body carries the server's text (e.g. which root is offline).
        let body = response
            .body_mut()
            .with_config()
            .limit(64 * 1024)
            .read_to_string()
            .unwrap_or_default();
        let error =
            RemoteError::from_audio_response(status, &body, &format!("GET {}", source.url), retry);
        return Err(if matches!(source.auth, crate::stream::SourceAuth::None) {
            error.without_token()
        } else {
            error
        });
    }
    let headers = response.headers();
    let etag = header(headers, "etag");
    if let (Some(expected), Some(actual)) = (&probe.etag, &etag)
        && expected != actual
    {
        return Err(RemoteError::Changed(format!(
            "ETag {actual} differs from {expected}"
        )));
    }
    let (mut received, total) = match status {
        206 if resume => {
            let range = header(headers, "content-range")
                .as_deref()
                .and_then(content_range)
                .ok_or_else(|| RemoteError::Protocol("206 without Content-Range".into()))?;
            if range.0 != existing {
                return Err(RemoteError::Protocol(format!(
                    "resume answered at byte {} instead of {existing}",
                    range.0
                )));
            }
            (existing, range.1.or(probe.length))
        }
        200 => (
            0,
            header(headers, "content-length")
                .and_then(|value| value.parse().ok())
                .or(probe.length),
        ),
        other => {
            return Err(RemoteError::Protocol(format!(
                "GET {}: HTTP {other}",
                source.url
            )));
        }
    };
    if let (Some(expected), Some(total)) = (probe.length, total)
        && expected != total
    {
        return Err(RemoteError::Changed(format!(
            "length {total} differs from {expected}"
        )));
    }
    let mut output = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(received == 0)
        .open(partial)?;
    output.seek(SeekFrom::Start(received))?;
    let mut reader = response.into_body().into_reader();
    let mut buffer = vec![0u8; 256 * 1024];
    report(Event::Bytes(received, total));
    loop {
        if cancel.load(Ordering::Acquire) {
            // The part file stays for a later resume (or stale-part cleanup).
            return Err(RemoteError::Cancelled);
        }
        let read = reader
            .read(&mut buffer)
            .map_err(|error| RemoteError::Unreachable(format!("GET {}: {error}", source.url)))?;
        if read == 0 {
            break;
        }
        output.write_all(&buffer[..read])?;
        received += read as u64;
        report(Event::Bytes(received, total));
    }
    output.sync_all()?;
    if total.is_some_and(|total| total != received) {
        return Err(RemoteError::Protocol(format!(
            "download truncated: received {received} of {} bytes",
            total.unwrap_or_default()
        )));
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::SourceAuth;
    use crate::test_server::{TestResponse, TestServer};
    use std::sync::{Arc, Mutex};

    const DISPOSITION: &str =
        "attachment; filename=\"audio-7.wav\"; filename*=UTF-8''%E9%9B%B7%E5%A3%B0%20%E8%BF%91.wav";

    #[test]
    fn content_disposition_prefers_the_rfc5987_name() {
        assert_eq!(
            content_disposition_filename(DISPOSITION).as_deref(),
            Some("雷声 近.wav")
        );
        assert_eq!(
            content_disposition_filename(
                "attachment; filename=\"door.wav\"; filename*=UTF-8''door.wav"
            )
            .as_deref(),
            Some("door.wav")
        );
        // Order does not matter; a broken extended value falls back to the plain name.
        assert_eq!(
            content_disposition_filename(
                "attachment; filename*=UTF-8''%E9%9B%B7.wav; filename=\"a.wav\""
            )
            .as_deref(),
            Some("雷.wav")
        );
        assert_eq!(
            content_disposition_filename(
                "attachment; filename*=UTF-8''%ZZ.wav; filename=\"a.wav\""
            )
            .as_deref(),
            Some("a.wav")
        );
        assert_eq!(
            content_disposition_filename(r#"attachment; filename="say \"hi\"; ok.wav""#).as_deref(),
            Some("say \"hi\"; ok.wav")
        );
        assert_eq!(
            content_disposition_filename("attachment; FILENAME=plain.wav").as_deref(),
            Some("plain.wav")
        );
        assert_eq!(
            content_disposition_filename("attachment; filename*=iso-8859-1'en'caf%E9.wav")
                .as_deref(),
            Some("café.wav")
        );
        assert_eq!(content_disposition_filename("attachment"), None);
        assert_eq!(content_disposition_filename("inline; filename=\"\""), None);
    }

    #[test]
    fn file_names_are_made_safe_for_windows() {
        assert_eq!(
            sanitize_filename("a<b>:c\"d/e\\f|g?h*.wav", "x.wav"),
            "a_b__c_d_e_f_g_h_.wav"
        );
        assert_eq!(sanitize_filename("雷声 近.wav", "x.wav"), "雷声 近.wav");
        assert_eq!(sanitize_filename("CON.wav", "x.wav"), "_CON.wav");
        assert_eq!(sanitize_filename("nul", "x.wav"), "_nul");
        assert_eq!(sanitize_filename("com1.tar.gz", "x.wav"), "_com1.tar.gz");
        assert_eq!(sanitize_filename("console.wav", "x.wav"), "console.wav");
        assert_eq!(sanitize_filename("take 1. . ", "x.wav"), "take 1");
        assert_eq!(
            sanitize_filename("tab\there\n.wav", "x.wav"),
            "tab_here_.wav"
        );
        assert_eq!(sanitize_filename("", "audio-7.wav"), "audio-7.wav");
        assert_eq!(sanitize_filename("../..", "audio-7.wav"), "audio-7.wav");
        let long = sanitize_filename(&format!("{}.wav", "长".repeat(300)), "x.wav");
        assert_eq!(long.chars().count(), MAX_NAME_CHARS);
        assert!(long.ends_with(".wav"));
        assert_eq!(etag_token("\"abc\"").len(), 16);
        assert_ne!(etag_token("\"abc\""), etag_token("\"abd\""));
    }

    struct Fixture {
        etag: String,
        body: Vec<u8>,
        gets: Vec<Option<String>>,
        heads: usize,
        cut_first_get: Option<usize>,
        /// Delay of the first request (a NAS disk waking up).
        wake: Option<Duration>,
        /// The record's root is offline (§6.4).
        root_offline: bool,
    }

    fn server(fixture: Arc<Mutex<Fixture>>) -> TestServer {
        TestServer::start(move |request| {
            let mut fixture = fixture.lock().unwrap();
            if request.path != "/api/v1/audio/7/stream?download=1"
                && request.path != "/api/v1/audio/7/stream"
            {
                return TestResponse::json(404, r#"{"error_code":"NOT_FOUND","message":"no"}"#);
            }
            let etag = fixture.etag.clone();
            let body = fixture.body.clone();
            if fixture.root_offline {
                let message = if request.method == "HEAD" {
                    String::new()
                } else {
                    OFFLINE_BODY.to_owned()
                };
                return TestResponse::json(503, &message).with_header("Retry-After", "5");
            }
            if request.method == "HEAD" {
                fixture.heads += 1;
                let response = TestResponse::bytes(200, body)
                    .with_header("ETag", &etag)
                    .with_header("Content-Disposition", DISPOSITION);
                return match fixture.wake.take() {
                    Some(delay) => response.delayed(delay),
                    None => response,
                };
            }
            fixture
                .gets
                .push(request.header("range").map(str::to_owned));
            if let (Some(range), Some(if_range)) =
                (request.header("range"), request.header("if-range"))
                && if_range == etag
            {
                let start: usize = range
                    .trim_start_matches("bytes=")
                    .trim_end_matches('-')
                    .parse()
                    .unwrap();
                let total = body.len();
                return TestResponse::bytes(206, body[start..].to_vec())
                    .with_header("ETag", &etag)
                    .with_header(
                        "Content-Range",
                        &format!("bytes {start}-{}/{total}", total - 1),
                    );
            }
            let response = TestResponse::bytes(200, body)
                .with_header("ETag", &etag)
                .with_header("Content-Disposition", DISPOSITION);
            match fixture.cut_first_get.take() {
                Some(bytes) => response.cut_after(bytes),
                None => response,
            }
        })
    }

    fn record() -> AudioFile {
        AudioFile::from(audiolib_core::AudioFileData {
            id: 7,
            file_path: "/Volumes/SFX/thunder.wav".into(),
            filename: "雷声 近.wav".into(),
            file_size: 300_000,
            ..audiolib_core::AudioFileData::default()
        })
    }

    #[test]
    fn empty_download_names_retain_the_supported_original_type() {
        for extension in ["MP3", "aif", "aiff", "bwf", "flac", "ogg"] {
            let mut file = record();
            file.filename.clear();
            file.file_path = format!("/sounds/source.{extension}");
            let name = sanitize_filename("", &fallback_audio_filename(&file));
            assert_eq!(
                Path::new(&name).extension().unwrap().to_str().unwrap(),
                extension.to_ascii_lowercase()
            );
        }
        let mut unknown = record();
        unknown.filename.clear();
        unknown.file_path = "/sounds/unknown.exe".into();
        assert!(fallback_audio_filename(&unknown).ends_with(".audio"));
    }

    fn item(server: &TestServer) -> DownloadItem {
        DownloadItem {
            file: record(),
            source: StreamSource::new(
                format!("{}/api/v1/audio/7/stream?download=1", server.url),
                SourceAuth::None,
            ),
        }
    }

    fn fixture(etag: &str, fill: u8) -> Arc<Mutex<Fixture>> {
        Arc::new(Mutex::new(Fixture {
            etag: etag.into(),
            body: vec![fill; 300_000],
            gets: Vec::new(),
            heads: 0,
            cut_first_get: None,
            wake: None,
            root_offline: false,
        }))
    }

    const OFFLINE_BODY: &str = r#"{"error_code":"UNAVAILABLE","message":"Library root \"SFX\" is offline: Root directory not found (volume not mounted?)"}"#;

    #[test]
    fn an_offline_root_reports_the_server_reason_or_uses_an_earlier_download() {
        let directory = tempfile::tempdir().unwrap();
        let state = fixture("\"v1\"", 4);
        let server = server(state.clone());
        let cache = DownloadCache::new(directory.path().join("cache"), u64::MAX);
        let cancel = AtomicBool::new(false);
        let first = cache
            .ensure_all("lib-1", &[item(&server)], None, &mut |_| {}, &cancel)
            .unwrap();
        state.lock().unwrap().root_offline = true;
        // An earlier complete download is used while the root is offline.
        let again = cache
            .ensure_all("lib-1", &[item(&server)], None, &mut |_| {}, &cancel)
            .unwrap();
        assert_eq!(again, first);
        // Nothing cached: the server's reason, at once and without a retry loop.
        let empty = DownloadCache::new(directory.path().join("other"), u64::MAX);
        let started = std::time::Instant::now();
        let result = empty.ensure_all("lib-1", &[item(&server)], None, &mut |_| {}, &cancel);
        match result {
            Err(RemoteError::RootOffline {
                message,
                retry_after,
            }) => {
                assert!(message.contains("\"SFX\" is offline"), "{message}");
                assert_eq!(retry_after, Some(5));
            }
            other => panic!("{other:?}"),
        }
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    fn waking_fixture(delay: Duration) -> Arc<Mutex<Fixture>> {
        let state = fixture("\"v1\"", 2);
        state.lock().unwrap().wake = Some(delay);
        state
    }

    #[test]
    fn a_waking_disk_is_waited_for_and_the_wait_is_reported() {
        let directory = tempfile::tempdir().unwrap();
        let server = server(waking_fixture(Duration::from_secs(3)));
        let cache = DownloadCache::new(directory.path().join("cache"), u64::MAX);
        let mut reports = Vec::new();
        let started = std::time::Instant::now();
        let paths = cache
            .ensure_all(
                "lib-1",
                &[item(&server)],
                None,
                &mut |p| reports.push(p),
                &AtomicBool::new(false),
            )
            .unwrap();
        assert!(started.elapsed() >= Duration::from_millis(2800));
        assert_eq!(std::fs::read(&paths[0]).unwrap(), vec![2u8; 300_000]);
        let longest = reports.iter().filter_map(|p| p.waiting).max().unwrap();
        assert!(longest >= crate::WAKING_HINT_AFTER, "{longest:?}");
        assert_eq!(reports.last().unwrap().received, 300_000);
    }

    #[test]
    fn waiting_for_a_waking_disk_is_cancelled_promptly() {
        let directory = tempfile::tempdir().unwrap();
        let server = server(waking_fixture(Duration::from_secs(3)));
        let cache = DownloadCache::new(directory.path().join("cache"), u64::MAX);
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(500));
                cancel.store(true, Ordering::Release);
            });
        }
        let started = std::time::Instant::now();
        let result = cache.ensure_all("lib-1", &[item(&server)], None, &mut |_| {}, &cancel);
        assert!(matches!(result, Err(RemoteError::Cancelled)), "{result:?}");
        assert!(
            started.elapsed() < Duration::from_millis(1200),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_disk_that_never_answers_times_out_after_the_limit() {
        let directory = tempfile::tempdir().unwrap();
        let server = server(waking_fixture(Duration::from_secs(3)));
        let cache = DownloadCache::new(directory.path().join("cache"), u64::MAX);
        let mut slow = item(&server);
        slow.source = slow.source.with_first_byte_timeout(Duration::from_secs(1));
        let started = std::time::Instant::now();
        let result = cache.ensure_all("lib-1", &[slow], None, &mut |_| {}, &AtomicBool::new(false));
        assert!(
            matches!(&result, Err(RemoteError::Timeout(message)) if message.contains("no response within 1 s")),
            "{result:?}"
        );
        assert!(started.elapsed() < Duration::from_millis(2500));
    }

    #[test]
    fn a_complete_download_is_used_when_revalidation_is_slow() {
        let directory = tempfile::tempdir().unwrap();
        let state = fixture("\"v1\"", 4);
        let server = server(state.clone());
        let cache = DownloadCache::new(directory.path().join("cache"), u64::MAX);
        let cancel = AtomicBool::new(false);
        let first = cache
            .ensure("lib-1", &item(&server), None, &mut |_, _| {}, &cancel)
            .unwrap();
        // The disk went to sleep: HEAD would take 8 s. The drag uses the cached copy.
        state.lock().unwrap().wake = Some(Duration::from_secs(8));
        let started = std::time::Instant::now();
        let again = cache
            .ensure("lib-1", &item(&server), None, &mut |_, _| {}, &cancel)
            .unwrap();
        assert_eq!(again, first);
        assert!(
            started.elapsed() < CACHED_REVALIDATE_TIMEOUT + Duration::from_millis(1500),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(
            state.lock().unwrap().gets.len(),
            1,
            "nothing downloaded again"
        );
    }

    #[test]
    fn downloads_are_cached_by_etag_and_revalidated_with_head() {
        let directory = tempfile::tempdir().unwrap();
        let state = fixture("\"v1\"", 1);
        let server = server(state.clone());
        let cache = DownloadCache::new(directory.path().join("cache"), u64::MAX);
        let cancel = AtomicBool::new(false);
        let mut reports = Vec::new();
        let paths = cache
            .ensure_all(
                "lib-1",
                &[item(&server)],
                None,
                &mut |p| reports.push(p),
                &cancel,
            )
            .unwrap();
        let first = &paths[0];
        assert_eq!(first.file_name().unwrap().to_string_lossy(), "雷声 近.wav");
        let folder = first.parent().unwrap();
        assert_eq!(
            folder.file_name().unwrap().to_string_lossy(),
            format!("7-{}", etag_token("\"v1\""))
        );
        assert!(folder.starts_with(cache.library_dir("lib-1")));
        assert!(cache.library_dir("lib-1").join(MARKER_FILE).is_file());
        assert_eq!(std::fs::read(first).unwrap(), vec![1u8; 300_000]);
        assert_eq!(
            reports.last(),
            Some(&DownloadProgress {
                index: 0,
                count: 1,
                received: 300_000,
                total: Some(300_000),
                waiting: None,
            })
        );
        assert_eq!(state.lock().unwrap().gets.len(), 1);

        // Same ETag: HEAD only, the cached file is reused.
        let again = cache
            .ensure("lib-1", &item(&server), None, &mut |_, _| {}, &cancel)
            .unwrap();
        assert_eq!(&again, first);
        assert_eq!(state.lock().unwrap().gets.len(), 1);
        assert_eq!(state.lock().unwrap().heads, 2);

        // New ETag: downloaded again into a new version folder, the old one removed.
        {
            let mut state = state.lock().unwrap();
            state.etag = "\"v2\"".into();
            state.body = vec![2u8; 300_000];
        }
        let changed = cache
            .ensure("lib-1", &item(&server), None, &mut |_, _| {}, &cancel)
            .unwrap();
        assert_ne!(&changed, first);
        assert_eq!(std::fs::read(&changed).unwrap(), vec![2u8; 300_000]);
        assert!(!folder.exists(), "stale version folder removed");
        assert_eq!(state.lock().unwrap().gets.len(), 2);

        // Cancelled before starting: nothing is fetched.
        assert!(matches!(
            cache.ensure_all(
                "lib-1",
                &[item(&server)],
                None,
                &mut |_| {},
                &AtomicBool::new(true)
            ),
            Err(RemoteError::Cancelled)
        ));
    }

    #[test]
    fn owned_original_retains_exact_version_across_eviction_and_offline_use() {
        let directory = tempfile::tempdir().unwrap();
        let state = fixture("\"owned-v1\"", 6);
        let server = server(state.clone());
        let cache = DownloadCache::new(directory.path().join("cache"), 0);
        let stage = tempfile::tempdir_in(directory.path()).unwrap();
        let first_path = stage.path().join("original.wav");
        let first = cache
            .prepare_owned_original(
                "lib-1",
                &item(&server),
                None,
                &first_path,
                &mut |_| {},
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(first.path, first_path);
        assert_eq!(first.etag.as_deref(), Some("\"owned-v1\""));
        assert_eq!(first.length, 300_000);
        assert!(!first.offline);
        let cached = cache
            .newest_complete(&cache.library_dir("lib-1"), 7)
            .unwrap();
        // The version receipt is outside each audio entry and cannot win the
        // newest-original selection even if its mtime is newer than the WAV.
        assert_eq!(cached.file_name().unwrap(), "雷声 近.wav");
        state.lock().unwrap().root_offline = true;
        let second_path = stage.path().join("offline.wav");
        let second = cache
            .prepare_owned_original(
                "lib-1",
                &item(&server),
                None,
                &second_path,
                &mut |_| {},
                &AtomicBool::new(false),
            )
            .unwrap();
        assert!(second.offline);
        assert_eq!(second.etag, first.etag);
        cache.evict(&[]);
        assert_eq!(std::fs::read(&first.path).unwrap(), vec![6; 300_000]);
        assert_eq!(std::fs::read(&second.path).unwrap(), vec![6; 300_000]);
        assert_eq!(state.lock().unwrap().gets.len(), 1);
    }

    #[test]
    fn staging_does_not_replace_files_or_publish_cancelled_work() {
        let directory = tempfile::tempdir().unwrap();
        let state = fixture("\"owned-v1\"", 4);
        let server = server(state);
        let cache = DownloadCache::new(directory.path().join("cache"), u64::MAX);
        let target = directory.path().join("keep.wav");
        std::fs::write(&target, b"keep").unwrap();
        assert!(
            cache
                .prepare_owned_original(
                    "lib-1",
                    &item(&server),
                    None,
                    &target,
                    &mut |_| {},
                    &AtomicBool::new(false),
                )
                .is_err()
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
        let cancelled = directory.path().join("cancelled.wav");
        let cancel = AtomicBool::new(false);
        assert!(matches!(
            cache.prepare_owned_original(
                "lib-1",
                &item(&server),
                None,
                &cancelled,
                &mut |progress| {
                    if progress.total == Some(progress.received) {
                        cancel.store(true, Ordering::Release);
                    }
                },
                &cancel,
            ),
            Err(RemoteError::Cancelled)
        ));
        assert!(!cancelled.exists());
    }

    #[test]
    fn an_offline_legacy_cache_reports_unknown_etag_without_inventing_one() {
        let directory = tempfile::tempdir().unwrap();
        let state = fixture("\"legacy\"", 5);
        let server = server(state.clone());
        let cache = DownloadCache::new(directory.path().join("cache"), u64::MAX);
        let cached = cache
            .ensure(
                "lib-1",
                &item(&server),
                None,
                &mut |_, _| {},
                &AtomicBool::new(false),
            )
            .unwrap();
        std::fs::remove_file(cache.source_version_path(&cached).unwrap()).unwrap();
        state.lock().unwrap().root_offline = true;
        let owned = cache
            .prepare_owned_original(
                "lib-1",
                &item(&server),
                None,
                &directory.path().join("legacy.wav"),
                &mut |_| {},
                &AtomicBool::new(false),
            )
            .unwrap();
        assert!(owned.offline);
        assert_eq!(owned.etag, None);
        assert_eq!(std::fs::read(&owned.path).unwrap(), vec![5; 300_000]);
    }

    #[test]
    fn exact_offline_version_can_use_an_older_verified_original_and_reject_unknown_receipts() {
        let directory = tempfile::tempdir().unwrap();
        let state = fixture("\"v2\"", 2);
        let server = server(state.clone());
        let cache = DownloadCache::new(directory.path().join("cache"), u64::MAX);
        let library = cache.library_dir("lib-1");
        let first = library
            .join(format!("7-{}", etag_token("\"v1\"")))
            .join("original.wav");
        let second = library
            .join(format!("7-{}", etag_token("\"v2\"")))
            .join("original.wav");
        for (path, etag, fill) in [(&first, "\"v1\"", 1), (&second, "\"v2\"", 2)] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, vec![fill; 300_000]).unwrap();
            cache
                .completed_original(
                    path.clone(),
                    &DownloadProbe {
                        etag: Some(etag.into()),
                        length: Some(300_000),
                        filename: Some("original.wav".into()),
                    },
                )
                .unwrap();
        }
        std::fs::OpenOptions::new()
            .write(true)
            .open(&second)
            .unwrap()
            .set_modified(SystemTime::now() + Duration::from_secs(60))
            .unwrap();
        assert_eq!(cache.newest_complete(&library, 7).unwrap(), second);
        state.lock().unwrap().root_offline = true;
        let expected = OriginalVersion {
            etag: "\"v1\"".into(),
            length: 300_000,
            offline: false,
        };
        assert_eq!(
            cache
                .original_version_expected(
                    "lib-1",
                    7,
                    &item(&server).source,
                    &AtomicBool::new(false),
                    &expected
                )
                .unwrap()
                .etag,
            expected.etag
        );
        let path = directory.path().join("exact-v1.wav");
        let original = cache
            .prepare_owned_original_version(
                "lib-1",
                &item(&server),
                None,
                (&path, &expected),
                &mut |_| {},
                &AtomicBool::new(false),
            )
            .unwrap();
        assert!(original.offline);
        assert_eq!(std::fs::read(&path).unwrap(), vec![1; 300_000]);
        std::fs::remove_file(cache.source_version_path(&first).unwrap()).unwrap();
        assert!(
            cache
                .original_version_expected(
                    "lib-1",
                    7,
                    &item(&server).source,
                    &AtomicBool::new(false),
                    &expected
                )
                .is_err(),
            "A legacy filename token cannot stand in for the missing v1 receipt"
        );
    }

    #[test]
    fn weak_or_missing_source_etags_are_never_export_identities() {
        assert!(strong_etag(None).is_err());
        assert!(strong_etag(Some("W/\"source\"".into())).is_err());
        assert!(strong_etag(Some("\"source\"".into())).is_ok());
    }

    #[test]
    fn complete_in_memory_original_with_the_same_etag_is_reused() {
        let directory = tempfile::tempdir().unwrap();
        let state = fixture("\"v1\"", 3);
        let server = server(state.clone());
        let playback = MemoryCache::new(crate::DEFAULT_MEMORY_CACHE_BYTES);
        // Audition streamed the original (`/stream`, no `?download=1`) completely.
        let stream = crate::MemoryStream::start(
            &playback,
            "7/stream",
            StreamSource::new(
                format!("{}/api/v1/audio/7/stream", server.url),
                SourceAuth::None,
            ),
        )
        .unwrap();
        stream.wait_complete(Duration::from_secs(5)).unwrap();
        state.lock().unwrap().gets.clear();
        let cache = DownloadCache::new(directory.path().join("cache"), u64::MAX);
        let path = cache
            .ensure(
                "lib-1",
                &item(&server),
                Some(&playback),
                &mut |_, _| {},
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), vec![3u8; 300_000]);
        assert!(state.lock().unwrap().gets.is_empty(), "no download needed");
        assert!(stream.is_complete(), "the audition copy stays");

        // Another version on the server: the in-memory copy is not reused.
        state.lock().unwrap().etag = "\"v2\"".into();
        let other = DownloadCache::new(directory.path().join("cache2"), u64::MAX);
        other
            .ensure(
                "lib-1",
                &item(&server),
                Some(&playback),
                &mut |_, _| {},
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(state.lock().unwrap().gets.len(), 1);
    }

    #[test]
    fn interrupted_download_resumes_with_range_and_if_range() {
        let directory = tempfile::tempdir().unwrap();
        let state = fixture("\"v1\"", 0);
        state.lock().unwrap().body = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        state.lock().unwrap().cut_first_get = Some(100_000);
        let server = server(state.clone());
        let cache = DownloadCache::new(directory.path().join("cache"), u64::MAX);
        let cancel = AtomicBool::new(false);
        let error = cache
            .ensure("lib-1", &item(&server), None, &mut |_, _| {}, &cancel)
            .unwrap_err();
        assert!(
            matches!(
                error,
                RemoteError::Protocol(_) | RemoteError::Unreachable(_)
            ),
            "{error:?}"
        );
        let path = cache
            .ensure("lib-1", &item(&server), None, &mut |_, _| {}, &cancel)
            .unwrap();
        let expected = state.lock().unwrap().body.clone();
        assert_eq!(std::fs::read(&path).unwrap(), expected);
        let gets = state.lock().unwrap().gets.clone();
        assert_eq!(gets.len(), 2);
        assert_eq!(gets[0], None);
        let resumed_at: u64 = gets[1]
            .as_deref()
            .unwrap()
            .trim_start_matches("bytes=")
            .trim_end_matches('-')
            .parse()
            .unwrap();
        assert!(resumed_at > 0 && resumed_at <= 100_000, "{gets:?}");
        assert!(
            std::fs::read_dir(path.parent().unwrap())
                .unwrap()
                .flatten()
                .all(|entry| !entry.file_name().to_string_lossy().ends_with(".part"))
        );
    }

    #[test]
    fn missing_record_and_offline_server_are_reported() {
        let directory = tempfile::tempdir().unwrap();
        let state = fixture("\"v1\"", 1);
        let server = server(state);
        let cache = DownloadCache::new(directory.path().join("cache"), u64::MAX);
        let mut missing = item(&server);
        missing.source.url = format!("{}/api/v1/audio/8/stream?download=1", server.url);
        missing.file.id = 8;
        assert!(matches!(
            cache.ensure(
                "lib-1",
                &missing,
                None,
                &mut |_, _| {},
                &AtomicBool::new(false)
            ),
            Err(RemoteError::NotFound(_))
        ));
        // Offline with a cached copy: the copy is used; without one: the error.
        let cached = cache
            .ensure(
                "lib-1",
                &item(&server),
                None,
                &mut |_, _| {},
                &AtomicBool::new(false),
            )
            .unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let mut offline = item(&server);
        offline.source.url = format!("http://127.0.0.1:{port}/api/v1/audio/7/stream?download=1");
        let used = cache
            .ensure(
                "lib-1",
                &offline,
                None,
                &mut |_, _| {},
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(used, cached);
        assert!(
            cache
                .ensure(
                    "lib-2",
                    &offline,
                    None,
                    &mut |_, _| {},
                    &AtomicBool::new(false)
                )
                .unwrap_err()
                .is_offline()
        );
    }

    #[test]
    fn eviction_only_touches_marked_download_folders() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("cache");
        let token = etag_token("x");
        let write = |path: &Path, size: usize| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, vec![0u8; size]).unwrap();
        };
        let library = root.join("lib-a");
        write(&library.join(MARKER_FILE), 10);
        let oldest = library.join(format!("1-{token}")).join("a.wav");
        let middle = library.join(format!("2-{token}")).join("b.wav");
        let newest = library.join(format!("3-{token}")).join("c.wav");
        for (path, age) in [(&oldest, 300), (&middle, 200), (&newest, 100)] {
            write(path, 1000);
            let handle = std::fs::OpenOptions::new().write(true).open(path).unwrap();
            handle
                .set_modified(SystemTime::now() - Duration::from_secs(age))
                .unwrap();
        }
        // Other data under cache/ and outside it must never be touched.
        let waveforms = root
            .join("waveforms")
            .join(format!("4-{token}"))
            .join("w.bin");
        write(&waveforms, 50_000);
        let unmarked = root.join("lib-b").join(format!("5-{token}")).join("d.wav");
        write(&unmarked, 50_000);
        let stray = library.join("notes.txt");
        write(&stray, 50_000);
        let outside = directory.path().join(format!("6-{token}")).join("e.wav");
        write(&outside, 50_000);
        let stale_part = library.join(format!("7-{token}")).join("f.wav.part");
        write(&stale_part, 50);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&stale_part)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(2 * 24 * 3600))
            .unwrap();

        let cache = DownloadCache::new(root.clone(), 2000);
        cache.evict(std::slice::from_ref(&oldest));
        assert!(oldest.is_file(), "kept: in use by the current drag");
        assert!(!middle.exists(), "least recently used goes first");
        assert!(newest.is_file());
        assert!(!stale_part.exists() && !stale_part.parent().unwrap().exists());
        for untouched in [&waveforms, &unmarked, &stray, &outside] {
            assert!(untouched.is_file(), "{untouched:?}");
        }
        DownloadCache::new(root.clone(), 0).evict(&[]);
        assert!(!oldest.exists() && !newest.exists());
        for untouched in [&waveforms, &unmarked, &stray, &outside] {
            assert!(untouched.is_file(), "{untouched:?}");
        }
        assert!(library.join(MARKER_FILE).is_file());
    }
}
