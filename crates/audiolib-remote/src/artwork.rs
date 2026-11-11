// crates/audiolib-remote/src/artwork.rs
//! Optional embedded-cover HTTP reads. Images never enter the snapshot or audio stream.
//!
//! The server keeps two decode slots. When they stay full it queues for about 500ms,
//! then answers `503 BUSY` with `Retry-After: 1` (seconds). This client never opens a
//! third cover request. Busy backoff itself lives with the desktop worker, which can
//! see the current job; [`cover_busy_delay`] is that wait. HTTP 405 is logged once per
//! server and still means no cover. HTTP 401, library-identity conflicts and protocol
//! errors are not retried here.
use crate::{Capabilities, RemoteClient, RemoteError, Result};
use std::collections::HashSet;
use std::sync::{
    Condvar, LazyLock, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

/// Largest encoded cover accepted from a server, matching the bounded image decoder.
pub const MAX_ARTWORK_BYTES: u64 = 8 * 1024 * 1024;
const ARTWORK_TIMEOUT: Duration = Duration::from_secs(30);
/// Cover requests this process will run at once. The server has the same two slots.
const ARTWORK_CLIENT_SLOTS: usize = 2;
/// Wait when `Retry-After` is missing or not a positive number of seconds.
const ARTWORK_BUSY_DEFAULT_WAIT: Duration = Duration::from_secs(1);
/// Longest wait for one busy retry. `Retry-After: 1` stays one second plus jitter.
const ARTWORK_BUSY_MAX_WAIT: Duration = Duration::from_secs(10);
/// Extra delay added after [`crate::error::retry_wait`], so clients do not retry in step.
const ARTWORK_BUSY_JITTER: Duration = Duration::from_millis(250);

fn canonical_artwork_directory(directory: &str) -> Result<String> {
    if directory.len() > 4096 {
        return Err(RemoteError::Validation(
            "Source directory exceeds 4096 UTF-8 bytes".into(),
        ));
    }
    let canonical = audiolib_db::normalize_source_path_identity(directory);
    if canonical.len() > 4096 {
        return Err(RemoteError::Validation(
            "Canonical source directory exceeds 4096 UTF-8 bytes".into(),
        ));
    }
    Ok(canonical)
}

fn directory_identity_hex(directory: &str) -> String {
    use std::fmt::Write as _;
    let mut hex = String::with_capacity(directory.len() * 2);
    for byte in directory.bytes() {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

fn directory_query(directory: &str) -> String {
    use std::fmt::Write as _;
    let mut query = String::new();
    for byte in directory.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            query.push(char::from(byte));
        } else {
            let _ = write!(query, "%{byte:02X}");
        }
    }
    query
}

fn native_album_source(state: &crate::CacheState) -> bool {
    state
        .format
        .as_ref()
        .and_then(|format| format.source.as_ref())
        .map_or(state.schema_version, |source| {
            i64::from(source.schema_version)
        })
        >= 8
}

/// An optional image response; decoding remains on the caller's image worker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArtworkReply {
    /// The caller's ETag still identifies the current image.
    NotModified,
    /// No image, or an older server without the endpoint (`404`/`405`).
    /// `405` is logged; both still present as no cover.
    Missing,
    /// A bounded JPEG/PNG response with its opaque entity tag.
    Data {
        bytes: Vec<u8>,
        etag: Option<String>,
        mime: String,
    },
}

impl Capabilities {
    /// Indexed-folder and NULL-source-parent semantics need this explicit API capability.
    pub fn album_folder_artwork_usable(&self) -> bool {
        self.album_entity_artwork_usable()
            && self.compatibility.as_ref().is_some_and(|manifest| {
                manifest
                    .features
                    .get(audiolib_core::compatibility::ALBUM_FOLDER_ARTWORK_FEATURE)
                    == Some(&audiolib_core::compatibility::ALBUM_FOLDER_ARTWORK_REVISION)
            })
    }

    /// A shared album representation is separate from the historical WAV endpoint.
    pub fn album_entity_artwork_usable(&self) -> bool {
        self.supported
            && self.compatibility.as_ref().is_some_and(|manifest| {
                manifest
                    .features
                    .get(audiolib_core::compatibility::ALBUM_ENTITY_ARTWORK_FEATURE)
                    == Some(&audiolib_core::compatibility::ALBUM_ENTITY_ARTWORK_REVISION)
            })
    }

    /// Covers are opt-in: an absent `album_artwork` marker disables every image request.
    pub fn album_artwork_usable(&self) -> bool {
        self.supported
            && self.compatibility.as_ref().is_some_and(|manifest| {
                manifest
                    .features
                    .get(audiolib_core::compatibility::ALBUM_ARTWORK_FEATURE)
                    .is_some_and(|revision| *revision >= 1)
            })
    }
}

struct ArtworkSlots {
    available: Mutex<usize>,
    wake: Condvar,
}

struct ArtworkPermit<'a> {
    slots: &'a ArtworkSlots,
}

impl Drop for ArtworkPermit<'_> {
    fn drop(&mut self) {
        let mut available = self.slots.available.lock().unwrap();
        *available += 1;
        self.slots.wake.notify_one();
    }
}

impl ArtworkSlots {
    const fn new(capacity: usize) -> Self {
        Self {
            available: Mutex::new(capacity),
            wake: Condvar::new(),
        }
    }

    fn acquire<'a>(&'a self, cancel: &AtomicBool) -> Result<ArtworkPermit<'a>> {
        let mut available = self.available.lock().unwrap();
        loop {
            if cancel.load(Ordering::Acquire) {
                return Err(RemoteError::Cancelled);
            }
            if *available > 0 {
                *available -= 1;
                return Ok(ArtworkPermit { slots: self });
            }
            let (guard, _) = self
                .wake
                .wait_timeout(available, Duration::from_millis(50))
                .unwrap();
            available = guard;
        }
    }
}

/// Process-wide cap. A waiting caller checks cancellation instead of opening a
/// third socket that the server would queue and then reject.
static ARTWORK_SLOTS: ArtworkSlots = ArtworkSlots::new(ARTWORK_CLIENT_SLOTS);

fn acquire_artwork_slot(cancel: &AtomicBool) -> Result<ArtworkPermit<'static>> {
    ARTWORK_SLOTS.acquire(cancel)
}

/// Servers that already logged HTTP 405 for artwork. One warning per API base URL.
static ARTWORK_405_WARNED: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// `true` the first time `server` is seen, so the 405 warning is logged once.
fn claim_artwork_method_not_allowed(server: &str) -> bool {
    ARTWORK_405_WARNED.lock().unwrap().insert(server.to_owned())
}

/// Wait before another cover read after [`RemoteError::Busy`].
///
/// Uses [`crate::error::retry_wait`]: a positive `Retry-After` is seconds, otherwise
/// one second, and the result never exceeds [`ARTWORK_BUSY_MAX_WAIT`]. A jitter of at
/// most 250ms is added so retries do not line up, still inside that cap. Callers must
/// not use this for 401, library-identity or protocol failures.
pub fn cover_busy_delay(retry_after: Option<u64>) -> Duration {
    let base = crate::error::retry_wait(
        ARTWORK_BUSY_DEFAULT_WAIT,
        retry_after,
        ARTWORK_BUSY_MAX_WAIT,
    );
    let room = ARTWORK_BUSY_MAX_WAIT.saturating_sub(base);
    let jitter = jitter_up_to(ARTWORK_BUSY_JITTER.min(room));
    base.saturating_add(jitter)
}

fn jitter_up_to(max: Duration) -> Duration {
    let millis = max.as_millis() as u64;
    if millis == 0 {
        return Duration::ZERO;
    }
    static MIX: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0xA5A5_1234);
    let tick = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|time| time.subsec_nanos())
        .unwrap_or(0);
    let mixed = MIX.fetch_add(tick ^ 0x9E37_79B9, Ordering::Relaxed) ^ tick;
    Duration::from_millis(u64::from(mixed) % (millis + 1))
}

impl RemoteClient {
    /// Read a cover only after [`Capabilities::album_artwork_usable`] is true.
    /// This unbound form supports older artwork endpoints. Snapshot readers use
    /// [`Self::artwork_for_library`]; current servers require that library binding.
    ///
    /// `side` bounds the long side to `1..=512`; omission requests the embedded image.
    /// Cancellation closes the request socket, including while a sleeping root opens.
    /// The caller must use a bounded decoder before displaying or caching pixels.
    /// Snapshot consumers use [`Self::artwork_for_library`]; identity-bound servers
    /// reject this older, unbound transport entry point.
    pub fn artwork(
        &self,
        id: i64,
        side: Option<u32>,
        if_none_match: Option<&str>,
        cancel: &AtomicBool,
    ) -> Result<ArtworkReply> {
        self.artwork_request(None, None, None, None, id, side, if_none_match, cancel)
    }

    /// Read a snapshot record's cover with an atomic server-side library identity check.
    /// Successful responses must repeat that identity, including conditional `304`s.
    pub fn artwork_for_library(
        &self,
        library_id: &str,
        id: i64,
        side: Option<u32>,
        if_none_match: Option<&str>,
        cancel: &AtomicBool,
    ) -> Result<ArtworkReply> {
        if library_id.trim().is_empty() || ureq::http::HeaderValue::from_str(library_id).is_err() {
            return Err(RemoteError::Validation(
                "Invalid cached library identity".into(),
            ));
        }
        self.artwork_request(
            Some(library_id),
            None,
            None,
            None,
            id,
            side,
            if_none_match,
            cancel,
        )
    }

    /// Read a server-authoritative album and verify its library, namespace and ID.
    #[allow(clippy::too_many_arguments)]
    pub fn album_artwork_for_library(
        &self,
        library_id: &str,
        expected_namespace: &str,
        album_id: i64,
        side: Option<u32>,
        if_none_match: Option<&str>,
        cancel: &AtomicBool,
    ) -> Result<ArtworkReply> {
        if album_id <= 0
            || library_id.trim().is_empty()
            || expected_namespace.trim().is_empty()
            || ureq::http::HeaderValue::from_str(library_id).is_err()
            || ureq::http::HeaderValue::from_str(expected_namespace).is_err()
        {
            return Err(RemoteError::Validation(
                "Invalid album or library identity".into(),
            ));
        }
        self.artwork_request(
            Some(library_id),
            Some(expected_namespace),
            None,
            None,
            album_id,
            side,
            if_none_match,
            cancel,
        )
    }

    /// Read only a native album's direct folder members. Callers first negotiate
    /// [`Capabilities::album_folder_artwork_usable`]; both 200 and 304 must repeat
    /// the exact folder identity, so an old server ignoring the query is rejected.
    #[allow(clippy::too_many_arguments)]
    pub fn album_artwork_in_folder_for_library(
        &self,
        library_id: &str,
        expected_namespace: &str,
        album_id: i64,
        folder_id: i64,
        side: Option<u32>,
        if_none_match: Option<&str>,
        cancel: &AtomicBool,
    ) -> Result<ArtworkReply> {
        if album_id <= 0
            || library_id.trim().is_empty()
            || expected_namespace.trim().is_empty()
            || ureq::http::HeaderValue::from_str(library_id).is_err()
            || ureq::http::HeaderValue::from_str(expected_namespace).is_err()
        {
            return Err(RemoteError::Validation(
                "Invalid album or library identity".into(),
            ));
        }
        self.artwork_request(
            Some(library_id),
            Some(expected_namespace),
            Some(folder_id),
            None,
            album_id,
            side,
            if_none_match,
            cancel,
        )
    }

    /// Read NULL-folder members of one canonical source parent, including the
    /// empty parent of a flat relative filename. Both successful status codes
    /// must repeat its lowercase UTF-8 hex identity; no filesystem lookup occurs.
    #[allow(clippy::too_many_arguments)]
    pub fn album_artwork_in_unassigned_directory_for_library(
        &self,
        library_id: &str,
        expected_namespace: &str,
        album_id: i64,
        directory: &str,
        side: Option<u32>,
        if_none_match: Option<&str>,
        cancel: &AtomicBool,
    ) -> Result<ArtworkReply> {
        self.album_artwork_bound_directory_for_library(
            library_id,
            expected_namespace,
            album_id,
            None,
            directory,
            side,
            if_none_match,
            cancel,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn album_artwork_bound_directory_for_library(
        &self,
        library_id: &str,
        expected_namespace: &str,
        album_id: i64,
        folder_id: Option<i64>,
        directory: &str,
        side: Option<u32>,
        if_none_match: Option<&str>,
        cancel: &AtomicBool,
    ) -> Result<ArtworkReply> {
        if album_id <= 0
            || library_id.trim().is_empty()
            || expected_namespace.trim().is_empty()
            || ureq::http::HeaderValue::from_str(library_id).is_err()
            || ureq::http::HeaderValue::from_str(expected_namespace).is_err()
        {
            return Err(RemoteError::Validation(
                "Invalid album or library identity".into(),
            ));
        }
        let directory = canonical_artwork_directory(directory)?;
        self.artwork_request(
            Some(library_id),
            Some(expected_namespace),
            folder_id,
            Some(&directory),
            album_id,
            side,
            if_none_match,
            cancel,
        )
    }

    /// Unified selected-directory resolver. Numeric folder IDs remain native
    /// server identities; NULL-folder members use a normalized source parent.
    /// Legacy sources receive only the prefiltered bounded audio IDs.
    #[allow(clippy::too_many_arguments)]
    pub fn album_artwork_in_directory_for_snapshot(
        &self,
        state: &crate::CacheState,
        capabilities: &Capabilities,
        expected_namespace: &str,
        album_id: i64,
        folder_id: Option<i64>,
        directory: &str,
        member_ids: &[i64],
        side: Option<u32>,
        if_none_match: Option<&str>,
        cancel: &AtomicBool,
    ) -> Result<ArtworkReply> {
        if cancel.load(std::sync::atomic::Ordering::Acquire) {
            return Err(RemoteError::Cancelled);
        }
        if member_ids.len() > audiolib_core::MAX_ALBUM_MEMBERS {
            return Err(RemoteError::Validation(
                "Too many album artwork candidates".into(),
            ));
        }
        if native_album_source(state) {
            if !capabilities.album_folder_artwork_usable() {
                return Err(RemoteError::Protocol("This server does not support directory artwork; upgrade the server to display directory covers".into()));
            }
            return self.album_artwork_bound_directory_for_library(
                &state.library_id,
                expected_namespace,
                album_id,
                folder_id,
                directory,
                side,
                if_none_match,
                cancel,
            );
        }
        self.album_artwork_for_snapshot(
            state,
            capabilities,
            expected_namespace,
            album_id,
            member_ids,
            side,
            if_none_match,
            cancel,
        )
    }

    /// Resolve directory artwork using native IDs only for a native schema-8
    /// source. Missing native folder capability requires a server upgrade, never
    /// a downgrade to unbound namespace semantics. Legacy member IDs must already
    /// be filtered by direct folder before the caller's bounded candidate limit.
    #[allow(clippy::too_many_arguments)]
    pub fn album_artwork_in_folder_for_snapshot(
        &self,
        state: &crate::CacheState,
        capabilities: &Capabilities,
        expected_namespace: &str,
        album_id: i64,
        folder_id: i64,
        member_ids: &[i64],
        side: Option<u32>,
        if_none_match: Option<&str>,
        cancel: &AtomicBool,
    ) -> Result<ArtworkReply> {
        if cancel.load(std::sync::atomic::Ordering::Acquire) {
            return Err(RemoteError::Cancelled);
        }
        if member_ids.len() > audiolib_core::MAX_ALBUM_MEMBERS {
            return Err(RemoteError::Validation(
                "Too many album artwork candidates".into(),
            ));
        }
        if native_album_source(state) {
            if !capabilities.album_folder_artwork_usable() {
                return Err(RemoteError::Protocol("This server does not support folder artwork; upgrade the server to display directory covers".into()));
            }
            return self.album_artwork_in_folder_for_library(
                &state.library_id,
                expected_namespace,
                album_id,
                folder_id,
                side,
                if_none_match,
                cancel,
            );
        }
        self.album_artwork_for_snapshot(
            state,
            capabilities,
            expected_namespace,
            album_id,
            member_ids,
            side,
            if_none_match,
            cancel,
        )
    }

    /// Use authoritative album IDs only for an original schema-8 server snapshot.
    /// Legacy projections use the same sorted whole-album member candidates on every
    /// selection, never a client-generated album ID or the currently selected WAV.
    #[allow(clippy::too_many_arguments)]
    pub fn album_artwork_for_snapshot(
        &self,
        state: &crate::CacheState,
        capabilities: &Capabilities,
        expected_namespace: &str,
        album_id: i64,
        member_ids: &[i64],
        side: Option<u32>,
        if_none_match: Option<&str>,
        cancel: &AtomicBool,
    ) -> Result<ArtworkReply> {
        if cancel.load(std::sync::atomic::Ordering::Acquire) {
            return Err(RemoteError::Cancelled);
        }
        if member_ids.len() > audiolib_core::MAX_ALBUM_MEMBERS {
            return Err(RemoteError::Validation(
                "Too many album artwork candidates".into(),
            ));
        }
        let authoritative = state
            .format
            .as_ref()
            .and_then(|format| format.source.as_ref())
            .is_some_and(|format| {
                format.schema_version >= 8 && format.features.get("album_entities") == Some(&1)
            });
        if authoritative && capabilities.album_entity_artwork_usable() {
            return self.album_artwork_for_library(
                &state.library_id,
                expected_namespace,
                album_id,
                side,
                if_none_match,
                cancel,
            );
        }
        if !capabilities.album_artwork_usable() {
            return Ok(ArtworkReply::Missing);
        }
        let mut candidates = member_ids.to_vec();
        candidates.sort_unstable();
        candidates.dedup();
        candidates.truncate(audiolib_core::MAX_ALBUM_MEMBERS);
        for id in candidates {
            match self.artwork_for_library(&state.library_id, id, side, if_none_match, cancel)? {
                ArtworkReply::Missing => {}
                reply => return Ok(reply),
            }
        }
        Ok(ArtworkReply::Missing)
    }

    #[allow(clippy::too_many_arguments)]
    fn artwork_request(
        &self,
        library_id: Option<&str>,
        expected_namespace: Option<&str>,
        expected_folder: Option<i64>,
        expected_directory: Option<&str>,
        id: i64,
        side: Option<u32>,
        if_none_match: Option<&str>,
        cancel: &AtomicBool,
    ) -> Result<ArtworkReply> {
        if side.is_some_and(|side| !(1..=512).contains(&side)) {
            return Err(RemoteError::Validation(
                "artwork side must be 1..=512".into(),
            ));
        }
        let collection = if expected_namespace.is_some() {
            "albums"
        } else {
            "audio"
        };
        let path = if let Some(directory) = expected_directory.filter(|_| expected_folder.is_none())
        {
            let query = directory_query(directory);
            match side {
                Some(side) => {
                    format!("/{collection}/{id}/artwork?unassigned_directory={query}&side={side}")
                }
                None => format!("/{collection}/{id}/artwork?unassigned_directory={query}"),
            }
        } else {
            match (side, expected_folder) {
                (Some(side), Some(folder)) => {
                    format!("/{collection}/{id}/artwork?folder_id={folder}&side={side}")
                }
                (None, Some(folder)) => format!("/{collection}/{id}/artwork?folder_id={folder}"),
                (Some(side), None) => format!("/{collection}/{id}/artwork?side={side}"),
                (None, None) => format!("/{collection}/{id}/artwork"),
            }
        };
        let library_id = library_id.map(str::to_owned);
        let expected_namespace = expected_namespace.map(str::to_owned);
        let expected_directory = expected_directory.map(directory_identity_hex);
        let if_none_match = if_none_match.map(str::to_owned);
        if cancel.load(Ordering::Acquire) {
            return Err(RemoteError::Cancelled);
        }
        // Held only for this exchange. The desktop worker retries Busy after the
        // permit is released, and sends the same If-None-Match again.
        let permit = acquire_artwork_slot(cancel)?;
        let client = self.clone();
        crate::client::call_cancellable(
            move |abort| {
                let _permit = permit;
                let url = format!("{}{path}", client.api);
                let context = format!("GET {path}");
                let agent = crate::abort::abortable_agent(ARTWORK_TIMEOUT, ARTWORK_TIMEOUT, abort);
                let response = crate::abort::call_fresh(|| {
                    let mut request = agent.get(&url).header("Accept", "image/jpeg, image/png");
                    if !client.token.is_empty() {
                        request =
                            request.header("Authorization", &format!("Bearer {}", client.token));
                    }
                    if let Some(etag) = &if_none_match {
                        request = request.header("If-None-Match", etag);
                    }
                    if let Some(library_id) = &library_id {
                        request = request.header(crate::LIBRARY_ID_HEADER, library_id);
                    }
                    if let Some(namespace) = &expected_namespace {
                        request = request.header("x-audiolib-album-namespace", namespace);
                    }
                    request
                })
                .map_err(|error| RemoteError::from_transport(error, &url))?;
                let status = response.status().as_u16();
                if status == 405 {
                    if claim_artwork_method_not_allowed(&client.api) {
                        tracing::warn!(
                            server = %client.api,
                            %path,
                            "artwork endpoint returned HTTP 405 Method Not Allowed; showing no cover"
                        );
                    }
                    return Ok(ArtworkReply::Missing);
                }
                if status == 404 {
                    return Ok(ArtworkReply::Missing);
                }
                // 401, 409 and other statuses are classified by status and
                // error_code below. This function does not retry them.
                if matches!(status, 200 | 304)
                    && let Some(expected) = &library_id
                {
                    let identities = response.headers().get_all("x-library-id");
                    let mut identities = identities.iter();
                    let identity = identities.next().and_then(|value| value.to_str().ok());
                    if identity != Some(expected.as_str()) || identities.next().is_some() {
                        return Err(RemoteError::LibraryChanged(
                            "Artwork does not belong to the opened snapshot; synchronize before displaying it".into(),
                        ));
                    }
                    if let Some(namespace) = &expected_namespace {
                        let mut identities = response.headers().get_all("x-album-id").iter();
                        let identity = identities.next().and_then(|value| value.to_str().ok());
                        let expected = id.to_string();
                        if identity != Some(expected.as_str()) || identities.next().is_some() {
                            return Err(RemoteError::LibraryChanged(
                                "Artwork does not belong to the requested album".into(),
                            ));
                        }
                        let mut namespaces = response.headers().get_all("x-album-namespace").iter();
                        let actual = namespaces.next().and_then(|value| value.to_str().ok());
                        if actual != Some(namespace.as_str()) || namespaces.next().is_some() {
                            return Err(RemoteError::LibraryChanged(
                                "Artwork does not belong to the opened album namespace".into(),
                            ));
                        }
                        if let Some(folder) = expected_folder {
                            let mut folders =
                                response.headers().get_all("x-album-folder-id").iter();
                            let actual = folders.next().and_then(|value| value.to_str().ok());
                            let expected = folder.to_string();
                            if actual != Some(expected.as_str()) || folders.next().is_some() {
                                return Err(RemoteError::LibraryChanged(
                                    "Artwork does not belong to the requested direct folder".into(),
                                ));
                            }
                        }
                        if let Some(expected) = &expected_directory {
                            let mut identities = response
                                .headers()
                                .get_all("x-album-directory-identity")
                                .iter();
                            let actual = identities.next().and_then(|value| value.to_str().ok());
                            if actual != Some(expected.as_str()) || identities.next().is_some() {
                                return Err(RemoteError::LibraryChanged(
                                    "Artwork does not belong to the requested source directory"
                                        .into(),
                                ));
                            }
                        }
                    }
                }
                let Some(mut response) =
                    client.checked_conditional(response, &context, if_none_match.is_some())?
                else {
                    return Ok(ArtworkReply::NotModified);
                };
                if response.status().as_u16() != 200 {
                    return Err(RemoteError::Protocol(format!(
                        "{context}: expected HTTP 200"
                    )));
                }
                let mime = response
                    .headers()
                    .get("content-type")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .split(';')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase();
                if !matches!(mime.as_str(), "image/png" | "image/jpeg") {
                    return Err(RemoteError::Protocol(format!(
                        "{context}: expected JPEG or PNG"
                    )));
                }
                let etag = response
                    .headers()
                    .get("etag")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                let bytes = response
                    .body_mut()
                    .with_config()
                    .limit(MAX_ARTWORK_BYTES)
                    .read_to_vec()
                    .map_err(|error| RemoteError::from_transport(error, &url))?;
                let valid_magic = match mime.as_str() {
                    "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
                    "image/jpeg" => bytes.starts_with(&[0xff, 0xd8, 0xff]),
                    _ => false,
                };
                if !valid_magic {
                    return Err(RemoteError::Protocol(format!(
                        "{context}: invalid image signature"
                    )));
                }
                Ok(ArtworkReply::Data { bytes, etag, mime })
            },
            cancel,
            &mut |_| {},
        )
    }
}

#[cfg(test)]
#[path = "folder_artwork_tests.rs"]
mod folder_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_server::{TestResponse, TestServer};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn optional_artwork_requires_its_explicit_feature() {
        assert!(!Capabilities::default().album_artwork_usable());
        assert!(!Capabilities::unknown().album_artwork_usable());
        let mut capability = Capabilities {
            compatibility: Some(audiolib_core::compatibility::compatibility_manifest()),
            ..Capabilities::default()
        };
        assert!(capability.album_artwork_usable());
        let features = &mut capability.compatibility.as_mut().unwrap().features;
        features.insert("album_artwork".into(), 2);
        assert!(capability.album_artwork_usable());
        capability
            .compatibility
            .as_mut()
            .unwrap()
            .features
            .remove("album_artwork");
        assert!(!capability.album_artwork_usable());
    }

    #[test]
    fn native_album_transport_binds_library_namespace_and_album_identity() {
        let server = TestServer::start(|request| {
            assert_eq!(request.path, "/api/v1/albums/11/artwork?side=40");
            assert_eq!(request.header("x-audiolib-library-id"), Some("library"));
            assert_eq!(
                request.header("x-audiolib-album-namespace"),
                Some("namespace")
            );
            let mut response = if request.header("if-none-match").is_some() {
                TestResponse::bytes(304, Vec::new())
            } else {
                TestResponse::bytes(200, b"\x89PNG\r\n\x1a\nfixture".to_vec())
            };
            response.content_type = "image/png";
            response
                .with_header("X-Library-Id", "library")
                .with_header("X-Album-Id", "11")
                .with_header("X-Album-Namespace", "namespace")
        });
        let client = RemoteClient::new(&server.url, "token").unwrap();
        let cancel = AtomicBool::new(false);
        assert!(matches!(
            client
                .album_artwork_for_library("library", "namespace", 11, Some(40), None, &cancel)
                .unwrap(),
            ArtworkReply::Data { .. }
        ));
        assert_eq!(
            client
                .album_artwork_for_library(
                    "library",
                    "namespace",
                    11,
                    Some(40),
                    Some("\"cached\""),
                    &cancel
                )
                .unwrap(),
            ArtworkReply::NotModified
        );
        let format = audiolib_core::compatibility::LibraryFormat::target();
        let state = crate::CacheState {
            library_id: "library".into(),
            schema_version: 8,
            format: Some(Box::new(crate::CacheFormat {
                source: Some(format.clone()),
                local: format,
                generation: 0,
            })),
            ..Default::default()
        };
        let capabilities = Capabilities {
            compatibility: Some(audiolib_core::compatibility::compatibility_manifest()),
            ..Default::default()
        };
        assert!(matches!(
            client
                .album_artwork_for_snapshot(
                    &state,
                    &capabilities,
                    "namespace",
                    11,
                    &[999],
                    Some(40),
                    None,
                    &cancel
                )
                .unwrap(),
            ArtworkReply::Data { .. }
        ));
        for status in [200, 304] {
            for invalid in 0..5 {
                let bad = TestServer::start(move |_| {
                    let response = TestResponse::bytes(status, Vec::new())
                        .with_header("X-Library-Id", "library");
                    let response = match invalid {
                        0 => response.with_header("X-Album-Id", "12"),
                        1 => response
                            .with_header("X-Album-Id", "11")
                            .with_header("X-Album-Id", "11"),
                        _ => response.with_header("X-Album-Id", "11"),
                    };
                    match invalid {
                        2 => response,
                        3 => response.with_header("X-Album-Namespace", "different"),
                        4 => response
                            .with_header("X-Album-Namespace", "namespace")
                            .with_header("X-Album-Namespace", "namespace"),
                        _ => response.with_header("X-Album-Namespace", "namespace"),
                    }
                });
                assert!(matches!(
                    RemoteClient::new(&bad.url, "token")
                        .unwrap()
                        .album_artwork_for_library(
                            "library",
                            "namespace",
                            11,
                            None,
                            Some("\"cached\""),
                            &cancel
                        ),
                    Err(RemoteError::LibraryChanged(_))
                ));
            }
        }
    }

    #[test]
    fn legacy_snapshot_uses_fixed_members_not_locally_projected_album_ids() {
        let paths = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = paths.clone();
        let server = TestServer::start(move |request| {
            assert_eq!(request.header("x-audiolib-album-namespace"), None);
            observed.lock().unwrap().push(request.path.clone());
            if request.path == "/api/v1/audio/1/artwork?side=40" {
                return TestResponse::bytes(404, Vec::new());
            }
            assert_eq!(request.path, "/api/v1/audio/2/artwork?side=40");
            let mut response = TestResponse::bytes(200, b"\x89PNG\r\n\x1a\nfixture".to_vec())
                .with_header("X-Library-Id", "library");
            response.content_type = "image/png";
            response
        });
        let local = audiolib_core::compatibility::LibraryFormat::target();
        let mut source = local.clone();
        source.schema_version = 7;
        source.features.remove("album_entities");
        source.features.remove("metadata_storage");
        let state = crate::CacheState {
            library_id: "library".into(),
            schema_version: 8,
            format: Some(Box::new(crate::CacheFormat {
                source: Some(source),
                local,
                generation: 1,
            })),
            ..Default::default()
        };
        let capabilities = Capabilities {
            compatibility: Some(audiolib_core::compatibility::compatibility_manifest()),
            ..Default::default()
        };
        let reply = RemoteClient::new(&server.url, "token")
            .unwrap()
            .album_artwork_for_snapshot(
                &state,
                &capabilities,
                "local-only-namespace",
                987654,
                &[9, 2, 1, 2],
                Some(40),
                None,
                &AtomicBool::new(false),
            )
            .unwrap();
        assert!(matches!(reply, ArtworkReply::Data { .. }));
        assert_eq!(
            *paths.lock().unwrap(),
            [
                "/api/v1/audio/1/artwork?side=40",
                "/api/v1/audio/2/artwork?side=40"
            ]
        );
    }

    #[test]
    fn artwork_uses_authentication_thumbnail_and_conditional_etag() {
        let server = TestServer::start(|request| {
            assert_eq!(request.path, "/api/v1/audio/7/artwork?side=192");
            assert_eq!(request.header("authorization"), Some("Bearer test-token"));
            if request.header("if-none-match") == Some("\"cover-1\"") {
                return TestResponse::bytes(304, Vec::new());
            }
            let mut response = TestResponse::bytes(200, b"\x89PNG\r\n\x1a\nfixture".to_vec())
                .with_header("ETag", "\"cover-1\"");
            response.content_type = "image/png";
            response
        });
        let client = RemoteClient::new(&server.url, "test-token").unwrap();
        let cancel = AtomicBool::new(false);
        assert!(
            matches!(client.artwork(7, Some(192), None, &cancel).unwrap(),
            ArtworkReply::Data { etag: Some(etag), .. } if etag == "\"cover-1\"")
        );
        assert_eq!(
            client
                .artwork(7, Some(192), Some("\"cover-1\""), &cancel)
                .unwrap(),
            ArtworkReply::NotModified
        );
    }

    #[test]
    fn snapshot_artwork_sends_and_verifies_library_identity_including_304() {
        let server = TestServer::start(|request| {
            assert_eq!(
                request.header("x-audiolib-library-id"),
                Some("displayed-library")
            );
            let mut response = if request.header("if-none-match").is_some() {
                TestResponse::bytes(304, Vec::new())
            } else {
                TestResponse::bytes(200, b"\x89PNG\r\n\x1a\nfixture".to_vec())
            }
            .with_header("X-Library-Id", "displayed-library");
            response.content_type = "image/png";
            response
        });
        let client = RemoteClient::new(&server.url, "token").unwrap();
        let cancel = AtomicBool::new(false);
        assert!(matches!(
            client
                .artwork_for_library("displayed-library", 1, None, None, &cancel)
                .unwrap(),
            ArtworkReply::Data { .. }
        ));
        assert_eq!(
            client
                .artwork_for_library("displayed-library", 1, None, Some("\"cached\""), &cancel)
                .unwrap(),
            ArtworkReply::NotModified
        );
        for identity in ["", "\r\nspoof"] {
            assert!(matches!(
                client.artwork_for_library(identity, 1, None, None, &cancel),
                Err(RemoteError::Validation(_))
            ));
        }
    }

    #[test]
    fn snapshot_artwork_rejects_missing_mismatched_and_duplicate_reply_identity() {
        for case in 0..4 {
            let server = TestServer::start(move |_| {
                let status = if case == 1 { 304 } else { 200 };
                let mut response =
                    TestResponse::bytes(status, b"\x89PNG\r\n\x1a\nfixture".to_vec());
                response.content_type = "image/png";
                if case != 0 {
                    response = response.with_header(
                        "X-Library-Id",
                        if case == 3 {
                            "displayed-library"
                        } else {
                            "other-library"
                        },
                    );
                }
                if case == 3 {
                    response = response.with_header("X-Library-Id", "displayed-library");
                }
                response
            });
            let client = RemoteClient::new(&server.url, "token").unwrap();
            assert!(
                matches!(
                    client.artwork_for_library(
                        "displayed-library",
                        1,
                        None,
                        Some("\"cached\""),
                        &AtomicBool::new(false)
                    ),
                    Err(RemoteError::LibraryChanged(_))
                ),
                "case {case}"
            );
        }
    }

    #[test]
    fn absent_and_legacy_endpoints_degrade_but_authentication_errors_remain_errors() {
        for status in [404, 405, 401] {
            let server = TestServer::start(move |_| TestResponse::json(status, "{}"));
            let result = RemoteClient::new(&server.url, "token").unwrap().artwork(
                1,
                None,
                None,
                &AtomicBool::new(false),
            );
            if status == 401 {
                assert!(matches!(result, Err(RemoteError::Unauthorized(_))));
            } else {
                assert_eq!(result.unwrap(), ArtworkReply::Missing);
            }
        }
    }

    #[test]
    fn malformed_oversized_and_unconditional_304_are_rejected() {
        for case in 0..4 {
            let server = TestServer::start(move |_| {
                let mut response = match case {
                    0 => TestResponse::bytes(304, Vec::new()),
                    1 => TestResponse::bytes(200, b"\x89PNG\r\n\x1a\n".to_vec()),
                    2 => TestResponse::bytes(200, b"not an image".to_vec()),
                    _ => TestResponse::bytes(200, vec![0; MAX_ARTWORK_BYTES as usize + 1]),
                };
                if case != 1 {
                    response.content_type = "image/png";
                }
                response
            });
            let client = RemoteClient::new(&server.url, "token").unwrap();
            assert!(
                client
                    .artwork(1, None, None, &AtomicBool::new(false))
                    .is_err(),
                "case {case}"
            );
        }
    }

    #[test]
    fn artwork_wait_is_cancelled_before_the_response_head() {
        let server = TestServer::start(|_| {
            TestResponse::bytes(200, Vec::new()).delayed(Duration::from_secs(3))
        });
        let client = RemoteClient::new(&server.url, "token").unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let signal = cancel.clone();
        let started = std::time::Instant::now();
        let pending = std::thread::spawn(move || client.artwork(1, None, None, &signal));
        std::thread::sleep(Duration::from_millis(150));
        cancel.store(true, Ordering::Release);
        assert!(matches!(
            pending.join().unwrap(),
            Err(RemoteError::Cancelled)
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn cover_busy_delay_uses_retry_after_seconds_default_cap_and_jitter() {
        let one =
            crate::error::retry_wait(ARTWORK_BUSY_DEFAULT_WAIT, Some(1), ARTWORK_BUSY_MAX_WAIT);
        assert_eq!(one, Duration::from_secs(1));
        for _ in 0..30 {
            let delay = cover_busy_delay(Some(1));
            assert!(delay >= one, "{delay:?}");
            assert!(delay <= one + ARTWORK_BUSY_JITTER, "{delay:?}");
            let missing = cover_busy_delay(None);
            assert!(missing >= ARTWORK_BUSY_DEFAULT_WAIT, "{missing:?}");
            assert!(
                missing <= ARTWORK_BUSY_DEFAULT_WAIT + ARTWORK_BUSY_JITTER,
                "{missing:?}"
            );
        }
        let zero = cover_busy_delay(Some(0));
        assert!(
            zero >= ARTWORK_BUSY_DEFAULT_WAIT
                && zero <= ARTWORK_BUSY_DEFAULT_WAIT + ARTWORK_BUSY_JITTER
        );
        let two = cover_busy_delay(Some(2));
        assert!(
            two >= Duration::from_secs(2) && two <= Duration::from_secs(2) + ARTWORK_BUSY_JITTER
        );
        assert_eq!(cover_busy_delay(Some(3_600)), ARTWORK_BUSY_MAX_WAIT);
    }

    #[test]
    fn busy_503_is_classified_from_the_error_code_and_retry_after_not_the_message() {
        let hits = Arc::new(AtomicUsize::new(0));
        let seen = hits.clone();
        let conditional = Arc::new(std::sync::Mutex::new(None));
        let saw = conditional.clone();
        let server = TestServer::start(move |request| {
            seen.fetch_add(1, Ordering::SeqCst);
            *saw.lock().unwrap() = request.header("if-none-match").map(str::to_owned);
            TestResponse::json(
                503,
                r#"{"error_code":"BUSY","message":"slot queue elapsed"}"#,
            )
            .with_header("Retry-After", "1")
        });
        let error = RemoteClient::new(&server.url, "token")
            .unwrap()
            .artwork(
                7,
                Some(40),
                Some("\"cached-cover\""),
                &AtomicBool::new(false),
            )
            .unwrap_err();
        assert!(
            matches!(
                error,
                RemoteError::Busy {
                    retry_after: Some(1),
                    ..
                }
            ),
            "{error:?}"
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "the transport does not retry"
        );
        assert_eq!(
            conditional.lock().unwrap().as_deref(),
            Some("\"cached-cover\"")
        );

        let misleading = TestServer::start(|_| {
            TestResponse::json(
                503,
                r#"{"error_code":"UNAVAILABLE","message":"server busy please retry"}"#,
            )
            .with_header("Retry-After", "1")
        });
        let error = RemoteClient::new(&misleading.url, "token")
            .unwrap()
            .artwork(7, None, None, &AtomicBool::new(false))
            .unwrap_err();
        assert!(
            matches!(error, RemoteError::Starting { .. }),
            "a busy-sounding message is not BUSY: {error:?}"
        );
    }

    #[test]
    fn artwork_409_and_root_offline_are_not_retried() {
        for (status, body, header, expect_busy) in [
            (
                409,
                r#"{"error_code":"BUSY","message":"conflict"}"#,
                None,
                true,
            ),
            (
                409,
                r#"{"error_code":"LIBRARY_CHANGED","message":"changed"}"#,
                None,
                false,
            ),
            (
                503,
                r#"{"error_code":"ROOT_OFFLINE","message":"Library root \"SFX\" is offline: down"}"#,
                Some("5"),
                false,
            ),
        ] {
            let hits = Arc::new(AtomicUsize::new(0));
            let seen = hits.clone();
            let server = TestServer::start(move |_| {
                seen.fetch_add(1, Ordering::SeqCst);
                let response = TestResponse::json(status, body);
                match header {
                    Some(value) => response.with_header("Retry-After", value),
                    None => response,
                }
            });
            let error = RemoteClient::new(&server.url, "token")
                .unwrap()
                .artwork(1, None, None, &AtomicBool::new(false))
                .unwrap_err();
            assert_eq!(hits.load(Ordering::SeqCst), 1, "{status} {body}");
            if expect_busy {
                assert!(matches!(error, RemoteError::Busy { .. }), "{error:?}");
            } else if status == 409 {
                assert!(matches!(error, RemoteError::LibraryChanged(_)), "{error:?}");
            } else {
                assert!(
                    matches!(error, RemoteError::RootOffline { .. }),
                    "{error:?}"
                );
            }
        }
    }

    #[test]
    fn unauthorized_and_protocol_failures_are_single_shot() {
        for (status, body, protocol) in [
            (
                401,
                r#"{"error_code":"UNAUTHORIZED","message":"token"}"#,
                false,
            ),
            (200, "not-an-image", true),
        ] {
            let hits = Arc::new(AtomicUsize::new(0));
            let seen = hits.clone();
            let server = TestServer::start(move |_| {
                seen.fetch_add(1, Ordering::SeqCst);
                if protocol {
                    let mut response = TestResponse::bytes(200, b"not-an-image".to_vec());
                    response.content_type = "image/png";
                    response
                } else {
                    TestResponse::json(status, body)
                }
            });
            let error = RemoteClient::new(&server.url, "token")
                .unwrap()
                .artwork(4, None, Some("\"etag\""), &AtomicBool::new(false))
                .unwrap_err();
            assert_eq!(hits.load(Ordering::SeqCst), 1, "{status}");
            if protocol {
                assert!(matches!(error, RemoteError::Protocol(_)), "{error:?}");
            } else {
                assert!(matches!(error, RemoteError::Unauthorized(_)), "{error:?}");
            }
        }
    }

    #[test]
    fn method_not_allowed_is_missing_and_logs_a_warning() {
        use std::sync::Once;
        use tracing::field::Visit;
        static WARN_405: AtomicUsize = AtomicUsize::new(0);
        static EXPECTED_SERVER: Mutex<String> = Mutex::new(String::new());
        static INSTALL: Once = Once::new();
        struct Probe;
        impl tracing::Subscriber for Probe {
            fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
                metadata.level() <= &tracing::Level::WARN
            }
            fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
            fn event(&self, event: &tracing::Event<'_>) {
                if *event.metadata().level() != tracing::Level::WARN {
                    return;
                }
                let expected = EXPECTED_SERVER.lock().unwrap().clone();
                if expected.is_empty() {
                    return;
                }
                let mut server = String::new();
                event.record(&mut ServerVisit(&mut server));
                if server == expected {
                    WARN_405.fetch_add(1, Ordering::SeqCst);
                }
            }
            fn enter(&self, _: &tracing::span::Id) {}
            fn exit(&self, _: &tracing::span::Id) {}
        }
        struct ServerVisit<'a>(&'a mut String);
        impl Visit for ServerVisit<'_> {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "server" {
                    let text = format!("{value:?}");
                    *self.0 = text.trim_matches('"').to_owned();
                }
            }
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                if field.name() == "server" {
                    *self.0 = value.to_owned();
                }
            }
        }
        INSTALL.call_once(|| {
            let _ = tracing::subscriber::set_global_default(Probe);
        });
        WARN_405.store(0, Ordering::SeqCst);
        let server = TestServer::start(|_| TestResponse::json(405, "{}"));
        let client = RemoteClient::new(&server.url, "token").unwrap();
        *EXPECTED_SERVER.lock().unwrap() = client.api_url().to_owned();
        let cancel = AtomicBool::new(false);
        assert_eq!(
            client.artwork(405_405, None, None, &cancel).unwrap(),
            ArtworkReply::Missing
        );
        assert_eq!(
            client.artwork(405_406, None, None, &cancel).unwrap(),
            ArtworkReply::Missing
        );
        assert_eq!(
            WARN_405.load(Ordering::SeqCst),
            1,
            "two HTTP 405 replies from the same server log one warning"
        );
        assert!(claim_artwork_method_not_allowed(
            "http://artwork-405-a.invalid/api/v1"
        ));
        assert!(!claim_artwork_method_not_allowed(
            "http://artwork-405-a.invalid/api/v1"
        ));
        assert!(claim_artwork_method_not_allowed(
            "http://artwork-405-b.invalid/api/v1"
        ));
    }

    #[test]
    fn artwork_client_keeps_at_most_two_cover_requests_in_flight() {
        let slots = ArtworkSlots::new(ARTWORK_CLIENT_SLOTS);
        let cancel = AtomicBool::new(false);
        let first = slots.acquire(&cancel).unwrap();
        let second = slots.acquire(&cancel).unwrap();
        let entered = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let waiting = scope.spawn(|| {
                let _third = slots.acquire(&cancel).unwrap();
                entered.store(true, Ordering::Release);
            });
            std::thread::sleep(Duration::from_millis(120));
            assert!(
                !entered.load(Ordering::Acquire),
                "a third cover request must wait for a free slot"
            );
            drop(first);
            waiting.join().unwrap();
        });
        assert!(entered.load(Ordering::Acquire));
        drop(second);

        let blocked = ArtworkSlots::new(1);
        let held = blocked.acquire(&cancel).unwrap();
        let stop = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let waiting = scope.spawn(|| blocked.acquire(&stop));
            std::thread::sleep(Duration::from_millis(40));
            stop.store(true, Ordering::Release);
            let result = waiting.join().unwrap();
            assert!(matches!(result, Err(RemoteError::Cancelled)));
        });
        drop(held);
    }
}
