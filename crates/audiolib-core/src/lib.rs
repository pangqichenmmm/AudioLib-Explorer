// crates/audiolib-core/src/lib.rs
//! Owned domain contracts shared by desktop, CLI, database, and audio services.
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use unicode_segmentation::UnicodeSegmentation;

pub mod albums;
pub mod compatibility;
pub mod config;
pub mod library;
pub mod list_waveform;
pub mod list_waveform_i8;
mod metadata;
pub mod profiles;
pub mod search_workspace;
pub mod server_url;
pub mod tag_filter;
pub mod waveform_cache;
pub mod waveform_rate;
pub mod window_layout;
pub use albums::{Album, AlbumFacet, AlbumKind, MAX_ALBUM_MEMBERS};
pub use library::*;
pub use list_waveform::{
    LIST_WAVEFORM_COLUMNS, LIST_WAVEFORM_ENCODING_VERSION, ListWaveform, ListWaveformBatch,
    ListWaveformItem, ListWaveformSource, ListWaveformState, MAX_LIST_WAVEFORM_IDS, decode_base64,
};
pub use list_waveform_i8::{
    LIST_WAVEFORM_ENCODING_I8, LIST_WAVEFORM_ENCODING_I16, LIST_WAVEFORM_I8_ETAG_SUFFIX,
    LIST_WAVEFORM_I8_MAX_BYTES, LIST_WAVEFORM_I8_ZOOM_LEVEL, ListWaveformI8, expand_i8_to_i16,
    i8_base64_len, quantize_i16_to_i8,
};
pub use metadata::{
    SOURCE_TAG_KEYS, album, display_tags, source_description, source_metadata_search_text,
    source_tags,
};
pub use search_workspace::{
    SavedSearchTask, SearchAudioRef, SearchFolderRef, SearchSession, SearchTask,
    SearchTaskDefinition, SearchTaskView, SearchWorkspace, SearchWorkspaceTarget,
};

/// Compatible operation envelope for CLI and local control replies.
pub fn success(data: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"success":true,"error":null,"error_code":null,"warnings":[],
        "metadata":{},"execution_time_ms":0.0,"timestamp":std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs(),"data":data})
}

/// An explicit failure rather than a misleading successful empty result.
pub fn failure(message: &str) -> serde_json::Value {
    serde_json::json!({"success":false,"error":message,"error_code":"OPERATION_FAILED",
        "warnings":[],"metadata":{},"execution_time_ms":0.0,"timestamp":std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs(),"data":null})
}

pub type Result<T> = std::result::Result<T, Error>;

/// Message of the I/O error a streaming reader returns when Stop interrupts a read that
/// is blocked on network data. Audio engines treat it as a stop, never as a failure.
pub const AUDIO_READ_INTERRUPTED: &str = "audio read interrupted";

/// The I/O error for a read interrupted by Stop (see [`AUDIO_READ_INTERRUPTED`]). Not
/// `ErrorKind::Interrupted`: `read_exact` would silently retry that kind.
pub fn read_interrupted_error() -> std::io::Error {
    std::io::Error::other(AUDIO_READ_INTERRUPTED)
}

impl Error {
    /// Whether this failure is a read deliberately interrupted by Stop (also when a
    /// decoder wrapped the I/O error into its own message).
    pub fn is_read_interrupted(&self) -> bool {
        match self {
            Error::Validation(_) | Error::Cancelled => false,
            error => error.to_string().contains(AUDIO_READ_INTERRUPTED),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Validation: {0}")]
    Validation(String),
    #[error("Database: {0}")]
    Database(String),
    #[error("Audio: {0}")]
    Audio(String),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("Operation cancelled")]
    Cancelled,
    #[error("Busy: {0}")]
    Busy(String),
    #[error("Not found: {0}")]
    NotFound(String),
    /// The database or snapshot was written by a newer AudioLib Explorer: its schema
    /// version or a feature revision exceeds what this client supports.
    #[error("{}", newer_database_message(what, *found, *supported, false))]
    NewerDatabase {
        what: String,
        found: i64,
        supported: i64,
    },
}

/// User-facing text for [`Error::NewerDatabase`]: the client is too old and must be upgraded.
pub fn newer_database_message(what: &str, found: i64, supported: i64, chinese: bool) -> String {
    if chinese {
        format!(
            "客户端版本过旧：服务器快照使用了更新的数据库格式（{what} {found} > {supported}），请升级 AudioLib Explorer 后重新同步。"
        )
    } else {
        format!(
            "This client is too old: the server snapshot uses a newer database format ({what} {found} > {supported}). Upgrade AudioLib Explorer, then sync again."
        )
    }
}

/// User-facing text for [`Error::NewerDatabase`] raised while opening a local library.
pub fn newer_local_database_message(
    what: &str,
    found: i64,
    supported: i64,
    chinese: bool,
) -> String {
    if chinese {
        format!(
            "客户端版本过旧：这个音效库使用了更新的数据库格式（{what} {found} > {supported}），请升级 AudioLib Explorer 后再打开。"
        )
    } else {
        format!(
            "This client is too old: this library uses a newer database format ({what} {found} > {supported}). Upgrade AudioLib Explorer, then open it again."
        )
    }
}

impl Error {
    /// Like [`Error::display_text`], but worded for opening a local library.
    pub fn local_display_text(&self, chinese: bool) -> String {
        match self {
            Error::NewerDatabase {
                what,
                found,
                supported,
            } => newer_local_database_message(what, *found, *supported, chinese),
            other => other.to_string(),
        }
    }

    /// Localized text for [`Error::NewerDatabase`]; `None` for every other error.
    pub fn newer_database_text(&self, chinese: bool) -> Option<String> {
        match self {
            Error::NewerDatabase {
                what,
                found,
                supported,
            } => Some(newer_database_message(what, *found, *supported, chinese)),
            _ => None,
        }
    }

    /// Localized text for a newer database, otherwise this error's ordinary display.
    pub fn display_text(&self, chinese: bool) -> String {
        self.newer_database_text(chinese)
            .unwrap_or_else(|| self.to_string())
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AudioMetadata {
    pub sample_rate: u32,
    pub bit_depth: u16,
    pub channels: u16,
    pub frames: u64,
    pub duration: f64,
    pub encoding: String,
    pub bext_description: String,
    pub attributes: serde_json::Value,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AudioFile {
    pub id: i64,
    pub file_path: String,
    pub filename: String,
    pub folder_id: Option<i64>,
    pub file_size: u64,
    pub sample_rate: u32,
    pub bit_depth: u16,
    pub channels: u16,
    pub duration: f64,
    pub tags: String,
    pub color: Option<String>,
    pub notes: String,
    pub favorite: bool,
    pub bext_description: String,
    pub attributes: serde_json::Value,
    /// Library/album declared by the file itself; empty when it declares none.
    #[serde(default)]
    pub album: String,
    /// Shared album membership; absent only on older, unconverted snapshots.
    #[serde(default)]
    pub album_id: Option<i64>,
    /// Captured identity for event-loop actions without another database read.
    #[serde(default)]
    pub album_entity: Option<Album>,
    #[serde(default)]
    pub missing: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Folder {
    pub id: i64,
    pub path: String,
    pub name: String,
    pub parent_id: Option<i64>,
    pub file_count: u64,
}

/// Upper bound on `include_tags` plus `exclude_tags`, keeping each search statement bounded.
pub const MAX_TAG_TERMS: usize = 64;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SearchFilters {
    #[serde(default)]
    pub visibility: FileVisibility,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u16>,
    pub channels: Option<u16>,
    pub duration_min: Option<f64>,
    pub duration_max: Option<f64>,
    pub favorites_only: bool,
    pub tagged_only: bool,
    /// Case-insensitive literal substring of library tags; percent/underscore are literal.
    pub tag_contains: Option<String>,
    /// Further include terms matched like `tag_contains`; a record must contain all of them.
    /// Omitted when empty and defaulted when absent, so older peers keep their wire shape.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include_tags: Vec<String>,
    /// Exclude terms matched like `tag_contains`; a record containing any of them is
    /// omitted. A term on both sides therefore matches nothing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_tags: Vec<String>,
    /// Exact library color, ignoring hex letter case.
    pub color: Option<String>,
    /// Exact library/album name declared by the files; absent keeps every album visible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    /// Exact shared entity filter. Mutually exclusive with the declared-name filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_id: Option<i64>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SortOrder {
    #[default]
    Relevance,
    Filename,
    Duration,
    SampleRate,
    Modified,
    /// Numeric channel count, with stable audio identity as the pagination tie-breaker.
    Channels,
    /// Case-insensitive library tags, or extracted source tags when library tags are empty.
    Tags,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    pub folder_id: Option<i64>,
    pub include_subfolders: bool,
    pub filters: SearchFilters,
    pub limit: usize,
    pub offset: usize,
    pub order_by: SortOrder,
    pub descending: bool,
}
impl Default for SearchRequest {
    fn default() -> Self {
        Self {
            query: String::new(),
            folder_id: None,
            include_subfolders: true,
            filters: SearchFilters::default(),
            limit: 200,
            offset: 0,
            order_by: SortOrder::Relevance,
            descending: false,
        }
    }
}
impl SearchRequest {
    /// A lone non-Han letter waits for another searchable character. Surrounding
    /// punctuation, whitespace and combining accents do not satisfy that minimum.
    /// Empty browsing, numeric codes, Han characters and multi-term queries remain ready.
    pub fn needs_more_search_text(&self) -> bool {
        // Inspect one base character per grapheme so Other_Alphabetic combining
        // marks (for example Hebrew points) cannot count as another input letter.
        let mut characters = self.query.graphemes(true).filter_map(|grapheme| {
            grapheme
                .chars()
                .find(|character| character.is_alphanumeric())
        });
        let Some(character) = characters.next() else {
            return false;
        };
        character.is_alphabetic()
            && !matches!(character,
                '\u{3400}'..='\u{4dbf}'
                | '\u{4e00}'..='\u{9fff}'
                | '\u{f900}'..='\u{faff}'
                | '\u{20000}'..='\u{2ffff}'
                | '\u{30000}'..='\u{323af}'
            )
            && characters.next().is_none()
    }

    /// Validate bounded search input before compiling a parameterized FTS5 query.
    pub fn validate(&self) -> Result<()> {
        if self.filters.album.is_some() && self.filters.album_id.is_some() {
            return Err(Error::Validation(
                "Album name and identity filters are mutually exclusive".into(),
            ));
        }
        if self.filters.album_id.is_some_and(|id| id <= 0) {
            return Err(Error::Validation("Album identity must be positive".into()));
        }
        if self.query.chars().count() > 500 || self.query.contains(';') || self.query.contains('\0')
        {
            return Err(Error::Validation(
                "Query exceeds 500 characters or contains a forbidden delimiter".into(),
            ));
        }
        if self.needs_more_search_text() {
            return Err(Error::Validation(
                "Enter at least two characters for a letter search".into(),
            ));
        }
        if self.limit == 0 || self.limit > 10_000 {
            return Err(Error::Validation("Search limit must be 1..=10000".into()));
        }
        for duration in [self.filters.duration_min, self.filters.duration_max]
            .into_iter()
            .flatten()
        {
            if !duration.is_finite() || duration < 0.0 {
                return Err(Error::Validation(
                    "Duration must be finite and nonnegative".into(),
                ));
            }
        }
        if matches!((self.filters.duration_min, self.filters.duration_max), (Some(min), Some(max)) if min > max)
        {
            return Err(Error::Validation("Minimum duration exceeds maximum".into()));
        }
        if self
            .filters
            .tag_contains
            .as_ref()
            .is_some_and(|tag| tag.chars().count() > 500 || tag.contains('\0'))
        {
            return Err(Error::Validation(
                "Tag filter exceeds 500 characters or contains NUL".into(),
            ));
        }
        // An empty term would match every tag text, so an exclusion would hide everything.
        let terms = self
            .filters
            .include_tags
            .iter()
            .chain(&self.filters.exclude_tags);
        if self.filters.include_tags.len() + self.filters.exclude_tags.len() > MAX_TAG_TERMS {
            return Err(Error::Validation(format!(
                "At most {MAX_TAG_TERMS} tag filter terms are allowed"
            )));
        }
        for tag in terms {
            if tag.trim().is_empty() || tag.chars().count() > 500 || tag.contains('\0') {
                return Err(Error::Validation(
                    "Tag filter terms must be 1..=500 characters without NUL".into(),
                ));
            }
        }
        if let Some(color) = &self.filters.color {
            MetadataPatch {
                color: Some(color.clone()),
                ..Default::default()
            }
            .validate()?;
        }
        // Album names originate in bounded WAV Metadata. Keep exact names usable even
        // when they exceed the free-text query's 500-character limit.
        if self
            .filters
            .album
            .as_ref()
            .is_some_and(|album| album.len() > 32 * 1024 * 1024 || album.contains('\0'))
        {
            return Err(Error::Validation(
                "Album filter exceeds the Metadata size limit or contains NUL".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SearchPage {
    pub results: Vec<AudioFile>,
    pub total_count: u64,
    pub query: String,
    pub execution_time_ms: f64,
    pub cache_hit: bool,
    pub returned_count: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MetadataPatch {
    pub tags: Option<String>,
    pub color: Option<String>,
    pub notes: Option<String>,
    pub favorite: Option<bool>,
}
impl MetadataPatch {
    /// Validate library-only editing; never modifies the source WAV.
    pub fn validate(&self) -> Result<()> {
        if self.tags.as_ref().is_some_and(|value| value.len() > 16_384)
            || self
                .notes
                .as_ref()
                .is_some_and(|value| value.len() > 1_048_576)
        {
            return Err(Error::Validation(
                "Metadata text exceeds the allowed size".into(),
            ));
        }
        if let Some(color) = &self.color
            && !color.is_empty()
            && !(color.len() == 7
                && color.starts_with('#')
                && color[1..].bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err(Error::Validation("Color must be empty or #RRGGBB".into()));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScanState {
    #[default]
    Running,
    Completed,
    Cancelled,
    Failed,
}
/// Scan progress. Also the server's scan reply shape (`/scans*`), so missing fields
/// default and unknown fields are ignored.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ScanProgress {
    pub session_id: i64,
    pub state: ScanState,
    /// Confirmed worker pause at a safe boundary; the session and scan lease remain live.
    #[serde(default)]
    pub paused: bool,
    /// Final atomic database reconciliation has begun; controls are no longer accepted.
    #[serde(default)]
    pub finalizing: bool,
    pub discovered: u64,
    pub processed: u64,
    pub added: u64,
    pub updated: u64,
    pub unchanged: u64,
    #[serde(default)]
    pub removed: u64,
    pub errors: u64,
    #[serde(default)]
    pub error_details: Vec<ScanIssue>,
    #[serde(default)]
    pub errors_truncated: u64,
    pub current_path: String,
    pub message: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LibraryStats {
    pub audio_files: u64,
    pub folders: u64,
    pub total_duration: f64,
    pub total_bytes: u64,
    #[serde(default)]
    pub missing_files: u64,
}

/// All business data operations pass through this interface and return owned DTOs.
pub trait DataProvider: Send + Sync {
    /// Read personal search tasks from client-local storage, including in server mode.
    /// This operation must never write a main library or downloaded snapshot.
    fn load_search_workspace(&self, _target: &SearchWorkspaceTarget) -> Result<SearchWorkspace> {
        Err(Error::Validation(
            "This provider has no personal search workspace storage".into(),
        ))
    }
    /// Atomically save personal tasks; the caller owns the desktop instance lease and
    /// orders saves on one worker, draining them before switching sources or closing.
    fn save_search_workspace(
        &self,
        _target: &SearchWorkspaceTarget,
        _workspace: &SearchWorkspace,
    ) -> Result<()> {
        Err(Error::Validation(
            "This provider has no personal search workspace storage".into(),
        ))
    }
    /// Provider-local change token for search caches. Compare only within this provider's
    /// lifetime, before and after reading; `None` disables cache reuse.
    fn search_revision(&self) -> Result<Option<u64>> {
        Ok(None)
    }
    fn search(&self, request: &SearchRequest) -> Result<SearchPage>;
    /// Search that can be superseded by a newer GUI request. Providers may additionally
    /// interrupt in-flight work; the default checks cancellation before and after I/O.
    fn search_cancellable(
        &self,
        request: &SearchRequest,
        cancel: Arc<AtomicBool>,
    ) -> Result<SearchPage> {
        if cancel.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        let result = self.search(request);
        if cancel.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        result
    }
    /// Positive directory matches for the query and filters, including descendant totals.
    /// Ignores the selected folder, pagination and sorting so sibling matches stay navigable.
    fn search_folders(&self, request: &SearchRequest) -> Result<Vec<Folder>>;
    /// Cancellable directory facets with the same scope semantics as `search_folders`.
    fn search_folders_cancellable(
        &self,
        request: &SearchRequest,
        cancel: Arc<AtomicBool>,
    ) -> Result<Vec<Folder>> {
        if cancel.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        let result = self.search_folders(request);
        if cancel.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        result
    }
    fn get_audio(&self, id: i64) -> Result<AudioFile>;
    /// Search-scoped album counts; clear both album filters for cross-navigation.
    fn album_facets(&self, _request: &SearchRequest) -> Result<Vec<AlbumFacet>> {
        Err(Error::Validation(
            "This provider has no shared album entities".into(),
        ))
    }
    /// Cancellable shared album aggregation.
    fn album_facets_cancellable(
        &self,
        request: &SearchRequest,
        cancel: Arc<AtomicBool>,
    ) -> Result<Vec<AlbumFacet>> {
        if cancel.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        self.album_facets(request)
    }
    /// Resolve an owned album by its identity within this provider.
    fn get_album(&self, _id: i64) -> Result<Album> {
        Err(Error::Validation(
            "This provider has no shared album entities".into(),
        ))
    }
    /// Indexed semantic lookup when rebinding a personal filter to another library.
    fn find_album_by_semantic(
        &self,
        _kind: AlbumKind,
        _metadata_key: Option<&str>,
        _folder_path: Option<&str>,
    ) -> Result<Option<Album>> {
        Err(Error::Validation(
            "This provider has no shared album entities".into(),
        ))
    }
    /// Present members of the entire album, ordered by audio ID, bounded to 64.
    fn album_members(
        &self,
        _id: i64,
        _limit: usize,
        _cancel: Arc<AtomicBool>,
    ) -> Result<Vec<AudioFile>> {
        Err(Error::Validation(
            "This provider has no shared album entities".into(),
        ))
    }
    /// Present members in exactly one indexed parent directory, ordered by audio ID.
    /// The positive album ID and limit `1..=64` are validated; folder IDs retain
    /// the full signed range accepted by `get_folder`. This never includes child
    /// directories or falls back to members elsewhere in the shared album.
    fn album_members_in_folder(
        &self,
        _id: i64,
        _folder_id: i64,
        _limit: usize,
        _cancel: Arc<AtomicBool>,
    ) -> Result<Vec<AudioFile>> {
        Err(Error::Validation(
            "This provider cannot resolve album members within a directory".into(),
        ))
    }
    /// Present unassigned members whose source path has exactly this parent directory.
    /// Only records without a folder ID are eligible. Directory identities use source
    /// path syntax, accept an empty parent for a relative filename, and are bounded
    /// to 4096 UTF-8 bytes. Results are ordered by audio ID and limited to `1..=64`.
    fn album_members_in_unassigned_directory(
        &self,
        _id: i64,
        _directory: &str,
        _limit: usize,
        _cancel: Arc<AtomicBool>,
    ) -> Result<Vec<AudioFile>> {
        Err(Error::Validation(
            "This provider cannot resolve unassigned album members within a directory".into(),
        ))
    }
    /// Paginate all member identities, including missing records, for local preferences.
    /// IDs are strictly ascending and the page limit is 1..=4096.
    fn album_member_ids(
        &self,
        _id: i64,
        _after_audio_id: Option<i64>,
        _limit: usize,
        _cancel: Arc<AtomicBool>,
    ) -> Result<Vec<i64>> {
        Err(Error::Validation(
            "This provider has no shared album entities".into(),
        ))
    }
    fn folders(&self, parent_id: Option<i64>) -> Result<Vec<Folder>>;
    /// Retrieve one owned folder by its stable identity; absence is `NotFound`.
    /// Providers may implement a direct indexed lookup instead of the tree fallback.
    fn get_folder(&self, id: i64) -> Result<Folder> {
        let mut parents = vec![None];
        let mut seen = std::collections::HashSet::new();
        while let Some(parent) = parents.pop() {
            for folder in self.folders(parent)? {
                if folder.id == id {
                    return Ok(folder);
                }
                if seen.insert(folder.id) {
                    parents.push(Some(folder.id));
                }
            }
        }
        Err(Error::NotFound(format!("Folder {id}")))
    }
    fn update_metadata(&self, id: i64, patch: &MetadataPatch) -> Result<AudioFile>;
    fn stats(&self) -> Result<LibraryStats>;
    fn library_info(&self) -> Result<LibraryInfo>;
    fn check_library(&self) -> Result<IntegrityReport>;
    fn backup_library(&self, destination: &Path) -> Result<BackupReport>;
    fn restore_library(&self, source: &Path) -> Result<RestoreReport>;
    fn maintain_library(&self, action: MaintenanceAction) -> Result<MaintenanceReport>;
    fn update_metadata_batch(&self, request: &BatchMetadataRequest) -> Result<BatchMetadataReport>;
    fn find_audio_by_path(&self, path: &Path) -> Result<Option<AudioFile>>;
    /// Resolve one indexed directory by its stored path, preferring an exact
    /// source spelling and refusing ambiguous aliases. Read-only snapshots must
    /// resolve their own path syntax without probing the reader's filesystem.
    /// Providers without indexed directory lookup explicitly refuse the operation.
    fn find_folder_by_path(&self, _path: &Path) -> Result<Option<Folder>> {
        Err(Error::Validation(
            "This data provider cannot resolve indexed folder paths".into(),
        ))
    }
    fn relink_audio(&self, id: i64, path: &Path) -> Result<AudioFile>;
    /// Relocate an indexed top-level folder as one operation while retaining record
    /// identities and annotations. Every indexed WAV must have a valid corresponding
    /// target beneath `target`; providers without this facility refuse the operation.
    fn relink_folder(&self, _source: &Path, _target: &Path) -> Result<u64> {
        Err(Error::Validation(
            "This data provider cannot relocate library folders".into(),
        ))
    }
    /// Relocation that stops before its atomic commit when the request is abandoned.
    fn relink_folder_cancellable(
        &self,
        source: &Path,
        target: &Path,
        cancel: Arc<AtomicBool>,
    ) -> Result<u64> {
        if cancel.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        self.relink_folder(source, target)
    }
    fn remove_audio(&self, ids: &[i64]) -> Result<u64>;
    fn remove_folder(&self, id: i64) -> Result<u64>;
    fn scan_history(&self, limit: usize, offset: usize) -> Result<Vec<ScanSession>>;
    fn export_library(
        &self,
        destination: &Path,
        format: LibraryExportFormat,
    ) -> Result<TransferReport>;
    fn import_annotations(&self, source: &Path) -> Result<TransferReport>;

    /// Where this provider's list thumbnail Waveforms come from (doc/api/README.md
    /// §9.1–§9.3). `Unavailable` (the default) hides the list Waveform column.
    fn list_waveform_source(&self) -> ListWaveformSource {
        ListWaveformSource::Unavailable
    }
    /// Blocking and cheap: answer up to [`MAX_LIST_WAVEFORM_IDS`] ids from stored list
    /// Waveforms without decoding audio (a local provider reads `waveform_cache` and only
    /// reads file Metadata to detect changed files; a server provider sends one
    /// `POST /waveforms`). Items keep the request order without duplicates. Callers run
    /// this on a background thread; `cancel` abandons the request.
    fn list_waveforms(&self, _ids: &[i64], _cancel: Arc<AtomicBool>) -> Result<ListWaveformBatch> {
        Err(Error::Validation(
            "This data provider has no list Waveforms".into(),
        ))
    }
    /// Blocking and expensive (decodes the whole file): compute, store and return the
    /// list Waveform of one record. Only [`ListWaveformSource::Local`] providers compute;
    /// a server computes `Pending` items itself. Callers run this on a low-priority
    /// background thread and set `cancel` to stop the decode at its next block.
    fn compute_list_waveform(
        &self,
        _id: i64,
        _cancel: Arc<AtomicBool>,
    ) -> Result<ListWaveformState> {
        Err(Error::Validation(
            "This data provider does not compute list Waveforms".into(),
        ))
    }
    /// Pre-generation backlog: ascending ids greater than `after` of present records that
    /// have no stored list Waveform, at most `limit`. Only local providers have one.
    fn list_waveform_backlog(&self, _after: i64, _limit: usize) -> Result<Vec<i64>> {
        Ok(Vec::new())
    }

    // Server list Waveform cache management (doc/api/README.md §9.5). Only server-mode
    // providers implement these; all block on the network, so callers run them on worker
    // threads (never the UI thread or audio callbacks). The defaults answer
    // `Unsupported`.

    /// Whether the server offers generation jobs (`list_waveform_jobs`) and management.
    fn waveform_cache_capabilities(
        &self,
    ) -> waveform_cache::WaveformCacheResult<waveform_cache::WaveformCacheCapabilities> {
        Err(waveform_cache::WaveformCacheError::unsupported())
    }
    /// `GET /waveforms/status`.
    fn waveform_cache_status(
        &self,
    ) -> waveform_cache::WaveformCacheResult<waveform_cache::WaveformCacheStatus> {
        Err(waveform_cache::WaveformCacheError::unsupported())
    }
    /// The waveform fields of `GET /settings`.
    fn waveform_cache_settings(
        &self,
    ) -> waveform_cache::WaveformCacheResult<waveform_cache::WaveformCacheSettings> {
        Err(waveform_cache::WaveformCacheError::unsupported())
    }
    /// `PATCH /settings` (`pregenerate_list_waveforms`, `list_waveform_rate`).
    fn update_waveform_cache_settings(
        &self,
        _patch: &waveform_cache::WaveformCacheSettingsPatch,
    ) -> waveform_cache::WaveformCacheResult<waveform_cache::WaveformCacheSettings> {
        Err(waveform_cache::WaveformCacheError::unsupported())
    }
    /// Start a generation job; `Rebuild` requires `confirm_rebuild`.
    fn start_waveform_cache_job(
        &self,
        _mode: waveform_cache::WaveformJobMode,
        _confirm_rebuild: bool,
    ) -> waveform_cache::WaveformCacheResult<waveform_cache::WaveformCacheStatus> {
        Err(waveform_cache::WaveformCacheError::unsupported())
    }
    /// Pause, resume or stop the current job.
    fn control_waveform_cache_job(
        &self,
        _control: waveform_cache::WaveformJobControl,
    ) -> waveform_cache::WaveformCacheResult<waveform_cache::WaveformCacheStatus> {
        Err(waveform_cache::WaveformCacheError::unsupported())
    }
    /// Legacy pre-generation pause/resume (servers without `list_waveform_jobs`).
    fn control_waveform_pregeneration(
        &self,
        _control: waveform_cache::WaveformJobControl,
    ) -> waveform_cache::WaveformCacheResult<()> {
        Err(waveform_cache::WaveformCacheError::unsupported())
    }
}

/// Resolve an existing scan root using actual filesystem identity, not prefix strings.
pub fn validate_scan_root(path: &Path, allowed_roots: &[PathBuf]) -> Result<PathBuf> {
    if path.as_os_str().is_empty() || !path.is_dir() {
        return Err(Error::Validation(
            "Scan root must be an existing directory".into(),
        ));
    }
    let canonical = path.canonicalize()?;
    if !allowed_roots.is_empty()
        && !allowed_roots.iter().any(|root| {
            root.canonicalize()
                .is_ok_and(|root| canonical.starts_with(root))
        })
    {
        return Err(Error::Validation(
            "Directory is outside allowed scan roots".into(),
        ));
    }
    Ok(canonical)
}

/// Resolve a WAV's real filesystem target before checking its permitted directory.
/// Callers must pass the returned path to the operation, rather than the original link.
pub fn validate_wav_path(path: &Path, allowed_roots: &[PathBuf]) -> Result<PathBuf> {
    let canonical = path.canonicalize()?;
    if !canonical.is_file()
        || !canonical
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("wav"))
    {
        return Err(Error::Validation(
            "Target must be an existing WAV file".into(),
        ));
    }
    let parent = canonical
        .parent()
        .ok_or_else(|| Error::Validation("WAV parent folder is required".into()))?;
    validate_scan_root(parent, allowed_roots)?;
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wav_validation_resolves_targets_and_rejects_invalid_or_outside_files() {
        let directory = tempfile::tempdir().unwrap();
        let allowed = directory.path().join("allowed");
        let outside = directory.path().join("outside");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let permitted_file = allowed.join("sound.WAV");
        let forbidden_file = outside.join("sound.wav");
        let wrong_extension = allowed.join("sound.txt");
        for path in [&permitted_file, &forbidden_file, &wrong_extension] {
            std::fs::write(path, []).unwrap();
        }
        let roots = [allowed.clone()];
        assert_eq!(
            validate_wav_path(&permitted_file, &roots).unwrap(),
            permitted_file.canonicalize().unwrap()
        );
        assert!(validate_wav_path(&forbidden_file, &roots).is_err());
        assert!(validate_wav_path(&wrong_extension, &roots).is_err());
        assert!(validate_wav_path(&allowed, &roots).is_err());
        assert!(validate_wav_path(&allowed.join("absent.wav"), &roots).is_err());
    }

    #[test]
    #[cfg(any(unix, windows))]
    fn wav_link_cannot_escape_allowed_roots_and_allowed_link_returns_real_target() {
        let directory = tempfile::tempdir().unwrap();
        let allowed = directory.path().join("allowed");
        let outside = directory.path().join("outside");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let target = outside.join("outside.wav");
        let allowed_target = allowed.join("real.wav");
        std::fs::write(&target, []).unwrap();
        std::fs::write(&allowed_target, []).unwrap();
        let forbidden_link = allowed.join("linked.wav");
        let permitted_link = allowed.join("permitted.wav");
        #[cfg(windows)]
        let link_result = std::os::windows::fs::symlink_file(&target, &forbidden_link);
        #[cfg(unix)]
        let link_result = std::os::unix::fs::symlink(&target, &forbidden_link);
        #[cfg(windows)]
        if link_result
            .as_ref()
            .is_err_and(|error| error.raw_os_error() == Some(1314))
        {
            eprintln!("Symbolic-link regression requires Windows Developer Mode or link privilege");
            return;
        }
        link_result.unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&allowed_target, &permitted_link).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&allowed_target, &permitted_link).unwrap();
        let roots = [allowed];
        assert!(validate_wav_path(&forbidden_link, &roots).is_err());
        assert_eq!(
            validate_wav_path(&permitted_link, &roots).unwrap(),
            allowed_target.canonicalize().unwrap()
        );
    }

    #[test]
    fn bounded_search_and_metadata() {
        let mut request = SearchRequest::default();
        assert!(request.validate().is_ok());
        request.filters.duration_min = Some(0.0);
        assert!(request.validate().is_ok());
        request.query = "x".repeat(501);
        assert!(request.validate().is_err());
        assert!(
            MetadataPatch {
                color: Some("red".into()),
                ..Default::default()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn album_filters_reject_nul_and_unbounded_input_without_truncating_declared_names() {
        let mut request = SearchRequest::default();
        request.filters.album = Some("名".repeat(1000));
        assert!(request.validate().is_ok());
        request.filters.album = Some("name\0suffix".into());
        assert!(request.validate().is_err());
        request.filters.album = Some("x".repeat(32 * 1024 * 1024 + 1));
        assert!(request.validate().is_err());
    }

    #[test]
    fn a_lone_letter_waits_without_restricting_other_searches() {
        for query in [
            "e",
            "E",
            "  e  ",
            "(e)*",
            "é",
            "e\u{301}",
            "e\u{345}",
            "Ä",
            "Ε",
            "a\u{5b0}",
            "(a\u{5b0})",
            "a\u{5b0}\u{345}",
            "א\u{5b0}",
            "ب\u{650}",
        ] {
            let request = SearchRequest {
                query: query.into(),
                ..Default::default()
            };
            assert!(request.needs_more_search_text(), "{query:?}");
            assert!(request.validate().is_err(), "{query:?}");
        }
        for query in [
            "",
            " ",
            "1",
            "1.2",
            "P-51",
            "P51",
            "e1",
            "雷",
            "爆",
            "𠀀",
            "ex",
            "plos",
            "sion",
            "e b",
            "a explosion",
            "e雷",
            "A.wav",
            "B.wav",
            "a\u{5b0}b",
            "雷\u{5b0}",
            "1\u{5b0}",
        ] {
            let request = SearchRequest {
                query: query.into(),
                ..Default::default()
            };
            assert!(!request.needs_more_search_text(), "{query:?}");
            assert!(request.validate().is_ok(), "{query:?}");
        }
    }

    #[test]
    fn audio_dto_ignores_retired_and_unknown_legacy_json_fields() {
        let file = AudioFile {
            id: 7,
            filename: "owned.wav".into(),
            notes: "retain this annotation".into(),
            attributes: serde_json::json!({"unknown_extension":{"preserved":true}}),
            ..Default::default()
        };
        let clean = serde_json::to_value(&file).unwrap();
        let mut legacy = clean.clone();
        legacy["play_count"] = serde_json::json!(17);
        legacy["last_played_at"] = serde_json::json!("2026-01-02T03:04:05Z");
        legacy["future_optional_response_field"] = serde_json::json!([1, 2]);
        let decoded: AudioFile = serde_json::from_value(legacy).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), clean);
    }

    #[test]
    fn sortable_columns_preserve_named_wire_contract() {
        for (name, expected) in [
            ("channels", SortOrder::Channels),
            ("tags", SortOrder::Tags),
            ("modified", SortOrder::Modified),
        ] {
            let encoded = serde_json::to_string(&expected).unwrap();
            assert_eq!(encoded, format!("\"{name}\""));
            assert_eq!(
                serde_json::from_str::<SortOrder>(&encoded).unwrap(),
                expected
            );
        }
    }
}
