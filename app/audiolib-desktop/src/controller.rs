// app/audiolib-desktop/src/controller.rs
//! Desktop presentation: owned provider DTOs, bounded workers and event-loop publication.
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Condvar, Mutex, RwLock, Weak,
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use crate::list_waveforms::{
    ListWaveformCell, ListWaveformHost, ListWaveforms, render_list_waveform, window_ids,
};
use crate::waveform_view::{WaveView, db_from_gain, gain_from_db, render_waveform};
use crate::{AppWindow, AudioRow, CrumbRow, FolderRow};
use audiolib_audio::{
    AudioBackend, AudioCommand, AudioDevice, AudioEvent, AudioHandle, AuditionSettings,
    PlaybackConfig, PlaybackState, RatePolicy, WaveformRequest,
};
use audiolib_core::config::{
    AppConfig, AudioTablePreferences, SpectralPeakPreferences, UI_SCALE_PERCENTS,
    normalize_ui_scale_percent,
};
use audiolib_core::{
    AudioFile, BatchMetadataRequest, DataProvider, FileVisibility, Folder, LibraryExportFormat,
    ListWaveformSource, MaintenanceAction, MetadataPatch, ScanSession, SearchFilters,
    SearchRequest, SortOrder, TagEditMode,
};
use audiolib_db::{ScanCancellation, ScanOptions, ScanService, SqliteProvider};
use slint::{ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};

const PAGE_SIZE: usize = 200;
const HISTORY_SIZE: usize = 50;
/// Owned scan presentation can be regenerated after a runtime language change.
#[derive(Clone)]
enum ScanPresentation {
    Progress(audiolib_core::ScanProgress, bool),
    Complete(audiolib_core::ScanProgress, bool),
    Failure(String, String),
}
#[cfg(test)]
#[path = "client_review_tests.rs"]
mod client_review_tests;
#[path = "controller_history.rs"]
mod history;
#[path = "search_tasks.rs"]
mod search_tasks;

fn table_sort_order(index: i32) -> SortOrder {
    match index {
        1 => SortOrder::Filename,
        2 => SortOrder::Duration,
        3 => SortOrder::SampleRate,
        4 => SortOrder::Channels,
        5 => SortOrder::Tags,
        6 => SortOrder::Modified,
        _ => SortOrder::Relevance,
    }
}

fn sort_index(order: SortOrder) -> i32 {
    match order {
        SortOrder::Relevance => 0,
        SortOrder::Filename => 1,
        SortOrder::Duration => 2,
        SortOrder::SampleRate => 3,
        SortOrder::Channels => 4,
        SortOrder::Tags => 5,
        SortOrder::Modified => 6,
    }
}

/// Appearance combo index for a persisted interface scale: 0 is Automatic.
pub(crate) fn ui_scale_index(percent: u32) -> i32 {
    let percent = normalize_ui_scale_percent(percent);
    UI_SCALE_PERCENTS
        .iter()
        .position(|candidate| *candidate == percent)
        .map_or(0, |position| position as i32 + 1)
}

/// Persisted interface scale for an Appearance combo index; unknown indices follow
/// the system.
pub(crate) fn ui_scale_percent(index: i32) -> u32 {
    usize::try_from(index - 1)
        .ok()
        .and_then(|position| UI_SCALE_PERCENTS.get(position).copied())
        .unwrap_or(0)
}

/// Show the interface scale in the Appearance page and apply it to Theme.ui-scale.
/// Automatic keeps 1.0: the OS display scale already applies to the window.
pub(crate) fn apply_ui_scale(ui: &AppWindow, percent: u32) {
    let percent = normalize_ui_scale_percent(percent);
    ui.set_ui_scale_index(ui_scale_index(percent));
    let scale = if percent == 0 {
        1.0
    } else {
        percent as f32 / 100.0
    };
    ui.global::<crate::Theme>().set_ui_scale(scale);
}

fn table_preferences(ui: &AppWindow) -> AudioTablePreferences {
    AudioTablePreferences {
        name_width: ui.get_table_name_width(),
        duration_width: ui.get_table_duration_width(),
        format_width: ui.get_table_format_width(),
        channels_width: ui.get_table_channels_width(),
        tags_width: ui.get_table_tags_width(),
        order_by: table_sort_order(ui.get_sort_index()),
        descending: ui.get_sort_descending(),
    }
    .normalized()
}

#[cfg(test)]
#[path = "click_preview_tests.rs"]
mod click_preview_tests;
#[cfg(test)]
#[path = "folder_search_tests.rs"]
mod folder_search_tests;
#[cfg(test)]
#[path = "library_context_tests.rs"]
mod library_context_tests;
#[path = "metadata_recovery.rs"]
mod metadata_recovery;
#[path = "remote_mode.rs"]
mod remote_mode;
#[cfg(test)]
#[path = "remote_output_tests.rs"]
mod remote_output_tests;
#[cfg(test)]
#[path = "request_recovery_tests.rs"]
mod request_recovery_tests;
#[cfg(test)]
#[path = "result_rows_tests.rs"]
mod result_rows_tests;
#[cfg(test)]
#[path = "single_click_play_tests.rs"]
mod single_click_play_tests;
#[cfg(test)]
#[path = "table_tests.rs"]
mod table_tests;
pub use remote_mode::{RemoteStartup, open_remote_placeholder, open_remote_startup_with_progress};

#[derive(Clone)]
enum LibraryOperation {
    Backup(PathBuf),
    Restore(PathBuf),
    Check,
    Maintain(MaintenanceAction),
    Switch {
        path: PathBuf,
        create: bool,
    },
    Export(PathBuf, LibraryExportFormat),
    Import(PathBuf),
    RemoveAudio(Vec<i64>),
    RemoveFolder(i64),
    Relink(i64, PathBuf),
    Batch(BatchMetadataRequest),
    /// Swap between the local library and a read-only server snapshot.
    DataSource(remote_mode::DataSourceSwitch),
    /// Server root mutations share the Metadata drain and library operation gate.
    ServerRoot(Box<remote_mode::RootOperation>),
}

impl LibraryOperation {
    fn needs_confirmation(&self) -> bool {
        matches!(
            self,
            Self::Restore(_)
                | Self::Maintain(_)
                | Self::Import(_)
                | Self::RemoveAudio(_)
                | Self::RemoveFolder(_)
        )
    }

    fn changes_records(&self) -> bool {
        matches!(
            self,
            Self::Restore(_)
                | Self::Switch { .. }
                | Self::Import(_)
                | Self::RemoveAudio(_)
                | Self::RemoveFolder(_)
                | Self::Relink(..)
                | Self::Batch(_)
                | Self::DataSource(_)
                | Self::ServerRoot(_)
        )
    }

    fn is_read_only(&self) -> bool {
        matches!(self, Self::Backup(_) | Self::Check | Self::Export(..))
    }

    fn retains_library_identity(&self) -> bool {
        matches!(
            self,
            Self::Import(_)
                | Self::RemoveAudio(_)
                | Self::RemoveFolder(_)
                | Self::Relink(..)
                | Self::Batch(_)
        )
    }

    fn affects_playback(&self, file: Option<&AudioFile>, removed_folders: &HashSet<i64>) -> bool {
        match self {
            Self::Restore(_) | Self::Switch { .. } | Self::DataSource(_) | Self::ServerRoot(_) => {
                true
            }
            Self::RemoveAudio(ids) => file.is_some_and(|file| ids.contains(&file.id)),
            Self::RemoveFolder(_) => file.is_some_and(|file| {
                file.folder_id
                    .is_some_and(|id| removed_folders.contains(&id))
            }),
            Self::Relink(id, _) => file.is_some_and(|file| file.id == *id),
            _ => false,
        }
    }

    fn confirmation(&self, cn: bool) -> String {
        match self {
            Self::Restore(path) => {
                if cn {
                    format!(
                        "用 {} 恢复当前素材库？当前库会先自动备份，可用于回滚。原始 WAV 不会修改。",
                        path.display()
                    )
                } else {
                    format!(
                        "Restore the current library from {}? An automatic backup preserves the current library for rollback. Source WAV files are unchanged.",
                        path.display()
                    )
                }
            }
            Self::Maintain(_) => {
                if cn {
                    "维护前会自动备份当前数据库。继续执行维护？".into()
                } else {
                    "An automatic backup is created before database maintenance. Continue?".into()
                }
            }
            Self::Import(path) => {
                if cn {
                    format!(
                        "从 {} 导入标注？匹配记录的标签、颜色、备注与收藏会被覆盖；未匹配记录不会导入。",
                        path.display()
                    )
                } else {
                    format!(
                        "Import annotations from {}? Tags, colors, notes and favorites of matching records will be replaced. Unmatched records are skipped.",
                        path.display()
                    )
                }
            }
            Self::RemoveAudio(ids) => {
                if cn {
                    format!(
                        "从素材库移除 {} 条记录及其标注/关联？此操作仅修改数据库，绝不删除 WAV 文件。",
                        ids.len()
                    )
                } else {
                    format!(
                        "Remove {} {} and their annotations/references from the library? This changes only the database and never deletes WAV files.",
                        ids.len(),
                        if ids.len() == 1 { "record" } else { "records" }
                    )
                }
            }
            Self::RemoveFolder(_) => {
                if cn {
                    "从素材库移除此目录及子目录记录？标注/关联将移除，磁盘目录和 WAV 文件不会删除。"
                        .into()
                } else {
                    "Remove this folder and its descendants from the library? Annotations/references are removed; disk folders and WAV files are preserved.".into()
                }
            }
            _ => String::new(),
        }
    }
}

/// A network audition that waits this long for data shows the "buffering" hint.
const BUFFERING_HINT_AFTER: Duration = Duration::from_millis(350);

#[derive(Clone)]
struct SearchJob {
    request: SearchRequest,
    generation: u64,
    not_before: Instant,
    cancel: Arc<AtomicBool>,
    facet_revision: u64,
    provider: Arc<SqliteProvider>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SearchPagePhase {
    Pending,
    Published,
    Failed,
}

/// Event-loop publication joins independently computed pages and directory counts.
/// A successful page precedes facets, and Ready follows the actual tree model.
struct SearchPublication {
    generation: u64,
    page: SearchPagePhase,
    pending_folders: Option<Result<Arc<Vec<Folder>>, String>>,
    expected_folders: Option<Arc<Vec<Folder>>>,
    model_published: bool,
    /// A failed close resumes reads without hiding the Metadata save failure.
    retained_status: Option<String>,
}

#[derive(Clone)]
struct SelectionJob {
    id: i64,
    play: bool,
    generation: u64,
    barrier: u64,
    provider: Arc<SqliteProvider>,
}

struct SelectionAudioJob {
    file: AudioFile,
    play: bool,
    remote: bool,
    generation: u64,
    cancel: Arc<AtomicBool>,
}

/// One query/filter result only; navigation does not change directory match counts.
struct FolderFacets {
    // Weak identity prevents pointer reuse without keeping an old library open.
    provider: Weak<SqliteProvider>,
    revision: u64,
    database_revision: Option<u64>,
    query: String,
    filters: SearchFilters,
    folders: Arc<Vec<Folder>>,
}

impl FolderFacets {
    fn matches(
        &self,
        request: &SearchRequest,
        revision: u64,
        database_revision: Option<u64>,
        provider: &Arc<SqliteProvider>,
    ) -> bool {
        self.provider.ptr_eq(&Arc::downgrade(provider))
            && database_revision.is_some()
            && self.database_revision == database_revision
            && self.revision == revision
            && self.query == request.query
            && self.filters == request.filters
    }
}

#[derive(Clone)]
struct WaveJob {
    path: PathBuf,
    /// Server mode: the record whose waveform the server computes (`/waveform`).
    remote_id: Option<i64>,
    /// Sample rate of the record (frames are in the original's frames).
    sample_rate: u32,
    frames: u64,
    start_frame: u64,
    end_frame: u64,
    duration: f64,
    view: WaveView,
    generation: u64,
    cancel: Arc<AtomicBool>,
    width: u32,
    height: u32,
}
#[derive(Clone, Default)]
struct TreeState {
    matches: Arc<Vec<Folder>>,
    visible: Arc<Vec<(Folder, usize, bool)>>,
    known: Arc<HashMap<i64, Folder>>,
    stats: Option<audiolib_core::LibraryStats>,
    expanded: HashSet<i64>,
    selected: Option<i64>,
    crumbs: Vec<Folder>,
}

/// Immutable rows are converted only when Slint's virtualized viewport requests them.
/// Publishing a large tree therefore does not allocate one UI string set per directory.
struct FolderTreeModel {
    visible: Arc<Vec<(Folder, usize, bool)>>,
    expanded: HashSet<i64>,
}

impl FolderTreeModel {
    fn has_same_rows(&self, visible: &[(Folder, usize, bool)], expanded: &HashSet<i64>) -> bool {
        self.expanded == *expanded
            && self.visible.len() == visible.len()
            && self.visible.iter().zip(visible).all(|(old, new)| {
                (
                    old.0.id,
                    &old.0.path,
                    &old.0.name,
                    old.0.file_count,
                    old.1,
                    old.2,
                ) == (
                    new.0.id,
                    &new.0.path,
                    &new.0.name,
                    new.0.file_count,
                    new.1,
                    new.2,
                )
            })
    }
}

impl Model for FolderTreeModel {
    type Data = FolderRow;

    fn row_count(&self) -> usize {
        self.visible.len()
    }

    fn row_data(&self, index: usize) -> Option<FolderRow> {
        self.visible
            .get(index)
            .map(|(folder, depth, children)| FolderRow {
                name: folder.name.clone().into(),
                count: folder.file_count.to_string().into(),
                path: folder.path.clone().into(),
                depth: *depth as i32,
                expanded: self.expanded.contains(&folder.id),
                has_children: *children,
            })
    }

    fn model_tracker(&self) -> &dyn slint::ModelTracker {
        &()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[derive(Clone)]
struct MetadataDraft {
    revision: u64,
    patch: MetadataPatch,
    /// Last confirmed value survives bounded saved-row cache eviction while editing.
    baseline: Option<Box<AudioFile>>,
}

#[derive(Clone)]
enum MetadataWrite {
    Draft(MetadataDraft),
    QuickAction(i32),
}

#[derive(Clone)]
struct MetadataJob {
    sequence: u64,
    id: i64,
    write: MetadataWrite,
}

/// Draft identity and save order are independent of the currently visible result row.
#[derive(Default)]
struct MetadataState {
    revision: u64,
    sequence: u64,
    completed: u64,
    drafts: HashMap<i64, MetadataDraft>,
    jobs: VecDeque<MetadataJob>,
    in_flight: Option<MetadataJob>,
    failures: HashMap<i64, String>,
    failed_actions: HashMap<i64, i32>,
    saved: HashMap<i64, (u64, AudioFile)>,
    saved_order: VecDeque<i64>,
}

impl MetadataState {
    fn edit(&mut self, id: i64, patch: MetadataPatch) {
        if self
            .drafts
            .get(&id)
            .is_some_and(|draft| same_patch(&draft.patch, &patch))
        {
            return;
        }
        let baseline = self
            .saved
            .get(&id)
            .map(|(_, file)| Box::new(file.clone()))
            .or_else(|| {
                self.drafts
                    .get(&id)
                    .and_then(|draft| draft.baseline.clone())
            });
        self.revision += 1;
        self.drafts.insert(
            id,
            MetadataDraft {
                revision: self.revision,
                patch,
                baseline,
            },
        );
    }

    fn enqueue_draft(&mut self, id: i64) -> Result<u64, String> {
        let Some(draft) = self.drafts.get(&id).cloned() else {
            return Ok(self.sequence);
        };
        draft.patch.validate().map_err(|error| error.to_string())?;
        let already_requested = self.jobs.iter().chain(self.in_flight.iter()).any(|job| {
            job.id == id && matches!(&job.write, MetadataWrite::Draft(requested) if requested.revision == draft.revision)
        });
        if !already_requested {
            self.enqueue(id, MetadataWrite::Draft(draft));
        }
        Ok(self.sequence)
    }

    fn enqueue(&mut self, id: i64, write: MetadataWrite) {
        self.sequence += 1;
        self.jobs.push_back(MetadataJob {
            sequence: self.sequence,
            id,
            write,
        });
    }

    fn enqueue_all(&mut self) -> Result<u64, String> {
        for draft in self.drafts.values() {
            draft.patch.validate().map_err(|error| error.to_string())?;
        }
        let mut ids = self.drafts.keys().copied().collect::<Vec<_>>();
        ids.sort_unstable();
        for id in ids {
            self.enqueue_draft(id)?;
        }
        let retries = self
            .failed_actions
            .iter()
            .map(|(id, action)| (*id, *action))
            .collect::<Vec<_>>();
        for (id, action) in retries {
            if !self.drafts.contains_key(&id) {
                self.enqueue(id, MetadataWrite::QuickAction(action));
            }
        }
        Ok(self.sequence)
    }

    fn next(&mut self) -> Option<MetadataJob> {
        let job = self.jobs.pop_front()?;
        self.in_flight = Some(job.clone());
        Some(job)
    }

    fn complete(&mut self, job: &MetadataJob, result: &Result<AudioFile, String>) {
        self.completed = job.sequence;
        self.in_flight = None;
        match result {
            Ok(file) => {
                self.failures.remove(&job.id);
                self.failed_actions.remove(&job.id);
                if let MetadataWrite::Draft(requested) = &job.write
                    && self
                        .drafts
                        .get(&job.id)
                        .is_some_and(|draft| draft.revision == requested.revision)
                {
                    self.drafts.remove(&job.id);
                }
                if let Some(draft) = self.drafts.get_mut(&job.id) {
                    // A save may complete underneath a newer unsaved edit.
                    draft.baseline = Some(Box::new(file.clone()));
                }
                self.saved.insert(job.id, (job.sequence, file.clone()));
                self.saved_order.retain(|id| *id != job.id);
                self.saved_order.push_back(job.id);
                if self.saved_order.len() > PAGE_SIZE
                    && let Some(id) = self.saved_order.pop_front()
                {
                    self.saved.remove(&id);
                }
            }
            Err(error) => {
                self.failures.insert(job.id, error.clone());
                if let MetadataWrite::QuickAction(action) = job.write {
                    self.failed_actions.insert(job.id, action);
                }
            }
        }
    }

    fn for_display(&self, fetched: &AudioFile, read_sequence: u64) -> (AudioFile, bool) {
        let mut file = self
            .saved
            .get(&fetched.id)
            .filter(|(sequence, _)| *sequence > read_sequence)
            .map_or_else(|| fetched.clone(), |(_, file)| file.clone());
        if let Some(draft) = self.drafts.get(&file.id) {
            apply_metadata_patch(&mut file, &draft.patch);
            (file, true)
        } else {
            (file, false)
        }
    }

    fn failure(&self) -> Option<(i64, String)> {
        self.failures
            .iter()
            .min_by_key(|(id, _)| *id)
            .map(|(id, error)| (*id, error.clone()))
    }

    /// After the library barrier drains, discard library-owned data while keeping
    /// sequence counters monotonic so already-waiting readers can finish their barrier.
    fn reset_library(&mut self) {
        debug_assert!(self.jobs.is_empty() && self.in_flight.is_none());
        *self = Self {
            revision: self.revision,
            sequence: self.sequence,
            completed: self.completed,
            ..Self::default()
        };
    }
}

/// How long closing waits for cancelled remote cache work before exiting anyway.
const REMOTE_CLOSE_GRACE: Duration = Duration::from_secs(15);

pub struct Controller {
    ui: slint::Weak<AppWindow>,
    sqlite: RwLock<Arc<SqliteProvider>>,
    library_generation: AtomicU64,
    library_busy: AtomicBool,
    source_changing: AtomicBool,
    source_generation: AtomicU64,
    close_after_operation: AtomicBool,
    pending_operation: Mutex<Option<LibraryOperation>>,
    multi_selection: Mutex<HashSet<i64>>,
    selection_anchor: Mutex<Option<i64>>,
    history_generation: AtomicU64,
    history_offset: Mutex<usize>,
    plays: Mutex<Vec<audiolib_db::local_play_history::LocalPlayRecord>>,
    pending_history_clear: Mutex<Option<history::PendingClear>>,
    scans: Mutex<Vec<ScanSession>>,
    audio: AudioHandle,
    /// A removed loaded record cannot be replayed through the UI until a new Load.
    removed_audio_generation: AtomicU64,
    config: Arc<Mutex<AppConfig>>,
    /// Latest GUI preference; its worker serializes disk writes with other settings.
    single_click_play: AtomicBool,
    /// Latest table layout, captured on the event loop and saved by the preferences worker.
    audio_table: Mutex<AudioTablePreferences>,
    task_workspace: Mutex<search_tasks::TaskWorkspaceState>,
    workspace_writes: Mutex<search_tasks::WorkspaceWrites>,
    workspace_wake: Condvar,
    max_search_tasks: AtomicU32,
    /// Window geometry and pane layout, captured on the UI thread when a pane
    /// divider is released and when closing; saved by the preferences worker.
    window_layout: Mutex<audiolib_core::window_layout::WindowLayoutPreferences>,
    single_click_play_pending: Mutex<bool>,
    single_click_play_wake: Condvar,
    playback: Mutex<PlaybackConfig>,
    records: Mutex<Vec<AudioFile>>,
    selected: Mutex<Option<AudioFile>>,
    metadata: Mutex<MetadataState>,
    metadata_wake: Condvar,
    /// Durable recovery artifact for drafts bound to a replaced server library.
    metadata_recovery_notice: Mutex<Option<PathBuf>>,
    closing: AtomicBool,
    /// Background drain completion is retained until the closing UI timer consumes
    /// it; a rejected cross-thread event-loop notification must not lose it.
    close_completion: Mutex<Option<Result<(), String>>>,
    tree: Mutex<TreeState>,
    tree_generation: AtomicU64,
    selection_generation: AtomicU64,
    selection_job: Mutex<Option<SelectionJob>>,
    latest_selection: Mutex<Option<SelectionJob>>,
    selection_wake: Condvar,
    selection_audio_job: Mutex<Option<SelectionAudioJob>>,
    selection_audio_wake: Condvar,
    /// Serialize remote route preparation and Load acknowledgment with output
    /// reconfiguration. Selection/Stop still cancel through the admission lock.
    remote_audio_prepare: Mutex<()>,
    search_generation: AtomicU64,
    search_job: Mutex<Option<SearchJob>>,
    search_wake: Condvar,
    facet_job: Mutex<Option<SearchJob>>,
    facet_wake: Condvar,
    search_publication: Mutex<Option<SearchPublication>>,
    search_cancel: Mutex<Arc<AtomicBool>>,
    facet_revision: AtomicU64,
    library_info_generation: AtomicU64,
    request: Mutex<SearchRequest>,
    waveform_generation: AtomicU64,
    waveform_cancel: Mutex<Arc<AtomicBool>>,
    waveform_job: Mutex<Option<WaveJob>>,
    waveform_wake: Condvar,
    waveform_size: Mutex<(u32, u32)>,
    wave_view: Mutex<(String, WaveView)>,
    devices: Mutex<Vec<AudioDevice>>,
    device_generation: AtomicU64,
    scan_generation: AtomicU64,
    scan_validating: AtomicBool,
    scan_presentation: Mutex<Option<ScanPresentation>>,
    scan_cancel: Mutex<Option<ScanCancellation>>,
    shutdown: AtomicBool,
    chinese: AtomicBool,
    remote: Mutex<remote_mode::RemoteRuntime>,
    /// The provider is a server snapshot: annotation writes go to the server, structural
    /// library writes are refused.
    remote_mode: AtomicBool,
    remote_generation: AtomicU64,
    remote_busy: AtomicBool,
    /// Longest wait for cancelled remote cache work while closing (injectable for tests);
    /// past it the window closes anyway and the process exit abandons the worker.
    remote_close_grace: Mutex<Duration>,
    /// Registered play workers include their reply and listener/cache settlement.
    remote_plays: Mutex<remote_mode::RemotePlayState>,
    remote_plays_wake: Condvar,
    remote_cancel: Mutex<audiolib_remote::SyncControl>,
    remote_audio_cancel: Mutex<Arc<AtomicBool>>,
    /// The selected record whose Load has not been confirmed. Local or server
    /// cancellation may leave the previous record in the engine, so Play reloads
    /// the selection until a current Load succeeds.
    audio_reload: Mutex<Option<i64>>,
    /// Server-mode in-memory stream of the selected record, registered as its playback
    /// source.
    remote_stream: Mutex<Option<remote_mode::RemotePlayback>>,
    /// Old engine source retained while a different representation is prepared.
    /// Stop/shutdown must also interrupt it after the new route is registered.
    remote_retained_stream: Mutex<Option<remote_mode::RemotePlayback>>,
    /// Wake counter for the background revision poller (bumped to check immediately).
    poll_signal: Mutex<u64>,
    poll_wake: Condvar,
    /// A drag-out is being prepared (server downloads) or running.
    drag_busy: AtomicBool,
    drag_cancel: Mutex<Arc<AtomicBool>>,
    /// Result-table thumbnail Waveforms (doc/api/README.md §9.3), one background worker.
    list_waveforms: ListWaveforms,
    /// The "pre-generate list Waveforms" preference (local mode).
    pregenerate_list_waveforms: AtomicBool,
    /// The "hold list-waveform batch requests during the first 3 s after audition start"
    /// preference (server mode).
    hold_list_waveform_batches: AtomicBool,
    ui_scale_percent: AtomicU32,
    /// Appearance theme identifier and font sizes, persisted with the other preferences.
    theme: Mutex<String>,
    ui_font_size: AtomicU32,
    list_font_size: AtomicU32,
}

fn post(ui: slint::Weak<AppWindow>, action: impl FnOnce(AppWindow) + Send + 'static) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(window) = ui.upgrade() {
            action(window);
        }
    });
}

/// Run one worker job. A panic becomes this job's error, so the worker thread keeps
/// serving later jobs and waiters see the job finish; the panic hook has logged it.
fn survive_panic<T, E>(
    run: impl FnOnce() -> Result<T, E>,
    panicked: impl FnOnce() -> E,
) -> Result<T, E> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)).unwrap_or_else(|_| Err(panicked()))
}

fn internal_error(chinese: bool, task_cn: &str, task_en: &str) -> String {
    if chinese {
        format!("{task_cn}时发生内部错误，详情见日志")
    } else {
        format!("{task_en} failed with an internal error; see the log for details")
    }
}

impl Controller {
    fn provider(&self) -> Arc<SqliteProvider> {
        self.sqlite.read().unwrap().clone()
    }

    fn spectral_peak_preferences(&self) -> SpectralPeakPreferences {
        let view = self.wave_view.lock().unwrap().1;
        SpectralPeakPreferences {
            noise: view.noise,
            variance: view.variance,
        }
        .normalized()
    }

    /// Provider for annotation writes: the local library, or in server mode the
    /// server API (`RemoteProvider`; the snapshot follows by the revision rule).
    fn metadata_provider(self: &Arc<Self>) -> Result<Arc<dyn DataProvider>, String> {
        self.require_metadata_writable()?;
        if self.remote_read_only() {
            Ok(self.remote_provider()? as Arc<dyn DataProvider>)
        } else {
            Ok(self.provider() as Arc<dyn DataProvider>)
        }
    }

    fn available(&self) -> bool {
        !self.library_busy.load(Ordering::Acquire)
            && !self.source_changing.load(Ordering::Acquire)
            && !self.closing.load(Ordering::Acquire)
            && !self.shutdown.load(Ordering::Acquire)
    }

    fn localized(&self, chinese: &str, english: &str) -> slint::SharedString {
        if self.chinese.load(Ordering::Acquire) {
            chinese
        } else {
            english
        }
        .into()
    }

    /// Bind desktop callbacks while all database and analysis work stays off the event loop.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new(
        ui: &AppWindow,
        sqlite: Arc<SqliteProvider>,
        audio: AudioHandle,
        config: AppConfig,
        playback: PlaybackConfig,
    ) -> Arc<Self> {
        Self::new_with_source(ui, sqlite, audio, config, playback, None)
    }

    /// Like [`Controller::new`]; `remote` selects server mode with the given snapshot.
    pub fn new_with_source(
        ui: &AppWindow,
        sqlite: Arc<SqliteProvider>,
        audio: AudioHandle,
        config: AppConfig,
        playback: PlaybackConfig,
        remote: Option<RemoteStartup>,
    ) -> Arc<Self> {
        let chinese = config.language == "cn";
        ui.set_chinese(chinese);
        ui.set_audition_play_on_single_click(config.audition_play_on_single_click);
        ui.set_pregenerate_list_waveforms(config.pregenerate_list_waveforms);
        ui.set_hold_list_waveform_batches(config.hold_list_waveform_batches_after_audition);
        apply_ui_scale(ui, config.ui_scale_percent);
        crate::window_layout::restore_panes(ui, &config.window_layout);
        crate::theme::apply_theme(ui, &config.theme);
        crate::theme::apply_font_sizes(ui, config.ui_font_size, config.list_font_size);
        let table = config.audio_table.normalized();
        let spectral_peaks = config.spectral_peaks.normalized();
        ui.set_peak_noise(spectral_peaks.noise);
        ui.set_peak_variance(spectral_peaks.variance);
        ui.set_table_name_width(table.name_width);
        ui.set_table_duration_width(table.duration_width);
        ui.set_table_format_width(table.format_width);
        ui.set_table_channels_width(table.channels_width);
        ui.set_table_tags_width(table.tags_width);
        ui.set_sort_index(sort_index(table.order_by));
        ui.set_sort_descending(table.descending);
        ui.set_volume(playback.volume);
        publish_audition(ui, playback.audition);
        publish_volume(ui, playback.volume);
        ui.set_backend_index(if playback.backend == AudioBackend::Asio {
            1
        } else {
            0
        });
        ui.set_rate_index(if playback.rate_policy == RatePolicy::Native {
            1
        } else {
            0
        });
        ui.set_buffer_frames(playback.buffer_frames.to_string().into());
        ui.set_pair_index(playback.output_pair.to_string().into());
        ui.set_source_pair(playback.source_pair.to_string().into());
        ui.set_asio_enabled(cfg!(feature = "asio"));
        let controller = Arc::new(Self {
            ui: ui.as_weak(),
            sqlite: RwLock::new(sqlite),
            library_generation: AtomicU64::new(0),
            library_busy: AtomicBool::new(false),
            source_changing: AtomicBool::new(false),
            source_generation: AtomicU64::new(0),
            close_after_operation: AtomicBool::new(false),
            pending_operation: Mutex::new(None),
            multi_selection: Mutex::new(HashSet::new()),
            selection_anchor: Mutex::new(None),
            history_generation: AtomicU64::new(0),
            history_offset: Mutex::new(0),
            plays: Mutex::new(Vec::new()),
            pending_history_clear: Mutex::new(None),
            scans: Mutex::new(Vec::new()),
            audio,
            removed_audio_generation: AtomicU64::new(u64::MAX),
            single_click_play: AtomicBool::new(config.audition_play_on_single_click),
            list_waveforms: ListWaveforms::new(config.pregenerate_list_waveforms)
                .with_hold_after_audition(config.hold_list_waveform_batches_after_audition),
            pregenerate_list_waveforms: AtomicBool::new(config.pregenerate_list_waveforms),
            ui_scale_percent: AtomicU32::new(normalize_ui_scale_percent(config.ui_scale_percent)),
            theme: Mutex::new(audiolib_core::config::normalize_theme(&config.theme)),
            ui_font_size: AtomicU32::new(audiolib_core::config::clamp_font_size(i64::from(
                config.ui_font_size,
            ))),
            list_font_size: AtomicU32::new(audiolib_core::config::clamp_font_size(i64::from(
                config.list_font_size,
            ))),
            hold_list_waveform_batches: AtomicBool::new(
                config.hold_list_waveform_batches_after_audition,
            ),
            audio_table: Mutex::new(table),
            task_workspace: Mutex::new(search_tasks::TaskWorkspaceState::default()),
            workspace_writes: Mutex::new(search_tasks::WorkspaceWrites::default()),
            workspace_wake: Condvar::new(),
            max_search_tasks: AtomicU32::new(config.max_search_tasks),
            window_layout: Mutex::new(config.window_layout.normalized()),
            single_click_play_pending: Mutex::new(false),
            single_click_play_wake: Condvar::new(),
            config: Arc::new(Mutex::new(config)),
            playback: Mutex::new(playback),
            records: Mutex::new(Vec::new()),
            selected: Mutex::new(None),
            metadata: Mutex::new(MetadataState::default()),
            metadata_wake: Condvar::new(),
            metadata_recovery_notice: Mutex::new(None),
            closing: AtomicBool::new(false),
            close_completion: Mutex::new(None),
            tree: Mutex::new(TreeState::default()),
            tree_generation: AtomicU64::new(0),
            selection_generation: AtomicU64::new(0),
            selection_job: Mutex::new(None),
            latest_selection: Mutex::new(None),
            selection_wake: Condvar::new(),
            selection_audio_job: Mutex::new(None),
            selection_audio_wake: Condvar::new(),
            remote_audio_prepare: Mutex::new(()),
            search_generation: AtomicU64::new(0),
            search_job: Mutex::new(None),
            search_wake: Condvar::new(),
            facet_job: Mutex::new(None),
            facet_wake: Condvar::new(),
            search_publication: Mutex::new(None),
            search_cancel: Mutex::new(Arc::new(AtomicBool::new(false))),
            facet_revision: AtomicU64::new(0),
            library_info_generation: AtomicU64::new(0),
            request: Mutex::new(SearchRequest::default()),
            waveform_generation: AtomicU64::new(0),
            waveform_cancel: Mutex::new(Arc::new(AtomicBool::new(false))),
            waveform_job: Mutex::new(None),
            waveform_wake: Condvar::new(),
            waveform_size: Mutex::new((1300, 100)),
            wave_view: Mutex::new((
                String::new(),
                WaveView {
                    noise: spectral_peaks.noise,
                    variance: spectral_peaks.variance,
                    ..Default::default()
                },
            )),
            devices: Mutex::new(Vec::new()),
            device_generation: AtomicU64::new(0),
            scan_generation: AtomicU64::new(0),
            scan_validating: AtomicBool::new(false),
            scan_presentation: Mutex::new(None),
            scan_cancel: Mutex::new(None),
            shutdown: AtomicBool::new(false),
            chinese: AtomicBool::new(chinese),
            remote: Mutex::new(remote_mode::RemoteRuntime::default()),
            remote_mode: AtomicBool::new(false),
            remote_generation: AtomicU64::new(0),
            remote_busy: AtomicBool::new(false),
            remote_close_grace: Mutex::new(REMOTE_CLOSE_GRACE),
            remote_plays: Mutex::new(remote_mode::RemotePlayState::default()),
            remote_plays_wake: Condvar::new(),
            remote_cancel: Mutex::new(audiolib_remote::SyncControl::new()),
            remote_audio_cancel: Mutex::new(Arc::new(AtomicBool::new(false))),
            audio_reload: Mutex::new(None),
            remote_stream: Mutex::new(None),
            remote_retained_stream: Mutex::new(None),
            poll_signal: Mutex::new(0),
            poll_wake: Condvar::new(),
            drag_busy: AtomicBool::new(false),
            drag_cancel: Mutex::new(Arc::new(AtomicBool::new(false))),
        });
        controller.bind(ui);
        controller.bind_remote(ui);
        let host_arc: Arc<dyn ListWaveformHost> = controller.clone();
        let host: Weak<dyn ListWaveformHost> = Arc::downgrade(&host_arc);
        drop(host_arc);
        controller.list_waveforms.start(host);
        ui.set_database_path(
            controller
                .provider()
                .database_path()
                .display()
                .to_string()
                .into(),
        );
        controller.start_workers();
        controller.refresh_library_info();
        controller.reload_tree();
        controller.request_search(ui, true, true);
        controller.init_remote(remote);
        controller.init_search_workspace();
        controller.find_metadata_recovery();
        controller
    }

    fn bind(self: &Arc<Self>, ui: &AppWindow) {
        self.bind_search_tasks(ui);
        let c = self.clone();
        ui.on_exit_requested(move || {
            if let Some(ui) = c.ui.upgrade() {
                c.request_close(&ui);
            }
        });
        let c = self.clone();
        ui.on_refresh_requested(move || {
            if c.available()
                && let Some(ui) = c.ui.upgrade()
            {
                if c.retry_failed_search_task(&ui) {
                    return;
                }
                c.reload_tree();
                c.refresh_library_info();
                c.request_search(&ui, true, true);
                if c.remote_read_only() {
                    // F5 in server mode also checks the server revision.
                    c.start_sync(false, false);
                }
            }
        });
        let c = self.clone();
        ui.on_library_action(move |action| c.library_action(action));
        let c = self.clone();
        ui.on_confirmation_requested(move |accepted| {
            if let Some(ui) = c.ui.upgrade() {
                ui.set_confirmation_visible(false);
            }
            if c.confirm_history_clear(accepted) {
                return;
            }
            let pending = c.pending_operation.lock().unwrap().take();
            if accepted && let Some(operation) = pending {
                c.run_library_operation(operation);
            }
        });
        let c = self.clone();
        ui.on_row_clicked(move |index, toggle, range, play| {
            c.select_row(index, toggle, range, play)
        });
        let c = self.clone();
        ui.on_single_click_play_requested(move |enabled| {
            if !c.available() {
                return;
            }
            // Never wait for settings I/O on the event loop. A single worker coalesces
            // requests, and shutdown flushes the latest atomic value under the save lock.
            c.single_click_play.store(enabled, Ordering::Release);
            *c.single_click_play_pending.lock().unwrap() = true;
            c.single_click_play_wake.notify_one();
        });
        let c = self.clone();
        ui.on_pregenerate_list_waveforms_requested(move |enabled| {
            if !c.available() {
                return;
            }
            c.pregenerate_list_waveforms
                .store(enabled, Ordering::Release);
            c.list_waveforms.set_pregenerate(enabled);
            *c.single_click_play_pending.lock().unwrap() = true;
            c.single_click_play_wake.notify_one();
        });
        let c = self.clone();
        ui.on_hold_list_waveform_batches_requested(move |enabled| {
            c.hold_list_waveform_batches
                .store(enabled, Ordering::Release);
            c.list_waveforms.set_hold_after_audition(enabled);
            *c.single_click_play_pending.lock().unwrap() = true;
            c.single_click_play_wake.notify_one();
        });
        let c = self.clone();
        ui.on_ui_scale_requested(move |index| {
            let percent = ui_scale_percent(index);
            if let Some(ui) = c.ui.upgrade() {
                apply_ui_scale(&ui, percent);
            }
            c.ui_scale_percent.store(percent, Ordering::Release);
            *c.single_click_play_pending.lock().unwrap() = true;
            c.single_click_play_wake.notify_one();
        });
        let c = self.clone();
        ui.on_theme_requested(move |index| {
            let id = crate::theme::theme_id(index);
            if let Some(ui) = c.ui.upgrade() {
                crate::theme::apply_theme(&ui, &id);
            }
            *c.theme.lock().unwrap() = id;
            *c.single_click_play_pending.lock().unwrap() = true;
            c.single_click_play_wake.notify_one();
        });
        let c = self.clone();
        ui.on_ui_font_requested(move |size| {
            let size = audiolib_core::config::clamp_font_size(i64::from(size));
            c.ui_font_size.store(size, Ordering::Release);
            if let Some(ui) = c.ui.upgrade() {
                crate::theme::apply_font_sizes(&ui, size, c.list_font_size.load(Ordering::Acquire));
            }
            *c.single_click_play_pending.lock().unwrap() = true;
            c.single_click_play_wake.notify_one();
        });
        let c = self.clone();
        ui.on_list_font_requested(move |size| {
            let size = audiolib_core::config::clamp_font_size(i64::from(size));
            c.list_font_size.store(size, Ordering::Release);
            if let Some(ui) = c.ui.upgrade() {
                crate::theme::apply_font_sizes(&ui, c.ui_font_size.load(Ordering::Acquire), size);
            }
            *c.single_click_play_pending.lock().unwrap() = true;
            c.single_click_play_wake.notify_one();
        });
        let c = self.clone();
        ui.on_result_window_changed(move |first, count| c.request_list_waveforms(first, count));
        let c = self.clone();
        ui.on_table_layout_changed(move || {
            if c.available()
                && let Some(ui) = c.ui.upgrade()
            {
                *c.audio_table.lock().unwrap() = table_preferences(&ui);
                *c.single_click_play_pending.lock().unwrap() = true;
                c.single_click_play_wake.notify_one();
            }
        });
        let c = self.clone();
        ui.on_pane_layout_changed(move || {
            if c.available()
                && let Some(ui) = c.ui.upgrade()
            {
                let mut layout = c.window_layout.lock().unwrap();
                *layout = crate::window_layout::capture(&ui, &layout);
                drop(layout);
                *c.single_click_play_pending.lock().unwrap() = true;
                c.single_click_play_wake.notify_one();
            }
        });
        let c = self.clone();
        ui.on_check_page_requested(move |checked| {
            if !c.available() {
                return;
            }
            let mut selected = c.multi_selection.lock().unwrap();
            if checked {
                selected.extend(c.records.lock().unwrap().iter().map(|file| file.id));
            } else {
                selected.clear();
                *c.selection_anchor.lock().unwrap() = None;
            }
            drop(selected);
            c.publish_checked();
        });
        let c = self.clone();
        ui.on_batch_requested(move || c.apply_batch());
        let c = self.clone();
        ui.on_history_requested(move |delta| c.load_history(delta));
        let c = self.clone();
        ui.on_history_selected(move |index| c.history_selected(index));
        let c = self.clone();
        ui.on_history_clear_requested(move || c.propose_history_clear());
        let c = self.clone();
        ui.on_search_requested(move |immediate| {
            if let Some(ui) = c.ui.upgrade() {
                c.request_search(&ui, immediate, true);
            }
        });
        let c = self.clone();
        ui.on_filter_requested(move || {
            if let Some(ui) = c.ui.upgrade() {
                c.request_navigation_search(&ui, true, true);
            }
        });
        let c = self.clone();
        ui.on_apply_filters(move || {
            if let Some(ui) = c.ui.upgrade() {
                if crate::tag_chips::commit_draft(&ui) {
                    c.request_navigation_search(&ui, true, true);
                } else {
                    let max = audiolib_core::MAX_TAG_TERMS;
                    ui.set_status(
                        if c.chinese.load(Ordering::Acquire) {
                            format!("最多只能有 {max} 个标签词，请先移除一些")
                        } else {
                            format!("At most {max} tag terms are allowed; remove some first")
                        }
                        .into(),
                    );
                }
            }
        });
        let c = self.clone();
        ui.on_tag_term_toggled(move |index| {
            if let Some(ui) = c.ui.upgrade() {
                crate::tag_chips::toggle(&ui, index);
                c.request_navigation_search(&ui, true, true);
            }
        });
        let c = self.clone();
        ui.on_tag_term_removed(move |index| {
            if let Some(ui) = c.ui.upgrade() {
                crate::tag_chips::remove(&ui, index);
                c.request_navigation_search(&ui, true, true);
            }
        });
        let c = self.clone();
        ui.on_tag_chips_relayout(move || {
            if let Some(ui) = c.ui.upgrade() {
                crate::tag_chips::relayout(&ui);
            }
        });
        let c = self.clone();
        ui.on_color_filter_edited(move || {
            if let Some(ui) = c.ui.upgrade() {
                crate::tag_chips::update_color_swatch(&ui);
            }
        });
        let c = self.clone();
        ui.on_reset_filters(move || {
            if let Some(ui) = c.ui.upgrade() {
                ui.set_visibility_index(0);
                ui.set_sample_filter(0);
                ui.set_bit_filter(0);
                ui.set_channel_filter(0);
                ui.set_favorites_only(false);
                ui.set_tagged_only(false);
                ui.set_tag_filter("".into());
                ui.set_tag_draft("".into());
                ui.set_color_filter("".into());
                ui.set_duration_min("".into());
                ui.set_duration_max("".into());
                c.request_navigation_search(&ui, true, true);
            }
        });
        let c = self.clone();
        ui.on_page_requested(move |delta| {
            if let Some(ui) = c.ui.upgrade() {
                let mut request = c.request.lock().unwrap();
                request.offset = if delta < 0 {
                    request.offset.saturating_sub(PAGE_SIZE)
                } else {
                    request.offset.saturating_add(PAGE_SIZE)
                };
                drop(request);
                c.request_navigation_search(&ui, true, false);
            }
        });
        let c = self.clone();
        ui.on_select_requested(move |index| c.select_row(index, false, false, false));
        let c = self.clone();
        ui.on_audition_requested(move |index| c.select(index, true));
        let c = self.clone();
        ui.on_selection_step_requested(move |delta| {
            if let Some(ui) = c.ui.upgrade() {
                let records = c.records.lock().unwrap();
                let count = records.len() as i32;
                if count > 0 {
                    let current = ui.get_selected_row();
                    let index = (current + delta).clamp(0, count - 1);
                    // Single-click playback covers the keyboard too: an arrow key that
                    // moves the selection auditions its new row exactly like a click.
                    // A clamped step (already at the first/last row) must not reload or
                    // restart the sound that is playing, and a missing record stays
                    // silent instead of opening a nonexistent file.
                    if index == current {
                        return;
                    }
                    let play =
                        ui.get_audition_play_on_single_click() && !records[index as usize].missing;
                    drop(records);
                    c.select_row(index, false, false, play);
                }
            }
        });
        let c = self.clone();
        ui.on_file_action_requested(move |id, action| {
            if let Ok(id) = id.parse::<i64>() {
                c.file_action(id, action);
            }
        });
        let c = self.clone();
        ui.on_drag_out_requested(move |id| {
            if let Ok(id) = id.parse::<i64>() {
                c.drag_out(id);
            }
        });
        let c = self.clone();
        ui.on_drag_cancel_requested(move || {
            c.drag_cancel.lock().unwrap().store(true, Ordering::Release);
        });
        let c = self.clone();
        ui.on_folder_action_requested(move |path, action| {
            // Server mode: removing a folder's records is a server call when this PC may
            // manage; everything else (reveal) stays local-only.
            if action == 1 && c.remote_read_only() {
                if c.refuse_server_removal() || !c.available() {
                    return;
                }
                let id = c
                    .tree
                    .lock()
                    .unwrap()
                    .known
                    .values()
                    .find(|folder| same_path(Path::new(&folder.path), Path::new(path.as_str())))
                    .map(|folder| folder.id);
                match id {
                    Some(id) => c.propose_operation(LibraryOperation::RemoveFolder(id)),
                    None => c.status(
                        if c.chinese.load(Ordering::Acquire) {
                            "找不到所选目录的记录，请刷新后再试。"
                        } else {
                            "The selected folder is unknown; reload the tree and try again."
                        }
                        .into(),
                    ),
                }
                return;
            }
            if c.refuse_remote_write() {
                return;
            }
            if action == 0 {
                c.open_explorer(path.to_string(), false);
            } else if action == 1 && c.available() {
                let id = c
                    .tree
                    .lock()
                    .unwrap()
                    .known
                    .values()
                    .find(|folder| same_path(Path::new(&folder.path), Path::new(path.as_str())))
                    .map(|folder| folder.id);
                if let Some(id) = id {
                    c.propose_operation(LibraryOperation::RemoveFolder(id));
                }
            }
        });
        let c = self.clone();
        ui.on_folder_requested(move |index| c.choose_folder(index));
        let c = self.clone();
        ui.on_folder_toggle_requested(move |index| {
            let mut tree = c.tree.lock().unwrap();
            let Some((folder, _, _)) = tree.visible.get(index as usize) else {
                return;
            };
            let id = folder.id;
            if !tree.expanded.remove(&id) {
                tree.expanded.insert(id);
            }
            drop(tree);
            c.reload_tree();
        });
        let c = self.clone();
        ui.on_all_library_requested(move || {
            c.tree.lock().unwrap().selected = None;
            c.request.lock().unwrap().folder_id = None;
            // Invalidate any pending publication carrying the previous folder.
            c.reload_tree();
            if let Some(ui) = c.ui.upgrade() {
                ui.set_selected_folder(-1);
                ui.set_selected_folder_path("".into());
                ui.set_crumbs(ModelRc::new(VecModel::<CrumbRow>::default()));
                c.request_navigation_search(&ui, true, true);
            }
        });
        let c = self.clone();
        ui.on_crumb_requested(move |index| {
            let id = c
                .tree
                .lock()
                .unwrap()
                .crumbs
                .get(index as usize)
                .map(|folder| folder.id);
            if let Some(id) = id {
                c.select_folder_id(id);
            }
        });
        let c = self.clone();
        ui.on_scan_requested(move |path| c.scan(PathBuf::from(path.to_string())));
        let c = self.clone();
        ui.on_scan_browse_requested(move || c.browse_scan_folder());
        let c = self.clone();
        ui.on_scan_cancel_requested(move || c.cancel_scan());
        let c = self.clone();
        ui.on_scan_pause_requested(move || c.toggle_scan_pause());
        let c = self.clone();
        ui.on_save_metadata_requested(move || {
            if let Some(ui) = c.ui.upgrade() {
                c.save_metadata(&ui);
            }
        });
        let c = self.clone();
        ui.on_metadata_edited(move || {
            if let Some(ui) = c.ui.upgrade() {
                c.capture_metadata(&ui);
            }
        });
        let c = self.clone();
        ui.on_restore_metadata(move || c.restore_metadata());
        let c = self.clone();
        ui.on_recover_metadata_and_close(move || {
            if let Some(ui) = c.ui.upgrade()
                && ui.get_close_recovery_available()
                && c.available()
            {
                c.request_close_mode(&ui, true);
            }
        });
        let c = self.clone();
        ui.on_reveal_requested(move || c.reveal());
        let c = self.clone();
        ui.on_play_requested(move || {
            if !c.available() {
                return;
            }
            if c.audio.status().generation == c.removed_audio_generation.load(Ordering::Acquire) {
                let selected = c.selected.lock().unwrap().as_ref().map(|file| file.id);
                if let Some(id) = selected {
                    c.select_id(id, true);
                }
                return;
            }
            let play = c.audio.status().state != PlaybackState::Playing;
            if play && let Some(id) = c.audio_reload_target() {
                c.select_id(id, true);
                return;
            }
            let command = if play {
                AudioCommand::Play
            } else {
                AudioCommand::Pause
            };
            c.command(command);
        });
        let c = self.clone();
        ui.on_stop_requested(move || {
            if let Err(error) = c.enqueue_stop() {
                c.status(error.to_string());
            }
        });
        let c = self.clone();
        ui.on_seek_requested(move |position| c.command(AudioCommand::Seek(position as f64)));
        let c = self.clone();
        ui.on_seek_play_requested(move |position| {
            if c.available() {
                c.command(AudioCommand::SeekAndPlay(position as f64));
            }
        });
        let c = self.clone();
        ui.on_volume_requested(move |volume| {
            c.command(AudioCommand::SetVolume(volume));
            c.playback.lock().unwrap().volume = volume;
            // The status poll skips volume publishing while the slider keeps focus,
            // so refresh the dB label here to follow the drag live.
            if let Some(ui) = c.ui.upgrade() {
                ui.set_volume_label(volume_label(volume));
            }
        });
        let c = self.clone();
        ui.on_start_requested(move || {
            if !c.available() {
                return;
            }
            match c.audio_reload_target() {
                Some(id) => c.select_id(id, true),
                None => c.command(AudioCommand::Play),
            }
        });
        let c = self.clone();
        ui.on_pause_requested(move || c.command(AudioCommand::Pause));
        let c = self.clone();
        ui.on_volume_db_requested(move |db| {
            // The audition slider reports dB through this callback (not
            // volume-requested). The status poll skips volume publishing while the
            // slider is being adjusted, so the label must follow the drag here.
            let volume = gain_from_db(db);
            c.command(AudioCommand::SetVolume(volume));
            c.playback.lock().unwrap().volume = volume;
            if let Some(ui) = c.ui.upgrade() {
                ui.set_volume_label(volume_label(volume));
            }
        });
        let c = self.clone();
        ui.on_transform_requested(move |pitch, rate, mono, repeat| {
            let settings = pitch
                .trim()
                .parse::<f32>()
                .ok()
                .zip(rate.trim().parse::<f32>().ok())
                .map(|(pitch_semitones, rate)| AuditionSettings {
                    pitch_semitones,
                    rate,
                    mono,
                    loop_enabled: repeat,
                });
            if let Some(settings) = settings.filter(|settings| settings.validate().is_ok()) {
                c.command(AudioCommand::SetAudition(settings));
            } else if let Some(ui) = c.ui.upgrade() {
                publish_audition(&ui, c.audio.status().audition);
                c.status(
                    if c.chinese.load(Ordering::Acquire) {
                        "Pitch 范围 -24 到 +24 半音；Rate 范围 0.25 到 4.0"
                    } else {
                        "Pitch: -24..+24 semitones; Rate: 0.25..4.0"
                    }
                    .into(),
                );
            }
        });
        let c = self.clone();
        ui.on_waveform_zoom_requested(move |factor| {
            let status = c.audio.status();
            let Some(file) = c.waveform_file(&status) else {
                return;
            };
            {
                let mut state = c.wave_view.lock().unwrap();
                if factor == 0.0 {
                    state.1.zoom = 1.0;
                    state.1.scroll = 0.0;
                } else {
                    state
                        .1
                        .zoom_at(f64::from(factor), status.position, file.duration);
                }
            }
            c.queue_waveform(&file);
        });
        let c = self.clone();
        ui.on_waveform_wheel_zoom_requested(move |factor, fraction| {
            let Some(ui) = c.ui.upgrade() else {
                return;
            };
            let status = c.audio.status();
            let Some(file) = c.waveform_file(&status) else {
                return;
            };
            zoom_waveform_at_pointer(
                &ui,
                &mut c.wave_view.lock().unwrap().1,
                factor,
                fraction,
                file.duration,
            );
            c.queue_waveform(&file);
        });
        let c = self.clone();
        ui.on_waveform_pan_requested(move |scroll| {
            c.wave_view.lock().unwrap().1.scroll = f64::from(scroll.clamp(0.0, 1.0));
            if let Some(file) = c.waveform_file(&c.audio.status()) {
                c.queue_waveform(&file);
            }
        });
        let c = self.clone();
        ui.on_peak_style_requested(move || {
            if let Some(ui) = c.ui.upgrade() {
                let preferences = SpectralPeakPreferences {
                    noise: ui.get_peak_noise(),
                    variance: ui.get_peak_variance(),
                }
                .normalized();
                let save_preferences;
                {
                    let mut state = c.wave_view.lock().unwrap();
                    let view = &mut state.1;
                    view.gain_db = ui.get_peak_gain();
                    view.opacity = ui.get_peak_opacity();
                    save_preferences =
                        view.noise != preferences.noise || view.variance != preferences.variance;
                    view.noise = preferences.noise;
                    view.variance = preferences.variance;
                    view.spectral = ui.get_peak_mode() == 1;
                }
                if save_preferences {
                    *c.single_click_play_pending.lock().unwrap() = true;
                    c.single_click_play_wake.notify_one();
                }
                if let Some(file) = c.waveform_file(&c.audio.status()) {
                    c.queue_waveform(&file);
                }
            }
        });
        let c = self.clone();
        ui.on_language_requested(move |chinese| {
            c.chinese.store(chinese, Ordering::Release);
            let saving = c.clone();
            std::thread::spawn(move || {
                let mut config = saving.config.lock().unwrap();
                config.language = if chinese { "cn" } else { "en" }.into();
                if let Err(error) = config.save() {
                    saving.status(error.to_string());
                }
                drop(config);
                saving.reload_tree();
            });
            if let Some(ui) = c.ui.upgrade() {
                c.publish_search_tasks(&ui);
                c.request_search(&ui, true, false);
                c.refresh_library_info();
                c.publish_data_source(&ui);
                c.refresh_generated_text(&ui);
                c.refresh_generated_source_text(&ui);
                if ui.get_library_visible() {
                    c.load_history(0);
                }
            }
        });
        let c = self.clone();
        ui.on_devices_requested(move |backend| c.refresh_devices(backend));
        let c = self.clone();
        ui.on_output_settings_requested(move || {
            if let Some(ui) = c.ui.upgrade() {
                c.restore_output_form(&ui);
                c.refresh_devices(ui.get_backend_index());
            }
        });
        let c = self.clone();
        ui.on_device_selected(move |index| c.describe_device(index));
        let c = self.clone();
        ui.on_output_apply_requested(move || {
            if let Some(ui) = c.ui.upgrade() {
                c.apply_output(&ui, false);
            }
        });
        let c = self.clone();
        ui.on_control_panel_requested(move || {
            if let Some(ui) = c.ui.upgrade() {
                c.apply_output(&ui, true);
            }
        });
        let c = self.clone();
        ui.on_waveform_resize_requested(move |width, height| {
            let size = (width.clamp(32, 4096) as u32, height.clamp(16, 2048) as u32);
            let old = *c.waveform_size.lock().unwrap();
            *c.waveform_size.lock().unwrap() = size;
            if old != size
                && let Some(file) = c.waveform_file(&c.audio.status())
            {
                c.queue_waveform(&file);
            }
        });
    }

    fn publish_checked(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let selected = self.multi_selection.lock().unwrap();
        let rows = self
            .records
            .lock()
            .unwrap()
            .iter()
            .map(|file| {
                let mut row = audio_row(file);
                row.checked = selected.contains(&file.id);
                row
            })
            .collect::<Vec<_>>();
        let rows = self.decorate_list_waveforms(rows);
        ui.set_checked_count(selected.len() as i32);
        publish_audio_rows(&ui, rows);
    }

    fn select_row(self: &Arc<Self>, index: i32, toggle: bool, range: bool, play: bool) {
        if !self.available() {
            return;
        }
        let ids: Vec<_> = self
            .records
            .lock()
            .unwrap()
            .iter()
            .map(|file| file.id)
            .collect();
        let id = update_row_selection(
            &ids,
            &mut self.multi_selection.lock().unwrap(),
            &mut self.selection_anchor.lock().unwrap(),
            index as usize,
            toggle,
            range,
        );
        if let Some(id) = id {
            if let Some(ui) = self.ui.upgrade() {
                ui.set_selected_row(index);
            }
            self.publish_checked();
            self.select_id(id, play && !toggle && !range);
        }
    }

    fn apply_batch(self: &Arc<Self>) {
        // Server mode sends the batch to the server (one transaction there).
        if !self.available() {
            return;
        }
        let Some(ui) = self.ui.upgrade() else { return };
        let mut ids = self
            .multi_selection
            .lock()
            .unwrap()
            .iter()
            .copied()
            .collect::<Vec<_>>();
        ids.sort_unstable();
        let patch = MetadataPatch {
            tags: ui
                .get_batch_tags_enabled()
                .then(|| ui.get_batch_tags().to_string()),
            notes: ui
                .get_batch_notes_enabled()
                .then(|| ui.get_batch_notes().to_string()),
            color: ui
                .get_batch_color_enabled()
                .then(|| ui.get_batch_color().trim().to_owned()),
            favorite: match ui.get_batch_favorite() {
                1 => Some(true),
                2 => Some(false),
                _ => None,
            },
        };
        if patch.tags.is_none()
            && patch.notes.is_none()
            && patch.color.is_none()
            && patch.favorite.is_none()
        {
            ui.set_status(
                if self.chinese.load(Ordering::Acquire) {
                    "请选择至少一个批量字段"
                } else {
                    "Enable at least one batch field"
                }
                .into(),
            );
            return;
        }
        let request = BatchMetadataRequest {
            ids,
            patch,
            tag_mode: match ui.get_batch_tag_mode() {
                1 => TagEditMode::Append,
                2 => TagEditMode::Remove,
                _ => TagEditMode::Replace,
            },
        };
        if let Err(error) = request.validate() {
            ui.set_status(error.to_string().into());
            return;
        }
        ui.set_batch_visible(false);
        self.run_library_operation(LibraryOperation::Batch(request));
    }

    fn library_action(self: &Arc<Self>, action: i32) {
        if !self.available() {
            return;
        }
        if action == 0 {
            self.refresh_library_info();
            self.load_history(0);
            return;
        }
        // Server mode: removing the checked records is a server call when this PC may
        // manage (the same confirmation and call as the 服务器音效库 page).
        if action == 11 && self.remote_read_only() {
            if self.refuse_server_removal() {
                return;
            }
            let mut ids = self
                .multi_selection
                .lock()
                .unwrap()
                .iter()
                .copied()
                .collect::<Vec<_>>();
            ids.sort_unstable();
            if ids.is_empty() {
                self.status(
                    if self.chinese.load(Ordering::Acquire) {
                        "请先在主列表勾选要移除的记录。"
                    } else {
                        "Check the records to remove in the main list first."
                    }
                    .into(),
                );
                return;
            }
            self.propose_operation(LibraryOperation::RemoveAudio(ids));
            return;
        }
        // Reads (backup copy, integrity check, export) stay available on a server snapshot.
        if !matches!(action, 1 | 3 | 8 | 9) && self.refuse_remote_write() {
            return;
        }
        let operation = match action {
            3 => Some(LibraryOperation::Check),
            4 => Some(LibraryOperation::Maintain(MaintenanceAction::Optimize)),
            5 => Some(LibraryOperation::Maintain(MaintenanceAction::RebuildSearch)),
            11 => {
                let mut ids = self
                    .multi_selection
                    .lock()
                    .unwrap()
                    .iter()
                    .copied()
                    .collect::<Vec<_>>();
                ids.sort_unstable();
                (!ids.is_empty()).then_some(LibraryOperation::RemoveAudio(ids))
            }
            _ => None,
        };
        if let Some(operation) = operation {
            self.propose_operation(operation);
            return;
        }
        use crate::dialogs::FileKind;
        let (save, kind) = match action {
            1 => (true, FileKind::Database),
            6 => (true, FileKind::NewLibrary),
            2 | 7 => (false, FileKind::Database),
            8 => (true, FileKind::JsonLines),
            9 => (true, FileKind::Csv),
            10 => (false, FileKind::Annotations),
            _ => return,
        };
        self.choose_file(save, kind, move |path| match action {
            1 => LibraryOperation::Backup(path),
            2 => LibraryOperation::Restore(path),
            6 | 7 => LibraryOperation::Switch {
                path,
                create: action == 6,
            },
            8 => LibraryOperation::Export(path, LibraryExportFormat::JsonLines),
            9 => LibraryOperation::Export(path, LibraryExportFormat::Csv),
            _ => LibraryOperation::Import(path),
        });
    }

    fn choose_file(
        self: &Arc<Self>,
        save: bool,
        kind: crate::dialogs::FileKind,
        operation: impl FnOnce(PathBuf) -> LibraryOperation + Send + 'static,
    ) {
        if !self.available() || self.library_busy.swap(true, Ordering::AcqRel) {
            return;
        }
        let scan_was_running = self.scan_cancel.lock().unwrap().is_some()
            || self.ui.upgrade().is_some_and(|ui| ui.get_scanning());
        let owner = self
            .ui
            .upgrade()
            .map_or(0, |ui| crate::dialogs::owner_handle(ui.window()));
        if let Some(ui) = self.ui.upgrade() {
            ui.set_library_busy(true);
            ui.set_file_choice_busy(true);
        }
        let c = self.clone();
        std::thread::spawn(move || {
            let result =
                crate::dialogs::choose(save, kind, c.chinese.load(Ordering::Acquire), owner);
            let c2 = c.clone();
            post(c.ui.clone(), move |ui| {
                c2.complete_file_choice(&ui, result, scan_was_running, operation);
            });
        });
    }

    /// A cancelled native dialog must still reveal scan results completed while busy.
    fn complete_file_choice(
        self: &Arc<Self>,
        ui: &AppWindow,
        result: Result<Option<PathBuf>, String>,
        scan_was_running: bool,
        operation: impl FnOnce(PathBuf) -> LibraryOperation,
    ) {
        self.library_busy.store(false, Ordering::Release);
        ui.set_library_busy(false);
        ui.set_file_choice_busy(false);
        let scanning = self.scan_cancel.lock().unwrap().is_some();
        ui.set_scanning(scanning);
        ui.set_scan_validating(scanning && self.scan_validating.load(Ordering::Acquire));
        if self.close_after_operation.swap(false, Ordering::AcqRel) {
            self.request_close(ui);
            return;
        }
        match result {
            Ok(Some(path)) => self.propose_operation(operation(path)),
            outcome => {
                if self.available() && scan_was_running && !scanning {
                    self.request_search(ui, true, true);
                    self.reload_tree();
                    self.refresh_library_info();
                    if ui.get_library_visible() {
                        self.load_history(0);
                    }
                }
                if let Err(error) = outcome {
                    ui.set_status(error.clone().into());
                    ui.set_management_report(error.clone().into());
                    ui.set_library_feedback(error.into());
                }
            }
        }
    }

    /// Keep folder selection off the event loop and retain the draft path on cancel.
    fn browse_scan_folder(self: &Arc<Self>) {
        if !self.available()
            || self.scan_cancel.lock().unwrap().is_some()
            || self.refuse_remote_write()
        {
            return;
        }
        if self.library_busy.swap(true, Ordering::AcqRel) {
            return;
        }
        let generation = self.library_generation.load(Ordering::Acquire);
        let owner = self
            .ui
            .upgrade()
            .map_or(0, |ui| crate::dialogs::owner_handle(ui.window()));
        if let Some(ui) = self.ui.upgrade() {
            ui.set_library_busy(true);
            ui.set_scan_browse_busy(true);
            ui.set_scan_error("".into());
        }
        let c = self.clone();
        std::thread::spawn(move || {
            let result = crate::dialogs::choose_folder(c.chinese.load(Ordering::Acquire), owner);
            let c2 = c.clone();
            post(c.ui.clone(), move |ui| {
                c2.library_busy.store(false, Ordering::Release);
                ui.set_library_busy(false);
                ui.set_scan_browse_busy(false);
                if c2.close_after_operation.swap(false, Ordering::AcqRel) {
                    c2.request_close(&ui);
                    return;
                }
                if !c2.available() || c2.library_generation.load(Ordering::Acquire) != generation {
                    return;
                }
                match result {
                    Ok(Some(path)) => ui.set_scan_path(path.display().to_string().into()),
                    Ok(None) => {}
                    Err(error) => {
                        ui.set_scan_error(error.clone().into());
                        ui.set_status(error.into());
                    }
                }
            });
        });
    }

    fn propose_operation(self: &Arc<Self>, operation: LibraryOperation) {
        self.pending_history_clear.lock().unwrap().take();
        if !self.available() {
            return;
        }
        if operation.needs_confirmation() {
            if let Some(ui) = self.ui.upgrade() {
                // Server mode: the 服务器音效库 page's wording for record removals.
                let mut text = match &operation {
                    LibraryOperation::RemoveAudio(ids) if self.remote_read_only() => {
                        self.server_removal_confirmation(Some(ids.len()), 0)
                    }
                    LibraryOperation::RemoveFolder(id) if self.remote_read_only() => {
                        self.server_removal_confirmation(None, *id)
                    }
                    _ => operation.confirmation(self.chinese.load(Ordering::Acquire)),
                };
                if let LibraryOperation::RemoveAudio(ids) = &operation {
                    text.push_str(&removal_selection_preview(
                        ids,
                        &self.records.lock().unwrap(),
                        self.chinese.load(Ordering::Acquire),
                    ));
                }
                ui.set_confirmation_text(text.into());
                *self.pending_operation.lock().unwrap() = Some(operation);
                ui.set_confirmation_visible(true);
            }
        } else {
            self.run_library_operation(operation);
        }
    }

    fn invalidate_library_reads(&self) {
        self.library_info_generation.fetch_add(1, Ordering::AcqRel);
        self.search_cancel
            .lock()
            .unwrap()
            .store(true, Ordering::Release);
        self.search_generation.fetch_add(1, Ordering::AcqRel);
        self.tree_generation.fetch_add(1, Ordering::AcqRel);
        self.selection_generation.fetch_add(1, Ordering::AcqRel);
        self.history_generation.fetch_add(1, Ordering::AcqRel);
        self.invalidate_waveform();
        self.list_waveforms.invalidate();
        *self.search_job.lock().unwrap() = None;
        *self.facet_job.lock().unwrap() = None;
        *self.search_publication.lock().unwrap() = None;
    }

    /// One library operation owns the interval after the Metadata FIFO has drained.
    /// UI edits and scanning are frozen until completion; failure preserves drafts/library.
    fn run_library_operation(self: &Arc<Self>, operation: LibraryOperation) {
        let source_handoff = self.source_changing.load(Ordering::Acquire)
            && matches!(operation, LibraryOperation::DataSource(_))
            && !self.library_busy.load(Ordering::Acquire)
            && !self.closing.load(Ordering::Acquire)
            && !self.shutdown.load(Ordering::Acquire);
        if !self.available() && !source_handoff {
            return;
        }
        let Some(ui) = self.ui.upgrade() else { return };
        let retain_search = matches!(&operation, LibraryOperation::DataSource(target)
            if self.is_same_library_snapshot_refresh(target));
        let retain_context = operation.retains_library_identity();
        let read_only = operation.is_read_only();
        self.persist_search_task(&ui);
        let switches_workspace = matches!(
            operation,
            LibraryOperation::Switch { .. }
                | LibraryOperation::Restore(_)
                | LibraryOperation::DataSource(_)
        );
        let selected_id = ui.get_selected_id().parse::<i64>().ok();
        self.capture_metadata(&ui);
        let recovering_handoff = matches!(operation, LibraryOperation::DataSource(_));
        let mut metadata_warning = None;
        let enqueue_result = if recovering_handoff {
            self.enqueue_transition_metadata()
        } else {
            self.metadata.lock().unwrap().enqueue_all()
        };
        let barrier = match enqueue_result {
            Ok(sequence) => sequence,
            Err(error) if read_only => {
                metadata_warning = Some(error);
                self.metadata.lock().unwrap().sequence
            }
            Err(error) => {
                ui.set_status(error.clone().into());
                ui.set_management_report(error.into());
                return;
            }
        };
        let already_busy = {
            let _plays = self.remote_plays.lock().unwrap();
            self.library_busy.swap(true, Ordering::AcqRel)
        };
        if already_busy {
            return;
        }
        if switches_workspace {
            self.freeze_search_workspace();
        }
        ui.set_library_busy(true);
        let opening_cache = matches!(
            operation,
            LibraryOperation::DataSource(remote_mode::DataSourceSwitch::Server { .. })
        );
        if opening_cache {
            // The provider open is the final cache stage. Keep its progress in the
            // manager while the write gate drains drafts/scans and installs it.
            ui.set_ds_syncing(true);
            ui.set_ds_progress(-1.0);
            ui.set_ds_decompression_info("".into());
            ui.set_ds_decompression_progress(-1.0);
            ui.set_ds_sync_info(
                if self.chinese.load(Ordering::Acquire) {
                    "正在打开缓存音效库…"
                } else {
                    "Opening the cached library…"
                }
                .into(),
            );
        }
        self.invalidate_library_reads();
        self.metadata_wake.notify_all();
        if let Some(cancel) = self.scan_cancel.lock().unwrap().as_ref() {
            cancel.cancel();
        }
        let provider = self.provider();
        // Server mode: record removals go to the server API through `RemoteProvider`
        // (database records only; the snapshot follows by a re-sync). The removed records
        // are hidden at once; the snapshot file itself is never modified.
        let server_removal = self.remote_read_only()
            && matches!(
                operation,
                LibraryOperation::RemoveAudio(_) | LibraryOperation::RemoveFolder(_)
            );
        let c = self.clone();
        std::thread::spawn(move || {
            // Ids of removed server records, when known (a folder removal hides its subtree).
            let mut removed_ids: Option<Vec<i64>> = None;
            let mut refreshed_selection = None;
            let mut removed_folders = HashSet::new();
            let mut retained_checked = None;
            let mut removed_audio_generation = None;
            let result = (|| -> Result<String, String> {
                c.wait_workspace_writes()?;
                // Keep the old snapshot and its drafts when a save fails. A refresh
                // must never discard an annotation merely because a newer snapshot exists.
                if recovering_handoff {
                    c.wait_transition_metadata(barrier)?;
                } else {
                    c.wait_metadata(barrier, !read_only)?;
                    if read_only {
                        metadata_warning = metadata_warning.take().or_else(|| {
                            c.metadata
                                .lock()
                                .unwrap()
                                .failure()
                                .map(|(id, error)| c.metadata_failure_message(id, &error))
                        });
                    }
                }
                c.wait_remote_plays()?;
                // The busy gate keeps list Waveform work from starting; cancel a running
                // decode and let its (lease-protected) write finish first.
                c.list_waveforms.suspend();
                c.list_waveforms.wait_idle(Duration::from_secs(5));
                c.list_waveforms.resume();
                while c.scan_cancel.lock().unwrap().is_some() {
                    if c.shutdown.load(Ordering::Acquire) {
                        return Err("Desktop is shutting down".into());
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                if let LibraryOperation::RemoveFolder(id) = &operation {
                    removed_folders = required_folder_subtree(provider.as_ref(), *id)
                        .map_err(|error| error.to_string())?;
                }
                let playing = c.audio.status();
                let playing_file = playing.file_path.as_deref().and_then(|path| {
                    if c.remote_read_only() {
                        c.remote_wave_file(path)
                    } else {
                        provider.find_audio_by_path(path).ok().flatten()
                    }
                });
                if operation.affects_playback(playing_file.as_ref(), &removed_folders) {
                    c.audio
                        .send_and_wait(AudioCommand::Stop, Duration::from_secs(4))
                        .map_err(|error| error.to_string())?;
                    if matches!(
                        operation,
                        LibraryOperation::RemoveAudio(_) | LibraryOperation::RemoveFolder(_)
                    ) {
                        removed_audio_generation = Some(playing.generation);
                    }
                }
                let json = |value: serde_json::Value| {
                    serde_json::to_string_pretty(&value).unwrap_or_default()
                };
                let report = match &operation {
                    LibraryOperation::Backup(path) => json(serde_json::json!(
                        provider.backup_library(path).map_err(|e| e.to_string())?
                    )),
                    LibraryOperation::Restore(path) => json(serde_json::json!(
                        provider.restore_library(path).map_err(|e| e.to_string())?
                    )),
                    LibraryOperation::Check => json(serde_json::json!(
                        provider.check_library().map_err(|e| e.to_string())?
                    )),
                    LibraryOperation::Maintain(action) => json(serde_json::json!(
                        provider
                            .maintain_library(*action)
                            .map_err(|e| e.to_string())?
                    )),
                    LibraryOperation::Export(path, format) => json(serde_json::json!(
                        provider
                            .export_library(path, *format)
                            .map_err(|e| e.to_string())?
                    )),
                    LibraryOperation::Import(path) => {
                        let rollback = annotation_rollback_path(provider.database_path());
                        provider
                            .backup_library(&rollback)
                            .map_err(|e| e.to_string())?;
                        let imported = provider
                            .import_annotations(path)
                            .map_err(|e| e.to_string())?;
                        json(serde_json::json!({"import": imported,"rollback_backup":rollback}))
                    }
                    LibraryOperation::RemoveAudio(ids) => {
                        let removed = c
                            .metadata_provider()?
                            .remove_audio(ids)
                            .map_err(|e| e.to_string())?;
                        if server_removal {
                            c.hide_removed(ids, &HashSet::new(), removed);
                            removed_ids = Some(ids.clone());
                        }
                        removal_summary(removed, server_removal, c.chinese.load(Ordering::Acquire))
                    }
                    LibraryOperation::RemoveFolder(id) => {
                        // Read the subtree first: the snapshot still lists it afterwards,
                        // but the server's folder no longer exists.
                        let subtree = if server_removal {
                            removed_folders.clone()
                        } else {
                            HashSet::new()
                        };
                        let removed = c
                            .metadata_provider()?
                            .remove_folder(*id)
                            .map_err(|e| e.to_string())?;
                        if server_removal {
                            c.hide_removed(&[], &subtree, removed);
                        }
                        removal_summary(removed, server_removal, c.chinese.load(Ordering::Acquire))
                    }
                    LibraryOperation::Relink(id, path) => {
                        let allowed_roots = c.config.lock().unwrap().allowed_scan_roots.clone();
                        let path = audiolib_core::validate_wav_path(path, &allowed_roots)
                            .map_err(|error| error.to_string())?;
                        if provider
                            .find_audio_by_path(&path)
                            .map_err(|e| e.to_string())?
                            .is_some_and(|file| file.id != *id)
                        {
                            return Err(if c.chinese.load(Ordering::Acquire) { "新路径已在库中，请先移除新路径记录（仅移除库记录），再重新关联旧记录。" } else { "The new path is already indexed. Remove the new-path library record first, then relink the old record." }.into());
                        }
                        let linked = provider
                            .relink_audio(*id, &path)
                            .map_err(|e| e.to_string())?;
                        if c.chinese.load(Ordering::Acquire) {
                            format!(
                                "已修复 {} 的文件引用，标签、备注与收藏已保留。\n{}",
                                linked.filename, linked.file_path
                            )
                        } else {
                            format!(
                                "Repaired the file reference for {}. Tags, notes and favorites were retained.\n{}",
                                linked.filename, linked.file_path
                            )
                        }
                    }
                    LibraryOperation::Batch(request) => json(serde_json::json!(
                        c.metadata_provider()?
                            .update_metadata_batch(request)
                            .map_err(|e| e.to_string())?
                    )),
                    LibraryOperation::DataSource(target) => c.switch_data_source(target)?,
                    LibraryOperation::ServerRoot(root) => c.execute_server_root(root)?,
                    LibraryOperation::Switch { path, create } => {
                        if *create && path.exists() {
                            return Err(c.localized("新建音效库的文件已存在，请选择新的文件名", "New database destination already exists; choose a new filename").to_string());
                        }
                        let selected_path = if *create {
                            path.parent()
                                .unwrap_or(Path::new("."))
                                .canonicalize()
                                .map_err(|error| error.to_string())?
                                .join(path.file_name().ok_or_else(|| {
                                    c.localized(
                                        "请输入数据库文件名",
                                        "Database filename is required",
                                    )
                                    .to_string()
                                })?)
                        } else {
                            path.canonicalize().map_err(|error| error.to_string())?
                        };
                        let history_config = c.config.lock().unwrap().clone();
                        let next = Arc::new(
                            if *create {
                                SqliteProvider::open_local_with_progress_cancellable(
                                    &selected_path,
                                    &history_config.data_root(),
                                    history_config.play_history_limit,
                                    &AtomicBool::new(false),
                                    |_| {},
                                )
                            } else {
                                SqliteProvider::open_existing_local_with_progress_cancellable(
                                    &selected_path,
                                    &history_config.data_root(),
                                    history_config.play_history_limit,
                                    &AtomicBool::new(false),
                                    |_| {},
                                )
                            }
                            .map_err(|e| e.local_display_text(c.chinese.load(Ordering::Acquire)))?,
                        );
                        let integrity = next.check_library().map_err(|error| error.to_string())?;
                        if !integrity.is_ok() {
                            return Err(format!(
                                "{}: {}",
                                c.localized(
                                    "所选音效库未通过校验",
                                    "Selected library failed validation"
                                ),
                                serde_json::to_string(&integrity).unwrap_or_default()
                            ));
                        }
                        crate::startup::prepare_local_search(&next);
                        let mut config = c.config.lock().unwrap();
                        let mut preferences = config.clone();
                        preferences
                            .select_database(&selected_path)
                            .map_err(|e| e.to_string())?;
                        preferences.save().map_err(|e| e.to_string())?;
                        *config = preferences;
                        *c.sqlite.write().unwrap() = next;
                        format!(
                            "{}: {}",
                            if c.chinese.load(Ordering::Acquire) {
                                "当前数据库"
                            } else {
                                "Active database"
                            },
                            path.display()
                        )
                    }
                };
                if let Some(generation) = removed_audio_generation {
                    c.removed_audio_generation
                        .store(generation, Ordering::Release);
                }
                if operation.changes_records() {
                    c.library_generation.fetch_add(1, Ordering::AcqRel);
                }
                if retain_context {
                    refreshed_selection = selected_id.and_then(|id| provider.get_audio(id).ok());
                    if let Some(file) = &refreshed_selection
                        && (removed_ids
                            .as_ref()
                            .is_some_and(|ids| ids.contains(&file.id))
                            || file
                                .folder_id
                                .is_some_and(|id| removed_folders.contains(&id)))
                    {
                        refreshed_selection = None;
                    }
                    if matches!(
                        operation,
                        LibraryOperation::RemoveAudio(_) | LibraryOperation::RemoveFolder(_)
                    ) {
                        let checked = c.multi_selection.lock().unwrap().clone();
                        retained_checked = Some(
                            checked
                                .into_iter()
                                .filter(|id| {
                                    if removed_ids.as_ref().is_some_and(|ids| ids.contains(id)) {
                                        return false;
                                    }
                                    provider.get_audio(*id).is_ok_and(|file| {
                                        !file
                                            .folder_id
                                            .is_some_and(|id| removed_folders.contains(&id))
                                    })
                                })
                                .collect(),
                        );
                    }
                }
                Ok(report)
            })();
            let c2 = c.clone();
            post(c.ui.clone(), move |ui| {
                c2.library_busy.store(false, Ordering::Release);
                ui.set_library_busy(false);
                if opening_cache {
                    ui.set_ds_syncing(false);
                    ui.set_ds_progress(0.0);
                }
                ui.set_scanning(false);
                ui.set_scan_validating(false);
                ui.set_searching(false);
                if removed_audio_generation.is_some() && result.is_ok() {
                    ui.set_audio_ready(false);
                }
                let succeeded = result.is_ok();
                if switches_workspace && !succeeded {
                    c2.thaw_search_workspace();
                }
                if matches!(operation, LibraryOperation::DataSource(_))
                    && c2.source_changing.load(Ordering::Acquire)
                {
                    c2.finish_source_change(&ui, succeeded);
                }
                if server_removal {
                    c2.server_removal_finished(&ui, &result, removed_ids.as_deref());
                }
                // A snapshot swap is the resync arriving: its snapshot wins, so the overlay
                // hiding removed records goes once the snapshot is past the removals.
                if matches!(operation, LibraryOperation::DataSource(_)) {
                    c2.settle_hidden();
                }
                match result {
                    Ok(report) => {
                        if operation.changes_records() {
                            if retain_context {
                                if let Some(checked) = retained_checked {
                                    *c2.multi_selection.lock().unwrap() = checked;
                                }
                                c2.refresh_same_library_view(
                                    &ui,
                                    refreshed_selection,
                                    &removed_folders,
                                );
                            } else {
                                c2.reset_library_view(
                                    &ui,
                                    matches!(
                                        operation,
                                        LibraryOperation::Switch { .. }
                                            | LibraryOperation::Restore(_)
                                            | LibraryOperation::DataSource(_)
                                    ) && !retain_search,
                                    retain_search,
                                );
                            }
                            crate::diagnostics::record(if retain_search {
                                crate::diagnostics::Event::SnapshotRefreshed
                            } else {
                                crate::diagnostics::Event::LibraryViewReset
                            });
                        }
                        ui.set_management_report(report.clone().into());
                        if matches!(
                            operation,
                            LibraryOperation::Relink(..)
                                | LibraryOperation::RemoveAudio(_)
                                | LibraryOperation::RemoveFolder(_)
                                | LibraryOperation::DataSource(_)
                        ) {
                            ui.set_library_feedback(report.clone().into());
                        }
                        ui.set_status(
                            if matches!(
                                operation,
                                LibraryOperation::Relink(..)
                                    | LibraryOperation::RemoveAudio(_)
                                    | LibraryOperation::RemoveFolder(_)
                                    | LibraryOperation::DataSource(_)
                            ) {
                                report.into()
                            } else {
                                if c2.chinese.load(Ordering::Acquire) {
                                    "音效库操作完成"
                                } else {
                                    "Sound library operation complete"
                                }
                                .into()
                            },
                        );
                    }
                    Err(error) => {
                        if matches!(operation, LibraryOperation::DataSource(_)) {
                            ui.set_ds_message(error.clone().into());
                        }
                        ui.set_library_feedback(error.clone().into());
                        ui.set_status(error.clone().into());
                        ui.set_management_report(error.into());
                    }
                }
                if matches!(operation, LibraryOperation::DataSource(_)) {
                    c2.publish_data_source(&ui);
                    if succeeded {
                        c2.refresh_source_manager_after_swap(&ui);
                        c2.reconcile_source_manager_after_swap(&ui);
                    }
                    c2.publish_metadata_recovery_notice(&ui);
                }
                if switches_workspace && succeeded {
                    c2.init_search_workspace();
                }
                if !operation.changes_records() || !succeeded {
                    c2.sync_editor(&ui);
                }
                c2.refresh_library_info();
                c2.reload_tree();
                if let Some(error) = metadata_warning.filter(|_| succeeded) {
                    let notice = if c2.chinese.load(Ordering::Acquire) {
                        format!("操作已完成，但部分标注尚未保存（未包含在结果中）：{error}")
                    } else {
                        format!("Operation completed with unsaved annotations excluded: {error}")
                    };
                    ui.set_library_feedback(notice.clone().into());
                    c2.resume_search(&ui, Some(notice));
                } else {
                    c2.request_search(&ui, true, false);
                }
                if ui.get_library_visible() {
                    c2.load_history(0);
                }
                if let LibraryOperation::ServerRoot(root) = &operation {
                    c2.server_root_finished(&ui, root, succeeded);
                }
                if c2.close_after_operation.swap(false, Ordering::AcqRel) && succeeded {
                    c2.request_close(&ui);
                }
            });
        });
    }

    fn sync_editor(&self, ui: &AppWindow) {
        let selected = self.selected.lock().unwrap().clone();
        if let Some(file) = selected
            && ui.get_selected_id().as_str() == file.id.to_string()
        {
            let (displayed, dirty) = self.metadata.lock().unwrap().for_display(&file, 0);
            *self.selected.lock().unwrap() = Some(displayed.clone());
            present_file(ui, &displayed);
            ui.set_metadata_dirty(dirty);
        }
    }

    /// Same-library mutations retain navigation and checked IDs; refresh the inspector
    /// from the worker's post-operation read without loading/stopping unrelated audio.
    fn refresh_same_library_view(
        self: &Arc<Self>,
        ui: &AppWindow,
        selected: Option<AudioFile>,
        removed_folders: &HashSet<i64>,
    ) {
        *self.latest_selection.lock().unwrap() = None;
        {
            let mut metadata = self.metadata.lock().unwrap();
            metadata.saved.clear();
            metadata.saved_order.clear();
        }
        {
            let mut tree = self.tree.lock().unwrap();
            tree.stats = None;
            tree.expanded.retain(|id| !removed_folders.contains(id));
            Arc::make_mut(&mut tree.known).retain(|id, _| !removed_folders.contains(id));
            Arc::make_mut(&mut tree.matches).retain(|folder| !removed_folders.contains(&folder.id));
            if tree
                .selected
                .is_some_and(|id| removed_folders.contains(&id))
            {
                tree.selected = None;
                let mut request = self.request.lock().unwrap();
                request.folder_id = None;
                request.offset = 0;
                ui.set_selected_folder(-1);
                ui.set_selected_folder_path("".into());
            }
        }
        ui.set_checked_count(self.multi_selection.lock().unwrap().len() as i32);
        ui.set_metadata_dirty(false);
        if let Some(file) = selected {
            let changed_path =
                self.selected.lock().unwrap().as_ref().is_none_or(|old| {
                    old.missing != file.missing || old.file_path != file.file_path
                });
            *self.selected.lock().unwrap() = Some(file.clone());
            present_file(ui, &file);
            if changed_path {
                self.mark_audio_reload(file.id);
            }
            if !file.missing {
                self.queue_waveform(&file);
            }
        } else {
            *self.selected.lock().unwrap() = None;
            ui.set_selected_id("".into());
            ui.set_selected_row(-1);
            ui.set_has_selection(false);
            ui.set_selected_missing(false);
            ui.set_selected_name("".into());
            ui.set_selected_path("".into());
            ui.set_selected_format("".into());
            ui.set_selected_attributes("".into());
            ui.set_selected_description("".into());
            ui.set_selected_source_tags("".into());
            ui.set_edit_tags("".into());
            ui.set_edit_notes("".into());
            ui.set_edit_favorite(false);
            ui.set_waveform_ready(false);
            ui.set_waveform_image(Image::default());
            ui.set_waveform_info("".into());
        }
    }

    fn reset_library_view(&self, ui: &AppWindow, reset_request: bool, retain_navigation: bool) {
        *self.latest_selection.lock().unwrap() = None;
        self.metadata.lock().unwrap().reset_library();
        *self.selected.lock().unwrap() = None;
        self.records.lock().unwrap().clear();
        self.multi_selection.lock().unwrap().clear();
        *self.selection_anchor.lock().unwrap() = None;
        if retain_navigation {
            // IDs remain scoped to this server library. Discard derived rows/counts,
            // retaining the folder, expansion and page the user was browsing.
            let mut tree = self.tree.lock().unwrap();
            tree.matches = Arc::default();
            tree.visible = Arc::default();
            tree.stats = None;
        } else {
            *self.tree.lock().unwrap() = TreeState::default();
            self.request.lock().unwrap().folder_id = None;
            self.request.lock().unwrap().offset = 0;
        }
        self.plays.lock().unwrap().clear();
        self.scans.lock().unwrap().clear();
        *self.scan_presentation.lock().unwrap() = None;
        if self.pending_history_clear.lock().unwrap().take().is_some() {
            ui.set_confirmation_visible(false);
        }
        ui.set_selected_row(-1);
        ui.set_selected_id("".into());
        ui.set_selected_folder(-1);
        ui.set_selected_folder_path("".into());
        ui.set_has_selection(false);
        ui.set_selected_missing(false);
        ui.set_metadata_dirty(false);
        ui.set_selected_name("".into());
        ui.set_selected_path("".into());
        ui.set_selected_format("".into());
        ui.set_checked_count(0);
        ui.set_audio_ready(false);
        ui.set_waveform_ready(false);
        ui.set_waveform_image(Image::default());
        ui.set_waveform_info("".into());
        if ui.get_menu_has_page_rows() {
            ui.set_menu_has_page_rows(false);
        }
        ui.set_rows(ModelRc::new(VecModel::<AudioRow>::default()));
        ui.set_folders(ModelRc::new(VecModel::<FolderRow>::default()));
        ui.set_crumbs(ModelRc::new(VecModel::<CrumbRow>::default()));
        ui.set_history_rows(ModelRc::new(VecModel::<slint::SharedString>::default()));
        ui.set_history_missing(ModelRc::new(VecModel::<bool>::default()));
        ui.set_history_detail("".into());
        ui.set_scan_summary("".into());
        ui.set_scan_error("".into());
        ui.set_library_health("".into());
        ui.set_library_feedback("".into());
        ui.set_selected_attributes("".into());
        ui.set_selected_description("".into());
        ui.set_selected_source_tags("".into());
        ui.set_edit_tags("".into());
        ui.set_edit_notes("".into());
        ui.set_edit_favorite(false);
        if reset_request {
            *self.request.lock().unwrap() = SearchRequest::default();
            ui.set_query("".into());
            ui.set_favorites_only(false);
            ui.set_tagged_only(false);
            ui.set_sample_filter(0);
            ui.set_bit_filter(0);
            ui.set_channel_filter(0);
            ui.set_tag_filter("".into());
            ui.set_tag_draft("".into());
            ui.set_color_filter("".into());
            ui.set_duration_min("".into());
            ui.set_duration_max("".into());
            ui.set_visibility_index(0);
        }
    }

    fn refresh_library_info(self: &Arc<Self>) {
        let provider = self.provider();
        let generation = self.library_generation.load(Ordering::Acquire);
        let info_generation = self.library_info_generation.fetch_add(1, Ordering::AcqRel) + 1;
        let c = self.clone();
        std::thread::spawn(move || {
            let result = provider.library_info();
            let c2 = c.clone();
            post(c.ui.clone(), move |ui| {
                c2.publish_library_info(&ui, generation, info_generation, result);
            });
        });
    }

    fn publish_library_info(
        &self,
        ui: &AppWindow,
        generation: u64,
        info_generation: u64,
        result: audiolib_core::Result<audiolib_core::LibraryInfo>,
    ) {
        if self.library_generation.load(Ordering::Acquire) != generation
            || self.library_info_generation.load(Ordering::Acquire) != info_generation
            || self.shutdown.load(Ordering::Acquire)
        {
            return;
        }
        match result {
            Ok(info) => {
                self.tree.lock().unwrap().stats = Some(info.stats.clone());
                let visible_audio_files = info
                    .stats
                    .audio_files
                    .saturating_sub(self.hidden_record_count());
                ui.set_library_summary(
                    format!(
                        "{} {} · {}",
                        visible_audio_files,
                        if self.chinese.load(Ordering::Acquire) {
                            "个声音"
                        } else {
                            "sounds"
                        },
                        format_time(info.stats.total_duration)
                    )
                    .into(),
                );
                ui.set_database_path(info.database.display().to_string().into());
                ui.set_library_health(
                    if self.chinese.load(Ordering::Acquire) {
                        format!(
                            "{} 个可用音效 · {} 个文件夹 · {} 个引用待修复",
                            visible_audio_files, info.stats.folders, info.stats.missing_files
                        )
                    } else {
                        format!(
                            "{} available sounds · {} folders · {} references to repair",
                            visible_audio_files, info.stats.folders, info.stats.missing_files
                        )
                    }
                    .into(),
                );
                ui.set_database_info(
                    format!(
                        "{} {} · {} {} · DB {:.1} MiB + WAL {:.1} MiB · schema {} · SQLite {}",
                        visible_audio_files,
                        if self.chinese.load(Ordering::Acquire) {
                            "存在"
                        } else {
                            "present"
                        },
                        info.stats.missing_files,
                        if self.chinese.load(Ordering::Acquire) {
                            "缺失"
                        } else {
                            "missing"
                        },
                        info.database_bytes as f64 / 1048576.0,
                        info.wal_bytes as f64 / 1048576.0,
                        info.schema_version,
                        info.sqlite_version
                    )
                    .into(),
                );
                crate::gui_search_benchmark::library_info_published(true);
            }
            Err(error) => {
                ui.set_management_report(error.to_string().into());
                crate::gui_search_benchmark::library_info_published(false);
            }
        }
    }

    fn load_history(self: &Arc<Self>, delta: i32) {
        if !self.available() {
            return;
        }
        let mut offset = self.history_offset.lock().unwrap();
        *offset = if delta == 0 {
            0
        } else if delta < 0 {
            offset.saturating_sub(HISTORY_SIZE)
        } else {
            offset.saturating_add(HISTORY_SIZE)
        };
        let offset = *offset;
        self.request_history_page(offset);
    }

    fn start_workers(self: &Arc<Self>) {
        self.start_workspace_worker();
        let c = self.clone();
        std::thread::Builder::new()
            .name("audiolib-selection".into())
            .spawn(move || {
                loop {
                    let mut pending = c.selection_job.lock().unwrap();
                    while pending.is_none() && !c.shutdown.load(Ordering::Acquire) {
                        pending = c.selection_wake.wait(pending).unwrap();
                    }
                    if c.shutdown.load(Ordering::Acquire) {
                        return;
                    }
                    let job = pending.take().unwrap();
                    drop(pending);
                    c.load_selection(job);
                }
            })
            .expect("spawn selection worker");
        let c = self.clone();
        std::thread::Builder::new()
            .name("audiolib-selection-audio".into())
            .spawn(move || {
                loop {
                    let mut pending = c.selection_audio_job.lock().unwrap();
                    while pending.is_none() && !c.shutdown.load(Ordering::Acquire) {
                        pending = c.selection_audio_wake.wait(pending).unwrap();
                    }
                    if c.shutdown.load(Ordering::Acquire) {
                        return;
                    }
                    let job = pending.take().unwrap();
                    drop(pending);
                    c.load_selection_audio(job);
                }
            })
            .expect("spawn selection audio worker");
        let c = self.clone();
        std::thread::Builder::new()
            .name("audiolib-preferences".into())
            .spawn(move || {
                loop {
                    let mut pending = c.single_click_play_pending.lock().unwrap();
                    while !*pending && !c.shutdown.load(Ordering::Acquire) {
                        pending = c.single_click_play_wake.wait(pending).unwrap();
                    }
                    if c.shutdown.load(Ordering::Acquire) {
                        return;
                    }
                    *pending = false;
                    drop(pending);
                    let spectral_peaks = c.spectral_peak_preferences();
                    let result = {
                        let mut config = c.config.lock().unwrap();
                        // Checking under the save lock prevents a late worker from
                        // overwriting shutdown's final preferences.
                        if c.shutdown.load(Ordering::Acquire) {
                            return;
                        }
                        config.audition_play_on_single_click =
                            c.single_click_play.load(Ordering::Acquire);
                        config.pregenerate_list_waveforms =
                            c.pregenerate_list_waveforms.load(Ordering::Acquire);
                        config.hold_list_waveform_batches_after_audition =
                            c.hold_list_waveform_batches.load(Ordering::Acquire);
                        config.ui_scale_percent = c.ui_scale_percent.load(Ordering::Acquire);
                        config.theme = c.theme.lock().unwrap().clone();
                        config.ui_font_size = c.ui_font_size.load(Ordering::Acquire);
                        config.list_font_size = c.list_font_size.load(Ordering::Acquire);
                        config.audio_table = c.audio_table.lock().unwrap().clone();
                        config.spectral_peaks = spectral_peaks;
                        config.max_search_tasks = c.max_search_tasks.load(Ordering::Acquire);
                        config.window_layout = c.window_layout.lock().unwrap().clone();
                        config.save()
                    };
                    if let Err(error) = result {
                        c.status(error.to_string());
                    }
                }
            })
            .expect("spawn preferences worker");

        let c = self.clone();
        std::thread::Builder::new()
            .name("audiolib-metadata".into())
            .spawn(move || {
                loop {
                    let mut state = c.metadata.lock().unwrap();
                    while state.jobs.is_empty() && !c.shutdown.load(Ordering::Acquire) {
                        state = c.metadata_wake.wait(state).unwrap();
                    }
                    if c.shutdown.load(Ordering::Acquire) && state.jobs.is_empty() {
                        return;
                    }
                    let job = state.next().unwrap();
                    drop(state);
                    let provider = c.metadata_provider();
                    let library_generation = c.library_generation.load(Ordering::Acquire);
                    let remote = c.remote_read_only();
                    let chinese = c.chinese.load(Ordering::Acquire);
                    let save = || -> audiolib_core::Result<AudioFile> {
                        let provider = provider.map_err(audiolib_core::Error::Validation)?;
                        match &job.write {
                            MetadataWrite::Draft(draft) => {
                                provider.update_metadata(job.id, &draft.patch)
                            }
                            MetadataWrite::QuickAction(action) => {
                                let file = provider.get_audio(job.id)?;
                                let patch = quick_metadata_patch(*action, file.favorite)
                                    .ok_or_else(|| {
                                        audiolib_core::Error::Validation(
                                            "Unknown Metadata action".into(),
                                        )
                                    })?;
                                provider.update_metadata(job.id, &patch)
                            }
                        }
                    };
                    let result = survive_panic(
                        || save().map_err(|error| error.to_string()),
                        || internal_error(chinese, "保存 Metadata", "Saving Metadata"),
                    );
                    // Queue the UI publication before completion becomes observable:
                    // a waiter (close, source switch) must not see the job completed
                    // and then have this older result overwrite its newer status.
                    let mut state = c.metadata.lock().unwrap();
                    state.complete(&job, &result);
                    let c2 = c.clone();
                    post(c.ui.clone(), move |ui| {
                        if c2.library_generation.load(Ordering::Acquire) != library_generation
                            || c2.library_busy.load(Ordering::Acquire)
                        {
                            return;
                        }
                        match result {
                            Ok(saved) => {
                                if ui.get_selected_id().as_str() == saved.id.to_string() {
                                    let state = c2.metadata.lock().unwrap();
                                    let (displayed, dirty) =
                                        state.for_display(&saved, job.sequence);
                                    drop(state);
                                    *c2.selected.lock().unwrap() = Some(displayed.clone());
                                    // Never replace controls while a newer draft exists.
                                    if !dirty {
                                        present_file(&ui, &displayed);
                                    }
                                    ui.set_metadata_dirty(dirty);
                                }
                                if !c2.closing.load(Ordering::Acquire) {
                                    let chinese = c2.chinese.load(Ordering::Acquire);
                                    ui.set_status(
                                        match (remote, chinese) {
                                            (true, true) => "Metadata 已保存到服务器 · 原始 WAV 未修改",
                                            (true, false) => {
                                                "Metadata saved to the server · Source WAV preserved"
                                            }
                                            (false, true) => "Metadata 已保存 · 原始 WAV 未修改",
                                            (false, false) => "Metadata saved · Source WAV preserved",
                                        }
                                        .into(),
                                    );
                                    c2.publish_metadata_update(&ui, &saved);
                                }
                            }
                            Err(error) => {
                                let message = c2.metadata_failure_message(job.id, &error);
                                ui.set_status(message.into());
                            }
                        }
                    });
                    drop(state);
                    c.metadata_wake.notify_all();
                }
            })
            .expect("spawn Metadata worker");

        let c = self.clone();
        std::thread::Builder::new()
            .name("audiolib-search".into())
            .spawn(move || {
                loop {
                    let mut pending = c.search_job.lock().unwrap();
                    while pending.is_none() && !c.shutdown.load(Ordering::Acquire) {
                        pending = c.search_wake.wait(pending).unwrap();
                    }
                    if c.shutdown.load(Ordering::Acquire) {
                        return;
                    }
                    if let Some(delay) = pending
                        .as_ref()
                        .unwrap()
                        .not_before
                        .checked_duration_since(Instant::now())
                    {
                        let (next, _) = c.search_wake.wait_timeout(pending, delay).unwrap();
                        drop(next);
                        continue;
                    }
                    let job = pending.take().unwrap();
                    drop(pending);
                    if c.search_generation.load(Ordering::Acquire) != job.generation {
                        continue;
                    }
                    let provider = &job.provider;
                    crate::gui_search_benchmark::worker_started(job.generation, false);
                    let revision_before = provider.search_revision().ok().flatten();
                    let chinese = c.chinese.load(Ordering::Acquire);
                    let mut result = survive_panic(
                        || provider.search_cancellable(&job.request, job.cancel.clone()),
                        || {
                            audiolib_core::Error::Database(internal_error(
                                chinese, "搜索", "Search",
                            ))
                        },
                    );
                    let revision_after = provider.search_revision().ok().flatten();
                    let workspace_revision =
                        revision_before.filter(|revision| Some(*revision) == revision_after);
                    crate::gui_search_benchmark::worker_completed(job.generation, false);
                    // Server removals stay hidden until the resync swaps the snapshot.
                    if let Ok(page) = result.as_mut() {
                        c.hide_removed_in_page(page);
                    }
                    if c.search_generation.load(Ordering::Acquire) != job.generation {
                        continue;
                    }
                    if result.is_err() {
                        job.cancel.store(true, Ordering::Release);
                    }
                    let c2 = c.clone();
                    post(c.ui.clone(), move |ui| {
                        if c2.search_generation.load(Ordering::Acquire) != job.generation
                            || c2.shutdown.load(Ordering::Acquire)
                        {
                            return;
                        }
                        ui.set_searching(false);
                        match result {
                            Ok(page) => {
                                if job.request.offset > 0
                                    && (job.request.offset as u64) >= page.total_count
                                {
                                    c2.request.lock().unwrap().offset =
                                        (page.total_count.saturating_sub(1) / PAGE_SIZE as u64)
                                            as usize
                                            * PAGE_SIZE;
                                    let retained_status = c2
                                        .search_publication
                                        .lock()
                                        .unwrap()
                                        .as_ref()
                                        .and_then(|state| state.retained_status.clone());
                                    c2.resume_search(&ui, retained_status);
                                    return;
                                }
                                let checked = c2.multi_selection.lock().unwrap().clone();
                                let rows = page
                                    .results
                                    .iter()
                                    .map(|file| {
                                        let mut row = audio_row(file);
                                        row.checked = checked.contains(&file.id);
                                        row
                                    })
                                    .collect::<Vec<_>>();
                                *c2.records.lock().unwrap() = page.results;
                                let rows = c2.decorate_list_waveforms(rows);
                                let selected_id = ui.get_selected_id();
                                let selected_row =
                                    rows.iter().position(|row| row.id == selected_id);
                                publish_audio_rows(&ui, rows);
                                ui.set_selected_row(selected_row.map_or(-1, |index| index as i32));
                                ui.set_can_previous(job.request.offset > 0);
                                ui.set_can_next(
                                    ((job.request.offset + page.returned_count) as u64)
                                        < page.total_count,
                                );
                                ui.set_page_label(
                                    format!(
                                        "{}/{}",
                                        job.request.offset / PAGE_SIZE + 1,
                                        page.total_count.div_ceil(PAGE_SIZE as u64).max(1)
                                    )
                                    .into(),
                                );
                                let shown = if page.total_count == 0 {
                                    "0".into()
                                } else {
                                    format!(
                                        "{}–{}",
                                        job.request.offset + 1,
                                        job.request.offset + page.returned_count
                                    )
                                };
                                let chinese = c2.chinese.load(Ordering::Acquire);
                                ui.set_result_summary(
                                    if chinese {
                                        format!(
                                            "{shown} / {} 条 · {:.1} ms",
                                            page.total_count, page.execution_time_ms
                                        )
                                    } else {
                                        format!(
                                            "{shown} of {} sounds · {:.1} ms",
                                            page.total_count, page.execution_time_ms
                                        )
                                    }
                                    .into(),
                                );
                                let retained_status = c2
                                    .search_publication
                                    .lock()
                                    .unwrap()
                                    .as_ref()
                                    .and_then(|state| state.retained_status.clone());
                                ui.set_status(
                                    retained_status
                                        .unwrap_or_else(|| {
                                            if chinese {
                                                "正在更新目录…"
                                            } else {
                                                "Updating folders…"
                                            }
                                            .into()
                                        })
                                        .into(),
                                );
                                if crate::gui_search_benchmark::enabled() {
                                    let ids = c2
                                        .records
                                        .lock()
                                        .unwrap()
                                        .iter()
                                        .map(|file| file.id)
                                        .collect();
                                    crate::gui_search_benchmark::page_published(
                                        &ui,
                                        job.generation,
                                        page.total_count,
                                        page.returned_count,
                                        ids,
                                        page.execution_time_ms,
                                        page.cache_hit,
                                    );
                                }
                                c2.search_page_published(&ui, job.generation, true);
                                c2.search_task_page_ready(&ui, workspace_revision);
                            }
                            Err(error) => {
                                ui.set_status(error.to_string().into());
                                c2.search_page_published(&ui, job.generation, false);
                            }
                        }
                    });
                }
            })
            .expect("spawn search worker");

        let c = self.clone();
        std::thread::Builder::new()
            .name("audiolib-search-folders".into())
            .spawn(move || {
                let mut facets: Option<FolderFacets> = None;
                loop {
                    let mut pending = c.facet_job.lock().unwrap();
                    while pending.is_none() && !c.shutdown.load(Ordering::Acquire) {
                        pending = c.facet_wake.wait(pending).unwrap();
                    }
                    if c.shutdown.load(Ordering::Acquire) {
                        return;
                    }
                    if let Some(delay) = pending
                        .as_ref()
                        .unwrap()
                        .not_before
                        .checked_duration_since(Instant::now())
                    {
                        let (next, _) = c.facet_wake.wait_timeout(pending, delay).unwrap();
                        drop(next);
                        continue;
                    }
                    let job = pending.take().unwrap();
                    drop(pending);
                    if c.search_generation.load(Ordering::Acquire) != job.generation
                        || job.cancel.load(Ordering::Acquire)
                    {
                        continue;
                    }
                    let provider = &job.provider;
                    crate::gui_search_benchmark::worker_started(job.generation, true);
                    let database_revision = provider.search_revision().ok().flatten();
                    let cached = facets.as_ref().filter(|cached| {
                        cached.matches(
                            &job.request,
                            job.facet_revision,
                            database_revision,
                            provider,
                        )
                    });
                    let cache_hit = cached.is_some();
                    let chinese = c.chinese.load(Ordering::Acquire);
                    let count_folders = || {
                        survive_panic(
                            || {
                                provider
                                    .search_folders_cancellable(&job.request, job.cancel.clone())
                                    .map(Arc::new)
                            },
                            || {
                                audiolib_core::Error::Database(internal_error(
                                    chinese,
                                    "统计目录",
                                    "Counting folders",
                                ))
                            },
                        )
                    };
                    let mut folders = if let Some(cached) = cached {
                        Ok(cached.folders.clone())
                    } else {
                        count_folders()
                    };
                    if c.search_generation.load(Ordering::Acquire) != job.generation
                        || job.cancel.load(Ordering::Acquire)
                    {
                        continue;
                    }
                    let after_revision = provider.search_revision().ok().flatten();
                    let stable = database_revision.is_some() && database_revision == after_revision;
                    if cache_hit && !stable {
                        folders = count_folders();
                    }
                    facets = None;
                    if stable && let Ok(folders) = &folders {
                        facets = Some(FolderFacets {
                            provider: Arc::downgrade(provider),
                            revision: job.facet_revision,
                            database_revision,
                            query: job.request.query.clone(),
                            filters: job.request.filters.clone(),
                            folders: folders.clone(),
                        });
                    }
                    crate::gui_search_benchmark::worker_completed(job.generation, true);
                    let c2 = c.clone();
                    post(c.ui.clone(), move |ui| {
                        if c2.search_generation.load(Ordering::Acquire) != job.generation {
                            return;
                        }
                        c2.search_task_folders_ready(database_revision.filter(|_| stable));
                        c2.search_folders_computed(
                            &ui,
                            job.generation,
                            folders.map_err(|error| error.to_string()),
                        );
                    });
                }
            })
            .expect("spawn directory search worker");

        let c = self.clone();
        std::thread::Builder::new()
            .name("audiolib-waveform".into())
            .spawn(move || {
                loop {
                    let mut pending = c.waveform_job.lock().unwrap();
                    while pending.is_none() && !c.shutdown.load(Ordering::Acquire) {
                        pending = c.waveform_wake.wait(pending).unwrap();
                    }
                    if c.shutdown.load(Ordering::Acquire) {
                        return;
                    }
                    let job = pending.take().unwrap();
                    drop(pending);
                    if job.cancel.load(Ordering::Acquire) {
                        continue;
                    }
                    let request = WaveformRequest {
                        samples_per_pixel: job
                            .frames
                            .div_ceil(job.width as u64)
                            .max(1)
                            .min(u32::MAX as u64) as u32,
                        start_frame: job.start_frame,
                        end_frame: Some(job.end_frame),
                        include_colors: true,
                    };
                    let chinese = c.chinese.load(Ordering::Acquire);
                    let result = survive_panic(
                        || match job.remote_id {
                            // Server mode: the server's waveform of the original, drawn as soon
                            // as it arrives, whatever the audition stream is doing.
                            Some(id) => c.remote_waveform(
                                id,
                                job.sample_rate,
                                request,
                                &job.cancel,
                                |data, progress| {
                                    c.publish_waveform(&job, data, progress);
                                },
                            ),
                            None => {
                                let cache = c.config.lock().unwrap().paths.cache.join("waveforms");
                                audiolib_audio::cached_waveform_with_cancel(
                                    &job.path,
                                    &cache,
                                    request,
                                    &job.cancel,
                                )
                                .map_err(|error| error.to_string())
                            }
                        },
                        || internal_error(chinese, "绘制 Waveform", "Drawing the Waveform"),
                    );
                    if job.cancel.load(Ordering::Acquire)
                        || c.waveform_generation.load(Ordering::Acquire) != job.generation
                    {
                        continue;
                    }
                    match result {
                        Ok(data) => c.publish_waveform(&job, &data, 1.0),
                        Err(error) => c.status(error),
                    }
                }
            })
            .expect("spawn waveform worker");

        let c = self.clone();
        let events = c.audio.subscribe_playback_starts();
        std::thread::Builder::new()
            .name("audiolib-status".into())
            .spawn(move || {
                let mut playback_history = PlaybackHistoryTracker::default();
                let mut external_wave_generation = None;
                let mut last_audition = None;
                let mut last_playback_error = None;
                let mut playback_progress = PlaybackProgress::default();
                // Network audition waiting for data: since when, and whether the hint shows.
                let mut buffering_since: Option<Instant> = None;
                let mut buffering_shown = false;
                while !c.shutdown.load(Ordering::Acquire) {
                    if c.library_busy.load(Ordering::Acquire) {
                        std::thread::sleep(Duration::from_millis(65));
                        continue;
                    }
                    let library_generation = c.library_generation.load(Ordering::Acquire);
                    let source_generation = c.source_generation.load(Ordering::Acquire);
                    let mut status = c.audio.status();
                    c.list_waveforms.observe_playback(matches!(
                        status.state,
                        PlaybackState::Loading | PlaybackState::Playing
                    ));
                    let history_events: Vec<_> = events.try_iter().collect();
                    if playback_history
                        .library_generation
                        .is_some_and(|generation| generation != library_generation)
                    {
                        // Management can pause this worker while playback events accumulate.
                        // Baseline the current source after discarding those old-library events.
                        status = c.audio.status();
                    }
                    if status.sample_rate > 0
                        && status.state != PlaybackState::Loading
                        && external_wave_generation != Some(status.generation)
                    {
                        external_wave_generation = Some(status.generation);
                        let selected = c.selected.lock().unwrap().clone();
                        // Server-mode audition plays a cached copy whose waveform the
                        // selection loader already queued.
                        let cached_copy = c.remote_read_only()
                            && status.file_path.as_ref().is_some_and(|path| {
                                remote_mode::is_remote_cache_path(
                                    &c.config.lock().unwrap().data_root(),
                                    path,
                                )
                            });
                        if !cached_copy
                            && !selected.as_ref().is_some_and(|file| {
                                status
                                    .file_path
                                    .as_ref()
                                    .is_some_and(|path| same_path(Path::new(&file.file_path), path))
                            })
                            && let Some(file) = c.waveform_file(&status)
                        {
                            c.queue_waveform(&file);
                        }
                    }
                    playback_history.observe(library_generation, &status, history_events);
                    if let Err(error) = playback_history.flush(|path| {
                        if !c.available()
                            || c.library_generation.load(Ordering::Acquire) != library_generation
                        {
                            return Ok(false);
                        }
                        c.record_history_best_effort(path, library_generation, source_generation);
                        Ok(true)
                    }) {
                        crate::diagnostics::record(
                            crate::diagnostics::Event::PlaybackHistoryFailed,
                        );
                        c.status(error.to_string());
                    }
                    // The engine pauses itself while a network stream runs dry; the hint
                    // shows only if that lasts (the short prebuffer at each start is silent).
                    if status.buffering && status.state == PlaybackState::Playing {
                        let since = *buffering_since.get_or_insert_with(Instant::now);
                        if !buffering_shown && since.elapsed() >= BUFFERING_HINT_AFTER {
                            buffering_shown = true;
                            c.buffering_hint(true);
                        }
                    } else {
                        buffering_since = None;
                        if buffering_shown {
                            buffering_shown = false;
                            c.buffering_hint(false);
                        }
                    }
                    let audition_changed = last_audition != Some(status.audition);
                    let error_changed = last_playback_error != status.error;
                    last_playback_error = status.error.clone();
                    let progress_continues = playback_progress.observe(&status);
                    last_audition = Some(status.audition);
                    c.playback.lock().unwrap().audition = status.audition;
                    // Server mode plays a cache copy: show the record's own file name.
                    let playback_name = status
                        .file_path
                        .as_deref()
                        .map(|path| c.playback_display_name(path));
                    let c2 = c.clone();
                    post(c.ui.clone(), move |ui| {
                        if c2.library_generation.load(Ordering::Acquire) != library_generation
                            || c2.library_busy.load(Ordering::Acquire)
                        {
                            return;
                        }
                        publish_playback_progress(&ui, &status, progress_continues);
                        let ready = !matches!(
                            status.state,
                            PlaybackState::Empty | PlaybackState::Loading | PlaybackState::Error
                        ) && status.file_path.is_some()
                            && status.generation
                                != c2.removed_audio_generation.load(Ordering::Acquire);
                        ui.set_audio_ready(ready);
                        if let Some(name) = &playback_name {
                            ui.set_playback_name(name.as_str().into());
                        }
                        if !ui.get_volume_adjusting() {
                            if (ui.get_volume() - status.volume).abs() > 0.001 {
                                ui.set_volume(status.volume);
                            }
                            publish_volume(&ui, status.volume);
                        }
                        if audition_changed {
                            publish_audition(&ui, status.audition);
                        }
                        ui.set_output_summary(output_summary(&status).into());
                        if error_changed && let Some(error) = status.error {
                            ui.set_status(error.into());
                        }
                    });
                    std::thread::sleep(Duration::from_millis(65));
                }
            })
            .expect("spawn playback status worker");
    }

    /// Annotation saves do not affect the unfiltered result order or folder counts.
    /// Refresh dependent searches, otherwise publish the confirmed row in place.
    fn publish_metadata_update(self: &Arc<Self>, ui: &AppWindow, saved: &AudioFile) {
        let request = self.request.lock().unwrap().clone();
        let pending_page = self
            .search_publication
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|state| state.page == SearchPagePhase::Pending);
        if metadata_affects_search(&request) || pending_page {
            self.request_search(ui, true, false);
            return;
        }
        let checked = self.multi_selection.lock().unwrap().clone();
        let rows = {
            let mut records = self.records.lock().unwrap();
            if let Some(file) = records.iter_mut().find(|file| file.id == saved.id) {
                *file = saved.clone();
            } else {
                return;
            }
            records
                .iter()
                .map(|file| {
                    let mut row = audio_row(file);
                    row.checked = checked.contains(&file.id);
                    row
                })
                .collect()
        };
        publish_audio_rows(ui, self.decorate_list_waveforms(rows));
    }

    fn request_search(self: &Arc<Self>, ui: &AppWindow, immediate: bool, reset: bool) {
        // Writes, scans, refresh and library changes invalidate even if their request
        // is superseded by navigation before the worker has recomputed the facets.
        self.facet_revision.fetch_add(1, Ordering::AcqRel);
        self.request_navigation_search(ui, immediate, reset);
    }

    fn resume_search(self: &Arc<Self>, ui: &AppWindow, retained_status: Option<String>) {
        self.request_search(ui, true, false);
        if let Some(publication) = self.search_publication.lock().unwrap().as_mut() {
            publication.retained_status = retained_status.clone();
        }
        if let Some(status) = retained_status {
            ui.set_status(status.into());
        }
    }

    fn search_page_published(self: &Arc<Self>, ui: &AppWindow, generation: u64, succeeded: bool) {
        let folders = {
            let mut publication = self.search_publication.lock().unwrap();
            let Some(state) = publication
                .as_mut()
                .filter(|state| state.generation == generation)
            else {
                return;
            };
            state.page = if succeeded {
                SearchPagePhase::Published
            } else {
                SearchPagePhase::Failed
            };
            if !succeeded {
                state.pending_folders = None;
                state.expected_folders = None;
                return;
            }
            state.pending_folders.take()
        };
        if let Some(folders) = folders {
            self.apply_search_folders(ui, generation, folders);
        }
    }

    fn search_folders_computed(
        self: &Arc<Self>,
        ui: &AppWindow,
        generation: u64,
        folders: Result<Arc<Vec<Folder>>, String>,
    ) {
        if self.search_generation.load(Ordering::Acquire) != generation {
            return;
        }
        {
            let mut publication = self.search_publication.lock().unwrap();
            let Some(state) = publication
                .as_mut()
                .filter(|state| state.generation == generation)
            else {
                return;
            };
            match state.page {
                SearchPagePhase::Pending => {
                    state.pending_folders = Some(folders);
                    return;
                }
                SearchPagePhase::Failed => return,
                SearchPagePhase::Published => {}
            }
        }
        self.apply_search_folders(ui, generation, folders);
    }

    fn apply_search_folders(
        self: &Arc<Self>,
        ui: &AppWindow,
        generation: u64,
        folders: Result<Arc<Vec<Folder>>, String>,
    ) {
        if self.search_generation.load(Ordering::Acquire) != generation {
            return;
        }
        match folders {
            Ok(folders) => {
                {
                    let mut publication = self.search_publication.lock().unwrap();
                    let Some(state) = publication.as_mut().filter(|state| {
                        state.generation == generation && state.page == SearchPagePhase::Published
                    }) else {
                        return;
                    };
                    state.expected_folders = Some(folders.clone());
                }
                self.tree.lock().unwrap().matches = folders;
                self.reload_tree();
            }
            Err(error) => ui.set_status(error.into()),
        }
    }

    /// Invalid input runs no search. Say so where the result count is shown, which
    /// stays until the next search, so the previous page is not taken for its results.
    fn reject_search(&self, ui: &AppWindow, message: String) {
        ui.set_result_summary(
            if self.chinese.load(Ordering::Acquire) {
                format!("未搜索：{message}（列表仍是上一次的结果）")
            } else {
                format!("Not searched: {message} (the list shows the previous results)")
            }
            .into(),
        );
        ui.set_status(message.into());
    }

    fn request_navigation_search(self: &Arc<Self>, ui: &AppWindow, immediate: bool, reset: bool) {
        if !self.available() {
            return;
        }
        self.persist_search_task(ui);
        // Even invalid input supersedes queued/in-flight work and its UI publications.
        self.search_cancel
            .lock()
            .unwrap()
            .store(true, Ordering::Release);
        *self.search_job.lock().unwrap() = None;
        *self.facet_job.lock().unwrap() = None;
        *self.search_publication.lock().unwrap() = None;
        let generation = self.search_generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.tree_generation.fetch_add(1, Ordering::AcqRel);
        ui.set_searching(false);
        if self.task_folder_unavailable(ui) {
            return;
        }
        let mut request = self.request.lock().unwrap().clone();
        request.query = ui.get_query().to_string();
        if request.needs_more_search_text() {
            ui.set_status(
                if self.chinese.load(Ordering::Acquire) {
                    "字母搜索请至少输入 2 个字符"
                } else {
                    "Enter at least 2 characters for a letter search"
                }
                .into(),
            );
            return;
        }
        request.limit = PAGE_SIZE;
        if reset {
            request.offset = 0;
        }
        let samples = [0, 44_100, 48_000, 88_200, 96_000, 192_000];
        let bits = [0, 16, 24, 32];
        let channels = [0, 1, 2, 4, 6, 8];
        let value = samples
            .get(ui.get_sample_filter() as usize)
            .copied()
            .unwrap_or(0);
        let bit = bits.get(ui.get_bit_filter() as usize).copied().unwrap_or(0);
        let channel = channels
            .get(ui.get_channel_filter() as usize)
            .copied()
            .unwrap_or(0);
        let duration = |value: slint::SharedString| -> Result<Option<f64>, String> {
            if value.trim().is_empty() {
                Ok(None)
            } else {
                value
                    .trim()
                    .parse::<f64>()
                    .map(Some)
                    .map_err(|_| "Duration must be a number in seconds".into())
            }
        };
        // The sidebar's filter count compares these fields with "": blank input is
        // no filter, so store it as empty.
        if ui.get_duration_min().trim().is_empty() && !ui.get_duration_min().is_empty() {
            ui.set_duration_min("".into());
        }
        if ui.get_duration_max().trim().is_empty() && !ui.get_duration_max().is_empty() {
            ui.set_duration_max("".into());
        }
        if ui.get_color_filter().trim().is_empty() && !ui.get_color_filter().is_empty() {
            ui.set_color_filter("".into());
        }
        let min = duration(ui.get_duration_min());
        let max = duration(ui.get_duration_max());
        let (Ok(min), Ok(max)) = (min, max) else {
            self.reject_search(
                ui,
                if self.chinese.load(Ordering::Acquire) {
                    "时长必须是以秒为单位的数字"
                } else {
                    "Duration must be a number in seconds"
                }
                .into(),
            );
            return;
        };
        request.filters = SearchFilters {
            visibility: match ui.get_visibility_index() {
                1 => FileVisibility::Missing,
                2 => FileVisibility::All,
                _ => FileVisibility::Present,
            },
            sample_rate: (value > 0).then_some(value),
            bit_depth: (bit > 0).then_some(bit),
            channels: (channel > 0).then_some(channel),
            duration_min: min,
            duration_max: max,
            favorites_only: ui.get_favorites_only(),
            tagged_only: ui.get_tagged_only(),
            color: (!ui.get_color_filter().trim().is_empty())
                .then(|| ui.get_color_filter().trim().to_owned()),
            ..SearchFilters::default()
        };
        crate::tag_chips::apply_to(ui, &mut request.filters);
        request.order_by = table_sort_order(ui.get_sort_index());
        request.descending = ui.get_sort_descending();
        if let Err(error) = request.validate() {
            self.reject_search(ui, error.to_string());
            return;
        }
        crate::gui_search_benchmark::request(ui, generation, &request);
        self.begin_task_search(reset);
        *self.request.lock().unwrap() = request.clone();
        self.persist_search_task(ui);
        let cancel = Arc::new(AtomicBool::new(false));
        std::mem::replace(&mut *self.search_cancel.lock().unwrap(), cancel.clone())
            .store(true, Ordering::Release);
        let job = SearchJob {
            cancel,
            facet_revision: self.facet_revision.load(Ordering::Acquire),
            request,
            generation,
            not_before: Instant::now()
                + if immediate {
                    Duration::ZERO
                } else {
                    // Coalesce a short typing burst while leaving the first
                    // results and complete folder counts enough render budget.
                    // New generations still replace queued work and cancel both
                    // active workers before publishing any stale models.
                    Duration::from_millis(30)
                },
            provider: self.provider(),
        };
        *self.search_publication.lock().unwrap() = Some(SearchPublication {
            generation,
            page: SearchPagePhase::Pending,
            pending_folders: None,
            expected_folders: None,
            model_published: false,
            retained_status: None,
        });
        *self.search_job.lock().unwrap() = Some(job.clone());
        *self.facet_job.lock().unwrap() = Some(job);
        ui.set_searching(true);
        ui.set_status(
            if self.chinese.load(Ordering::Acquire) {
                "正在搜索…"
            } else {
                "Searching…"
            }
            .into(),
        );
        self.search_wake.notify_one();
        self.facet_wake.notify_one();
    }

    fn choose_folder(self: &Arc<Self>, index: i32) {
        let id = self
            .tree
            .lock()
            .unwrap()
            .visible
            .get(index as usize)
            .map(|(folder, _, _)| folder.id);
        if let Some(id) = id {
            self.select_folder_id(id);
        }
    }

    fn select_folder_id(self: &Arc<Self>, id: i64) {
        self.clear_task_folder_error();
        let path = {
            let tree = self.tree.lock().unwrap();
            if tree.selected == Some(id) {
                return;
            }
            tree.known.get(&id).map(|folder| folder.path.clone())
        };
        if let Some(ui) = self.ui.upgrade() {
            // Visible folders and breadcrumbs already have a validated owned
            // path. Keep the scope indicator stable while workers publish.
            ui.set_selected_folder_path(path.unwrap_or_default().into());
        }
        self.tree.lock().unwrap().selected = Some(id);
        self.request.lock().unwrap().folder_id = Some(id);
        self.reload_tree();
        if let Some(ui) = self.ui.upgrade() {
            self.request_navigation_search(&ui, true, true);
        }
    }

    fn reload_tree(self: &Arc<Self>) {
        if !self.available() {
            return;
        }
        let generation = self.tree_generation.fetch_add(1, Ordering::AcqRel) + 1;
        let (expanded, selected, matches, known) = {
            let tree = self.tree.lock().unwrap();
            (
                tree.expanded.clone(),
                tree.selected,
                tree.matches.clone(),
                tree.known.clone(),
            )
        };
        let ready_generation = self
            .search_publication
            .lock()
            .unwrap()
            .as_ref()
            .filter(|state| {
                state.page == SearchPagePhase::Published
                    && !state.model_published
                    && state
                        .expected_folders
                        .as_ref()
                        .is_some_and(|expected| Arc::ptr_eq(expected, &matches))
            })
            .map(|state| state.generation);
        let c = self.clone();
        std::thread::spawn(move || {
            if c.tree_generation.load(Ordering::Acquire) != generation {
                return;
            }
            // Clone owned folder strings on the worker, never under the UI's tree lock.
            let mut known = known.as_ref().clone();
            let mut matches = matches.as_ref().clone();
            c.hide_removed_folders(&mut matches);
            let mut children = HashMap::<Option<i64>, Vec<Folder>>::new();
            for folder in matches {
                known.insert(folder.id, folder.clone());
                children.entry(folder.parent_id).or_default().push(folder);
            }
            let mut visible = Vec::new();
            fn visit(
                children: &HashMap<Option<i64>, Vec<Folder>>,
                parent: Option<i64>,
                depth: usize,
                expanded: &HashSet<i64>,
                visible: &mut Vec<(Folder, usize, bool)>,
                seen: &mut HashSet<i64>,
            ) {
                if depth > 64 {
                    return;
                }
                for folder in children.get(&parent).into_iter().flatten() {
                    if !seen.insert(folder.id) {
                        continue;
                    }
                    let has_children = children.contains_key(&Some(folder.id));
                    visible.push((folder.clone(), depth, has_children));
                    if expanded.contains(&folder.id) {
                        visit(
                            children,
                            Some(folder.id),
                            depth + 1,
                            expanded,
                            visible,
                            seen,
                        );
                    }
                }
            }
            visit(
                &children,
                None,
                0,
                &expanded,
                &mut visible,
                &mut HashSet::new(),
            );
            if c.tree_generation.load(Ordering::Acquire) != generation {
                return;
            }
            let mut crumbs = Vec::new();
            let mut next = selected;
            let mut seen = HashSet::new();
            while let Some(id) = next {
                if !seen.insert(id) {
                    break;
                }
                let Some(folder) = known.get(&id) else {
                    break;
                };
                crumbs.push(folder.clone());
                next = folder.parent_id;
            }
            crumbs.reverse();
            let selected_row = visible
                .iter()
                .position(|(folder, _, _)| Some(folder.id) == selected)
                .map_or(-1, |index| index as i32);
            let visible = Arc::new(visible);
            let c2 = c.clone();
            post(c.ui.clone(), move |ui| {
                if c2.tree_generation.load(Ordering::Acquire) != generation {
                    return;
                }
                ui.set_selected_folder(selected_row);
                // The selected descendant can remain active when its parent is
                // collapsed. Publish its owned path separately from row indices.
                ui.set_selected_folder_path(
                    selected
                        .and_then(|id| known.get(&id))
                        .map_or_else(String::new, |folder| folder.path.clone())
                        .into(),
                );
                // Selection-only publications must keep the same TouchAreas:
                // Slint resets double-click counting when the target changes.
                let unchanged = ui
                    .get_folders()
                    .as_any()
                    .downcast_ref::<FolderTreeModel>()
                    .is_some_and(|model| model.has_same_rows(&visible, &expanded));
                if !unchanged {
                    ui.set_folders(ModelRc::new(FolderTreeModel {
                        visible: visible.clone(),
                        expanded,
                    }));
                }
                ui.set_crumbs(ModelRc::new(VecModel::from(
                    crumbs
                        .iter()
                        .map(|folder| CrumbRow {
                            name: folder.name.clone().into(),
                        })
                        .collect::<Vec<_>>(),
                )));
                let mut tree = c2.tree.lock().unwrap();
                tree.visible = visible;
                tree.known = Arc::new(known);
                tree.crumbs = crumbs;
                let stats = tree.stats.clone();
                drop(tree);
                if let Some(stats) = stats {
                    ui.set_library_summary(
                        format!(
                            "{} {} · {}",
                            stats.audio_files.saturating_sub(c2.hidden_record_count()),
                            if c2.chinese.load(Ordering::Acquire) {
                                "个声音"
                            } else {
                                "sounds"
                            },
                            format_time(stats.total_duration)
                        )
                        .into(),
                    );
                }
                if let Some(generation) = ready_generation
                    && c2.search_generation.load(Ordering::Acquire) == generation
                {
                    let ready_status = {
                        let mut publication = c2.search_publication.lock().unwrap();
                        if let Some(state) = publication.as_mut().filter(|state| {
                            state.generation == generation
                                && state.page == SearchPagePhase::Published
                                && !state.model_published
                        }) {
                            state.model_published = true;
                            Some(state.retained_status.clone())
                        } else {
                            None
                        }
                    };
                    if let Some(retained_status) = ready_status {
                        c2.search_task_directory_ready(&ui);
                        ui.set_searching(false);
                        ui.set_status(
                            retained_status
                                .unwrap_or_else(|| {
                                    if c2.chinese.load(Ordering::Acquire) {
                                        "就绪 · 选择声音查看 Metadata"
                                    } else {
                                        "Ready · Select a sound for Metadata"
                                    }
                                    .into()
                                })
                                .into(),
                        );
                        if crate::gui_search_benchmark::enabled() {
                            let folders = c2.tree.lock().unwrap().matches.clone();
                            crate::gui_search_benchmark::folders_published(
                                &ui, generation, folders,
                            );
                        }
                    }
                }
            });
        });
    }

    fn select(self: &Arc<Self>, index: i32, play: bool) {
        let Some(file) = self.records.lock().unwrap().get(index as usize).cloned() else {
            return;
        };
        self.select_id(file.id, play);
    }

    // Resolve by immutable library ID so a search refresh cannot retarget a menu action.
    fn select_id(self: &Arc<Self>, id: i64, play: bool) {
        self.select_id_guarded(id, play, None);
    }

    /// Admit a deferred IPC audition only while its original selection remains current.
    fn select_id_guarded(self: &Arc<Self>, id: i64, play: bool, expected_generation: Option<u64>) {
        if !self.available()
            || expected_generation.is_some_and(|expected| {
                self.selection_generation.load(Ordering::Acquire) != expected
            })
        {
            return;
        }
        let Some(ui) = self.ui.upgrade() else { return };
        let barrier = match self.enqueue_current_metadata(&ui) {
            Ok(sequence) => sequence,
            Err(error) => {
                ui.set_status(error.into());
                return;
            }
        };
        let mut remembered = self.latest_selection.lock().unwrap();
        // IPC Stop can arrive while the UI captures/enqueues Metadata. Checking
        // again under the admission lock makes cancellation atomic with the new job.
        if expected_generation
            .is_some_and(|expected| self.selection_generation.load(Ordering::Acquire) != expected)
        {
            return;
        }
        let generation = self.selection_generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.invalidate_waveform();
        self.remote_audio_cancel
            .lock()
            .unwrap()
            .store(true, Ordering::Release);
        let job = SelectionJob {
            id,
            play,
            generation,
            barrier,
            provider: self.provider(),
        };
        *remembered = Some(job.clone());
        *self.selection_job.lock().unwrap() = Some(job);
        self.selection_wake.notify_one();
        self.metadata_wake.notify_all();
    }

    /// Resume only an unfinished selection cancelled by a source gate. The original
    /// FIFO barrier remains binding; no draft is enqueued or implicitly retried here.
    fn resume_source_selection(self: &Arc<Self>, resume_pending: bool) {
        let mut remembered = self.latest_selection.lock().unwrap();
        let pending = remembered.take();
        if resume_pending
            && let Some(mut job) = pending
            && Arc::ptr_eq(&job.provider, &self.provider())
        {
            job.generation = self.selection_generation.fetch_add(1, Ordering::AcqRel) + 1;
            self.invalidate_waveform();
            *remembered = Some(job.clone());
            *self.selection_job.lock().unwrap() = Some(job);
            self.selection_wake.notify_one();
            self.metadata_wake.notify_all();
            return;
        }
        drop(remembered);
        let status = self.audio.status();
        let waveform = self.waveform_file(&status).or_else(|| {
            status
                .file_path
                .is_none()
                .then(|| self.selected.lock().unwrap().clone())
                .flatten()
        });
        if let Some(file) = waveform {
            self.queue_waveform(&file);
        }
    }

    /// A consumed play request must not be replayed by a later profile save.
    fn selection_request_completed(&self, generation: u64) {
        let mut pending = self.latest_selection.lock().unwrap();
        if pending
            .as_ref()
            .is_some_and(|job| job.generation == generation)
        {
            *pending = None;
        }
    }

    fn wait_selection_metadata(&self, barrier: u64, generation: u64) -> Option<u64> {
        let mut state = self.metadata.lock().unwrap();
        loop {
            if !self.available() || self.selection_generation.load(Ordering::Acquire) != generation
            {
                return None;
            }
            if state.completed >= barrier {
                return Some(state.completed);
            }
            state = self
                .metadata_wake
                .wait_timeout(state, Duration::from_millis(20))
                .unwrap()
                .0;
        }
    }

    fn load_selection(self: &Arc<Self>, job: SelectionJob) {
        let SelectionJob {
            id,
            play,
            generation,
            barrier,
            provider,
        } = job;
        let c = self.clone();
        let Some(read_sequence) = c.wait_selection_metadata(barrier, generation) else {
            return;
        };
        if c.selection_generation.load(Ordering::Acquire) != generation {
            return;
        }
        let loaded = provider.get_audio(id);
        let file = match loaded {
            Ok(file) => file,
            Err(error) => {
                if c.selection_generation.load(Ordering::Acquire) == generation && c.available() {
                    c.selection_request_completed(generation);
                    let c2 = c.clone();
                    post(c.ui.clone(), move |ui| {
                        if c2.selection_generation.load(Ordering::Acquire) == generation
                            && c2.available()
                        {
                            ui.set_status(error.to_string().into());
                        }
                    });
                }
                return;
            }
        };
        if c.selection_generation.load(Ordering::Acquire) != generation {
            return;
        }
        let c2 = c.clone();
        post(c.ui.clone(), move |ui| {
            if c2.selection_generation.load(Ordering::Acquire) != generation
                || c2.closing.load(Ordering::Acquire)
            {
                return;
            }
            // Also save edits entered after the selection request, while its read ran.
            if let Err(error) = c2.enqueue_current_metadata(&ui) {
                c2.selection_request_completed(generation);
                ui.set_status(error.into());
                return;
            }
            let (displayed, dirty) = c2
                .metadata
                .lock()
                .unwrap()
                .for_display(&file, read_sequence);
            // File identity and its controls change together on the event loop.
            *c2.selected.lock().unwrap() = Some(displayed.clone());
            present_file(&ui, &displayed);
            // A page can publish while this selection waits for Metadata. Locate
            // the completed identity in the current page instead of retaining its
            // old row index or the previous inspector's highlight.
            let selected_row = c2
                .records
                .lock()
                .unwrap()
                .iter()
                .position(|file| file.id == displayed.id);
            ui.set_selected_row(selected_row.map_or(-1, |index| index as i32));
            ui.set_metadata_dirty(dirty);
            ui.set_audio_ready(false);
            if displayed.missing {
                c2.selection_request_completed(generation);
                ui.set_waveform_ready(false);
                ui.set_waveform_image(Image::default());
                ui.set_waveform_info(
                    if c2.chinese.load(Ordering::Acquire) {
                        "文件缺失，请重新关联 WAV"
                    } else {
                        "Missing file: relink a WAV to audition"
                    }
                    .into(),
                );
                if play {
                    ui.set_status(
                        if c2.chinese.load(Ordering::Acquire) {
                            "缺失记录无法试听"
                        } else {
                            "Missing records cannot be auditioned"
                        }
                        .into(),
                    );
                }
                return;
            }
            let remote = c2.remote_read_only();
            // Until this selection's Load is confirmed, Play must reload its ID
            // rather than audition the previous source retained by the engine.
            c2.mark_audio_reload(displayed.id);
            // Server mode too: the server computes it, independent of the audio.
            c2.queue_waveform(&displayed);
            // A newer selection cancels an older server request before queueing.
            let download_cancel = Arc::new(AtomicBool::new(false));
            std::mem::replace(
                &mut *c2.remote_audio_cancel.lock().unwrap(),
                download_cancel.clone(),
            )
            .store(true, Ordering::Release);
            *c2.selection_audio_job.lock().unwrap() = Some(SelectionAudioJob {
                file: displayed,
                play,
                remote,
                generation,
                cancel: download_cancel,
            });
            c2.selection_audio_wake.notify_one();
        });
    }

    fn load_selection_audio(self: &Arc<Self>, job: SelectionAudioJob) {
        let _prepare = self.remote_audio_prepare.lock().unwrap();
        let SelectionAudioJob {
            file: displayed,
            play,
            remote,
            generation,
            cancel: download_cancel,
        } = job;
        let loading = self.clone();
        if remote && !play {
            // Arrowing through rows: only a selection that rests opens a server
            // request (and an engine Load); passed-over rows cost nothing.
            let settle = Instant::now() + remote_mode::SELECTION_SETTLE;
            while Instant::now() < settle {
                if loading.selection_generation.load(Ordering::Acquire) != generation
                    || download_cancel.load(Ordering::Acquire)
                {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        if loading.selection_generation.load(Ordering::Acquire) != generation
            || loading.closing.load(Ordering::Acquire)
        {
            return;
        }
        let path = match loading.playable_path(&displayed, &download_cancel, generation) {
            Ok(playable) => playable,
            Err(error) => {
                if !download_cancel.load(Ordering::Acquire) {
                    loading.selection_request_completed(generation);
                    loading.status(error);
                }
                return;
            }
        };
        let completion = {
            // Stop and selection changes use the same lock. An old Load either
            // enters the FIFO before Stop or is cancelled before it can enter.
            let _remembered = loading.latest_selection.lock().unwrap();
            if loading.selection_generation.load(Ordering::Acquire) != generation
                || loading.closing.load(Ordering::Acquire)
                || download_cancel.load(Ordering::Acquire)
            {
                return;
            }
            loading.list_waveforms.note_audition_start();
            loading.audio.enqueue_with_ack(AudioCommand::Load(path))
        };
        match completion.and_then(|completion| completion.wait(Duration::from_secs(4))) {
            Ok(_) => {
                // Hold the remembered-request lock through dispatch: recovery cannot
                // observe a consumed Play before it has actually entered the queue.
                let mut remembered = loading.latest_selection.lock().unwrap();
                if loading.selection_generation.load(Ordering::Acquire) != generation
                    || loading.closing.load(Ordering::Acquire)
                {
                    return;
                }
                loading.clear_audio_reload(displayed.id);
                if remembered
                    .as_ref()
                    .is_some_and(|job| job.generation == generation)
                {
                    *remembered = None;
                }
                // Stop pressed after the first response (while loading) wins.
                if play && !download_cancel.load(Ordering::Acquire) {
                    loading.command(AudioCommand::Play);
                }
                drop(remembered);
                let confirmed = loading.clone();
                post(loading.ui.clone(), move |ui| {
                    if confirmed.selection_generation.load(Ordering::Acquire) == generation {
                        ui.set_audio_ready(true);
                    }
                });
            }
            Err(error) => {
                // A load interrupted by Stop is not a failure worth reporting.
                if !download_cancel.load(Ordering::Acquire) {
                    loading.selection_request_completed(generation);
                    loading.status(error.to_string());
                }
            }
        }
    }

    fn mark_audio_reload(&self, id: i64) {
        *self.audio_reload.lock().unwrap() = Some(id);
    }

    fn clear_audio_reload(&self, id: i64) {
        let mut pending = self.audio_reload.lock().unwrap();
        if *pending == Some(id) {
            *pending = None;
        }
    }

    /// The selected record to reload on Play, see [`Self::audio_reload`].
    fn audio_reload_target(&self) -> Option<i64> {
        let selected = self.selected.lock().unwrap().as_ref().map(|file| file.id);
        reload_target(&mut self.audio_reload.lock().unwrap(), selected)
    }

    /// Publish the image and its source timeline together; stale partials never win.
    fn publish_waveform(
        self: &Arc<Self>,
        job: &WaveJob,
        data: &audiolib_audio::WaveformData,
        progress: f64,
    ) {
        if job.cancel.load(Ordering::Acquire)
            || self.waveform_generation.load(Ordering::Acquire) != job.generation
        {
            return;
        }
        let job = job.clone();
        let c2 = self.clone();
        let pixels = render_waveform(data, job.width, job.height, job.view);
        let info = format!(
            "{} ch · {:.1} kHz · {}",
            data.channels,
            data.sample_rate as f64 / 1000.0,
            format_time(job.duration)
        );
        let info = if progress < 1.0 {
            format!("{info} · {:.0}%", (progress * 100.0).floor())
        } else {
            info
        };
        post(self.ui.clone(), move |ui| {
            if job.cancel.load(Ordering::Acquire)
                || c2.waveform_generation.load(Ordering::Acquire) != job.generation
            {
                return;
            }
            ui.set_waveform_image(Image::from_rgba8(
                SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&pixels, job.width, job.height),
            ));
            let (start, end) = job.view.range(job.duration);
            ui.set_view_start(start as f32);
            ui.set_view_end(end as f32);
            ui.set_wave_zoom(job.view.zoom as f32);
            ui.set_wave_scroll(job.view.scroll as f32);
            let labels = (0..=10)
                .map(|index| {
                    let time = start + (end - start) * index as f64 / 10.0;
                    if end - start < 10.0 {
                        format!("{}:{:04.1}", (time / 60.0) as u64, time % 60.0).into()
                    } else {
                        format!("{}:{:02}", (time / 60.0) as u64, time as u64 % 60).into()
                    }
                })
                .collect::<Vec<slint::SharedString>>();
            ui.set_ruler_labels(ModelRc::new(VecModel::from(labels)));
            ui.set_waveform_ready(true);
            ui.set_waveform_info(info.into());
        });
    }

    fn invalidate_waveform(&self) {
        self.waveform_generation.fetch_add(1, Ordering::AcqRel);
        self.waveform_cancel
            .lock()
            .unwrap()
            .store(true, Ordering::Release);
        *self.waveform_job.lock().unwrap() = None;
    }

    fn waveform_file(&self, status: &audiolib_audio::PlaybackStatus) -> Option<AudioFile> {
        if self.remote_read_only() {
            let mapped = status
                .file_path
                .as_deref()
                .and_then(|path| self.remote_wave_file(path));
            if let Some(path) = status.file_path.as_deref()
                && mapped.is_none()
                && !remote_mode::is_remote_cache_path(
                    &self.config.lock().unwrap().data_root(),
                    path,
                )
            {
                // IPC may load a local WAV while the library is in server mode. Its
                // playback timeline wins over the inspector's unrelated remote row.
                return playback_wave_file(status);
            }
            if let Some(file) = self.selected.lock().unwrap().as_ref() {
                return (!file.missing).then(|| file.clone());
            }
            return mapped;
        }
        playback_wave_file(status)
    }

    fn queue_waveform(&self, file: &AudioFile) {
        if !self.available() || file.missing {
            return;
        }
        self.waveform_cancel
            .lock()
            .unwrap()
            .store(true, Ordering::Release);
        let cancel = Arc::new(AtomicBool::new(false));
        *self.waveform_cancel.lock().unwrap() = cancel.clone();
        let generation = self.waveform_generation.fetch_add(1, Ordering::AcqRel) + 1;
        let (width, height) = *self.waveform_size.lock().unwrap();
        let (view, changed) = {
            let mut state = self.wave_view.lock().unwrap();
            let changed = state.0 != file.file_path;
            if changed {
                state.0 = file.file_path.clone();
                state.1.zoom = 1.0;
                state.1.scroll = 0.0;
            }
            (state.1, changed)
        };
        let (start, end) = view.range(file.duration);
        let start_frame = (start * file.sample_rate as f64).round() as u64;
        let end_frame = (end * file.sample_rate as f64).round() as u64;
        *self.waveform_job.lock().unwrap() = Some(WaveJob {
            path: PathBuf::from(&file.file_path),
            remote_id: (self.remote_read_only() && file.id > 0).then_some(file.id),
            sample_rate: file.sample_rate,
            frames: end_frame.saturating_sub(start_frame),
            start_frame,
            end_frame,
            duration: file.duration,
            view,
            generation,
            cancel,
            width,
            height,
        });
        if changed {
            post(self.ui.clone(), |ui| {
                ui.set_waveform_ready(false);
                ui.set_waveform_image(Image::default());
            });
        }
        self.waveform_wake.notify_one();
    }

    fn save_metadata(self: &Arc<Self>, ui: &AppWindow) {
        if !self.available() {
            return;
        }
        if let Err(error) = self.require_metadata_writable() {
            ui.set_status(error.into());
            return;
        }
        match self.enqueue_current_metadata(ui) {
            Ok(_) => ui.set_status(
                if self.chinese.load(Ordering::Acquire) {
                    "正在保存 Metadata…"
                } else {
                    "Saving Metadata…"
                }
                .into(),
            ),
            Err(error) => ui.set_status(error.into()),
        }
    }

    fn capture_metadata(&self, ui: &AppWindow) {
        // Server mode records drafts too; the Metadata worker sends them to the server.
        if self.available() && ui.get_metadata_dirty() {
            let selected = self.selected.lock().unwrap().clone();
            let mut state = self.metadata.lock().unwrap();
            let id = ui.get_selected_id().parse::<i64>().ok();
            let new_draft = id.is_some_and(|id| !state.drafts.contains_key(&id));
            capture_editor_draft(&mut state, ui);
            if new_draft
                && let Some(id) = id
                && let Some(draft) = state.drafts.get_mut(&id)
                && draft.baseline.is_none()
            {
                // With no existing draft, the selected owned file is confirmed
                // provider/save output; editor changes have not been applied to it.
                draft.baseline = selected.filter(|file| file.id == id).map(Box::new);
            }
        }
    }

    /// Restore only the displayed draft, after previously requested saves settle.
    /// A newer edit or selection must never be overwritten by this asynchronous read.
    fn restore_metadata(self: &Arc<Self>) {
        if !self.available() {
            return;
        }
        let Some(ui) = self.ui.upgrade() else { return };
        let Ok(id) = ui.get_selected_id().parse::<i64>() else {
            return;
        };
        self.capture_metadata(&ui);
        let (revision, sequence) = {
            let state = self.metadata.lock().unwrap();
            (
                state.drafts.get(&id).map(|draft| draft.revision),
                state.sequence,
            )
        };
        let provider = self.provider();
        let remote = self.remote_read_only();
        let generation = self.library_generation.load(Ordering::Acquire);
        let c = self.clone();
        std::thread::spawn(move || {
            if c.wait_metadata(sequence, false).is_err() {
                return;
            }
            let fetched = provider.get_audio(id).map_err(|error| error.to_string());
            let c2 = c.clone();
            post(c.ui.clone(), move |ui| {
                if !c2.available()
                    || c2.library_generation.load(Ordering::Acquire) != generation
                    || ui.get_selected_id().as_str() != id.to_string()
                {
                    return;
                }
                let mut state = c2.metadata.lock().unwrap();
                if state.drafts.get(&id).map(|draft| draft.revision) != revision {
                    return;
                }
                // A server may have confirmed a write while its snapshot still
                // needs resync. Never revert that response to older cached values.
                let confirmed = remote
                    .then(|| {
                        state
                            .saved
                            .get(&id)
                            .map(|(_, file)| file.clone())
                            .or_else(|| {
                                state
                                    .drafts
                                    .get(&id)
                                    .and_then(|draft| draft.baseline.as_deref().cloned())
                            })
                    })
                    .flatten();
                match confirmed.map(Ok).unwrap_or(fetched) {
                    Ok(file) => {
                        state.drafts.remove(&id);
                        state.failures.remove(&id);
                        state.failed_actions.remove(&id);
                        drop(state);
                        *c2.selected.lock().unwrap() = Some(file.clone());
                        present_file(&ui, &file);
                        ui.set_status(
                            if c2.chinese.load(Ordering::Acquire) {
                                "已还原为已保存的 Metadata"
                            } else {
                                "Restored the saved Metadata"
                            }
                            .into(),
                        );
                    }
                    Err(error) => ui.set_status(c2.metadata_failure_message(id, &error).into()),
                }
            });
        });
    }

    fn metadata_failure_message(&self, id: i64, error: &str) -> String {
        if self.chinese.load(Ordering::Acquire) {
            format!(
                "记录 {id} 的 Metadata 保存失败：{error}。草稿已保留，可重试、还原或保存恢复文件后退出。"
            )
        } else {
            format!(
                "Metadata for record {id} could not be saved: {error}. Drafts retained; retry, restore, or save a recovery file and exit."
            )
        }
    }

    fn enqueue_current_metadata(&self, ui: &AppWindow) -> Result<u64, String> {
        self.capture_metadata(ui);
        let mut state = self.metadata.lock().unwrap();
        let sequence = if let Ok(id) = ui.get_selected_id().parse::<i64>() {
            state.enqueue_draft(id)?
        } else {
            state.sequence
        };
        drop(state);
        self.metadata_wake.notify_all();
        Ok(sequence)
    }

    fn wait_metadata(&self, sequence: u64, require_success: bool) -> Result<u64, String> {
        let mut state = self.metadata.lock().unwrap();
        while state.completed < sequence && !self.shutdown.load(Ordering::Acquire) {
            state = self.metadata_wake.wait(state).unwrap();
        }
        if require_success && let Some((id, error)) = state.failure() {
            Err(self.metadata_failure_message(id, &error))
        } else if self.shutdown.load(Ordering::Acquire) {
            Err("Desktop is shutting down".into())
        } else {
            Ok(state.completed)
        }
    }

    /// Keep the window alive until dirty drafts persist and cancelled scanning drains.
    pub fn request_close(self: &Arc<Self>, ui: &AppWindow) {
        self.request_close_mode(ui, false);
    }

    /// Recovery mode requires an explicit user action and never retries rejected writes.
    fn request_close_mode(self: &Arc<Self>, ui: &AppWindow, recover: bool) {
        if self.shutdown.load(Ordering::Acquire) {
            return;
        }
        // The source transition already drained its old-provider drafts before
        // saving the new credentials. A pending network sync can be cancelled
        // immediately; recapturing those drafts here would retarget their writes.
        let pending_source_sync = ui.get_ds_source_syncing()
            && self.source_changing.load(Ordering::Acquire)
            && !self.library_busy.load(Ordering::Acquire);
        if self.library_busy.load(Ordering::Acquire)
            || (self.source_changing.load(Ordering::Acquire) && !pending_source_sync)
        {
            self.close_after_operation.store(true, Ordering::Release);
            return;
        }
        if self.closing.load(Ordering::Acquire) {
            return;
        }
        self.persist_search_task(ui);
        *self.audio_table.lock().unwrap() = table_preferences(ui);
        {
            let mut layout = self.window_layout.lock().unwrap();
            *layout = crate::window_layout::capture(ui, &layout);
        }
        let barrier = if pending_source_sync {
            self.metadata.lock().unwrap().sequence
        } else {
            self.capture_metadata(ui);
            match if recover {
                Ok(self.metadata.lock().unwrap().sequence)
            } else {
                self.enqueue_transition_metadata()
            } {
                Ok(sequence) => sequence,
                Err(error) => {
                    ui.set_status(error.into());
                    ui.set_close_recovery_available(true);
                    return;
                }
            }
        };
        self.close_after_operation.store(false, Ordering::Release);
        *self.close_completion.lock().unwrap() = None;
        {
            let _plays = self.remote_plays.lock().unwrap();
            self.closing.store(true, Ordering::Release);
        }
        self.cancel_pending_task_restore_for_close(ui);
        self.cancel_remote();
        tracing::info!(barrier, "Desktop close requested; draining Metadata");
        self.invalidate_library_reads();
        ui.set_searching(false);
        ui.set_close_recovery_available(false);
        ui.set_closing(true);
        self.metadata_wake.notify_all();
        self.schedule_close_poll(Some(Instant::now() + Duration::from_secs(5)));
        let c = self.clone();
        std::thread::spawn(move || {
            let metadata_result = c.wait_workspace_writes().and_then(|_| {
                if recover {
                    c.wait_metadata(barrier, false)
                        .and_then(|_| c.preserve_metadata_recovery(false))
                } else {
                    c.wait_transition_metadata(barrier)
                }
            });
            let mut result = metadata_result.and_then(|_| {
                tracing::info!("Desktop Metadata drained; cancelling scan");
                if let Some(cancel) = c.scan_cancel.lock().unwrap().as_ref() {
                    cancel.cancel();
                }
                // The worker releases its database scan lease before clearing
                // this token. Wait off the event loop before exiting the process.
                while c.scan_cancel.lock().unwrap().is_some() {
                    if c.shutdown.load(Ordering::Acquire) {
                        return Err("Desktop is shutting down".into());
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                tracing::info!("Desktop scan drained");
                Ok(())
            });
            // Admission is closed before this drain. A sent play retains its
            // reply/cache settlement; queued unsent plays cancel themselves.
            if let Err(error) = c.wait_remote_plays() {
                result = Err(error);
            }
            // Closing has cancelled sync even if Metadata saving failed. Drain
            // its owned cache work before either hiding or restoring the window;
            // the cancelled generation cannot publish its final UI state.
            let grace = *c.remote_close_grace.lock().unwrap();
            let remote_deadline = Instant::now() + grace;
            while c.remote_busy.load(Ordering::Acquire) {
                if c.shutdown.load(Ordering::Acquire) {
                    result = Err("Desktop is shutting down".into());
                    break;
                }
                if Instant::now() >= remote_deadline {
                    // Cancelled cache work only touches its own staging file; exiting
                    // without it leaves the active cache intact (stale staging is
                    // pruned on a later start).
                    tracing::warn!(
                        grace_ms = grace.as_millis() as u64,
                        "Desktop remote preparation did not drain; closing anyway"
                    );
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            if !c.remote_busy.load(Ordering::Acquire) {
                tracing::info!("Desktop remote preparation drained");
            }
            tracing::info!(
                success = result.is_ok(),
                "Desktop close preparation complete"
            );
            let mut completion = c.close_completion.lock().unwrap();
            if !c.shutdown.load(Ordering::Acquire) {
                *completion = Some(result);
            }
        });
    }

    /// Called only on the UI thread, and only while a close drain is in progress.
    /// Single-shot timers retain no non-Send timer handle inside the controller.
    fn schedule_close_poll(self: &Arc<Self>, warn_after: Option<Instant>) {
        let c = self.clone();
        slint::Timer::single_shot(Duration::from_millis(50), move || {
            if c.shutdown.load(Ordering::Acquire) || !c.closing.load(Ordering::Acquire) {
                return;
            }
            let Some(ui) = c.ui.upgrade() else { return };
            // Release the mailbox mutex before hiding the window or restoring drafts.
            let result = c.close_completion.lock().unwrap().take();
            let Some(result) = result else {
                let warn_after = if warn_after.is_some_and(|deadline| Instant::now() >= deadline) {
                    tracing::warn!("Desktop close still draining; retaining window and drafts");
                    None
                } else {
                    warn_after
                };
                c.schedule_close_poll(warn_after);
                return;
            };
            c.finish_close(&ui, result);
        });
    }

    fn finish_close(self: &Arc<Self>, ui: &AppWindow, result: Result<(), String>) {
        self.publish_metadata_recovery_notice(ui);
        let result = result.and_then(|_| {
            // KeepWindowShown rejects the native close. After safely draining,
            // explicitly hide the retained window before requesting loop exit.
            tracing::info!("Desktop close completion consumed; hiding window");
            ui.hide().map_err(|error| error.to_string())?;
            tracing::info!("Desktop writes and scan drained; window hidden");
            slint::quit_event_loop().map_err(|error| error.to_string())
        });
        if let Err(error) = result {
            tracing::warn!(%error, "Desktop close failed; retaining drafts");
            self.closing.store(false, Ordering::Release);
            let resumed_task = self.resume_search_task_after_failed_close(ui);
            let scanning = self.scan_cancel.lock().unwrap().is_some();
            if !resumed_task {
                self.resume_source_selection(false);
            }
            self.refresh_library_info();
            if !resumed_task {
                self.resume_search(ui, Some(error.clone()));
            }
            if ui.get_library_visible() {
                let offset = *self.history_offset.lock().unwrap();
                self.request_history_page(offset);
            }
            resume_after_failed_close(ui, &error, scanning);
            ui.set_close_recovery_available(true);
            self.publish_metadata_recovery_notice(ui);
            ui.set_scan_validating(scanning && self.scan_validating.load(Ordering::Acquire));
            if let Err(error) = ui.show() {
                tracing::error!(%error, "Restore desktop after failed close");
            }
        }
    }

    fn scan(self: &Arc<Self>, path: PathBuf) {
        if self.refuse_remote_write() {
            if let Some(ui) = self.ui.upgrade() {
                ui.set_import_visible(false);
            }
            return;
        }
        self.scan_with_validator(path, audiolib_core::validate_scan_root);
    }

    /// Cancelling validation retains the token until filesystem resolution drains.
    fn cancel_scan(&self) {
        if let Some(cancel) = self.scan_cancel.lock().unwrap().as_ref() {
            cancel.cancel();
            if cancel.is_cancelled()
                && let Some(ui) = self.ui.upgrade()
            {
                ui.set_scan_cancelling(true);
                ui.set_scan_paused(false);
                ui.set_scan_summary(
                    if self.chinese.load(Ordering::Acquire) {
                        "正在取消扫描，等待当前操作结束…"
                    } else {
                        "Cancelling scan, waiting for the current operation…"
                    }
                    .into(),
                );
            }
        }
    }

    /// Only request control changes on the event loop; the worker acknowledges its pause.
    fn toggle_scan_pause(&self) {
        if !self.available() || self.scan_validating.load(Ordering::Acquire) {
            return;
        }
        let current = self.scan_cancel.lock().unwrap();
        let Some(control) = current.as_ref() else {
            return;
        };
        if control.is_cancelled() || control.is_finishing() {
            return;
        }
        if control.is_pause_requested() {
            control.resume();
        } else if !control.pause() {
            return;
        }
        if let Some(ui) = self.ui.upgrade() {
            let paused = control.is_pause_requested();
            ui.set_scan_paused(paused);
            ui.set_scan_summary(
                match (paused, self.chinese.load(Ordering::Acquire)) {
                    (true, true) => "正在暂停，等待当前操作结束…",
                    (true, false) => "Pausing, waiting for the current operation…",
                    (false, true) => "正在继续扫描…",
                    (false, false) => "Resuming scan…",
                }
                .into(),
            );
        }
    }

    fn publish_scan_progress(
        &self,
        ui: &AppWindow,
        progress: audiolib_core::ScanProgress,
        full: bool,
    ) {
        *self.scan_presentation.lock().unwrap() =
            Some(ScanPresentation::Progress(progress.clone(), full));
        ui.set_scan_finalizing(progress.finalizing);
        ui.set_scan_current_path(progress.current_path.clone().into());
        if ui.get_scan_cancelling() {
            return;
        }
        let chinese = self.chinese.load(Ordering::Acquire);
        let state = if progress.finalizing {
            if chinese {
                "正在整理扫描结果…"
            } else {
                "Finalizing scan…"
            }
        } else if ui.get_scan_paused() {
            match (progress.paused, chinese) {
                (true, true) => "扫描已暂停",
                (true, false) => "Scan paused",
                (false, true) => "正在暂停…",
                (false, false) => "Pausing…",
            }
        } else if chinese {
            if full {
                "全量扫描中"
            } else {
                "增量扫描中"
            }
        } else if full {
            "Full scan"
        } else {
            "Incremental scan"
        };
        let message = if chinese {
            format!(
                "{state} · 已检查 {} 个音效 · 新增 {} · 更新 {} · 未变化 {} · 问题 {}",
                progress.processed,
                progress.added,
                progress.updated,
                progress.unchanged,
                progress.errors
            )
        } else {
            format!(
                "{state} · {} sounds checked · {} added · {} updated · {} unchanged · {} issues",
                progress.processed,
                progress.added,
                progress.updated,
                progress.unchanged,
                progress.errors
            )
        };
        ui.set_scan_summary(message.clone().into());
        ui.set_status(format!("{message} · {}", progress.current_path).into());
    }

    /// The injectable filesystem resolver makes pending admission cancellation testable.
    fn scan_with_validator(
        self: &Arc<Self>,
        path: PathBuf,
        validate: impl FnOnce(&Path, &[PathBuf]) -> audiolib_core::Result<PathBuf> + Send + 'static,
    ) {
        if !self.available() {
            return;
        }
        let full = self.ui.upgrade().is_some_and(|ui| ui.get_scan_mode() == 1);
        let provider = self.provider();
        let library_generation = self.library_generation.load(Ordering::Acquire);
        let mut current = self.scan_cancel.lock().unwrap();
        if current.is_some() {
            return;
        }
        let cancel = ScanCancellation::default();
        *current = Some(cancel.clone());
        let scan_generation = self.scan_generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.scan_validating.store(true, Ordering::Release);
        *self.scan_presentation.lock().unwrap() = None;
        drop(current);
        if let Some(ui) = self.ui.upgrade() {
            ui.set_scan_error("".into());
            ui.set_scanning(true);
            ui.set_scan_validating(true);
            ui.set_scan_paused(false);
            ui.set_scan_cancelling(false);
            ui.set_scan_finalizing(false);
            ui.set_scan_current_path(path.display().to_string().into());
            ui.set_scan_summary(
                if self.chinese.load(Ordering::Acquire) {
                    "正在检查音效文件夹…"
                } else {
                    "Checking the sound folder…"
                }
                .into(),
            );
        }
        let c = self.clone();
        std::thread::spawn(move || {
            let options = ScanOptions {
                allowed_roots: c.config.lock().unwrap().allowed_scan_roots.clone(),
                incremental: !full,
                ..Default::default()
            };
            let validated = if c.closing.load(Ordering::Acquire)
                || c.shutdown.load(Ordering::Acquire)
                || cancel.is_cancelled()
            {
                Err(audiolib_core::Error::Cancelled)
            } else {
                validate(&path, &options.allowed_roots)
            };
            let mut admitted = false;
            let scanner = ScanService::new(provider);
            let mut last = Instant::now() - Duration::from_secs(1);
            let mut last_phase = (false, false);
            // At most one progress event is queued. A busy event loop receives the
            // newest owned snapshot, not an ever-growing backlog of old paths.
            let mailbox = Arc::new(Mutex::new(None));
            let result = if c.closing.load(Ordering::Acquire)
                || c.shutdown.load(Ordering::Acquire)
                || cancel.is_cancelled()
            {
                Err(audiolib_core::Error::Cancelled)
            } else {
                validated.and_then(|root| {
                    // Start scanning only after the event loop accepts this request.
                    // A cancelled or obsolete queued admission cannot hide a new draft.
                    if !c.admit_scan(library_generation, scan_generation, &cancel)
                        || cancel.is_cancelled()
                    {
                        return Err(audiolib_core::Error::Cancelled);
                    }
                    admitted = true;
                    scanner.scan(&root, &options, &cancel, |progress| {
                        let phase = (progress.paused, progress.finalizing);
                        if c.available()
                            && c.library_generation.load(Ordering::Acquire) == library_generation
                            && (last.elapsed() >= Duration::from_millis(120) || phase != last_phase)
                        {
                            last = Instant::now();
                            last_phase = phase;
                            let already_queued =
                                mailbox.lock().unwrap().replace(progress).is_some();
                            if already_queued {
                                return;
                            }
                            let pending = mailbox.clone();
                            let c2 = c.clone();
                            post(c.ui.clone(), move |ui| {
                                let progress = pending.lock().unwrap().take();
                                if c2.available()
                                    && ui.get_scanning()
                                    && c2.library_generation.load(Ordering::Acquire)
                                        == library_generation
                                    && c2.scan_generation.load(Ordering::Acquire) == scan_generation
                                    && let Some(progress) = progress
                                {
                                    c2.publish_scan_progress(&ui, progress, full);
                                }
                            });
                        }
                    })
                })
            };
            {
                let mut current = c.scan_cancel.lock().unwrap();
                if c.scan_generation.load(Ordering::Acquire) == scan_generation {
                    c.scan_validating.store(false, Ordering::Release);
                    *current = None;
                }
            }
            let c2 = c.clone();
            post(c.ui.clone(), move |ui| {
                c2.publish_scan_completion(
                    &ui,
                    library_generation,
                    scan_generation,
                    full,
                    admitted,
                    result,
                );
            });
        });
    }

    /// Resolve admission on the event loop, with bounded cancellation-aware waiting.
    fn admit_scan(
        self: &Arc<Self>,
        library_generation: u64,
        scan_generation: u64,
        cancel: &ScanCancellation,
    ) -> bool {
        if self.closing.load(Ordering::Acquire)
            || self.shutdown.load(Ordering::Acquire)
            || cancel.is_cancelled()
        {
            return false;
        }
        let receive = self.post_scan_admission(library_generation, scan_generation, cancel);
        let started = Instant::now();
        loop {
            if self.closing.load(Ordering::Acquire)
                || self.shutdown.load(Ordering::Acquire)
                || cancel.is_cancelled()
                || started.elapsed() >= Duration::from_secs(10)
            {
                return false;
            }
            match receive.recv_timeout(Duration::from_millis(20)) {
                Ok(admitted) => return admitted,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return false,
            }
        }
    }

    /// A queued admission must acknowledge rejection without touching a later draft.
    fn post_scan_admission(
        self: &Arc<Self>,
        library_generation: u64,
        scan_generation: u64,
        cancel: &ScanCancellation,
    ) -> std::sync::mpsc::Receiver<bool> {
        let (reply, receive) = std::sync::mpsc::sync_channel(1);
        let c = self.clone();
        let pending_cancel = cancel.clone();
        post(self.ui.clone(), move |ui| {
            let admitted = c.available()
                && !pending_cancel.is_cancelled()
                && c.library_generation.load(Ordering::Acquire) == library_generation
                && c.scan_generation.load(Ordering::Acquire) == scan_generation;
            if reply.send(admitted).is_err() || !admitted {
                return;
            }
            c.scan_validating.store(false, Ordering::Release);
            ui.set_scan_validating(false);
            ui.set_import_visible(false);
            ui.set_scan_summary(
                if c.chinese.load(Ordering::Acquire) {
                    "正在准备扫描音效文件夹…"
                } else {
                    "Preparing to scan the sound folder…"
                }
                .into(),
            );
        });
        receive
    }

    /// Terminal scan state is published even while a native dialog owns UI admission.
    fn publish_scan_completion(
        self: &Arc<Self>,
        ui: &AppWindow,
        library_generation: u64,
        scan_generation: u64,
        full: bool,
        admitted: bool,
        result: audiolib_core::Result<audiolib_core::ScanProgress>,
    ) {
        if self.library_generation.load(Ordering::Acquire) != library_generation
            || self.scan_generation.load(Ordering::Acquire) != scan_generation
        {
            return;
        }
        ui.set_scanning(false);
        ui.set_scan_validating(false);
        ui.set_scan_paused(false);
        ui.set_scan_cancelling(false);
        ui.set_scan_finalizing(false);
        ui.set_scan_current_path("".into());
        match result {
            Ok(progress) => {
                *self.scan_presentation.lock().unwrap() =
                    Some(ScanPresentation::Complete(progress.clone(), full));
                let message =
                    scan_result_summary(&progress, full, self.chinese.load(Ordering::Acquire));
                ui.set_scan_summary(message.clone().into());
                ui.set_status(message.into());
            }
            Err(audiolib_core::Error::Cancelled) if !admitted => {
                *self.scan_presentation.lock().unwrap() = Some(ScanPresentation::Failure(
                    "已取消，未开始扫描音效文件夹。".into(),
                    "Cancelled before scanning the sound folder.".into(),
                ));
                let message = if self.chinese.load(Ordering::Acquire) {
                    "已取消，未开始扫描音效文件夹。"
                } else {
                    "Cancelled before scanning the sound folder."
                };
                ui.set_scan_error("".into());
                ui.set_scan_summary(message.into());
                ui.set_status(message.into());
            }
            Err(error) => {
                *self.scan_presentation.lock().unwrap() = Some(ScanPresentation::Failure(
                    scan_error_message(&error, true),
                    scan_error_message(&error, false),
                ));
                let message = scan_error_message(&error, self.chinese.load(Ordering::Acquire));
                ui.set_scan_error(message.clone().into());
                ui.set_scan_summary(message.clone().into());
                ui.set_status(message.into());
            }
        }
        if !self.available() || !admitted {
            return;
        }
        // Rescanned files may have changed; an enabled pre-generation starts a pass.
        self.list_waveforms.scan_finished();
        self.request_search(ui, true, true);
        self.reload_tree();
        self.refresh_library_info();
        if ui.get_library_visible() {
            self.load_history(0);
        }
    }

    /// Regenerate owned presentation without changing edits or reissuing writes.
    fn refresh_generated_text(&self, ui: &AppWindow) {
        if ui.get_devices_loading() {
            ui.set_device_info(self.localized("正在查找输出设备…", "Finding output devices…"));
        } else {
            self.describe_device(ui.get_device_index());
        }
        let presentation = self.scan_presentation.lock().unwrap().clone();
        match presentation {
            Some(ScanPresentation::Progress(progress, full)) if ui.get_scanning() => {
                self.publish_scan_progress(ui, progress, full);
            }
            Some(ScanPresentation::Complete(progress, full)) => {
                ui.set_scan_summary(
                    scan_result_summary(&progress, full, self.chinese.load(Ordering::Acquire))
                        .into(),
                );
            }
            Some(ScanPresentation::Failure(chinese, english)) => {
                let message = self.localized(&chinese, &english);
                ui.set_scan_summary(message.clone());
                if !ui.get_scan_error().is_empty() {
                    ui.set_scan_error(message);
                }
            }
            _ => ui.set_scan_summary(if ui.get_scan_validating() {
                self.localized("正在验证扫描文件夹…", "Validating scan folder…")
            } else {
                "".into()
            }),
        }
    }

    /// Reopen the output form from acknowledged engine configuration.
    fn restore_output_form(&self, ui: &AppWindow) {
        let playback = self.playback.lock().unwrap();
        ui.set_backend_index(i32::from(playback.backend == AudioBackend::Asio));
        ui.set_rate_index(i32::from(playback.rate_policy == RatePolicy::Native));
        ui.set_buffer_frames(playback.buffer_frames.to_string().into());
        ui.set_pair_index(playback.output_pair.to_string().into());
        ui.set_source_pair(playback.source_pair.to_string().into());
    }

    fn refresh_devices(self: &Arc<Self>, backend: i32) {
        let generation = self.device_generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.devices.lock().unwrap().clear();
        if let Some(ui) = self.ui.upgrade() {
            ui.set_devices_loading(true);
            ui.set_device_info(
                if self.chinese.load(Ordering::Acquire) {
                    "正在查找输出设备…"
                } else {
                    "Finding output devices…"
                }
                .into(),
            );
        }
        let c = self.clone();
        std::thread::spawn(move || {
            let backend = if backend == 1 {
                AudioBackend::Asio
            } else {
                AudioBackend::Wasapi
            };
            let result = audiolib_audio::list_devices(backend);
            if c.device_generation.load(Ordering::Acquire) != generation {
                return;
            }
            match result {
                Ok(devices) => {
                    let current = c.playback.lock().unwrap().device_id.clone();
                    let index = current
                        .as_ref()
                        .and_then(|id| devices.iter().position(|device| &device.id == id))
                        .or_else(|| devices.iter().position(|device| device.is_default))
                        .unwrap_or(0);
                    let names = if devices.is_empty() {
                        vec![
                            if c.chinese.load(Ordering::Acquire) {
                                "没有可用设备"
                            } else {
                                "No available devices"
                            }
                            .to_string(),
                        ]
                    } else {
                        devices.iter().map(|device| device.name.clone()).collect()
                    };
                    *c.devices.lock().unwrap() = devices;
                    let c2 = c.clone();
                    post(c.ui.clone(), move |ui| {
                        if c2.device_generation.load(Ordering::Acquire) != generation {
                            return;
                        }
                        ui.set_devices_loading(false);
                        ui.set_device_names(ModelRc::new(VecModel::from(
                            names
                                .into_iter()
                                .map(Into::into)
                                .collect::<Vec<slint::SharedString>>(),
                        )));
                        ui.set_device_index(index as i32);
                        c2.describe_device(index as i32);
                    });
                }
                Err(error) => {
                    *c.devices.lock().unwrap() = Vec::new();
                    let c2 = c.clone();
                    post(c.ui.clone(), move |ui| {
                        if c2.device_generation.load(Ordering::Acquire) != generation {
                            return;
                        }
                        ui.set_devices_loading(false);
                        ui.set_device_names(ModelRc::new(VecModel::from(vec![
                            c2.localized("不可用", "Unavailable"),
                        ])));
                        ui.set_device_index(0);
                        ui.set_device_info(error.to_string().into());
                    });
                }
            }
        });
    }

    fn describe_device(&self, index: i32) {
        let chinese = self.chinese.load(Ordering::Acquire);
        let pair = self
            .ui
            .upgrade()
            .and_then(|ui| ui.get_pair_index().parse::<usize>().ok())
            .unwrap_or(0);
        let info = self
            .devices
            .lock()
            .unwrap()
            .get(index as usize)
            .map(|device| {
                let names=device.output_channel_names.iter().skip(pair*2).take(2).cloned().collect::<Vec<_>>().join(" / ");
                let template_values = (
                    device.max_output_channels,
                    device.default_sample_rate as f64 / 1000.0,
                    if device.is_default {
                        if chinese { " · 系统默认" } else { " · system default" }
                    } else {
                        ""
                    },
                    device
                        .buffer_min
                        .map_or(if chinese { "驱动" } else { "driver" }.into(), |value| value.to_string()),
                    device
                        .buffer_max
                        .map_or(if chinese { "默认" } else { "default" }.into(), |value| value.to_string()),
                    device
                        .buffer_preferred
                        .map_or(if chinese { "驱动默认" } else { "driver default" }.into(), |value| value.to_string()),
                    if names.is_empty(){String::new()}else{format!("\n{names}")}
                );
                let (channels, rate, default, min, max, preferred, names) = template_values;
                if chinese {
                    format!("{channels} 个输出声道 · 默认 {rate:.1} kHz{default} · 缓冲 {min}–{max} 帧 · 首选 {preferred}{names}")
                } else {
                    format!("{channels} output channels · default {rate:.1} kHz{default} · buffer {min}–{max} frames · preferred {preferred}{names}")
                }
            })
            .unwrap_or_else(|| self.localized("此驱动没有可用的输出设备", "No output devices available for this driver").to_string());
        if let Some(ui) = self.ui.upgrade() {
            ui.set_device_info(info.into());
        }
    }

    fn apply_output(self: &Arc<Self>, ui: &AppWindow, open_panel: bool) {
        if ui.get_output_applying() || ui.get_devices_loading() {
            return;
        }
        let frames = match ui.get_buffer_frames().trim().parse::<u32>() {
            Ok(value) if value <= 65_536 => value,
            _ => {
                ui.set_device_info(self.localized(
                    "缓冲必须是 0（驱动默认）或 1–65536 帧",
                    "Buffer must be 0 (driver default) or 1–65536 frames",
                ));
                return;
            }
        };
        let pair = match ui.get_pair_index().trim().parse::<u16>() {
            Ok(value) if value < 128 => value,
            _ => {
                ui.set_device_info(
                    self.localized("监听声道对必须是 0–127", "Listening pair must be 0–127"),
                );
                return;
            }
        };
        let source_pair = match ui.get_source_pair().trim().parse::<u16>() {
            Ok(value) if value < 128 => value,
            _ => {
                ui.set_device_info(
                    self.localized("源声道对必须是 0–127", "Source pair must be 0–127"),
                );
                return;
            }
        };
        let backend = if ui.get_backend_index() == 1 {
            AudioBackend::Asio
        } else {
            AudioBackend::Wasapi
        };
        let devices = self.devices.lock().unwrap();
        let device = devices.get(ui.get_device_index() as usize);
        if device.is_none() {
            ui.set_device_info(
                self.localized("请选择可用的输出设备", "Choose an available output device"),
            );
            return;
        }
        let device = device.unwrap();
        if device.backend != backend {
            ui.set_device_info(self.localized(
                "请等待此驱动的设备列表加载完成",
                "Wait for this driver's device list to finish loading",
            ));
            return;
        }
        if frames > 0
            && (device.buffer_min.is_some_and(|min| frames < min)
                || device.buffer_max.is_some_and(|max| frames > max))
        {
            ui.set_device_info(self.localized(
                "所选缓冲超出了设备支持范围",
                "Requested buffer is outside the device range",
            ));
            return;
        }
        if frames > 0 {
            let supported = match device.buffer_granularity {
                Some(-1) => frames.is_power_of_two(),
                Some(step) if step > 0 => device
                    .buffer_min
                    .is_none_or(|min| (frames.saturating_sub(min)).is_multiple_of(step as u32)),
                _ => true,
            };
            if !supported {
                ui.set_device_info(self.localized(
                    "所选缓冲不符合 ASIO 驱动的步长或二次幂规则",
                    "Requested buffer does not match the ASIO driver's step/power-of-two rule",
                ));
                return;
            }
        }
        if pair * 2 >= device.max_output_channels {
            ui.set_device_info(self.localized(
                "监听声道对超出了设备输出声道范围",
                "Listening pair is outside the device output channels",
            ));
            return;
        }
        let playback = PlaybackConfig {
            backend,
            device_id: Some(device.id.clone()),
            buffer_frames: frames,
            output_pair: pair,
            source_pair,
            volume: ui.get_volume(),
            audition: self.audio.status().audition,
            rate_policy: if backend == AudioBackend::Asio && ui.get_rate_index() == 1 {
                RatePolicy::Native
            } else {
                RatePolicy::DeviceDefault
            },
        };
        drop(devices);
        ui.set_output_applying(true);
        ui.set_device_info(
            if self.chinese.load(Ordering::Acquire) {
                "正在应用输出设置…"
            } else {
                "Applying output settings…"
            }
            .into(),
        );
        let c = self.clone();
        std::thread::spawn(move || {
            let result = c.configure_output(playback.clone());
            if let Err(error) = result {
                post(c.ui.clone(), move |ui| {
                    ui.set_output_applying(false);
                    ui.set_device_info(error.into());
                });
                return;
            }
            if c.shutdown.load(Ordering::Acquire) {
                return;
            }
            let playback = c.playback.lock().unwrap().clone();
            let mut config = c.config.lock().unwrap();
            config.playback = serde_json::to_value(playback).unwrap_or_default();
            let saved = config.save();
            drop(config);
            if let Err(error) = saved {
                c.status(error.to_string());
            }
            post(c.ui.clone(), move |ui| {
                ui.set_output_applying(false);
                ui.set_settings_visible(false);
            });
            if open_panel {
                c.command(AudioCommand::ControlPanel);
            }
        });
    }

    /// Re-evaluate a server audition when its source pair changes. Preparing a
    /// different representation never stops/cancels the currently loaded source.
    /// The guarded engine command consumes the then-current cursor and state.
    fn configure_output(self: &Arc<Self>, playback: PlaybackConfig) -> Result<(), String> {
        let _prepare = self.remote_audio_prepare.lock().unwrap();
        let status = self.audio.status();
        let file = status.file_path.as_deref().and_then(|path| {
            (self.remote_read_only()
                && self.playback.lock().unwrap().source_pair != playback.source_pair)
                .then(|| self.remote_wave_file(path))
                .flatten()
        });
        let Some(file) = file else {
            self.audio
                .send_and_wait(
                    AudioCommand::Configure(playback.clone()),
                    Duration::from_secs(4),
                )
                .map_err(|error| error.to_string())?;
            *self.playback.lock().unwrap() = playback;
            return Ok(());
        };
        let generation = self.selection_generation.load(Ordering::Acquire);
        let cancel = Arc::new(AtomicBool::new(false));
        std::mem::replace(
            &mut *self.remote_audio_cancel.lock().unwrap(),
            cancel.clone(),
        )
        .store(true, Ordering::Release);
        let retained = self.remote_stream.lock().unwrap().clone();
        *self.remote_retained_stream.lock().unwrap() = retained.clone();
        let path = match self.playable_path_for_pair(
            &file,
            &cancel,
            generation,
            playback.source_pair,
            true,
        ) {
            Ok(path) => path,
            Err(error) => {
                *self.remote_retained_stream.lock().unwrap() = None;
                return Err(error);
            }
        };
        let completion = {
            let _admission = self.latest_selection.lock().unwrap();
            if self.selection_generation.load(Ordering::Acquire) != generation
                || cancel.load(Ordering::Acquire)
                || self.closing.load(Ordering::Acquire)
                || self.shutdown.load(Ordering::Acquire)
            {
                self.restore_prepared_stream(&path, retained, false);
                return Err("The audition changed while output settings were prepared".into());
            }
            self.audio.enqueue_with_ack(AudioCommand::ConfigureSource {
                expected_generation: status.generation,
                expected_path: status.file_path.unwrap(),
                path: path.clone(),
                config: playback.clone(),
            })
        };
        let result = completion
            .and_then(|completion| completion.wait_until_completed())
            .map_err(|error| error.to_string());
        self.restore_prepared_stream(&path, retained, result.is_ok());
        if let Ok(status) = &result {
            let mut playback = playback;
            playback.audition = status.audition;
            *self.playback.lock().unwrap() = playback;
        }
        result.map(|_| ())
    }

    /// Commit source registration only after engine acknowledgment; on failure
    /// restore the old stream unless Stop/shutdown already removed the registration.
    fn restore_prepared_stream(
        &self,
        path: &Path,
        retained: Option<remote_mode::RemotePlayback>,
        committed: bool,
    ) {
        *self.remote_retained_stream.lock().unwrap() = None;
        if retained.as_ref().is_some_and(|old| old.path == path) {
            // New registrations always have distinct paths, even when server and
            // WebDAV both use the raw route. Equality therefore means reuse.
            return;
        }
        let mut current = self.remote_stream.lock().unwrap();
        if committed {
            if let Some(old) = retained {
                // Restarting an incomplete source can retain its cache entry.
                // Its old registration can go, but cancelling the shared entry
                // would also cancel the acknowledged replacement.
                if !current
                    .as_ref()
                    .is_some_and(|new| old.stream.shares_buffer_with(&new.stream))
                {
                    old.stream.cancel();
                }
                audiolib_audio::unregister_stream_source(&old.path);
            }
        } else if current.as_ref().is_some_and(|new| new.path == path) {
            let restoring = self.available();
            if let Some(new) = current.take() {
                if !restoring
                    || !retained
                        .as_ref()
                        .is_some_and(|old| old.stream.shares_buffer_with(&new.stream))
                {
                    new.stream.cancel();
                }
                audiolib_audio::unregister_stream_source(&new.path);
            }
            if restoring {
                *current = retained;
            } else if let Some(old) = retained {
                old.stream.cancel();
                audiolib_audio::unregister_stream_source(&old.path);
            }
        }
    }

    /// The result table shows rows `first .. first + count`: ask for their list
    /// Waveforms, then one screen below and above. Cheap; runs on the event loop.
    fn request_list_waveforms(&self, first: i32, count: i32) {
        let ids: Vec<i64> = self
            .records
            .lock()
            .unwrap()
            .iter()
            .map(|file| file.id)
            .collect();
        let first = usize::try_from(first).unwrap_or(0);
        let count = usize::try_from(count).unwrap_or(0).max(1);
        self.list_waveforms
            .set_window(window_ids(&ids, first, count), count);
    }

    /// Keep already known list Waveforms on rows being (re)published.
    fn decorate_list_waveforms(&self, mut rows: Vec<AudioRow>) -> Vec<AudioRow> {
        let scale = self
            .ui
            .upgrade()
            .map_or(1.0, |ui| ui.window().scale_factor());
        for row in &mut rows {
            if let Ok(id) = row.id.parse::<i64>()
                && let Some(cell) = self.list_waveforms.cached(id)
            {
                apply_list_waveform(row, &cell, scale);
            }
        }
        rows
    }

    fn command(&self, command: AudioCommand) {
        if matches!(
            command,
            AudioCommand::Play | AudioCommand::SeekAndPlay(_) | AudioCommand::Seek(_)
        ) {
            self.list_waveforms.note_audition_start();
        }
        if let Err(error) = self.audio.send(command) {
            self.status(error.to_string());
        }
    }

    /// Cancel pending auditions and enqueue Stop atomically with selection Load/Play.
    /// No audio acknowledgment or network I/O is awaited while holding the lock.
    fn enqueue_stop(&self) -> audiolib_core::Result<audiolib_audio::CommandCompletion> {
        let mut remembered = self.latest_selection.lock().unwrap();
        self.selection_generation.fetch_add(1, Ordering::AcqRel);
        *remembered = None;
        *self.selection_job.lock().unwrap() = None;
        *self.selection_audio_job.lock().unwrap() = None;
        self.remote_audio_cancel
            .lock()
            .unwrap()
            .store(true, Ordering::Release);
        // Interrupt decoder reads blocked on an incomplete server stream too.
        self.interrupt_remote_audio();
        let completion = self.audio.enqueue_with_ack(AudioCommand::Stop);
        drop(remembered);
        self.metadata_wake.notify_all();
        completion
    }
    fn status(&self, message: String) {
        post(self.ui.clone(), move |ui| ui.set_status(message.into()));
    }

    fn reveal(self: &Arc<Self>) {
        if self.refuse_remote_write() {
            return;
        }
        let path = self
            .selected
            .lock()
            .unwrap()
            .as_ref()
            .map(|file| file.file_path.clone());
        if let Some(path) = path {
            self.open_explorer(path, true);
        }
    }

    fn open_explorer(self: &Arc<Self>, path: String, select_file: bool) {
        let c = self.clone();
        std::thread::spawn(move || {
            #[cfg(windows)]
            if let Err(error) = std::process::Command::new("explorer.exe")
                .arg(if select_file {
                    format!("/select,{path}")
                } else {
                    path
                })
                .spawn()
            {
                c.status(error.to_string());
            }
            #[cfg(not(windows))]
            {
                let _ = (path, select_file);
                c.status("Reveal in Explorer requires Windows".into());
            }
        });
    }

    fn file_action(self: &Arc<Self>, id: i64, action: i32) {
        if !self.available() {
            return;
        }
        if self.closing.load(Ordering::Acquire) || self.library_busy.load(Ordering::Acquire) {
            return;
        }
        let Some(ui) = self.ui.upgrade() else { return };
        if matches!(action, 0 | 1) {
            let index = self
                .records
                .lock()
                .unwrap()
                .iter()
                .position(|file| file.id == id);
            ui.set_selected_row(index.map_or(-1, |index| index as i32));
            self.select_id(id, action == 0);
            return;
        }
        // Annotation quick actions go to the server in server mode. Removing a record
        // does too when the server lets this PC manage; repair and reveal stay local-only.
        if action == 21 && self.remote_read_only() {
            if self.refuse_server_removal() {
                return;
            }
        } else if quick_metadata_patch(action, false).is_none() && self.refuse_remote_write() {
            return;
        }
        if quick_metadata_patch(action, false).is_some()
            && let Err(error) = self.require_metadata_writable()
        {
            ui.set_status(error.into());
            return;
        }
        // Match the inspector's favorite button: update its draft without
        // implicitly submitting unrelated tags and notes. Selection autosaves it.
        if ui.get_selected_id().as_str() == id.to_string()
            && let Some(patch) = quick_metadata_patch(action, ui.get_edit_favorite())
        {
            if let Some(favorite) = patch.favorite {
                ui.set_edit_favorite(favorite);
            }
            ui.set_metadata_dirty(true);
            self.capture_metadata(&ui);
            return;
        }
        if action == 20 {
            self.choose_file(false, crate::dialogs::FileKind::Wav, move |path| {
                LibraryOperation::Relink(id, path)
            });
        } else if action == 21 {
            self.propose_operation(LibraryOperation::RemoveAudio(vec![id]));
        } else if action == 2 {
            let provider = self.provider();
            let c = self.clone();
            std::thread::spawn(move || match provider.get_audio(id) {
                Ok(file) => c.open_explorer(file.file_path, true),
                Err(error) => c.status(error.to_string()),
            });
        } else if quick_metadata_patch(action, false).is_some() {
            self.metadata
                .lock()
                .unwrap()
                .enqueue(id, MetadataWrite::QuickAction(action));
            self.metadata_wake.notify_all();
        }
    }

    /// Drag the row (or every checked row when the dragged one is checked) to another
    /// program as real files (CF_HDROP, copy only). Local mode offers the library files
    /// directly. Server mode first makes complete copies in the download cache
    /// (`<data dir>/cache`), with progress and Cancel in the status bar. The native drag
    /// only starts while the mouse button is still held; otherwise the prepared files are
    /// cached and the next drag starts at once.
    fn drag_out(self: &Arc<Self>, id: i64) {
        const MAX_DRAG_FILES: usize = 500;
        if !self.available() || self.drag_busy.swap(true, Ordering::AcqRel) {
            return;
        }
        let chinese = self.chinese.load(Ordering::Acquire);
        let ids: Vec<i64> = {
            let checked = self.multi_selection.lock().unwrap();
            if checked.contains(&id) {
                let mut ids: Vec<i64> = checked.iter().copied().collect();
                ids.sort_unstable();
                ids
            } else {
                vec![id]
            }
        };
        if ids.len() > MAX_DRAG_FILES {
            self.drag_busy.store(false, Ordering::Release);
            self.status(if chinese {
                format!("一次最多拖出 {MAX_DRAG_FILES} 个文件")
            } else {
                format!("Drag at most {MAX_DRAG_FILES} files at once")
            });
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        *self.drag_cancel.lock().unwrap() = cancel.clone();
        let provider = self.provider();
        let remote = self.remote_read_only();
        let source = if remote {
            match self.remote_drag_source() {
                Ok(source) => Some(source),
                Err(error) => {
                    self.drag_busy.store(false, Ordering::Release);
                    self.status(error);
                    return;
                }
            }
        } else {
            None
        };
        let source_generation = self.source_generation.load(Ordering::Acquire);
        let library_generation = self.library_generation.load(Ordering::Acquire);
        let c = self.clone();
        std::thread::spawn(move || {
            let result = (|| -> Result<Vec<PathBuf>, String> {
                let mut files = Vec::with_capacity(ids.len());
                for id in &ids {
                    if !c.drag_request_current(source_generation, library_generation, &cancel) {
                        return Err(c
                            .localized(
                                "数据源已更改，已取消拖出",
                                "Drag cancelled because the data source changed",
                            )
                            .to_string());
                    }
                    let file = provider.get_audio(*id).map_err(|error| error.to_string())?;
                    if !file.missing {
                        files.push(file);
                    }
                }
                if files.is_empty() {
                    return Err(if chinese {
                        "所选记录的文件缺失，无法拖出".into()
                    } else {
                        "The selected records' files are missing; nothing to drag".into()
                    });
                }
                if !c.drag_request_current(source_generation, library_generation, &cancel) {
                    return Err(c
                        .localized(
                            "数据源已更改，已取消拖出",
                            "Drag cancelled because the data source changed",
                        )
                        .to_string());
                }
                let Some(source) = source else {
                    return Ok(files
                        .into_iter()
                        .map(|file| PathBuf::from(file.file_path))
                        .filter(|path| path.is_file())
                        .collect());
                };
                let c2 = c.clone();
                let pending_cancel = cancel.clone();
                post(c.ui.clone(), move |ui| {
                    if c2.drag_request_current(
                        source_generation,
                        library_generation,
                        &pending_cancel,
                    ) {
                        ui.set_drag_progress("".into());
                        ui.set_drag_preparing(true);
                    }
                });
                let names: Vec<String> = files.iter().map(|file| file.filename.clone()).collect();
                let mut last = Instant::now() - Duration::from_secs(1);
                c.prepare_remote_drag(
                    &source,
                    &files,
                    &mut |progress| {
                        if !c.drag_request_current(source_generation, library_generation, &cancel) {
                            return;
                        }
                        let done = progress.total == Some(progress.received);
                        if last.elapsed() < Duration::from_millis(100) && !done {
                            return;
                        }
                        last = Instant::now();
                        let percent = progress
                            .total
                            .filter(|total| *total > 0)
                            .map(|total| progress.received * 100 / total)
                            .unwrap_or(0);
                        let name = names.get(progress.index).cloned().unwrap_or_default();
                        // A sleeping NAS disk: same hint as playback; Cancel stays usable.
                        if let Some(waited) = progress.waiting {
                            if let Some(hint) = remote_mode::waking_hint(&name, waited, chinese) {
                                let text =
                                    format!("{}/{} · {hint}", progress.index + 1, progress.count);
                                let c2 = c.clone();
                                let pending_cancel = cancel.clone();
                                post(c.ui.clone(), move |ui| {
                                    if c2.drag_request_current(
                                        source_generation,
                                        library_generation,
                                        &pending_cancel,
                                    ) {
                                        ui.set_drag_progress(text.into());
                                    }
                                });
                            }
                            return;
                        }
                        let text = if chinese {
                            format!(
                                "正在准备拖出 {}/{}：{name} {percent}%",
                                progress.index + 1,
                                progress.count
                            )
                        } else {
                            format!(
                                "Preparing drag {}/{}: {name} {percent}%",
                                progress.index + 1,
                                progress.count
                            )
                        };
                        let c2 = c.clone();
                        let pending_cancel = cancel.clone();
                        post(c.ui.clone(), move |ui| {
                            if c2.drag_request_current(
                                source_generation,
                                library_generation,
                                &pending_cancel,
                            ) {
                                ui.set_drag_progress(text.into());
                            }
                        });
                    },
                    &cancel,
                )
            })();
            let c2 = c.clone();
            post(c.ui.clone(), move |ui| {
                ui.set_drag_preparing(false);
                if !c2.drag_request_current(source_generation, library_generation, &cancel) {
                    c2.drag_busy.store(false, Ordering::Release);
                    if c2.available()
                        && c2.source_generation.load(Ordering::Acquire) == source_generation
                        && c2.library_generation.load(Ordering::Acquire) == library_generation
                    {
                        ui.set_status(
                            if chinese {
                                "已取消拖出"
                            } else {
                                "Drag cancelled"
                            }
                            .into(),
                        );
                    }
                    return;
                }
                match result {
                    Ok(paths) if paths.is_empty() => ui.set_status(
                        if chinese {
                            "没有可拖出的本地文件"
                        } else {
                            "No local files to drag"
                        }
                        .into(),
                    ),
                    Ok(paths) if audiolib_windows::primary_button_down() => {
                        match audiolib_windows::drag_files(&paths) {
                            Ok(audiolib_windows::DragOutcome::Copied) => ui.set_status(
                                if chinese {
                                    format!("已拖出 {} 个文件（复制）", paths.len())
                                } else {
                                    format!("Dragged {} file(s) (copy)", paths.len())
                                }
                                .into(),
                            ),
                            Ok(audiolib_windows::DragOutcome::Cancelled) => {}
                            Err(error) => ui.set_status(error.to_string().into()),
                        }
                    }
                    Ok(paths) => ui.set_status(
                        if chinese {
                            format!("已准备好 {} 个文件，请再次拖动到目标程序", paths.len())
                        } else {
                            format!(
                                "{} file(s) ready; drag again to the target program",
                                paths.len()
                            )
                        }
                        .into(),
                    ),
                    Err(error) => ui.set_status(error.into()),
                }
                c2.drag_busy.store(false, Ordering::Release);
            });
        });
    }

    /// Stop background workers before the desktop event loop is dropped.
    pub fn shutdown(&self) {
        if let Some(ui) = self.ui.upgrade() {
            self.persist_search_task(&ui);
        }
        if let Err(error) = self.wait_workspace_writes() {
            tracing::warn!(%error, "Save search workspace during shutdown");
        }
        self.shutdown.store(true, Ordering::Release);
        {
            let _writes = self.workspace_writes.lock().unwrap();
            self.workspace_wake.notify_all();
        }
        self.list_waveforms.shutdown();
        *self.close_completion.lock().unwrap() = None;
        *self.latest_selection.lock().unwrap() = None;
        // Use the predicate lock when waking an idle preference worker, so shutdown
        // cannot race its transition into a condition-variable wait.
        {
            let _pending = self.single_click_play_pending.lock().unwrap();
            self.single_click_play_wake.notify_all();
        }
        self.cancel_remote();
        self.remote_audio_cancel
            .lock()
            .unwrap()
            .store(true, Ordering::Release);
        self.drag_cancel
            .lock()
            .unwrap()
            .store(true, Ordering::Release);
        self.stop_remote_stream();
        self.waveform_cancel
            .lock()
            .unwrap()
            .store(true, Ordering::Release);
        if let Some(cancel) = self.scan_cancel.lock().unwrap().as_ref() {
            cancel.cancel();
        }
        self.search_cancel
            .lock()
            .unwrap()
            .store(true, Ordering::Release);
        // Every condition-variable notification must hold its predicate mutex:
        // a worker may have just checked shutdown before entering wait(). The
        // mutex makes that transition atomic with this final wakeup.
        {
            let mut pending = self.search_job.lock().unwrap();
            *pending = None;
            self.search_wake.notify_all();
        }
        {
            let mut pending = self.facet_job.lock().unwrap();
            *pending = None;
            self.facet_wake.notify_all();
        }
        *self.search_publication.lock().unwrap() = None;
        {
            let _pending = self.waveform_job.lock().unwrap();
            self.waveform_wake.notify_all();
        }
        {
            let _state = self.metadata.lock().unwrap();
            self.metadata_wake.notify_all();
        }
        {
            let _pending = self.selection_job.lock().unwrap();
            self.selection_wake.notify_all();
        }
        {
            let _pending = self.selection_audio_job.lock().unwrap();
            self.selection_audio_wake.notify_all();
        }
        {
            let _signal = self.poll_signal.lock().unwrap();
            self.poll_wake.notify_all();
        }
        let confirmed = self
            .audio
            .send_and_wait(AudioCommand::Synchronize, Duration::from_secs(4));
        let spectral_peaks = self.spectral_peak_preferences();
        let mut config = self.config.lock().unwrap();
        config.audition_play_on_single_click = self.single_click_play.load(Ordering::Acquire);
        config.pregenerate_list_waveforms = self.pregenerate_list_waveforms.load(Ordering::Acquire);
        config.hold_list_waveform_batches_after_audition =
            self.hold_list_waveform_batches.load(Ordering::Acquire);
        config.ui_scale_percent = self.ui_scale_percent.load(Ordering::Acquire);
        config.theme = self.theme.lock().unwrap().clone();
        config.ui_font_size = self.ui_font_size.load(Ordering::Acquire);
        config.list_font_size = self.list_font_size.load(Ordering::Acquire);
        config.audio_table = self.audio_table.lock().unwrap().clone();
        config.spectral_peaks = spectral_peaks;
        config.max_search_tasks = self.max_search_tasks.load(Ordering::Acquire);
        config.window_layout = self.window_layout.lock().unwrap().clone();
        if let Ok(status) = confirmed {
            self.playback.lock().unwrap().audition = status.audition;
        }
        config.playback =
            serde_json::to_value(self.playback.lock().unwrap().clone()).unwrap_or_default();
        if let Err(error) = config.save() {
            tracing::warn!(%error,"Save desktop preferences");
        }
        let _ = self.audio.send(AudioCommand::Shutdown);
    }

    /// Process the bounded local playback control protocol; never transports PCM.
    pub fn ipc(self: &Arc<Self>, value: serde_json::Value) -> serde_json::Value {
        if self.library_busy.load(Ordering::Acquire) && value["command"] != "status" {
            return audiolib_core::failure("Library operation in progress");
        }
        let command = match value["command"].as_str() {
            Some("status") => {
                let mut reply = audiolib_core::success(serde_json::json!(self.audio.status()));
                let settings = self.config.lock().unwrap().paths.settings.clone();
                reply["metadata"]["settings_path"] =
                    serde_json::json!(settings.canonicalize().unwrap_or(settings));
                return reply;
            }
            Some("load") => match value["path"].as_str() {
                Some(path) => AudioCommand::Load(PathBuf::from(path)),
                None => return audiolib_core::failure("load requires path"),
            },
            Some("play") => {
                let reload = {
                    // Capture before a later Stop can consume this deferred request.
                    let _admission = self.latest_selection.lock().unwrap();
                    let generation = self.selection_generation.load(Ordering::Acquire);
                    self.audio_reload_target().map(|id| (id, generation))
                };
                if let Some((id, generation)) = reload {
                    // The engine still holds an earlier record: load the selection first.
                    let c = self.clone();
                    post(self.ui.clone(), move |_| {
                        c.select_id_guarded(id, true, Some(generation));
                    });
                    return audiolib_core::success(serde_json::json!(self.audio.status()));
                }
                AudioCommand::Play
            }
            Some("pause") => AudioCommand::Pause,
            Some("stop") => {
                return match self
                    .enqueue_stop()
                    .and_then(|completion| completion.wait(Duration::from_secs(4)))
                {
                    Ok(status) => audiolib_core::success(serde_json::json!(status)),
                    Err(error) => audiolib_core::failure(&error.to_string()),
                };
            }
            Some("seek") => match value["seconds"].as_f64() {
                Some(seconds) if seconds.is_finite() && seconds >= 0.0 => {
                    AudioCommand::Seek(seconds)
                }
                _ => return audiolib_core::failure("seek requires nonnegative seconds"),
            },
            Some("audition") => {
                match serde_json::from_value::<AuditionSettings>(value["settings"].clone()) {
                    Ok(settings) => AudioCommand::SetAudition(settings),
                    Err(error) => {
                        return audiolib_core::failure(&format!(
                            "Invalid audition settings: {error}"
                        ));
                    }
                }
            }
            Some("volume") => match value["volume"].as_f64() {
                Some(volume) if volume.is_finite() && (0.0..=1.0).contains(&volume) => {
                    AudioCommand::SetVolume(volume as f32)
                }
                _ => return audiolib_core::failure("volume must be 0..=1"),
            },
            _ => return audiolib_core::failure("Unknown playback command"),
        };
        if matches!(
            command,
            AudioCommand::Load(_) | AudioCommand::Play | AudioCommand::Seek(_)
        ) {
            self.list_waveforms.note_audition_start();
        }
        match self.audio.send_and_wait(command, Duration::from_secs(4)) {
            Ok(status) => {
                if value["command"] == "volume" {
                    self.playback.lock().unwrap().volume = status.volume;
                }
                audiolib_core::success(serde_json::json!(status))
            }
            Err(error) => audiolib_core::failure(&error.to_string()),
        }
    }
}

/// Keep native pointer gestures and the viewport when a refresh retains the same row
/// identities and order. New identities replace delegates, cancelling old gestures.
fn publish_audio_rows(ui: &AppWindow, rows: Vec<AudioRow>) {
    let has_page_rows = !rows.is_empty();
    if ui.get_menu_has_page_rows() != has_page_rows {
        ui.set_menu_has_page_rows(has_page_rows);
    }
    let existing = ui.get_rows();
    if let Some(model) = existing.as_any().downcast_ref::<VecModel<AudioRow>>()
        && model.row_count() == rows.len()
        && rows.iter().enumerate().all(|(index, row)| {
            model
                .row_data(index)
                .is_some_and(|current| current.id == row.id)
        })
    {
        for (index, row) in rows.into_iter().enumerate() {
            if model.row_data(index).as_ref() != Some(&row) {
                model.set_row_data(index, row);
            }
        }
        return;
    }
    ui.set_rows(ModelRc::new(VecModel::from(rows)));
}

/// Retain the selected reload until its Load succeeds, so cancelled Play requests
/// can be retried. A reload for a record that is no longer selected is dropped.
fn reload_target(pending: &mut Option<i64>, selected: Option<i64>) -> Option<i64> {
    match *pending {
        Some(id) if Some(id) == selected => Some(id),
        _ => {
            *pending = None;
            None
        }
    }
}

fn scan_state_label(state: audiolib_core::ScanState, chinese: bool) -> &'static str {
    use audiolib_core::ScanState;
    match (state, chinese) {
        (ScanState::Running, true) => "扫描中",
        (ScanState::Completed, true) => "已完成",
        (ScanState::Cancelled, true) => "已取消",
        (ScanState::Failed, true) => "失败",
        (ScanState::Running, false) => "Running",
        (ScanState::Completed, false) => "Completed",
        (ScanState::Cancelled, false) => "Cancelled",
        (ScanState::Failed, false) => "Failed",
    }
}

fn scan_result_summary(
    progress: &audiolib_core::ScanProgress,
    full: bool,
    chinese: bool,
) -> String {
    let state = scan_state_label(progress.state, chinese);
    let mut summary = if chinese {
        format!(
            "{}{} · 新增 {} · {} {} · 缺失 {} · 未变化 {} · 问题 {}",
            if full { "全量扫描" } else { "增量扫描" },
            state,
            progress.added,
            if full { "刷新" } else { "更新" },
            progress.updated,
            progress.removed,
            progress.unchanged,
            progress.errors
        )
    } else {
        format!(
            "{} {} · {} added · {} {} · {} missing · {} unchanged · {} issues",
            if full {
                "Full scan"
            } else {
                "Incremental scan"
            },
            state,
            progress.added,
            progress.updated,
            if full { "refreshed" } else { "updated" },
            progress.removed,
            progress.unchanged,
            progress.errors
        )
    };
    if full && (progress.errors > 0 || progress.state != audiolib_core::ScanState::Completed) {
        summary.push_str(if chinese {
            " · 本次未标记缺失文件"
        } else {
            " · Missing-file detection skipped"
        });
    }
    summary
}

fn scan_error_message(error: &audiolib_core::Error, chinese: bool) -> String {
    use audiolib_core::Error;
    match error {
        Error::Validation(message) if message == "Scan root must be an existing directory" => {
            if chinese {
                "请选择存在的音效文件夹，或检查粘贴的路径。"
            } else {
                "Choose an existing sound folder, or check the pasted path."
            }
            .into()
        }
        Error::Validation(message) if message == "Directory is outside allowed scan roots" => {
            if chinese {
                "此文件夹不在允许的扫描范围内，请选择已配置的音效目录。"
            } else {
                "This folder is outside the permitted scan locations. Choose a configured sound folder."
            }
            .into()
        }
        Error::Busy(_) => {
            if chinese {
                "音效库正在执行另一项任务，请等待完成后重新扫描。"
            } else {
                "The sound library is running another task. Try scanning again after it finishes."
            }
            .into()
        }
        _ => {
            if chinese {
                format!("扫描未完成：{error}")
            } else {
                format!("The scan could not finish: {error}")
            }
        }
    }
}

/// Mirror the database removal's complete descendant scope, without a UI depth cap.
/// Failing a read aborts before mutation rather than silently retaining deleted IDs.
fn required_folder_subtree(
    provider: &dyn DataProvider,
    id: i64,
) -> audiolib_core::Result<HashSet<i64>> {
    provider.get_folder(id)?;
    let mut seen = HashSet::from([id]);
    let mut pending = vec![id];
    while let Some(parent) = pending.pop() {
        for folder in provider.folders(Some(parent))? {
            if seen.insert(folder.id) {
                pending.push(folder.id);
            }
        }
    }
    Ok(seen)
}

fn metadata_affects_search(request: &SearchRequest) -> bool {
    !request.query.trim().is_empty()
        || matches!(request.order_by, SortOrder::Tags | SortOrder::Modified)
        || request.filters.favorites_only
        || request.filters.tagged_only
        || request.filters.tag_contains.is_some()
        || !request.filters.include_tags.is_empty()
        || !request.filters.exclude_tags.is_empty()
        || request.filters.color.is_some()
}

fn removal_selection_preview(ids: &[i64], visible: &[AudioFile], chinese: bool) -> String {
    let names: HashMap<_, _> = visible
        .iter()
        .map(|file| (file.id, file.filename.as_str()))
        .collect();
    let hidden = ids.iter().filter(|id| !names.contains_key(*id)).count();
    let mut text = if chinese {
        format!("\n\n其中 {hidden} 条不在当前列表中。将移除：")
    } else {
        format!(
            "\n\n{hidden} selected {} outside the current list. Removing:",
            if hidden == 1 {
                "record is"
            } else {
                "records are"
            }
        )
    };
    for id in ids.iter().take(5) {
        text.push_str("\n• ");
        if let Some(name) = names.get(id) {
            text.extend(name.chars().take(80));
            if name.chars().count() > 80 {
                text.push('…');
            }
        } else if chinese {
            text.push_str(&format!("记录 #{id}（不在当前列表中）"));
        } else {
            text.push_str(&format!("Record #{id} (outside the current list)"));
        }
    }
    if ids.len() > 5 {
        text.push_str(&if chinese {
            format!("\n另有 {} 条记录。", ids.len() - 5)
        } else {
            format!("\n… and {} more.", ids.len() - 5)
        });
    }
    text
}

fn removal_summary(removed: u64, server: bool, chinese: bool) -> String {
    if server {
        return if chinese {
            format!(
                "已从服务器音效库移除 {removed} 个音效记录；服务器上的 WAV 文件未改动，重新扫描会以新的 id 加回。"
            )
        } else {
            format!(
                "Removed {removed} sound records from the server library. WAV files on the server are untouched; a rescan adds them back with new ids."
            )
        };
    }
    if chinese {
        format!("已从音效库移除 {removed} 个音效记录，磁盘中的 WAV 文件已保留。")
    } else {
        format!(
            "Removed {removed} sound records from the library. WAV files on disk were retained."
        )
    }
}

fn scan_session_detail(scan: &ScanSession, chinese: bool) -> String {
    let mode = if chinese {
        if scan.scan_type == "full" {
            "全量扫描（重新读取并检查缺失）"
        } else {
            "增量扫描（新增和修改）"
        }
    } else if scan.scan_type == "full" {
        "Full scan (refresh and detect missing files)"
    } else {
        "Incremental scan (new and modified files)"
    };
    let state = scan_state_label(scan.state, chinese);
    let mut detail = if chinese {
        format!(
            "{mode} · {state}\n文件夹：{}\n开始：{} · 结束：{}\n已检查 {} 个音效 · 新增 {} · 刷新 {} · 缺失 {} · 未变化 {} · 问题 {}",
            scan.scan_path,
            scan.start_time,
            scan.end_time.as_deref().unwrap_or("进行中"),
            scan.scanned,
            scan.added,
            scan.updated,
            scan.removed,
            scan.unchanged,
            scan.errors
        )
    } else {
        format!(
            "{mode} · {state}\nFolder: {}\nStarted: {} · Finished: {}\n{} sounds checked · {} added · {} refreshed · {} missing · {} unchanged · {} issues",
            scan.scan_path,
            scan.start_time,
            scan.end_time.as_deref().unwrap_or("In progress"),
            scan.scanned,
            scan.added,
            scan.updated,
            scan.removed,
            scan.unchanged,
            scan.errors
        )
    };
    if scan.scan_type == "full"
        && (scan.errors > 0 || scan.state != audiolib_core::ScanState::Completed)
    {
        detail.push_str(if chinese {
            "\n扫描未完整完成，本次未标记缺失文件。"
        } else {
            "\nThis scan did not finish cleanly; missing-file detection was skipped."
        });
    }
    for issue in &scan.error_details {
        detail.push_str(&format!("\n{}\n{}", issue.path, issue.message));
    }
    if scan.errors_truncated > 0 {
        detail.push_str(&if chinese {
            format!("\n另有 {} 个问题未列出。", scan.errors_truncated)
        } else {
            format!(
                "\n{} additional issues are not listed.",
                scan.errors_truncated
            )
        });
    }
    detail
}

fn annotation_rollback_path(database: &Path) -> PathBuf {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    database
        .parent()
        .unwrap_or(Path::new("."))
        .join("backups")
        .join(format!("annotations-{timestamp}-{}.db", std::process::id()))
}

fn playback_wave_file(status: &audiolib_audio::PlaybackStatus) -> Option<AudioFile> {
    if status.sample_rate == 0 || status.state == PlaybackState::Loading {
        return None;
    }
    status.file_path.as_ref().map(|path| AudioFile {
        file_path: path.to_string_lossy().into_owned(),
        sample_rate: status.sample_rate,
        channels: status.channels,
        duration: status.duration,
        ..Default::default()
    })
}

fn output_summary(status: &audiolib_audio::PlaybackStatus) -> String {
    let output = match status.active_backend {
        Some(AudioBackend::Asio) => "ASIO",
        Some(AudioBackend::Wasapi) => "WASAPI",
        None => "Output idle",
    };
    let pair = |value: Option<u16>| {
        value.map_or_else(
            || "—".into(),
            |value| format!("{}/{}", value as u32 * 2 + 1, value as u32 * 2 + 2),
        )
    };
    let policy = match status.active_rate_policy {
        Some(RatePolicy::Native) => "native",
        Some(RatePolicy::DeviceDefault) => "resampling",
        None => "—",
    };
    format!(
        "{output} · {:.1} kHz · {} frames · src {} → out {} · {policy} · {} xruns",
        status.device_sample_rate as f64 / 1000.0,
        status.actual_buffer_frames,
        pair(status.active_source_pair),
        pair(status.active_output_pair),
        status.underruns
    )
}

// Keep the range anchor by file identity: a refreshed/sorted page must not reuse
// an obsolete row index. Shift replaces the range; Ctrl+Shift extends it.
fn update_row_selection(
    visible_ids: &[i64],
    selected: &mut HashSet<i64>,
    anchor: &mut Option<i64>,
    index: usize,
    toggle: bool,
    range: bool,
) -> Option<i64> {
    let &id = visible_ids.get(index)?;
    if range {
        let start = anchor.and_then(|id| visible_ids.iter().position(|&item| item == id));
        let start = start.unwrap_or_else(|| {
            *anchor = Some(id);
            index
        });
        if !toggle {
            selected.clear();
        }
        selected.extend(&visible_ids[start.min(index)..=start.max(index)]);
    } else {
        if toggle {
            if !selected.remove(&id) {
                selected.insert(id);
            }
        } else {
            selected.clear();
            selected.insert(id);
        }
        *anchor = Some(id);
    }
    Some(id)
}

fn same_path(left: &Path, right: &Path) -> bool {
    let key = |path: &Path| {
        let value = path.to_string_lossy();
        value
            .strip_prefix(r"\\?\UNC\")
            .map(|suffix| format!(r"\\{suffix}"))
            .unwrap_or_else(|| value.strip_prefix(r"\\?\").unwrap_or(&value).to_string())
            .replace('/', "\\")
            .to_lowercase()
    };
    key(left) == key(right)
}

fn metadata_patch(ui: &AppWindow) -> MetadataPatch {
    MetadataPatch {
        tags: Some(ui.get_edit_tags().to_string()),
        // Color is not editable in the inspector. Omit it so a stale or legacy
        // value cannot overwrite another writer's color or reject unrelated edits.
        color: None,
        notes: Some(ui.get_edit_notes().to_string()),
        favorite: Some(ui.get_edit_favorite()),
    }
}

fn capture_editor_draft(state: &mut MetadataState, ui: &AppWindow) {
    if ui.get_metadata_dirty()
        && let Ok(id) = ui.get_selected_id().parse::<i64>()
    {
        state.edit(id, metadata_patch(ui));
    }
}

fn resume_after_failed_close(ui: &AppWindow, error: &str, scanning: bool) {
    ui.set_closing(false);
    // Scan completion publications are skipped during closing. Reconcile the
    // indicator with the worker token so a failed save cannot freeze scan menus.
    ui.set_scanning(scanning);
    // The close worker also drains cancelled remote work on a save failure.
    // Its obsolete/rejected publication cannot restore these indicators.
    ui.set_ds_syncing(false);
    ui.set_ds_sync_pausing(false);
    ui.set_ds_sync_can_pause(false);
    ui.set_ds_progress(0.0);
    ui.set_ds_decompression_info("".into());
    ui.set_ds_decompression_progress(-1.0);
    ui.set_status(error.into());
}

fn apply_metadata_patch(file: &mut AudioFile, patch: &MetadataPatch) {
    if let Some(tags) = &patch.tags {
        file.tags = tags.clone();
    }
    if let Some(notes) = &patch.notes {
        file.notes = notes.clone();
    }
    if let Some(favorite) = patch.favorite {
        file.favorite = favorite;
    }
    if let Some(color) = &patch.color {
        file.color = (!color.is_empty()).then(|| color.clone());
    }
}
fn quick_metadata_patch(action: i32, favorite: bool) -> Option<MetadataPatch> {
    match action {
        5 => Some(MetadataPatch {
            favorite: Some(!favorite),
            ..Default::default()
        }),
        _ => None,
    }
}

fn same_patch(left: &MetadataPatch, right: &MetadataPatch) -> bool {
    left.tags == right.tags
        && left.notes == right.notes
        && left.color == right.color
        && left.favorite == right.favorite
}

fn present_file(ui: &AppWindow, file: &AudioFile) {
    ui.set_selected_id(file.id.to_string().into());
    ui.set_has_selection(true);
    ui.set_selected_missing(file.missing);
    ui.set_selected_name(file.filename.clone().into());
    ui.set_selected_path(file.file_path.clone().into());
    ui.set_selected_format(
        format!(
            "{:.1} kHz / {} bit / {} ch / {}",
            file.sample_rate as f64 / 1000.0,
            file.bit_depth,
            file.channels,
            format_time(file.duration)
        )
        .into(),
    );
    ui.set_selected_description(
        audiolib_core::source_description(&file.bext_description, &file.attributes).into(),
    );
    ui.set_selected_source_tags(
        audiolib_core::source_tags(&file.attributes)
            .join(" · ")
            .into(),
    );
    ui.set_selected_attributes(
        serde_json::to_string_pretty(&file.attributes)
            .unwrap_or_default()
            .into(),
    );
    ui.set_edit_tags(file.tags.clone().into());
    ui.set_edit_notes(file.notes.clone().into());
    ui.set_edit_favorite(file.favorite);
    ui.set_metadata_dirty(false);
}
/// Row Waveform states shown by the table: 0 placeholder, 1 thumbnail, 2 center line
/// (empty file), 3 no Waveform (missing or undecodable), 4 root offline (server mode:
/// a stale thumbnail with the offline tint, or an offline placeholder).
fn apply_list_waveform(row: &mut AudioRow, cell: &ListWaveformCell, scale: f32) {
    match cell {
        ListWaveformCell::Ready(waveform) => match render_list_waveform(waveform, scale) {
            Some(buffer) => {
                row.wave = Image::from_rgba8(buffer);
                row.wave_state = 1;
            }
            None => {
                row.wave = Image::default();
                row.wave_state = 2;
            }
        },
        ListWaveformCell::Unavailable => {
            row.wave = Image::default();
            row.wave_state = 3;
        }
        ListWaveformCell::Offline(stale) => {
            row.wave = stale
                .as_ref()
                .and_then(|waveform| render_list_waveform(waveform, scale))
                .map(Image::from_rgba8)
                .unwrap_or_default();
            row.wave_state = 4;
        }
    }
}

/// Publish worker results into the matching rows of the current model.
fn publish_list_waveforms(ui: &AppWindow, cells: Vec<(i64, ListWaveformCell)>) {
    let rows = ui.get_rows();
    let Some(model) = rows.as_any().downcast_ref::<VecModel<AudioRow>>() else {
        return;
    };
    let scale = ui.window().scale_factor();
    let cells: HashMap<String, ListWaveformCell> = cells
        .into_iter()
        .map(|(id, cell)| (id.to_string(), cell))
        .collect();
    for index in 0..model.row_count() {
        if let Some(mut row) = model.row_data(index)
            && let Some(cell) = cells.get(row.id.as_str())
        {
            apply_list_waveform(&mut row, cell, scale);
            model.set_row_data(index, row);
        }
    }
}

impl ListWaveformHost for Controller {
    fn list_waveform_provider(&self) -> Option<Arc<dyn DataProvider>> {
        if !self.available() {
            return None;
        }
        if self.remote_read_only() {
            // Server mode never reads the snapshot's waveform_cache rows (§9.3): the
            // provider answers from `POST /waveforms` (`ListWaveformSource::Server`, or
            // `Unavailable` when the server has no usable `list_waveforms` capability).
            return self
                .remote_list_waveform_provider()
                .map(|provider| provider as Arc<dyn DataProvider>);
        }
        Some(self.provider() as Arc<dyn DataProvider>)
    }
    fn list_waveform_scanning(&self) -> bool {
        self.scan_cancel.lock().unwrap().is_some()
    }
    fn list_waveform_source_changed(&self, source: ListWaveformSource) {
        post(self.ui.clone(), move |ui| {
            ui.set_list_waveforms_server(source == ListWaveformSource::Server);
        });
    }
    fn deliver_list_waveforms(&self, generation: u64, cells: Vec<(i64, ListWaveformCell)>) {
        let service = self.list_waveforms.clone();
        post(self.ui.clone(), move |ui| {
            if service.generation() == generation {
                publish_list_waveforms(&ui, cells);
            }
        });
    }
}

fn audio_row(file: &AudioFile) -> AudioRow {
    let marker = file
        .color
        .as_deref()
        .and_then(parse_color)
        .unwrap_or(slint::Color::from_rgb_u8(51, 58, 69));
    AudioRow {
        id: file.id.to_string().into(),
        file_path: file.file_path.clone().into(),
        name: file.filename.clone().into(),
        folder: Path::new(&file.file_path)
            .parent()
            .map_or(String::new(), |path| path.to_string_lossy().into_owned())
            .into(),
        duration: format!("{:.2}s", file.duration).into(),
        format: format!(
            "{:.1}k / {}b",
            file.sample_rate as f64 / 1000.0,
            file.bit_depth
        )
        .into(),
        channels: file.channels.to_string().into(),
        tags: audiolib_core::display_tags(&file.tags, &file.attributes).into(),
        favorite: file.favorite,
        missing: file.missing,
        checked: false,
        marker,
        ..Default::default()
    }
}
fn parse_color(value: &str) -> Option<slint::Color> {
    let value = value.strip_prefix('#')?;
    if value.len() != 6 {
        return None;
    }
    let value = u32::from_str_radix(value, 16).ok()?;
    Some(slint::Color::from_rgb_u8(
        (value >> 16) as u8,
        (value >> 8) as u8,
        value as u8,
    ))
}
pub fn format_time(seconds: f64) -> String {
    let ms = (seconds.max(0.0) * 1000.0) as u64;
    format!("{:02}:{:02}.{:03}", ms / 60000, (ms / 1000) % 60, ms % 1000)
}

/// Resolve the pointer against the displayed image, while accumulating pending zoom.
pub(crate) fn zoom_waveform_at_pointer(
    ui: &AppWindow,
    view: &mut WaveView,
    factor: f32,
    fraction: f32,
    duration: f64,
) {
    let start = f64::from(ui.get_view_start());
    let end = f64::from(ui.get_view_end());
    if end <= start {
        return;
    }
    let fraction = f64::from(fraction).clamp(0.0, 1.0);
    let anchor = start + (end - start) * fraction;
    view.zoom_at_anchor(f64::from(factor), anchor, fraction, duration);
}

/// A transport discontinuity invalidates prediction even between two Playing snapshots.
#[derive(Default)]
pub(crate) struct PlaybackProgress {
    identity: Option<(u64, u64)>,
}

impl PlaybackProgress {
    pub(crate) fn observe(&mut self, status: &audiolib_audio::PlaybackStatus) -> bool {
        let identity = (status.generation, status.transport_revision);
        let continuous = self.identity == Some(identity);
        self.identity = Some(identity);
        continuous
    }
}

/// Publish confirmed source time and enable interpolation only within one transport revision.
pub(crate) fn publish_playback_progress(
    ui: &AppWindow,
    status: &audiolib_audio::PlaybackStatus,
    progress_continues: bool,
) {
    let forward_step = status.position as f32 - ui.get_play_position();
    ui.set_playback_clock(format!("{}:{}", status.generation, status.transport_revision).into());
    ui.set_confirmed_playback_rate(status.audition.rate);
    ui.set_playback_smooth(
        progress_continues
            && status.state == PlaybackState::Playing
            && ui.get_playing()
            && (status.duration as f32 - ui.get_play_duration()).abs() < 0.001
            && (0.0..=0.4).contains(&forward_step),
    );
    ui.set_playing(status.state == PlaybackState::Playing);
    ui.set_play_position(status.position as f32);
    ui.set_play_duration(status.duration as f32);
    ui.set_play_time(format_time(status.position).into());
    ui.set_play_end(format_time(status.duration).into());
}

fn publish_audition(ui: &AppWindow, settings: AuditionSettings) {
    ui.set_pitch_text(format!("{:.2}", settings.pitch_semitones).into());
    ui.set_speed_text(format!("{:.2}", settings.rate).into());
    ui.set_audition_mono(settings.mono);
    ui.set_audition_repeat(settings.loop_enabled);
}
fn publish_volume(ui: &AppWindow, volume: f32) {
    let db = db_from_gain(volume);
    ui.set_volume_db(db);
    ui.set_volume_label(volume_label(volume));
}
fn volume_label(volume: f32) -> slint::SharedString {
    if volume <= 0.001 {
        "−∞ dB".into()
    } else {
        format!("{:.1} dB", db_from_gain(volume)).into()
    }
}

/// History follows successful playback sessions, not a polled Playing state.
/// Entries remain pending until their database write succeeds, including Busy retries.
#[derive(Default)]
struct PlaybackHistoryTracker {
    library_generation: Option<u64>,
    minimum_media_generation: u64,
    queued_through: HashMap<u64, u64>,
    pending: VecDeque<PendingPlay>,
}

struct PendingPlay {
    generation: u64,
    play_session_id: u64,
    file_path: PathBuf,
}

impl PlaybackHistoryTracker {
    fn enqueue(&mut self, generation: u64, play_session_id: u64, file_path: &Path) {
        // The fresh cutover snapshot may follow the first event drain. A start
        // from an older loaded source published in that gap still belongs to the
        // previous library and must be rejected on the next drain.
        if generation < self.minimum_media_generation {
            return;
        }
        let queued = self.queued_through.entry(generation).or_default();
        while *queued < play_session_id {
            *queued += 1;
            self.pending.push_back(PendingPlay {
                generation,
                play_session_id: *queued,
                file_path: file_path.to_owned(),
            });
        }
    }

    fn observe(
        &mut self,
        library_generation: u64,
        status: &audiolib_audio::PlaybackStatus,
        events: impl IntoIterator<Item = AudioEvent>,
    ) {
        if self
            .library_generation
            .is_some_and(|old| old != library_generation)
        {
            self.pending.clear();
            self.queued_through.clear();
            self.minimum_media_generation = status.generation;
            // A retained audition source may belong to the previous library. Its
            // existing sessions must never be attributed to a new provider.
            self.queued_through
                .insert(status.generation, status.play_session_id);
            self.library_generation = Some(library_generation);
            return;
        }
        self.library_generation = Some(library_generation);
        for event in events {
            if let AudioEvent::PlaybackStarted {
                generation,
                play_session_id,
                file_path,
            } = event
            {
                self.enqueue(generation, play_session_id, &file_path);
            }
        }
        if let Some(path) = &status.file_path {
            // Session IDs are cumulative, so a bounded event queue or a complete
            // Play/Stop between polls does not lose the current source's starts.
            self.enqueue(status.generation, status.play_session_id, path);
        }
        // The worker snapshots status before draining events. Engine control is
        // serial: all older-generation events are published before that snapshot.
        // Keep future generations observed in events and any retryable old entries.
        self.queued_through.retain(|generation, _| {
            *generation >= status.generation
                || self
                    .pending
                    .iter()
                    .any(|play| play.generation == *generation)
        });
    }

    fn flush(
        &mut self,
        mut record: impl FnMut(&Path) -> audiolib_core::Result<bool>,
    ) -> audiolib_core::Result<()> {
        while let Some(play) = self.pending.front() {
            debug_assert!(
                self.queued_through
                    .get(&play.generation)
                    .is_some_and(|queued| *queued >= play.play_session_id)
            );
            if !record(&play.file_path)? {
                break;
            }
            self.pending.pop_front();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Timers and loop exit work, but every cross-thread UI notification fails.
    /// This reproduces the lost close-completion path without a native user window.
    fn init_close_without_post_delivery() -> Arc<AtomicU64> {
        use i_slint_backend_testing::{TestingBackend, TestingBackendOptions};
        use slint::platform::{EventLoopProxy, Platform, PlatformError, WindowAdapter};
        use std::rc::Rc;

        struct RejectPosts {
            backend: TestingBackend,
            rejected: Arc<AtomicU64>,
        }
        struct RejectPostProxy {
            inner: Box<dyn EventLoopProxy>,
            rejected: Arc<AtomicU64>,
        }
        impl EventLoopProxy for RejectPostProxy {
            fn quit_event_loop(&self) -> Result<(), slint::EventLoopError> {
                self.inner.quit_event_loop()
            }
            fn invoke_from_event_loop(
                &self,
                _event: Box<dyn FnOnce() + Send>,
            ) -> Result<(), slint::EventLoopError> {
                self.rejected.fetch_add(1, Ordering::AcqRel);
                Err(slint::EventLoopError::EventLoopTerminated)
            }
        }
        impl Platform for RejectPosts {
            fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
                self.backend.create_window_adapter()
            }
            fn run_event_loop(&self) -> Result<(), PlatformError> {
                self.backend.run_event_loop()
            }
            fn duration_since_start(&self) -> Duration {
                self.backend.duration_since_start()
            }
            fn new_event_loop_proxy(&self) -> Option<Box<dyn EventLoopProxy>> {
                self.backend.new_event_loop_proxy().map(|inner| {
                    Box::new(RejectPostProxy {
                        inner,
                        rejected: self.rejected.clone(),
                    }) as Box<dyn EventLoopProxy>
                })
            }
        }
        let rejected = Arc::new(AtomicU64::new(0));
        slint::platform::set_platform(Box::new(RejectPosts {
            backend: TestingBackend::new(TestingBackendOptions {
                threading: true,
                ..Default::default()
            }),
            rejected: rejected.clone(),
        }))
        .unwrap();
        assert!(
            slint::invoke_from_event_loop(|| panic!("Rejected UI event was delivered")).is_err()
        );
        rejected
    }

    #[test]
    fn play_retains_the_selected_reload_until_its_load_succeeds() {
        let mut pending = None;
        assert_eq!(reload_target(&mut pending, Some(5)), None);
        pending = Some(5);
        assert_eq!(reload_target(&mut pending, Some(5)), Some(5));
        assert_eq!(pending, Some(5), "an admitted Play is not a confirmed Load");
        assert_eq!(reload_target(&mut pending, Some(5)), Some(5));
        assert_eq!(reload_target(&mut pending, Some(6)), None);
        assert_eq!(pending, None, "a reload for another record is dropped");
        pending = Some(5);
        assert_eq!(reload_target(&mut pending, None), None);
    }
    use audiolib_audio::{WaveformData, WaveformPeak};

    #[test]
    fn missing_selection_rejects_queued_and_late_waveform_partials() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "controller::tests::child_missing_selection_rejects_waveform_partials",
                "--ignored",
                "--test-threads=1",
            ])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "Missing-selection Waveform regression failed: {}\n{stdout}\n{stderr}",
            output.status
        );
    }

    #[test]
    #[ignore = "helper invoked by the missing-selection regression in an isolated process"]
    fn child_missing_selection_rejects_waveform_partials() {
        i_slint_backend_testing::init_integration_test_with_system_time();
        let root = std::env::temp_dir().join(format!(
            "audiolib-desktop-missing-waveform-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let sounds = root.join("sounds");
        std::fs::create_dir_all(&sounds).unwrap();
        let mut wav = Vec::new();
        wav.extend(b"RIFF");
        wav.extend(38u32.to_le_bytes());
        wav.extend(b"WAVEfmt ");
        wav.extend(16u32.to_le_bytes());
        wav.extend(1u16.to_le_bytes());
        wav.extend(1u16.to_le_bytes());
        wav.extend(48000u32.to_le_bytes());
        wav.extend(96000u32.to_le_bytes());
        wav.extend(2u16.to_le_bytes());
        wav.extend(16u16.to_le_bytes());
        wav.extend(b"data");
        wav.extend(2u32.to_le_bytes());
        wav.extend(0i16.to_le_bytes());
        for name in ["present.wav", "missing.wav"] {
            std::fs::write(sounds.join(name), &wav).unwrap();
        }
        let provider = Arc::new(SqliteProvider::open(&root.join("library.db")).unwrap());
        let scan = |options: &ScanOptions| {
            ScanService::new(provider.clone())
                .scan(&sounds, options, &ScanCancellation::default(), |_| {})
                .unwrap();
        };
        scan(&ScanOptions::default());
        let files = provider.search(&SearchRequest::default()).unwrap().results;
        let present = files
            .iter()
            .find(|file| file.filename == "present.wav")
            .unwrap();
        let missing = files
            .iter()
            .find(|file| file.filename == "missing.wav")
            .unwrap();
        let missing_id = missing.id;
        std::fs::rename(sounds.join("missing.wav"), root.join("held.wav")).unwrap();
        scan(&ScanOptions {
            incremental: false,
            ..Default::default()
        });
        assert!(provider.get_audio(missing_id).unwrap().missing);
        let engine = audiolib_audio::AudioEngine::new(PlaybackConfig::default()).unwrap();
        let ui = AppWindow::new().unwrap();
        let controller = Controller::new(
            &ui,
            provider.clone(),
            engine.handle(),
            AppConfig::load_in(root.join("preferences")).unwrap(),
            PlaybackConfig::default(),
        );
        *controller.selected.lock().unwrap() = Some(present.clone());
        present_file(&ui, present);
        let job = WaveJob {
            path: PathBuf::from(&present.file_path),
            remote_id: Some(present.id),
            sample_rate: 48000,
            frames: 1,
            start_frame: 0,
            end_frame: 1,
            duration: present.duration,
            view: WaveView::default(),
            generation: controller.waveform_generation.load(Ordering::Acquire),
            cancel: controller.waveform_cancel.lock().unwrap().clone(),
            width: 32,
            height: 16,
        };
        let data = WaveformData {
            sample_rate: 48000,
            channels: 1,
            frames: 1,
            samples_per_pixel: 1,
            peaks: vec![vec![WaveformPeak {
                min: -0.5,
                max: 0.5,
                ..Default::default()
            }]],
        };
        controller.publish_waveform(&job, &data, 0.5);
        let stage = std::rc::Rc::new(std::cell::Cell::new(0));
        let observed = stage.clone();
        let failure = std::rc::Rc::new(std::cell::RefCell::new(None::<String>));
        let errors = failure.clone();
        let c = controller.clone();
        let window = ui.as_weak();
        let started = Instant::now();
        let timer = slint::Timer::default();
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(15),
            move || {
                let Some(ui) = window.upgrade() else { return };
                let result = (|| -> Result<(), String> {
                    if started.elapsed() > Duration::from_secs(8) {
                        return Err("Missing-selection Waveform flow timed out".into());
                    }
                    match observed.get() {
                        0 if ui.get_waveform_ready() => {
                            // This callback is queued with the old valid generation.
                            c.publish_waveform(&job, &data, 0.75);
                            c.select_id(missing_id, false);
                            if !job.cancel.load(Ordering::Acquire)
                                || c.waveform_generation.load(Ordering::Acquire) == job.generation
                            {
                                return Err(
                                    "Selection did not invalidate the previous Waveform".into()
                                );
                            }
                            observed.set(1);
                        }
                        1 if ui.get_selected_id() == missing_id.to_string() => {
                            if !ui.get_selected_missing() || ui.get_waveform_ready() {
                                return Err(
                                    "Missing selection retained the previous Waveform".into()
                                );
                            }
                            // A range returned after presentation must also be rejected.
                            c.publish_waveform(&job, &data, 1.0);
                            observed.set(2);
                        }
                        2 => {
                            if ui.get_waveform_ready()
                                || ui.get_waveform_image().size().width != 0
                                || !ui.get_selected_missing()
                            {
                                return Err(
                                    "A stale partial overwrote the missing-record display".into()
                                );
                            }
                            observed.set(3);
                            slint::quit_event_loop().unwrap();
                        }
                        _ => {}
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    *errors.borrow_mut() = Some(error);
                    slint::quit_event_loop().ok();
                }
            },
        );
        ui.show().unwrap();
        slint::run_event_loop().unwrap();
        timer.stop();
        controller.shutdown();
        ui.hide().unwrap();
        drop(controller);
        drop(engine);
        assert!(failure.borrow().is_none(), "{:?}", failure.borrow());
        assert_eq!(stage.get(), 3);
        assert_eq!(std::fs::read(root.join("held.wav")).unwrap(), wav);
        drop(provider);
        assert_eq!(root.parent(), Some(std::env::temp_dir().as_path()));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn row_multiselection_toggles_ranges_and_reanchors_after_page_changes() {
        let ids = [10, 20, 30, 40, 50];
        let mut selected = HashSet::new();
        let mut anchor = None;
        for (index, toggle, range, expected) in [
            (1, false, false, vec![20]),
            (3, true, false, vec![20, 40]),
            (3, true, false, vec![20]),
            (0, false, true, vec![10, 20, 30, 40]),
            (2, false, true, vec![30, 40]),
            (4, true, true, vec![30, 40, 50]),
            (0, false, false, vec![10]),
            (0, true, false, vec![]),
            (0, true, false, vec![10]),
        ] {
            assert_eq!(
                update_row_selection(&ids, &mut selected, &mut anchor, index, toggle, range),
                Some(ids[index])
            );
            assert_eq!(selected, expected.into_iter().collect());
        }
        update_row_selection(&[30, 10, 20], &mut selected, &mut anchor, 2, false, true);
        assert_eq!(selected, HashSet::from([10, 20]));
        update_row_selection(&[50, 60], &mut selected, &mut anchor, 1, false, true);
        assert_eq!(selected, HashSet::from([60]));
        assert_eq!(anchor, Some(60));
        assert_eq!(
            update_row_selection(&[], &mut selected, &mut anchor, 0, false, false),
            None
        );
        assert_eq!(selected, HashSet::from([60]));
    }

    #[test]
    fn check_current_page_action_tracks_empty_pages_and_keeps_operation_guards() {
        use std::{cell::RefCell, rc::Rc};

        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().unwrap();
        let requests = Rc::new(RefCell::new(Vec::new()));
        let observed = requests.clone();
        ui.on_check_page_requested(move |checked| observed.borrow_mut().push(checked));
        let rows = || {
            vec![AudioRow {
                id: "42".into(),
                name: "Current page.wav".into(),
                ..Default::default()
            }]
        };

        ui.invoke_check_current_page();
        assert!(requests.borrow().is_empty());
        publish_audio_rows(&ui, rows());
        assert!(ui.get_menu_has_page_rows());
        ui.invoke_check_current_page();
        assert_eq!(&*requests.borrow(), &[true]);

        // Publishing a zero-hit page must update the actual model, while the
        // native menu action safely does nothing instead of retaining old IDs.
        publish_audio_rows(&ui, Vec::new());
        assert!(!ui.get_menu_has_page_rows());
        assert_eq!(ui.get_rows().row_count(), 0);
        ui.invoke_check_current_page();
        assert_eq!(&*requests.borrow(), &[true]);

        publish_audio_rows(&ui, rows());
        ui.set_library_busy(true);
        ui.invoke_check_current_page();
        ui.set_library_busy(false);
        ui.set_closing(true);
        ui.invoke_check_current_page();
        ui.set_closing(false);
        ui.set_settings_visible(true);
        ui.invoke_check_current_page();
        assert_eq!(&*requests.borrow(), &[true]);
        ui.set_settings_visible(false);
        ui.invoke_check_current_page();
        assert_eq!(&*requests.borrow(), &[true, true]);
    }

    #[test]
    fn themed_controls_preserve_pointer_keyboard_and_accessible_actions() {
        use i_slint_backend_testing::{AccessibleRole, ElementHandle, ElementQuery};
        use slint::platform::{Key, PointerEventButton, WindowEvent};
        use std::{cell::Cell, rc::Rc};

        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().unwrap();
        ui.set_audio_ready(true);
        ui.set_has_selection(true);
        ui.show().unwrap();
        let volume = ElementQuery::from_root(&ui)
            .match_descendants()
            .match_accessible_role(AccessibleRole::Slider)
            .find_all()
            .into_iter()
            .find(|item| item.accessible_value_minimum() == Some(-60.0))
            .expect("the standard volume slider stays accessible");
        volume.set_accessible_value("-12");
        assert!((ui.get_volume_db() + 12.0).abs() < 0.01);
        volume.mock_single_click(PointerEventButton::Left);
        assert!((ui.get_volume_db() + 30.0).abs() < 0.2);
        ui.window().dispatch_event(WindowEvent::KeyPressed {
            text: Key::RightArrow.into(),
        });
        ui.window().dispatch_event(WindowEvent::KeyReleased {
            text: Key::RightArrow.into(),
        });
        assert!((ui.get_volume_db() + 29.9).abs() < 0.01);
        assert_eq!(volume_label(gain_from_db(-6.0)).as_str(), "-6.0 dB");
        assert_eq!(volume_label(0.0).as_str(), "−∞ dB");

        let favorite = ElementHandle::find_by_accessible_label(&ui, "Add to favorites")
            .next()
            .unwrap();
        favorite.mock_single_click(PointerEventButton::Left);
        assert!(ui.get_edit_favorite() && ui.get_metadata_dirty());
        favorite.invoke_accessible_default_action();
        assert!(!ui.get_edit_favorite());

        let transformed = Rc::new(Cell::new(false));
        ui.on_transform_requested({
            let transformed = transformed.clone();
            move |pitch, _, _, _| transformed.set(pitch == "7.00")
        });
        let pitch = ElementHandle::find_by_accessible_label(
            &ui,
            "Pitch semitones, applied on Enter or focus loss",
        )
        .next()
        .unwrap();
        let rate = ElementHandle::find_by_accessible_label(
            &ui,
            "Pitch-preserving speed, applied on Enter or focus loss",
        )
        .next()
        .unwrap();
        assert!(
            pitch.absolute_position().x + pitch.size().width <= rate.absolute_position().x,
            "Narrow input paint and hit regions must not overlap the next field"
        );
        pitch.set_accessible_value("7.00");
        pitch.mock_single_click(PointerEventButton::Left);
        ui.window().dispatch_event(WindowEvent::KeyPressed {
            text: Key::Return.into(),
        });
        ui.window().dispatch_event(WindowEvent::KeyReleased {
            text: Key::Return.into(),
        });
        assert!(transformed.get());
        ui.hide().unwrap();
    }

    fn playback_start(generation: u64, play_session_id: u64, path: &str) -> AudioEvent {
        AudioEvent::PlaybackStarted {
            generation,
            play_session_id,
            file_path: PathBuf::from(path),
        }
    }

    fn session_status(
        generation: u64,
        play_session_id: u64,
        path: &str,
    ) -> audiolib_audio::PlaybackStatus {
        audiolib_audio::PlaybackStatus {
            generation,
            play_session_id,
            state: PlaybackState::Stopped,
            file_path: Some(PathBuf::from(path)),
            ..Default::default()
        }
    }

    #[test]
    fn playback_history_counts_fast_replays_once_and_recovers_missing_events() {
        let mut history = PlaybackHistoryTracker::default();
        let status = session_status(4, 3, "a.wav");
        history.observe(
            0,
            &status,
            [playback_start(4, 1, "a.wav"), playback_start(4, 2, "a.wav")],
        );
        let mut writes = Vec::new();
        history
            .flush(|path| {
                writes.push(path.to_owned());
                Ok(true)
            })
            .unwrap();
        assert_eq!(writes, vec![PathBuf::from("a.wav"); 3]);
        // Resumed Playing/Seek/Configure retain the same cumulative session ID.
        let resumed = audiolib_audio::PlaybackStatus {
            state: PlaybackState::Playing,
            ..status
        };
        history.observe(0, &resumed, [playback_start(4, 3, "a.wav")]);
        history
            .flush(|path| {
                writes.push(path.to_owned());
                Ok(true)
            })
            .unwrap();
        assert_eq!(writes.len(), 3);
    }

    #[test]
    fn playback_history_uses_event_source_when_load_has_already_changed() {
        let mut history = PlaybackHistoryTracker::default();
        let mut writes = Vec::new();
        history.observe(
            0,
            &session_status(2, 1, "new.wav"),
            [
                playback_start(1, 1, "old.wav"),
                playback_start(1, 2, "old.wav"),
                playback_start(2, 1, "new.wav"),
            ],
        );
        history
            .flush(|path| {
                writes.push(path.to_owned());
                Ok(true)
            })
            .unwrap();
        assert_eq!(writes, ["old.wav", "old.wav", "new.wav"].map(PathBuf::from));
        history.observe(0, &session_status(2, 1, "new.wav"), []);
        assert!(history.pending.is_empty());
        assert!(!history.queued_through.contains_key(&1));
    }

    #[test]
    fn playback_history_retries_busy_and_does_not_mark_unwritten_entries_complete() {
        let mut history = PlaybackHistoryTracker::default();
        let status = session_status(8, 2, "busy.wav");
        history.observe(0, &status, [playback_start(8, 1, "busy.wav")]);
        let mut written = 0;
        assert!(
            history
                .flush(|_| {
                    if written == 0 {
                        written += 1;
                        Ok(true)
                    } else {
                        Err(audiolib_core::Error::Busy("administration".into()))
                    }
                })
                .is_err()
        );
        assert_eq!(history.pending.len(), 1);
        history.observe(0, &status, [playback_start(8, 2, "busy.wav")]);
        history.flush(|_| Ok(false)).unwrap();
        assert_eq!(history.pending.len(), 1);
        history
            .flush(|_| {
                written += 1;
                Ok(true)
            })
            .unwrap();
        assert_eq!(written, 2);
        assert!(history.pending.is_empty());
    }

    #[test]
    fn playback_history_drops_previous_library_events_and_baselines_retained_source() {
        let mut history = PlaybackHistoryTracker::default();
        let status = session_status(5, 1, "shared.wav");
        history.observe(1, &status, [playback_start(5, 1, "shared.wav")]);
        history.flush(|_| Ok(false)).unwrap();
        // A and B can both index this exact path. Queued A events are not B plays.
        let switched = session_status(5, 2, "shared.wav");
        history.observe(2, &switched, [playback_start(5, 2, "shared.wav")]);
        let mut writes = 0;
        history
            .flush(|_| {
                writes += 1;
                Ok(true)
            })
            .unwrap();
        history.observe(2, &switched, [playback_start(5, 2, "shared.wav")]);
        history
            .flush(|_| {
                writes += 1;
                Ok(true)
            })
            .unwrap();
        assert_eq!(writes, 0);
        history.observe(
            2,
            &session_status(5, 3, "shared.wav"),
            [playback_start(5, 3, "shared.wav")],
        );
        history
            .flush(|_| {
                writes += 1;
                Ok(true)
            })
            .unwrap();
        assert_eq!(writes, 1);
    }

    #[test]
    fn playback_history_rejects_late_old_source_events_after_library_cutover() {
        let mut history = PlaybackHistoryTracker::default();
        history.observe(1, &session_status(5, 1, "shared.wav"), []);
        history.observe(2, &session_status(6, 0, "new.wav"), []);
        // Old generation 5 can be published between the first event drain and
        // the fresh cutover snapshot of generation 6, then arrive at the next poll.
        history.observe(
            2,
            &session_status(6, 1, "new.wav"),
            [
                playback_start(5, 2, "shared.wav"),
                playback_start(6, 1, "new.wav"),
            ],
        );
        let mut writes = Vec::new();
        history
            .flush(|path| {
                writes.push(path.to_owned());
                Ok(true)
            })
            .unwrap();
        assert_eq!(writes, vec![PathBuf::from("new.wav")]);
    }

    #[test]
    fn close_finishes_within_remote_grace_when_remote_work_never_drains() {
        // Slint binds its platform to the first thread that creates a window; run the
        // flow in its own process like the other close regressions.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "controller::tests::child_close_finishes_within_remote_grace",
                "--ignored",
                "--test-threads=1",
            ])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "Isolated close-grace regression failed: {}\n{stdout}\n{stderr}",
            output.status
        );
    }

    #[test]
    #[ignore = "helper invoked by the close-grace regression in an isolated process"]
    fn child_close_finishes_within_remote_grace() {
        let root = std::env::temp_dir().join(format!(
            "audiolib-desktop-close-grace-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let provider = Arc::new(SqliteProvider::open(&root.join("library.db")).unwrap());
        let engine = audiolib_audio::AudioEngine::new(PlaybackConfig::default()).unwrap();
        let ui = AppWindow::new().unwrap();
        let controller = Controller::new(
            &ui,
            provider,
            engine.handle(),
            AppConfig::load_in(root.join("preferences")).unwrap(),
            PlaybackConfig::default(),
        );
        let grace = Duration::from_millis(200);
        *controller.remote_close_grace.lock().unwrap() = grace;
        // A remote worker which never releases its busy flag.
        controller.remote_busy.store(true, Ordering::Release);
        let started = Instant::now();
        controller.request_close(&ui);
        while controller.close_completion.lock().unwrap().is_none()
            && started.elapsed() < Duration::from_secs(10)
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        let elapsed = started.elapsed();
        let completion = controller.close_completion.lock().unwrap().take();
        assert!(matches!(completion, Some(Ok(()))), "{completion:?}");
        assert!(elapsed >= grace, "close waited for the grace period");
        assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
        assert!(controller.remote_busy.load(Ordering::Acquire));
        controller.shutdown();
        ui.hide().ok();
        drop(controller);
        drop(engine);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn close_saves_drafts_and_waits_for_scan_drain_without_blocking_the_event_loop() {
        // Slint's system-time backend installs a process-global event-loop proxy.
        // Isolate this flow from other integration tests in the same test binary.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "controller::tests::child_close_saves_drafts_and_waits_for_scan_drain",
                "--ignored",
                "--test-threads=1",
            ])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "Isolated close regression failed: {}\n{stdout}\n{stderr}",
            output.status
        );
    }

    #[test]
    #[ignore = "helper invoked by the close/scan-drain regression in an isolated process"]
    fn child_close_saves_drafts_and_waits_for_scan_drain() {
        let rejected = init_close_without_post_delivery();
        let root = std::env::temp_dir().join(format!(
            "audiolib-desktop-close-scan-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("sounds")).unwrap();
        let mut wav = Vec::new();
        wav.extend(b"RIFF");
        wav.extend(38u32.to_le_bytes());
        wav.extend(b"WAVEfmt ");
        wav.extend(16u32.to_le_bytes());
        wav.extend(1u16.to_le_bytes());
        wav.extend(1u16.to_le_bytes());
        wav.extend(48000u32.to_le_bytes());
        wav.extend(96000u32.to_le_bytes());
        wav.extend(2u16.to_le_bytes());
        wav.extend(16u16.to_le_bytes());
        wav.extend(b"data");
        wav.extend(2u32.to_le_bytes());
        wav.extend(0i16.to_le_bytes());
        let source = root.join("sounds/close.wav");
        std::fs::write(&source, &wav).unwrap();
        let provider = Arc::new(SqliteProvider::open(&root.join("library.db")).unwrap());
        ScanService::new(provider.clone())
            .scan(
                &root.join("sounds"),
                &ScanOptions::default(),
                &ScanCancellation::default(),
                |_| {},
            )
            .unwrap();
        let file = provider.search(&SearchRequest::default()).unwrap().results[0].clone();
        let file_id = file.id;
        let engine = audiolib_audio::AudioEngine::new(PlaybackConfig::default()).unwrap();
        let ui = AppWindow::new().unwrap();
        let controller = Controller::new(
            &ui,
            provider.clone(),
            engine.handle(),
            AppConfig::load_in(root.join("preferences")).unwrap(),
            PlaybackConfig::default(),
        );
        *controller.selected.lock().unwrap() = Some(file.clone());
        present_file(&ui, &file);
        ui.set_edit_notes("save before scan cancellation".into());
        ui.set_metadata_dirty(true);
        let cancellation = ScanCancellation::default();
        *controller.scan_cancel.lock().unwrap() = Some(cancellation.clone());
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let drained = Arc::new(AtomicBool::new(false));
        let worker_controller = controller.clone();
        let worker_drained = drained.clone();
        let scan_worker = std::thread::spawn(move || {
            // The event loop must run after cancellation before this simulated
            // scan worker can release its lease/token. Bound the wait on failure.
            let released = release_rx.recv_timeout(Duration::from_secs(8)).is_ok();
            worker_drained.store(released, Ordering::Release);
            *worker_controller.scan_cancel.lock().unwrap() = None;
        });
        let responsive_ticks = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let observed_ticks = responsive_ticks.clone();
        let failure = std::rc::Rc::new(std::cell::RefCell::new(None::<String>));
        let errors = failure.clone();
        let check_provider = provider.clone();
        let check_controller = controller.clone();
        let check_release = release_tx.clone();
        let window = ui.as_weak();
        let started = Instant::now();
        let timer = slint::Timer::default();
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(15),
            move || {
                if started.elapsed() > Duration::from_secs(10) {
                    *errors.borrow_mut() = Some("Close did not finish after scan drain".into());
                    slint::quit_event_loop().ok();
                    return;
                }
                if !cancellation.is_cancelled() || observed_ticks.get() >= 5 {
                    return;
                }
                let Some(ui) = window.upgrade() else { return };
                if check_provider.get_audio(file_id).unwrap().notes
                    != "save before scan cancellation"
                    || !ui.window().is_visible()
                    || !ui.get_closing()
                    || check_controller.scan_cancel.lock().unwrap().is_none()
                {
                    *errors.borrow_mut() = Some(
                        "Close cancelled scanning before saving or closed before drain".into(),
                    );
                    slint::quit_event_loop().ok();
                    return;
                }
                observed_ticks.set(observed_ticks.get() + 1);
                if observed_ticks.get() == 5 {
                    check_release.send(()).unwrap();
                }
            },
        );
        ui.show().unwrap();
        // A real failed sync has no valid endpoint. Its final publication is
        // rejected by this backend, so worker-owned busy release must unblock close.
        controller.start_sync(false, false);
        controller.request_close(&ui);
        slint::run_event_loop().unwrap();
        timer.stop();
        let was_drained_before_quit = drained.load(Ordering::Acquire)
            && responsive_ticks.get() == 5
            && controller.scan_cancel.lock().unwrap().is_none();
        let was_hidden_before_quit = !ui.window().is_visible();
        let remote_drained_before_quit = !controller.remote_busy.load(Ordering::Acquire);
        // Release the simulated worker even if an early-quit regression failed.
        release_tx.send(()).ok();
        scan_worker.join().unwrap();
        controller.shutdown();
        ui.hide().unwrap();
        drop(controller);
        drop(engine);
        assert!(failure.borrow().is_none(), "{:?}", failure.borrow());
        assert!(
            was_drained_before_quit,
            "Close must keep the event loop responsive until the scan token is released"
        );
        assert!(
            was_hidden_before_quit,
            "Successful close must hide the window that rejected the native close request"
        );
        assert!(rejected.load(Ordering::Acquire) > 1);
        assert!(remote_drained_before_quit);
        assert_eq!(
            provider.get_audio(file_id).unwrap().notes,
            "save before scan cancellation"
        );
        assert_eq!(std::fs::read(source).unwrap(), wav);
        drop(provider);
        assert_eq!(root.parent(), Some(std::env::temp_dir().as_path()));
        std::thread::sleep(Duration::from_millis(100));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn close_failure_preserves_drafts_and_retries_without_ui_notifications() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "controller::tests::child_close_failure_retries_without_ui_notifications",
                "--ignored",
                "--test-threads=1",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    #[ignore = "isolated close failure/retry helper with rejected UI notifications"]
    fn child_close_failure_retries_without_ui_notifications() {
        use audiolib_core::config::AppPaths;
        use std::{cell::Cell, cell::RefCell, rc::Rc};

        let rejected = init_close_without_post_delivery();
        let root = std::env::temp_dir().join(format!(
            "audiolib-close-mailbox-retry-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for name in ["sounds", "cache", "logs"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        let mut wav = Vec::new();
        wav.extend(b"RIFF");
        wav.extend(38u32.to_le_bytes());
        wav.extend(b"WAVEfmt ");
        wav.extend(16u32.to_le_bytes());
        wav.extend(1u16.to_le_bytes());
        wav.extend(1u16.to_le_bytes());
        wav.extend(48000u32.to_le_bytes());
        wav.extend(96000u32.to_le_bytes());
        wav.extend(2u16.to_le_bytes());
        wav.extend(16u16.to_le_bytes());
        wav.extend(b"data");
        wav.extend(2u32.to_le_bytes());
        wav.extend(0i16.to_le_bytes());
        let source = root.join("sounds/mailbox.wav");
        std::fs::write(&source, &wav).unwrap();
        let provider = Arc::new(SqliteProvider::open(&root.join("library.db")).unwrap());
        ScanService::new(provider.clone())
            .scan(
                &root.join("sounds"),
                &ScanOptions::default(),
                &ScanCancellation::default(),
                |_| {},
            )
            .unwrap();
        let file = provider.search(&SearchRequest::default()).unwrap().results[0].clone();
        let file_id = file.id;
        let playback = PlaybackConfig {
            volume: 0.0,
            ..Default::default()
        };
        let config = AppConfig {
            paths: AppPaths {
                database: root.join("library.db"),
                cache: root.join("cache"),
                logs: root.join("logs"),
                settings: root.join("settings.json"),
                legacy_database: None,
                audio_terms: None,
            },
            language: "en".into(),
            playback: serde_json::to_value(&playback).unwrap(),
            audition_play_on_single_click: false,
            play_history_limit: audiolib_core::config::PLAY_HISTORY_LIMIT_PER_LIBRARY,
            max_search_tasks: 6,
            pregenerate_list_waveforms: false,
            hold_list_waveform_batches_after_audition: false,
            ui_scale_percent: 0,
            theme: audiolib_core::config::DEFAULT_THEME.into(),
            ui_font_size: audiolib_core::config::DEFAULT_FONT_SIZE,
            list_font_size: audiolib_core::config::DEFAULT_FONT_SIZE,
            audio_table: Default::default(),
            spectral_peaks: Default::default(),
            window_layout: Default::default(),
            allowed_scan_roots: Vec::new(),
            data_source: Default::default(),
            data_source_fallback: None,
        };
        config.save().unwrap();
        let engine = audiolib_audio::AudioEngine::new(playback.clone()).unwrap();
        let ui = AppWindow::new().unwrap();
        let controller = Controller::new(&ui, provider.clone(), engine.handle(), config, playback);
        *controller.selected.lock().unwrap() = Some(file.clone());
        present_file(&ui, &file);
        ui.set_edit_notes("draft retained after failed close".into());
        ui.set_metadata_dirty(true);
        // Real cross-instance administration exclusion rejects the first save.
        // Release only after the UI has consumed the failed close completion.
        let lease = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("library.db.admin.lock"))
            .unwrap();
        lease.try_lock().unwrap();
        let lease = Rc::new(RefCell::new(Some(lease)));
        let retry_lease = lease.clone();
        let retried = Rc::new(Cell::new(false));
        let observed_retry = retried.clone();
        let failure = Rc::new(RefCell::new(None::<String>));
        let errors = failure.clone();
        let c = controller.clone();
        let check_provider = provider.clone();
        let weak = ui.as_weak();
        let started = Instant::now();
        let timer = slint::Timer::default();
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(15),
            move || {
                if started.elapsed() > Duration::from_secs(5) {
                    *errors.borrow_mut() = Some("Close failure/retry did not complete".into());
                    slint::quit_event_loop().unwrap();
                    return;
                }
                if observed_retry.get() || c.closing.load(Ordering::Acquire) {
                    return;
                }
                let ui = weak.upgrade().unwrap();
                let state = c.metadata.lock().unwrap();
                let preserved = state.failure().is_some() && state.drafts.contains_key(&file_id);
                drop(state);
                if !preserved
                    || !ui.window().is_visible()
                    || ui.get_closing()
                    || !ui.get_metadata_dirty()
                    || ui.get_ds_syncing()
                    || ui.get_ds_progress() != 0.0
                    || c.remote_busy.load(Ordering::Acquire)
                    || ui.get_edit_notes() != "draft retained after failed close"
                    || check_provider.get_audio(file_id).unwrap().notes
                        == "draft retained after failed close"
                    || c.close_completion.lock().unwrap().is_some()
                {
                    *errors.borrow_mut() = Some("Failed close lost its draft or window".into());
                    slint::quit_event_loop().unwrap();
                    return;
                }
                retry_lease.borrow_mut().take().unwrap().unlock().unwrap();
                observed_retry.set(true);
                c.request_close(&ui);
                c.request_close(&ui);
            },
        );
        ui.show().unwrap();
        controller.start_sync(false, false);
        controller.request_close(&ui);
        controller.request_close(&ui);
        slint::run_event_loop().unwrap();
        timer.stop();
        let hidden = !ui.window().is_visible();
        let state = controller.metadata.lock().unwrap();
        let saved = state.failure().is_none()
            && !state.drafts.contains_key(&file_id)
            && state.sequence == 2
            && state.completed == 2;
        drop(state);
        controller.shutdown();
        ui.hide().unwrap();
        drop(engine);
        assert!(failure.borrow().is_none(), "{:?}", failure.borrow());
        assert!(retried.get() && hidden && saved);
        assert!(rejected.load(Ordering::Acquire) > 1);
        assert!(controller.close_completion.lock().unwrap().is_none());
        assert_eq!(
            provider.get_audio(file_id).unwrap().notes,
            "draft retained after failed close"
        );
        assert_eq!(std::fs::read(source).unwrap(), wav);
        drop(ui);
        drop(controller);
        drop(provider);
        drop(lease);
        assert_eq!(root.parent(), Some(std::env::temp_dir().as_path()));
        std::thread::sleep(Duration::from_millis(100));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn scan_submission_recovers_from_invalid_paths_and_preserves_annotations() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "controller::tests::child_scan_submission_recovers_and_preserves_annotations",
                "--ignored",
                "--test-threads=1",
            ])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "Isolated scan interaction regression failed: {}\n{stdout}\n{stderr}",
            output.status
        );
    }

    #[test]
    fn scan_validation_cancellation_drains_and_rejects_queued_admission() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "controller::tests::child_scan_validation_cancellation",
                "--ignored",
                "--test-threads=1",
            ])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "Isolated scan cancellation regression failed: {}\n{stdout}\n{stderr}",
            output.status
        );
    }

    #[test]
    #[ignore = "helper invoked by the isolated pending-scan cancellation regression"]
    fn child_scan_validation_cancellation() {
        i_slint_backend_testing::init_integration_test_with_system_time();
        let root = std::env::temp_dir().join(format!(
            "audiolib-desktop-scan-cancel-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        assert_eq!(root.parent(), Some(std::env::temp_dir().as_path()));
        let sounds = root.join("sounds");
        std::fs::create_dir_all(&sounds).unwrap();
        let mut wav = Vec::new();
        wav.extend(b"RIFF");
        wav.extend(38u32.to_le_bytes());
        wav.extend(b"WAVEfmt ");
        wav.extend(16u32.to_le_bytes());
        wav.extend(1u16.to_le_bytes());
        wav.extend(1u16.to_le_bytes());
        wav.extend(48000u32.to_le_bytes());
        wav.extend(96000u32.to_le_bytes());
        wav.extend(2u16.to_le_bytes());
        wav.extend(16u16.to_le_bytes());
        wav.extend(b"data");
        wav.extend(2u32.to_le_bytes());
        wav.extend(0i16.to_le_bytes());
        std::fs::write(sounds.join("keep.wav"), &wav).unwrap();
        std::fs::write(sounds.join("moved.wav"), &wav).unwrap();
        let provider = Arc::new(SqliteProvider::open(&root.join("library.db")).unwrap());
        ScanService::new(provider.clone())
            .scan(
                &sounds,
                &ScanOptions::default(),
                &ScanCancellation::default(),
                |_| {},
            )
            .unwrap();
        let files = provider.search(&SearchRequest::default()).unwrap().results;
        let moved_id = files
            .iter()
            .find(|file| file.filename == "moved.wav")
            .unwrap()
            .id;
        for file in &files {
            provider
                .update_metadata(
                    file.id,
                    &MetadataPatch {
                        tags: Some("kept tag".into()),
                        notes: Some("kept annotation".into()),
                        favorite: Some(true),
                        ..Default::default()
                    },
                )
                .unwrap();
        }
        // An accidentally admitted full scan would now mark this reference missing.
        std::fs::remove_file(sounds.join("moved.wav")).unwrap();
        let engine = audiolib_audio::AudioEngine::new(PlaybackConfig::default()).unwrap();
        let ui = AppWindow::new().unwrap();
        let controller = Controller::new(
            &ui,
            provider.clone(),
            engine.handle(),
            AppConfig::load_in(root.join("preferences")).unwrap(),
            PlaybackConfig::default(),
        );
        ui.set_scan_mode(1);
        ui.set_scan_path(sounds.display().to_string().into());
        ui.set_import_visible(true);
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        controller.scan_with_validator(sounds.clone(), move |path, allowed_roots| {
            entered_tx.send(()).unwrap();
            release_rx
                .recv_timeout(Duration::from_secs(8))
                .map_err(|_| audiolib_core::Error::Busy("Test resolver was not released".into()))?;
            audiolib_core::validate_scan_root(path, allowed_roots)
        });
        assert!(ui.get_scanning() && ui.get_scan_validating() && ui.get_import_visible());
        entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let duplicate_calls = Arc::new(AtomicU64::new(0));
        let duplicate = duplicate_calls.clone();
        controller.scan_with_validator(sounds.clone(), move |path, allowed_roots| {
            duplicate.fetch_add(1, Ordering::AcqRel);
            audiolib_core::validate_scan_root(path, allowed_roots)
        });
        ui.invoke_scan_cancel_requested();
        assert!(
            controller
                .scan_cancel
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .is_cancelled()
        );
        assert!(ui.get_scanning() && ui.get_scan_validating());
        controller.library_busy.store(true, Ordering::Release);
        ui.set_library_busy(true);
        controller.complete_file_choice(&ui, Ok(None), true, |_| LibraryOperation::Check);
        assert!(ui.get_scanning() && ui.get_scan_validating());
        let later_draft = root.join("later-folder").display().to_string();
        ui.set_scan_path(later_draft.clone().into());
        ui.set_scan_mode(0);
        ui.set_import_visible(true);
        release_tx.send(()).unwrap();

        let stage = std::rc::Rc::new(std::cell::Cell::new(0));
        let observed = stage.clone();
        let queued = std::rc::Rc::new(std::cell::RefCell::new(
            None::<std::sync::mpsc::Receiver<bool>>,
        ));
        let pending = queued.clone();
        let failure = std::rc::Rc::new(std::cell::RefCell::new(None::<String>));
        let errors = failure.clone();
        let c = controller.clone();
        let check_provider = provider.clone();
        let window = ui.as_weak();
        let scan_root = sounds.clone();
        let started = Instant::now();
        let timer = slint::Timer::default();
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(15),
            move || {
                if started.elapsed() > Duration::from_secs(12) {
                    *errors.borrow_mut() = Some("Pending scan cancellation did not settle".into());
                    slint::quit_event_loop().ok();
                    return;
                }
                let Some(ui) = window.upgrade() else { return };
                let result = (|| -> Result<(), String> {
                    match observed.get() {
                        0 if !ui.get_scanning() && !ui.get_scan_validating() => {
                            if !ui.get_import_visible()
                                || ui.get_scan_path().as_str() != later_draft
                                || !ui.get_scan_error().is_empty()
                                || c.scan_cancel.lock().unwrap().is_some()
                                || check_provider.scan_history(10, 0).unwrap().len() != 1
                                || check_provider.get_audio(moved_id).unwrap().missing
                            {
                                return Err("Cancelled validation admitted a scan or changed the later dialog".into());
                            }
                            let cancellation = ScanCancellation::default();
                            *c.scan_cancel.lock().unwrap() = Some(cancellation.clone());
                            c.scan_validating.store(true, Ordering::Release);
                            let generation = c.scan_generation.fetch_add(1, Ordering::AcqRel) + 1;
                            ui.set_scanning(true);
                            ui.set_scan_validating(true);
                            *pending.borrow_mut() = Some(c.post_scan_admission(
                                c.library_generation.load(Ordering::Acquire),
                                generation,
                                &cancellation,
                            ));
                            ui.invoke_scan_cancel_requested();
                            // A terminal publication from the previous request is stale.
                            c.publish_scan_completion(
                                &ui,
                                c.library_generation.load(Ordering::Acquire),
                                generation - 1,
                                true,
                                false,
                                Err(audiolib_core::Error::Cancelled),
                            );
                            if !ui.get_scanning() || !ui.get_scan_validating() {
                                return Err("Stale completion cleared the current admission state".into());
                            }
                            observed.set(1);
                        }
                        1 => {
                            let outcome = pending.borrow().as_ref().unwrap().try_recv();
                            match outcome {
                                Ok(false) => {
                                    if !ui.get_import_visible()
                                        || ui.get_scan_path().as_str() != later_draft
                                        || check_provider.scan_history(10, 0).unwrap().len() != 1
                                    {
                                        return Err("Cancelled queued admission hid a later draft".into());
                                    }
                                    c.scan_validating.store(false, Ordering::Release);
                                    *c.scan_cancel.lock().unwrap() = None;
                                    c.publish_scan_completion(
                                        &ui,
                                        c.library_generation.load(Ordering::Acquire),
                                        c.scan_generation.load(Ordering::Acquire),
                                        true,
                                        false,
                                        Err(audiolib_core::Error::Cancelled),
                                    );
                                    ui.set_scan_path(scan_root.display().to_string().into());
                                    ui.invoke_scan_requested(ui.get_scan_path());
                                    observed.set(2);
                                }
                                Ok(true) => return Err("Queued admission accepted a cancelled token".into()),
                                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                                Err(error) => return Err(format!("Queued admission lost its acknowledgement: {error}")),
                            }
                        }
                        2 if !ui.get_scanning() && !ui.get_scan_validating() => {
                            if ui.get_import_visible()
                                || !ui.get_scan_error().is_empty()
                                || check_provider.scan_history(10, 0).unwrap().len() != 2
                                || check_provider.get_audio(moved_id).unwrap().missing
                            {
                                return Err("Retry after cancelled validation failed or marked a missing reference".into());
                            }
                            observed.set(3);
                            slint::quit_event_loop().ok();
                        }
                        _ => {}
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    *errors.borrow_mut() = Some(error);
                    slint::quit_event_loop().ok();
                }
            },
        );
        ui.show().unwrap();
        slint::run_event_loop().unwrap();
        timer.stop();
        drop(timer);
        controller.shutdown();
        ui.hide().unwrap();
        assert!(failure.borrow().is_none(), "{:?}", failure.borrow());
        assert_eq!(stage.get(), 3);
        assert_eq!(duplicate_calls.load(Ordering::Acquire), 0);
        for file in files {
            let retained = provider.get_audio(file.id).unwrap();
            assert_eq!(retained.tags, "kept tag");
            assert_eq!(retained.notes, "kept annotation");
            assert!(retained.favorite);
        }
        assert_eq!(std::fs::read(sounds.join("keep.wav")).unwrap(), wav);
        drop(controller);
        drop(engine);
        drop(provider);
        std::thread::sleep(Duration::from_millis(100));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    #[ignore = "helper invoked by the isolated folder-scan interaction regression"]
    fn child_scan_submission_recovers_and_preserves_annotations() {
        i_slint_backend_testing::init_integration_test_with_system_time();
        let root = std::env::temp_dir().join(format!(
            "audiolib-desktop-scan-flow-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        assert_eq!(root.parent(), Some(std::env::temp_dir().as_path()));
        let sounds = root.join("sounds");
        std::fs::create_dir_all(&sounds).unwrap();
        let mut wav = Vec::new();
        wav.extend(b"RIFF");
        wav.extend(38u32.to_le_bytes());
        wav.extend(b"WAVEfmt ");
        wav.extend(16u32.to_le_bytes());
        wav.extend(1u16.to_le_bytes());
        wav.extend(1u16.to_le_bytes());
        wav.extend(48000u32.to_le_bytes());
        wav.extend(96000u32.to_le_bytes());
        wav.extend(2u16.to_le_bytes());
        wav.extend(16u16.to_le_bytes());
        wav.extend(b"data");
        wav.extend(2u32.to_le_bytes());
        wav.extend(0i16.to_le_bytes());
        std::fs::write(sounds.join("keep.wav"), &wav).unwrap();
        std::fs::write(sounds.join("moved.wav"), &wav).unwrap();
        let provider = Arc::new(SqliteProvider::open(&root.join("library.db")).unwrap());
        ScanService::new(provider.clone())
            .scan(
                &sounds,
                &ScanOptions::default(),
                &ScanCancellation::default(),
                |_| {},
            )
            .unwrap();
        let files = provider.search(&SearchRequest::default()).unwrap().results;
        let keep_id = files
            .iter()
            .find(|file| file.filename == "keep.wav")
            .unwrap()
            .id;
        let moved_id = files
            .iter()
            .find(|file| file.filename == "moved.wav")
            .unwrap()
            .id;
        for file in &files {
            provider
                .update_metadata(
                    file.id,
                    &MetadataPatch {
                        tags: Some("user tag".into()),
                        notes: Some("keep my annotation".into()),
                        favorite: Some(true),
                        ..Default::default()
                    },
                )
                .unwrap();
        }
        let engine = audiolib_audio::AudioEngine::new(PlaybackConfig::default()).unwrap();
        let ui = AppWindow::new().unwrap();
        let mut config = AppConfig::load_in(root.join("preferences")).unwrap();
        config.language = "cn".into();
        let controller = Controller::new(
            &ui,
            provider.clone(),
            engine.handle(),
            config,
            PlaybackConfig::default(),
        );
        let invalid_path = root.join("not-a-folder").display().to_string();
        ui.set_scan_path(invalid_path.clone().into());
        ui.set_import_visible(true);
        ui.invoke_scan_requested(invalid_path.clone().into());
        let stage = std::rc::Rc::new(std::cell::Cell::new(0));
        let observed = stage.clone();
        let failure = std::rc::Rc::new(std::cell::RefCell::new(None::<String>));
        let errors = failure.clone();
        let c = controller.clone();
        let check_provider = provider.clone();
        let window = ui.as_weak();
        let scan_root = sounds.clone();
        let new_wav = wav.clone();
        let started = Instant::now();
        let timer = slint::Timer::default();
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(15),
            move || {
                if started.elapsed() > Duration::from_secs(12) {
                    *errors.borrow_mut() = Some("Scan interaction did not finish".into());
                    slint::quit_event_loop().ok();
                    return;
                }
                let Some(ui) = window.upgrade() else { return };
                if c.scan_cancel.lock().unwrap().is_some() {
                    return;
                }
                let result = (|| -> Result<(), String> {
                    match observed.get() {
                        0 if !ui.get_scan_error().is_empty() => {
                            if !ui.get_import_visible()
                                || ui.get_scanning()
                                || ui.get_scan_path().as_str() != invalid_path
                                || check_provider.scan_history(10, 0).unwrap().len() != 1
                            {
                                return Err("Rejected scan lost the path/modal or wrote a scan session".into());
                            }
                            ui.set_scan_path(scan_root.display().to_string().into());
                            ui.set_scan_mode(0);
                            ui.invoke_scan_requested(ui.get_scan_path());
                            observed.set(1);
                        }
                        1 if ui.get_scan_summary().contains("增量扫描已完成") => {
                            if ui.get_import_visible()
                                || ui.get_scanning()
                                || !ui.get_scan_error().is_empty()
                                || !ui.get_scan_summary().contains("未变化 2")
                            {
                                return Err("Successful incremental scan did not settle its dialog and summary".into());
                            }
                            std::fs::remove_file(scan_root.join("moved.wav")).unwrap();
                            std::fs::write(scan_root.join("new.wav"), &new_wav).unwrap();
                            ui.set_import_visible(true);
                            ui.set_scan_mode(1);
                            ui.invoke_scan_requested(ui.get_scan_path());
                            observed.set(2);
                        }
                        2 if ui.get_scan_summary().contains("全量扫描已完成") => {
                            if ui.get_import_visible()
                                || ui.get_scanning()
                                || !ui.get_scan_summary().contains("缺失 1")
                                || !check_provider.get_audio(moved_id).unwrap().missing
                            {
                                return Err("Full scan did not detect missing references and settle its summary".into());
                            }
                            for id in [keep_id, moved_id] {
                                let file = check_provider.get_audio(id).unwrap();
                                if file.tags != "user tag"
                                    || file.notes != "keep my annotation"
                                    || !file.favorite
                                {
                                    return Err("A scan discarded user annotations".into());
                                }
                            }
                            *c.scans.lock().unwrap() = check_provider.scan_history(10, 0).unwrap();
                            ui.set_history_kind(0);
                            c.history_selected(0);
                            let detail = ui.get_history_detail();
                            if !detail.contains("全量扫描")
                                || !detail.contains("缺失 1")
                                || detail.trim_start().starts_with('{')
                            {
                                return Err("History details did not describe the scan in human terms".into());
                            }
                            // A native picker can stay open after the scan worker
                            // releases its token. Reproduce that terminal publication
                            // without opening an interactive dialog in the test.
                            let search_before = c.search_generation.load(Ordering::Acquire);
                            c.library_busy.store(true, Ordering::Release);
                            ui.set_library_busy(true);
                            ui.set_scanning(true);
                            c.publish_scan_completion(
                                &ui,
                                c.library_generation.load(Ordering::Acquire),
                                c.scan_generation.load(Ordering::Acquire),
                                true,
                                true,
                                Ok(audiolib_core::ScanProgress {
                                    state: audiolib_core::ScanState::Completed,
                                    removed: 1,
                                    ..Default::default()
                                }),
                            );
                            if ui.get_scanning()
                                || !ui.get_scan_summary().contains("全量扫描已完成")
                                || c.search_generation.load(Ordering::Acquire) != search_before
                            {
                                return Err("Terminal scan publication was lost while a native picker was busy".into());
                            }
                            c.complete_file_choice(&ui, Ok(None), true, |_| LibraryOperation::Check);
                            if !c.available()
                                || ui.get_scanning()
                                || c.search_generation.load(Ordering::Acquire) <= search_before
                            {
                                return Err("Cancelling the picker did not recover the completed scan view".into());
                            }
                            ui.set_scan_mode(0);
                            ui.invoke_scan_requested(ui.get_scan_path());
                            observed.set(3);
                        }
                        3 if ui.get_scan_summary().contains("增量扫描已完成") => {
                            if ui.get_scanning() || !ui.get_scan_summary().contains("未变化 2") {
                                return Err("A new scan remained disabled after native-picker cancellation".into());
                            }
                            observed.set(4);
                            slint::quit_event_loop().ok();
                        }
                        _ => {}
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    *errors.borrow_mut() = Some(error);
                    slint::quit_event_loop().ok();
                }
            },
        );
        ui.show().unwrap();
        slint::run_event_loop().unwrap();
        timer.stop();
        drop(timer);
        controller.shutdown();
        ui.hide().unwrap();
        assert!(failure.borrow().is_none(), "{:?}", failure.borrow());
        assert_eq!(stage.get(), 4);
        assert_eq!(std::fs::read(sounds.join("keep.wav")).unwrap(), wav);
        drop(controller);
        drop(engine);
        drop(provider);
        std::thread::sleep(Duration::from_millis(100));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn scan_history_explains_incomplete_missing_detection_and_error_paths() {
        let session = ScanSession {
            scan_path: "D:/Sounds/车辆".into(),
            scan_type: "full".into(),
            state: audiolib_core::ScanState::Completed,
            scanned: 12,
            added: 2,
            updated: 9,
            errors: 3,
            error_details: vec![audiolib_core::ScanIssue {
                path: "D:/Sounds/车辆/broken.wav".into(),
                message: "WAV header could not be read".into(),
            }],
            errors_truncated: 2,
            ..Default::default()
        };
        let chinese = scan_session_detail(&session, true);
        let english = scan_session_detail(&session, false);
        assert!(chinese.contains("本次未标记缺失文件"));
        assert!(chinese.contains("另有 2 个问题未列出"));
        assert!(english.contains("missing-file detection was skipped"));
        for detail in [chinese, english] {
            assert!(detail.contains(&session.scan_path));
            assert!(detail.contains("broken.wav"));
            assert!(detail.contains("WAV header could not be read"));
            assert!(!detail.trim_start().starts_with('{'));
            assert!(!detail.contains("\"scan_type\""));
        }
    }

    #[test]
    fn library_management_drains_drafts_before_batch_switch_and_close() {
        i_slint_backend_testing::init_integration_test_with_system_time();
        let root = std::env::temp_dir().join(format!(
            "audiolib-desktop-flow-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("sounds-a")).unwrap();
        std::fs::create_dir_all(root.join("sounds-b")).unwrap();
        let wav = |path: &Path| {
            let mut bytes = Vec::new();
            bytes.extend(b"RIFF");
            bytes.extend(38u32.to_le_bytes());
            bytes.extend(b"WAVEfmt ");
            bytes.extend(16u32.to_le_bytes());
            bytes.extend(1u16.to_le_bytes());
            bytes.extend(1u16.to_le_bytes());
            bytes.extend(48000u32.to_le_bytes());
            bytes.extend(96000u32.to_le_bytes());
            bytes.extend(2u16.to_le_bytes());
            bytes.extend(16u16.to_le_bytes());
            bytes.extend(b"data");
            bytes.extend(2u32.to_le_bytes());
            bytes.extend(0i16.to_le_bytes());
            std::fs::write(path, bytes).unwrap();
        };
        wav(&root.join("sounds-a/A.wav"));
        wav(&root.join("sounds-a/A2.wav"));
        wav(&root.join("sounds-b/B.wav"));
        let a = Arc::new(SqliteProvider::open(&root.join("a.db")).unwrap());
        let b = Arc::new(SqliteProvider::open(&root.join("b.db")).unwrap());
        for (provider, folder) in [(a.clone(), "sounds-a"), (b.clone(), "sounds-b")] {
            ScanService::new(provider)
                .scan(
                    &root.join(folder),
                    &ScanOptions::default(),
                    &ScanCancellation::default(),
                    |_| {},
                )
                .unwrap();
        }
        let files = a.search(&SearchRequest::default()).unwrap().results;
        let a_id = files[0].id;
        let b_id = b.search(&SearchRequest::default()).unwrap().results[0].id;
        let engine = audiolib_audio::AudioEngine::new(PlaybackConfig::default()).unwrap();
        let ui = AppWindow::new().unwrap();
        let config = AppConfig::load_in(root.join("preferences")).unwrap();
        let controller = Controller::new(
            &ui,
            a.clone(),
            engine.handle(),
            config,
            PlaybackConfig::default(),
        );
        present_file(&ui, &files[0]);
        a.update_metadata(
            a_id,
            &MetadataPatch {
                color: Some("#66DBB4".into()),
                ..Default::default()
            },
        )
        .unwrap();
        ui.set_selected_folder_path("previous folder".into());
        controller.select_folder_id(a.folders(None).unwrap()[0].id);
        assert_eq!(ui.get_selected_folder_path(), "");
        ui.set_edit_notes("draft before batch".into());
        ui.set_metadata_dirty(true);
        controller
            .multi_selection
            .lock()
            .unwrap()
            .extend(files.iter().map(|file| file.id));
        ui.set_batch_tags_enabled(true);
        ui.set_batch_tags("bulk".into());
        ui.set_batch_tag_mode(1);
        controller.apply_batch();
        let stage = std::rc::Rc::new(std::cell::Cell::new(0));
        let observed = stage.clone();
        let failure = std::rc::Rc::new(std::cell::RefCell::new(None::<String>));
        let errors = failure.clone();
        let c = controller.clone();
        let a_check = a.clone();
        let b_check = b.clone();
        let target_root = root.clone();
        let window = ui.as_weak();
        let started = Instant::now();
        let timer = slint::Timer::default();
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(15),
            move || {
                if started.elapsed() > Duration::from_secs(12) {
                    *errors.borrow_mut() = Some("Management flow timed out".into());
                    slint::quit_event_loop().ok();
                    return;
                }
                if c.library_busy.load(Ordering::Acquire) {
                    return;
                }
                let Some(ui) = window.upgrade() else { return };
                let step = (|| -> Result<(), String> {
                    match observed.get() {
                        0 => {
                            let file = a_check.get_audio(a_id).unwrap();
                            if file.notes != "draft before batch"
                                || !file.tags.contains("bulk")
                                || file.color.as_deref() != Some("#66DBB4")
                            {
                                return Err("Batch overtook the inspector draft".into());
                            }
                            *c.selected.lock().unwrap() = Some(file.clone());
                            present_file(&ui, &file);
                            ui.set_edit_notes("draft before backup".into());
                            ui.set_metadata_dirty(true);
                            c.run_library_operation(LibraryOperation::Backup(
                                target_root.join("backup.db"),
                            ));
                            observed.set(1);
                        }
                        1 => {
                            if a_check.get_audio(a_id).unwrap().notes != "draft before backup"
                                || ui.get_metadata_dirty()
                            {
                                return Err("Backup did not drain/synchronize the editor".into());
                            }
                            let backup =
                                SqliteProvider::open_read_only(&target_root.join("backup.db"))
                                    .unwrap();
                            if backup.get_audio(a_id).unwrap().notes != "draft before backup" {
                                return Err("Backup missed the latest annotation".into());
                            }
                            c.run_library_operation(LibraryOperation::Switch {
                                path: target_root.join("b.db"),
                                create: false,
                            });
                            observed.set(2);
                        }
                        2 => {
                            if c.provider().database_path()
                                != target_root.join("b.db").canonicalize().unwrap()
                            {
                                return Err("Switch did not install B".into());
                            }
                            if !b_check.get_audio(b_id).unwrap().tags.is_empty()
                                || !c.multi_selection.lock().unwrap().is_empty()
                            {
                                return Err("A's state leaked into B".into());
                            }
                            present_file(&ui, &b_check.get_audio(b_id).unwrap());
                            ui.set_edit_notes("B draft survives rejected switch".into());
                            ui.set_edit_tags("x".repeat(16_385).into());
                            ui.set_metadata_dirty(true);
                            c.run_library_operation(LibraryOperation::Switch {
                                path: target_root.join("a.db"),
                                create: false,
                            });
                            if c.library_busy.load(Ordering::Acquire)
                                || !ui.get_metadata_dirty()
                                || c.provider().database_path()
                                    != target_root.join("b.db").canonicalize().unwrap()
                            {
                                return Err(
                                    "Invalid draft did not retain the current library".into()
                                );
                            }
                            ui.set_edit_tags("".into());
                            c.run_library_operation(LibraryOperation::Switch {
                                path: target_root.join("a.db"),
                                create: false,
                            });
                            observed.set(3);
                        }
                        3 => {
                            if b_check.get_audio(b_id).unwrap().notes
                                != "B draft survives rejected switch"
                            {
                                return Err("Switch lost the B draft".into());
                            }
                            let file = a_check.get_audio(a_id).unwrap();
                            *c.selected.lock().unwrap() = Some(file.clone());
                            present_file(&ui, &file);
                            ui.set_edit_notes("pending at close".into());
                            ui.set_metadata_dirty(true);
                            c.run_library_operation(LibraryOperation::Check);
                            c.request_close(&ui);
                            observed.set(4);
                        }
                        _ => {}
                    }
                    Ok(())
                })();
                if let Err(error) = step {
                    *errors.borrow_mut() = Some(error);
                    slint::quit_event_loop().ok();
                }
            },
        );
        ui.show().unwrap();
        slint::run_event_loop().unwrap();
        timer.stop();
        controller.shutdown();
        ui.hide().unwrap();
        drop(controller);
        drop(engine);
        assert!(failure.borrow().is_none(), "{:?}", failure.borrow());
        assert_eq!(stage.get(), 4);
        assert_eq!(a.get_audio(a_id).unwrap().notes, "pending at close");
        assert_eq!(
            std::fs::read(root.join("sounds-a/A.wav")).unwrap().len(),
            46
        );
        drop(a);
        drop(b);
        // The path is a freshly generated direct child of the OS temporary directory.
        assert_eq!(root.parent(), Some(std::env::temp_dir().as_path()));
        std::thread::sleep(Duration::from_millis(100));
        std::fs::remove_dir_all(root).ok();
    }

    fn annotation(tags: &str, notes: &str) -> MetadataPatch {
        MetadataPatch {
            tags: Some(tags.into()),
            notes: Some(notes.into()),
            ..Default::default()
        }
    }

    fn saved_file(job: &MetadataJob) -> AudioFile {
        let mut file = AudioFile {
            id: job.id,
            ..Default::default()
        };
        if let MetadataWrite::Draft(draft) = &job.write {
            apply_metadata_patch(&mut file, &draft.patch);
        }
        file
    }

    #[test]
    fn library_reset_releases_an_old_barrier_and_new_writes_remain_monotonic() {
        let mut initial = MetadataState::default();
        initial.edit(1, annotation("library A", "old notes"));
        let barrier = initial.enqueue_all().unwrap();
        let pending = initial.next().unwrap();
        let shared = Arc::new((Mutex::new(initial), Condvar::new()));
        let worker_state = shared.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let (state, wake) = worker_state.as_ref();
            let state = state.lock().unwrap();
            ready_tx.send(()).unwrap();
            // The bounded wait fails the regression instead of hanging a test process.
            let (state, timeout) = wake
                .wait_timeout_while(state, Duration::from_millis(250), |state| {
                    state.completed < barrier
                })
                .unwrap();
            done_tx
                .send((state.completed, timeout.timed_out()))
                .unwrap();
        });
        ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let (state, wake) = shared.as_ref();
        let mut state = state.lock().unwrap();
        state.complete(&pending, &Ok(saved_file(&pending)));
        let revision = state.revision;
        // Reset under the lock before the old selection worker can observe completion.
        state.reset_library();
        assert!(state.drafts.is_empty() && state.jobs.is_empty() && state.in_flight.is_none());
        assert!(
            state.saved.is_empty()
                && state.saved_order.is_empty()
                && state.failures.is_empty()
                && state.failed_actions.is_empty()
        );
        assert_eq!(
            (state.sequence, state.completed, state.revision),
            (barrier, barrier, revision)
        );
        drop(state);
        wake.notify_all();
        let completion = done_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        waiter.join().unwrap();
        assert_eq!(
            completion,
            (barrier, false),
            "Old barrier must complete after a library reset"
        );
        let mut state = shared.0.lock().unwrap();
        state.edit(1, annotation("library B", "new notes"));
        let next_barrier = state.enqueue_all().unwrap();
        let next = state.next().unwrap();
        assert!(next_barrier > barrier && state.revision > revision);
        state.complete(&next, &Ok(saved_file(&next)));
        assert_eq!(state.completed, next_barrier);
        let (file, dirty) = state.for_display(
            &AudioFile {
                id: 1,
                ..Default::default()
            },
            barrier,
        );
        assert_eq!(file.tags, "library B");
        assert_eq!(file.notes, "new notes");
        assert!(!dirty);
    }

    #[test]
    fn save_confirmation_preserves_edits_made_while_the_write_was_pending() {
        let mut state = MetadataState::default();
        state.edit(1, annotation("engine", "first note"));
        state.enqueue_draft(1).unwrap();
        let pending = state.next().unwrap();
        state.edit(1, annotation("engine, aircraft", "typed during save"));
        let saved = saved_file(&pending);
        state.complete(&pending, &Ok(saved.clone()));
        let (displayed, dirty) = state.for_display(&saved, pending.sequence);
        assert!(dirty);
        assert_eq!(displayed.tags, "engine, aircraft");
        assert_eq!(displayed.notes, "typed during save");
        assert!(state.drafts.contains_key(&1));
    }

    #[test]
    fn returning_to_a_file_preserves_its_draft_and_a_late_save_overrides_a_stale_read() {
        let mut state = MetadataState::default();
        let stale_a = AudioFile {
            id: 1,
            tags: "old database value".into(),
            ..Default::default()
        };
        state.edit(1, annotation("new A", "pending A"));
        state.enqueue_draft(1).unwrap();
        let pending_a = state.next().unwrap();
        state.edit(2, annotation("B only", "B note"));
        let (returned_a, dirty) = state.for_display(&stale_a, 0);
        assert!(dirty);
        assert_eq!(returned_a.tags, "new A");
        state.complete(&pending_a, &Ok(saved_file(&pending_a)));
        let (returned_a, dirty) = state.for_display(&stale_a, 0);
        assert!(!dirty);
        assert_eq!(returned_a.tags, "new A");
        assert_eq!(state.drafts[&2].patch.tags.as_deref(), Some("B only"));
    }

    #[test]
    fn close_barrier_includes_requested_writes_and_every_unsaved_draft_in_fifo_order() {
        let mut state = MetadataState::default();
        state.edit(1, annotation("A first", ""));
        state.enqueue_draft(1).unwrap();
        let first = state.next().unwrap();
        state.edit(1, annotation("A latest", "latest notes"));
        state.edit(2, annotation("B latest", ""));
        let barrier = state.enqueue_all().unwrap();
        assert!(state.completed < barrier);
        state.complete(&first, &Ok(saved_file(&first)));
        assert!(state.drafts.contains_key(&1));
        assert!(state.completed < barrier);
        let second = state.next().unwrap();
        assert_eq!(saved_file(&second).tags, "A latest");
        state.complete(&second, &Ok(saved_file(&second)));
        let third = state.next().unwrap();
        assert_eq!(third.id, 2);
        state.complete(&third, &Ok(saved_file(&third)));
        assert_eq!(state.completed, barrier);
        assert!(state.jobs.is_empty() && state.in_flight.is_none() && state.drafts.is_empty());
    }

    #[test]
    fn failed_close_write_retains_the_draft_and_a_retry_can_complete() {
        let mut state = MetadataState::default();
        state.edit(1, annotation("must survive", "busy database"));
        state.enqueue_all().unwrap();
        let pending = state.next().unwrap();
        state.complete(&pending, &Err("database is busy".into()));
        assert_eq!(state.failure(), Some((1, "database is busy".into())));
        assert_eq!(state.drafts[&1].patch.tags.as_deref(), Some("must survive"));
        let retry_barrier = state.enqueue_all().unwrap();
        let retry = state.next().unwrap();
        state.complete(&retry, &Ok(saved_file(&retry)));
        assert_eq!(state.completed, retry_barrier);
        assert!(state.failure().is_none() && state.drafts.is_empty());
    }

    #[test]
    fn editor_identity_stays_with_the_visible_file_and_failed_close_keeps_the_window_usable() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().unwrap();
        ui.show().unwrap();
        let mut state = MetadataState::default();
        present_file(
            &ui,
            &AudioFile {
                id: 1,
                filename: "A.wav".into(),
                ..Default::default()
            },
        );
        ui.set_edit_tags("A draft".into());
        ui.set_metadata_dirty(true);
        // B's background read has finished, but B's event-loop publication has not.
        let background_b = AudioFile {
            id: 2,
            filename: "B.wav".into(),
            ..Default::default()
        };
        capture_editor_draft(&mut state, &ui);
        capture_editor_draft(&mut state, &ui);
        assert!(state.drafts.contains_key(&1));
        assert!(!state.drafts.contains_key(&background_b.id));
        present_file(&ui, &background_b);
        assert_eq!(ui.get_selected_id(), "2");
        ui.set_edit_notes("B draft after publication".into());
        ui.set_metadata_dirty(true);
        capture_editor_draft(&mut state, &ui);
        assert_eq!(
            state.drafts[&2].patch.notes.as_deref(),
            Some("B draft after publication")
        );
        ui.set_closing(true);
        ui.set_scanning(true);
        ui.set_ds_syncing(true);
        ui.set_ds_progress(0.5);
        resume_after_failed_close(&ui, "database is busy", false);
        assert!(ui.window().is_visible());
        assert!(!ui.get_closing());
        assert!(!ui.get_scanning());
        assert!(!ui.get_ds_syncing());
        assert_eq!(ui.get_ds_progress(), 0.0);
        assert!(ui.get_metadata_dirty());
        assert_eq!(ui.get_edit_notes(), "B draft after publication");
        // A still-running scan must retain its cancellation action on retry.
        ui.set_closing(true);
        resume_after_failed_close(&ui, "database is busy", true);
        assert!(!ui.get_closing());
        assert!(ui.get_scanning());
        ui.hide().unwrap();
    }

    #[test]
    fn output_summary_uses_actual_routes_and_policy_after_asio_fallback() {
        let fallback = audiolib_audio::PlaybackStatus {
            active_backend: Some(AudioBackend::Wasapi),
            active_source_pair: Some(1),
            active_output_pair: Some(0),
            active_rate_policy: Some(RatePolicy::DeviceDefault),
            ..Default::default()
        };
        let text = output_summary(&fallback);
        assert!(
            text.contains("WASAPI")
                && text.contains("src 3/4 → out 1/2")
                && text.contains("resampling")
        );
        let idle = output_summary(&audiolib_audio::PlaybackStatus::default());
        assert!(idle.contains("Output idle") && idle.contains("src — → out —"));
    }

    #[test]
    fn inspector_drafts_allow_legacy_colors_without_submitting_them() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().unwrap();
        present_file(
            &ui,
            &AudioFile {
                id: 42,
                color: Some("legacy-blue".into()),
                ..Default::default()
            },
        );
        ui.set_edit_favorite(true);
        ui.set_metadata_dirty(true);
        let mut state = MetadataState::default();
        capture_editor_draft(&mut state, &ui);
        let patch = &state.drafts[&42].patch;
        patch.validate().unwrap();
        assert_eq!(patch.favorite, Some(true));
        assert!(patch.color.is_none());
    }

    #[test]
    fn quick_menu_edits_preserve_unrelated_annotations() {
        let favorite = quick_metadata_patch(5, false).unwrap();
        assert_eq!(favorite.favorite, Some(true));
        assert!(favorite.tags.is_none() && favorite.notes.is_none() && favorite.color.is_none());
        // Rating actions no longer exist.
        assert!(quick_metadata_patch(10, false).is_none());
        assert!(quick_metadata_patch(14, true).is_none());
        assert!(quick_metadata_patch(16, false).is_none());
    }

    #[test]
    fn pending_edit_detection_includes_notes_and_favorite() {
        let saved = MetadataPatch {
            tags: Some("engine".into()),
            ..Default::default()
        };
        let mut changed = saved.clone();
        assert!(same_patch(&saved, &changed));
        changed.notes = Some("typed during save".into());
        assert!(!same_patch(&saved, &changed));
        changed = saved.clone();
        changed.favorite = Some(true);
        assert!(!same_patch(&saved, &changed));
    }

    #[test]
    fn metadata_color_and_timeline_display_remain_bounded() {
        assert_eq!(format_time(-1.0), "00:00.000");
        assert_eq!(format_time(65.125), "01:05.125");
        assert!(parse_color("#66DBB4").is_some());
        assert!(parse_color("#ab").is_none());
        assert!(parse_color("#zzzzzz").is_none());
    }

    #[test]
    fn multichannel_waveform_keeps_channels_in_separate_lanes() {
        let peak = |frequency| WaveformPeak {
            min: -0.5,
            max: 0.5,
            rms: 0.0,
            dominant_hz: frequency,
            tonality: 1.0,
            ..Default::default()
        };
        let data = WaveformData {
            sample_rate: 48_000,
            channels: 2,
            frames: 8,
            samples_per_pixel: 2,
            peaks: vec![vec![peak(241.0); 4], vec![peak(36.0); 4]],
        };
        let pixels = render_waveform(
            &data,
            4,
            20,
            WaveView {
                opacity: 1.0,
                ..Default::default()
            },
        );
        assert_eq!(pixels.len(), 4 * 20 * 4);
        let pixel = |x: usize, y: usize| &pixels[(y * 4 + x) * 4..(y * 4 + x) * 4 + 4];
        assert_eq!(pixel(2, 5), [61, 255, 0, 255]);
        assert_eq!(pixel(2, 15), [255, 0, 24, 255]);
        assert_eq!(pixel(2, 10), [132, 144, 148, 255]);
        assert!(pixels.chunks_exact(4).all(|pixel| pixel[3] == 255));
    }
}
