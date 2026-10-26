// app/audiolib-cli/src/main.rs
//! Rust command-line library operations and local desktop transport control.
mod migration;
use audiolib_core::{
    BatchMetadataRequest, DataProvider, FileVisibility, LibraryExportFormat, MaintenanceAction,
    MetadataPatch, SearchFilters, SearchRequest, SortOrder, TagEditMode, config::AppConfig,
};
use audiolib_db::{
    SqliteProvider, import_legacy_local,
    scanner::{ScanCancellation, ScanOptions, ScanService},
};
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
    time::Instant,
};

#[derive(Parser)]
#[command(name = "AudioLibCli", version, about = "AudioLib Explorer Rust CLI")]
struct Arguments {
    /// Override the new Rust library database (legacy databases should be imported).
    #[arg(long, global = true)]
    database: Option<PathBuf>,
    #[arg(long, global = true, value_enum, default_value = "json")]
    format: Format,
    #[command(subcommand)]
    command: Command,
}
#[derive(Clone, Copy, ValueEnum)]
enum Format {
    Json,
    Table,
    Csv,
}
#[derive(Clone, Copy, ValueEnum)]
enum Visibility {
    Present,
    Missing,
    All,
}
impl From<Visibility> for FileVisibility {
    fn from(value: Visibility) -> Self {
        match value {
            Visibility::Present => Self::Present,
            Visibility::Missing => Self::Missing,
            Visibility::All => Self::All,
        }
    }
}
#[derive(Clone, Copy, ValueEnum)]
enum TagsMode {
    Replace,
    Append,
    Remove,
}
impl From<TagsMode> for TagEditMode {
    fn from(value: TagsMode) -> Self {
        match value {
            TagsMode::Replace => Self::Replace,
            TagsMode::Append => Self::Append,
            TagsMode::Remove => Self::Remove,
        }
    }
}
#[derive(Clone, Copy, ValueEnum)]
enum ExportFormat {
    Jsonl,
    Csv,
}
impl From<ExportFormat> for LibraryExportFormat {
    fn from(value: ExportFormat) -> Self {
        match value {
            ExportFormat::Jsonl => Self::JsonLines,
            ExportFormat::Csv => Self::Csv,
        }
    }
}
#[derive(Subcommand)]
enum Command {
    Search {
        #[arg(long, value_enum, default_value = "present")]
        visibility: Visibility,
        #[arg(default_value = "")]
        query: String,
        #[arg(long)]
        folder: Option<i64>,
        #[arg(long)]
        direct_only: bool,
        #[arg(long)]
        sample_rate: Option<u32>,
        #[arg(long)]
        bit_depth: Option<u16>,
        #[arg(long)]
        channels: Option<u16>,
        #[arg(long)]
        duration_min: Option<f64>,
        #[arg(long)]
        duration_max: Option<f64>,
        #[arg(long)]
        favorites: bool,
        #[arg(long)]
        tagged: bool,
        /// Literal substring in user tags (percent/underscore are not wildcards); repeat to
        /// require every value.
        #[arg(long)]
        tag: Vec<String>,
        /// Omit files whose tags contain this literal substring; may be repeated.
        #[arg(long)]
        exclude_tag: Vec<String>,
        #[arg(long)]
        color: Option<String>,
        #[arg(long, default_value_t = 200)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Sort by relevance, filename, duration, sample_rate, channels, tags or modified.
        #[arg(long, default_value = "relevance")]
        sort: String,
        #[arg(long)]
        descending: bool,
    },
    Scan {
        path: PathBuf,
        #[arg(long)]
        no_recursive: bool,
        #[arg(long)]
        full: bool,
        /// Metadata workers (0: automatic, 1: serial, maximum 16).
        #[arg(long, default_value_t = 0)]
        workers: usize,
    },
    Stats,
    Folders {
        #[arg(long)]
        parent: Option<i64>,
    },
    Info {
        id: i64,
    },
    Edit {
        id: i64,
        #[arg(long)]
        tags: Option<String>,
        #[arg(long)]
        color: Option<String>,
        #[arg(long)]
        notes: Option<String>,
        #[arg(long)]
        favorite: Option<bool>,
    },
    /// Atomically edit a bounded selection; tags support replace, append and remove.
    EditBatch {
        #[arg(long, value_delimiter = ',', required = true)]
        ids: Vec<i64>,
        #[arg(long)]
        tags: Option<String>,
        #[arg(long, value_enum, default_value = "replace")]
        tag_mode: TagsMode,
        #[arg(long)]
        color: Option<String>,
        #[arg(long)]
        notes: Option<String>,
        #[arg(long)]
        favorite: Option<bool>,
    },
    /// Reassociate an existing ID with a real WAV while preserving its annotations.
    Relink {
        id: i64,
        path: PathBuf,
    },
    /// Remove library records and references; source WAV files are never removed.
    Remove {
        #[arg(value_delimiter = ',')]
        ids: Vec<i64>,
        #[arg(long)]
        confirm: bool,
    },
    RemoveFolder {
        id: i64,
        #[arg(long)]
        confirm: bool,
    },
    ScanHistory {
        #[arg(long, default_value_t = 100)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        offset: usize,
    },
    PlayHistory {
        #[arg(long, default_value_t = 100)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        offset: usize,
    },
    /// Export the whole library, including annotations and missing records.
    Export {
        destination: PathBuf,
        #[arg(long, value_enum, default_value = "jsonl")]
        export_format: ExportFormat,
    },
    /// Restore annotations from JSONL or CSV by path, without modifying WAV headers.
    ImportAnnotations {
        source: PathBuf,
        #[arg(long)]
        replace_annotations: bool,
    },
    Db {
        #[command(subcommand)]
        command: DbCommand,
    },
    /// Server data source diagnostics using the settings saved by the desktop app.
    Remote {
        #[command(subcommand)]
        command: RemoteCommand,
    },
    Probe {
        path: PathBuf,
    },
    Play {
        /// Omit the path to resume the loaded file at its current position.
        path: Option<PathBuf>,
    },
    Stop,
    Pause,
    Status,
    Seek {
        seconds: f64,
    },
    /// Set independent audition Pitch/Rate and monitor options in the running desktop.
    Audition {
        #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
        pitch: f32,
        #[arg(long, default_value_t = 1.0)]
        rate: f32,
        #[arg(long)]
        mono: bool,
        #[arg(long)]
        repeat: bool,
    },
    Volume {
        value: f32,
    },
}
#[derive(Subcommand)]
enum RemoteCommand {
    /// Show the configured data source, its parsed address and TLS policy, and the cached
    /// server snapshot (no network).
    Status {
        /// Show this server address instead of the saved one (not saved).
        #[arg(long)]
        url: Option<String>,
    },
    /// Check DNS/TCP, the TLS handshake (https), health, the token, library, roots, a
    /// stream and preview sample and each root's WebDAV URL.
    Test {
        /// Test this server address (e.g. https://audiolib.example.com:8443) instead of
        /// the saved one; not saved. The saved token is still sent.
        #[arg(long)]
        url: Option<String>,
    },
    /// Download a new server snapshot when the revision changed (or always with --force).
    Sync {
        #[arg(long)]
        force: bool,
        /// Discard this endpoint's pending transfer and download a fresh snapshot.
        #[arg(long)]
        restart: bool,
    },
    /// Edit annotations on the server: one ID uses PATCH, several IDs (or a tag mode other
    /// than replace) the atomic batch. The cached snapshot follows by the revision rule,
    /// otherwise a sync runs.
    Edit {
        #[arg(value_delimiter = ',', required = true)]
        ids: Vec<i64>,
        #[arg(long)]
        tags: Option<String>,
        #[arg(long, value_enum, default_value = "replace")]
        tag_mode: TagsMode,
        /// `#RRGGBB`, or an empty string to clear the color.
        #[arg(long)]
        color: Option<String>,
        #[arg(long)]
        notes: Option<String>,
        #[arg(long)]
        favorite: Option<bool>,
    },
    /// Download records into the drag-out cache (`<data dir>/cache`) and print the paths.
    Download {
        #[arg(value_delimiter = ',', required = true)]
        ids: Vec<i64>,
    },
    /// List saved server profiles and the active source (tokens are never printed).
    Profiles,
    /// Server roots with status, missing count and latest scan (`GET /roots`).
    Roots {
        /// Query this server address instead of the saved one (not saved).
        #[arg(long)]
        url: Option<String>,
    },
    /// The server's management mode (`GET /capabilities`; `unknown` on older servers).
    Capabilities {
        #[arg(long)]
        url: Option<String>,
    },
    /// Read-only scan status: the running scan (`GET /scans/current`) or one session
    /// (`--id`, `GET /scans/{id}`). Never starts, pauses or cancels a scan.
    Scans {
        #[arg(long)]
        id: Option<i64>,
        #[arg(long)]
        url: Option<String>,
    },
    /// Server scan or play history, newest first (`GET /history/scans|plays`).
    History {
        #[arg(value_enum)]
        kind: HistoryKind,
        #[arg(long, default_value_t = 100)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long)]
        url: Option<String>,
    },
}
#[derive(Clone, Copy, ValueEnum)]
enum HistoryKind {
    Scans,
}
#[derive(Subcommand)]
enum DbCommand {
    /// Inspect or manage a source-bound database migration without opening a provider.
    Migration {
        #[command(subcommand)]
        command: migration::Command,
    },
    Status,
    Import {
        source: PathBuf,
    },
    Create {
        path: PathBuf,
    },
    /// Select the active desktop library for subsequent launches.
    Use {
        path: PathBuf,
    },
    Backup {
        destination: PathBuf,
    },
    Restore {
        source: PathBuf,
        #[arg(long)]
        replace: bool,
    },
    Check,
    Optimize,
    /// Backfill the derived tag column, then rebuild the search index (FTS) when the library
    /// predates the current search-text revision; otherwise rebuild it anyway (repair) unless
    /// `--if-needed`. Takes minutes on a ~1M-row library: run it in a maintenance window.
    RebuildSearch {
        /// Do nothing when the library is already current.
        #[arg(long)]
        if_needed: bool,
    },
}

fn open_provider(
    config: &AppConfig,
    explicit: &Option<PathBuf>,
) -> anyhow::Result<Arc<SqliteProvider>> {
    let path = explicit.as_ref().unwrap_or(&config.paths.database);
    let default_database = config
        .paths
        .settings
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join("library.db");
    let provider = if path.exists() {
        SqliteProvider::open_existing_local_with_progress_cancellable(
            path,
            &config.data_root(),
            config.play_history_limit,
            &AtomicBool::new(false),
            |_| {},
        )?
    } else {
        anyhow::ensure!(
            explicit.is_none() && !config.paths.settings.exists() && path == &default_database,
            "Library does not exist: {}. Use db use <existing-library.db> to select a library, or db create <new-library.db> to create one explicitly.",
            path.display()
        );
        if let Some(source) = &config.paths.legacy_database {
            import_legacy_local(source, path, &config.data_root(), config.play_history_limit)?;
            SqliteProvider::open_existing_local_with_progress_cancellable(
                path,
                &config.data_root(),
                config.play_history_limit,
                &AtomicBool::new(false),
                |_| {},
            )?
        } else {
            SqliteProvider::open(path)?
        }
    };
    let provider = Arc::new(provider);
    if explicit.is_none() && !config.paths.settings.exists() {
        // Persist the one-time legacy preferences together with the new default
        // library; later launches deliberately no longer read legacy YAML.
        config.save()?;
    }
    Ok(provider)
}

fn server_local_play_history(
    config: &AppConfig,
    limit: usize,
    offset: usize,
) -> anyhow::Result<Value> {
    let library_id = config
        .data_source
        .active_profile()
        .and_then(|profile| profile.library_id.clone())
        .or_else(|| {
            let state = audiolib_remote::RemoteCache::new(&config.data_root()).active()?;
            (state.server_url == config.data_source.server_url).then_some(state.library_id)
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Synchronize the selected server once to establish its local history identity"
            )
        })?;
    let scope = format!("server-{library_id}");
    let history = audiolib_db::local_play_history::LocalPlayHistory::open(
        &config.data_root(),
        &scope,
        config.play_history_limit,
    )?;
    Ok(json!(history.page(limit, offset)?))
}

/// Read commands without `--database` use the configured default library, which
/// is often empty; say so on stderr (stdout/JSON stay unchanged).
fn default_database_notice(
    explicit: &Option<PathBuf>,
    database: &std::path::Path,
) -> Option<String> {
    explicit
        .is_none()
        .then(|| format!("正在使用默认库：{}", database.display()))
}

/// An advisory lookup only after a search has no matches. Ignore lookup failures so
/// they cannot turn an already successful search into an error.
fn empty_library_notice(
    total_count: u64,
    library_is_empty: impl FnOnce() -> audiolib_core::Result<bool>,
) -> Option<&'static str> {
    if total_count != 0 {
        return None;
    }
    library_is_empty()
        .ok()
        .filter(|empty| *empty)
        .map(|_| "当前库为空，可用 --database 指定库路径")
}

fn require_closed_desktop(settings: &std::path::Path) -> anyhow::Result<std::fs::File> {
    let directory = settings.parent().unwrap_or(std::path::Path::new("."));
    let path = directory.join("desktop-instance.lock");
    let guard = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    let running_message = "Use the desktop library manager while it is running, or close it before changing the selected library or restoring through CLI";
    match guard.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => anyhow::bail!(running_message),
        Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
    }
    // Older desktops do not acquire the sidecar. Current desktops identify their
    // settings path so a separate validation/user-data profile is not blocked.
    let same_profile_is_running = audiolib_ipc::request(json!({"command":"status"}))
        .is_ok_and(|reply| desktop_profile_running(&reply, settings));
    if same_profile_is_running {
        anyhow::bail!(running_message);
    }
    Ok(guard)
}

fn desktop_profile_running(reply: &Value, settings: &std::path::Path) -> bool {
    reply["success"] == true
        && reply["metadata"]["settings_path"]
            .as_str()
            .is_none_or(|path| {
                let active = std::path::Path::new(path);
                match (active.canonicalize(), settings.canonicalize()) {
                    (Ok(active), Ok(requested)) => active == requested,
                    _ => active == settings,
                }
            })
}

fn operation(args: &Arguments) -> anyhow::Result<Value> {
    if let Command::Db {
        command: DbCommand::Migration { command },
    } = &args.command
    {
        return migration::run(args.database.as_deref(), command);
    }
    match &args.command {
        Command::Probe { path } => return Ok(json!(audiolib_audio::probe(path)?)),
        Command::Play { path } => {
            if let Some(path) = path {
                let path = path.canonicalize()?;
                let loaded = audiolib_ipc::request(json!({"command":"load","path":path}))?;
                if loaded["success"] != true {
                    return Ok(loaded);
                }
            }
            return Ok(audiolib_ipc::request(json!({"command":"play"}))?);
        }
        Command::Stop => return Ok(audiolib_ipc::request(json!({"command":"stop"}))?),
        Command::Pause => return Ok(audiolib_ipc::request(json!({"command":"pause"}))?),
        Command::Status => return Ok(audiolib_ipc::request(json!({"command":"status"}))?),
        Command::Seek { seconds } => {
            return Ok(audiolib_ipc::request(
                json!({"command":"seek","seconds":seconds}),
            )?);
        }
        Command::Audition {
            pitch,
            rate,
            mono,
            repeat,
        } => {
            return Ok(audiolib_ipc::request(
                json!({"command":"audition","settings":{
                    "pitch_semitones":pitch,"rate":rate,"mono":mono,"loop_enabled":repeat
                }}),
            )?);
        }
        Command::Volume { value } => {
            return Ok(audiolib_ipc::request(
                json!({"command":"volume","volume":value}),
            )?);
        }
        _ => {}
    }
    let mut config = AppConfig::load_default()?;
    if let Command::Remote { command } = &args.command {
        return remote_operation(&config, command);
    }
    if args.database.is_none()
        && config.data_source.mode == audiolib_core::config::DataSourceMode::Server
        && let Command::PlayHistory { limit, offset } = &args.command
    {
        return server_local_play_history(&config, *limit, *offset);
    }
    let mut _preferences_guard = None;
    match &args.command {
        Command::Db {
            command: DbCommand::Create { path },
        } => {
            anyhow::ensure!(
                !path.exists(),
                "Create destination already exists; choose a new path"
            );
            let provider = SqliteProvider::open(path)?;
            return Ok(json!(provider.library_info()?));
        }
        Command::Db {
            command: DbCommand::Use { path },
        } => {
            _preferences_guard = Some(require_closed_desktop(&config.paths.settings)?);
            config = AppConfig::load_default()?;
            let provider = SqliteProvider::open_existing_local_with_progress_cancellable(
                path,
                &config.data_root(),
                config.play_history_limit,
                &AtomicBool::new(false),
                |_| {},
            )?;
            let integrity = provider.check_library()?;
            anyhow::ensure!(
                integrity.is_ok(),
                "Selected library failed integrity checks: {:?}",
                integrity.errors
            );
            config.select_database(path)?;
            config.save()?;
            return Ok(json!(provider.library_info()?));
        }
        Command::Db {
            command: DbCommand::Restore { replace: false, .. },
        } => anyhow::bail!(
            "Restore replaces this library; pass --replace. A rollback backup is created automatically."
        ),
        Command::Db {
            command: DbCommand::Restore { replace: true, .. },
        } => {
            _preferences_guard = Some(require_closed_desktop(&config.paths.settings)?);
            config = AppConfig::load_default()?;
        }
        Command::Remove { confirm: false, .. } | Command::RemoveFolder { confirm: false, .. } => {
            anyhow::bail!(
                "Pass --confirm to remove library records; source audio files remain intact."
            )
        }
        Command::ImportAnnotations {
            replace_annotations: false,
            ..
        } => anyhow::bail!(
            "Pass --replace-annotations to apply the exported annotations to matching paths."
        ),
        _ => {}
    }
    if let Command::Db {
        command: DbCommand::Import { source },
    } = &args.command
    {
        let path = args.database.as_ref().unwrap_or(&config.paths.database);
        let report =
            import_legacy_local(source, path, &config.data_root(), config.play_history_limit)?;
        if args.database.is_none() && !config.paths.settings.exists() {
            config.save()?;
        }
        return Ok(json!(report));
    }
    let provider = open_provider(&config, &args.database)?;
    if matches!(args.command, Command::Search { .. } | Command::Stats)
        && let Some(message) = default_database_notice(&args.database, provider.database_path())
    {
        eprintln!("{message}");
    }
    let value = match &args.command {
        Command::Search {
            visibility,
            query,
            folder,
            direct_only,
            sample_rate,
            bit_depth,
            channels,
            duration_min,
            duration_max,
            favorites,
            tagged,
            tag,
            exclude_tag,
            color,
            limit,
            offset,
            sort,
            descending,
        } => {
            let order_by = match sort.as_str() {
                "relevance" => SortOrder::Relevance,
                "filename" => SortOrder::Filename,
                "duration" => SortOrder::Duration,
                "sample_rate" => SortOrder::SampleRate,
                "modified" => SortOrder::Modified,
                "channels" => SortOrder::Channels,
                "tags" => SortOrder::Tags,
                _ => anyhow::bail!("Unknown sort field: {sort}"),
            };
            let request = SearchRequest {
                query: query.clone(),
                folder_id: *folder,
                include_subfolders: !direct_only,
                filters: SearchFilters {
                    visibility: (*visibility).into(),
                    sample_rate: *sample_rate,
                    bit_depth: *bit_depth,
                    channels: *channels,
                    duration_min: *duration_min,
                    duration_max: *duration_max,
                    favorites_only: *favorites,
                    tagged_only: *tagged,
                    tag_contains: tag.first().cloned(),
                    include_tags: tag.iter().skip(1).cloned().collect(),
                    exclude_tags: exclude_tag.clone(),
                    color: color.clone(),
                },
                limit: *limit,
                offset: *offset,
                order_by,
                descending: *descending,
            };
            let page = provider.search(&request)?;
            if let Some(message) = empty_library_notice(page.total_count, || {
                provider.stats().map(|stats| stats.audio_files == 0)
            }) {
                eprintln!("{message}");
            }
            json!(page)
        }
        Command::Scan {
            path,
            no_recursive,
            full,
            workers,
        } => {
            let cancellation = ScanCancellation::default();
            let signal = cancellation.clone();
            ctrlc::set_handler(move || signal.cancel())?;
            let options = ScanOptions {
                recursive: !no_recursive,
                incremental: !full,
                batch_size: 200,
                workers: *workers,
                allowed_roots: config.allowed_scan_roots.clone(),
            };
            let service = ScanService::new(provider.clone());
            let mut last_update = Instant::now();
            json!(service.scan(path, &options, &cancellation, |progress| {
                if last_update.elapsed().as_millis() > 500 {
                    eprintln!(
                        "scan: discovered={} processed={} added={} updated={} errors={}",
                        progress.discovered,
                        progress.processed,
                        progress.added,
                        progress.updated,
                        progress.errors
                    );
                    last_update = Instant::now();
                }
            })?)
        }
        Command::Stats => json!(provider.stats()?),
        Command::Folders { parent } => json!(provider.folders(*parent)?),
        Command::Info { id } => json!(provider.get_audio(*id)?),
        Command::Edit {
            id,
            tags,
            color,
            notes,
            favorite,
        } => json!(provider.update_metadata(
            *id,
            &MetadataPatch {
                tags: tags.clone(),
                color: color.clone(),
                notes: notes.clone(),
                favorite: *favorite
            }
        )?),
        Command::EditBatch {
            ids,
            tags,
            tag_mode,
            color,
            notes,
            favorite,
        } => json!(provider.update_metadata_batch(&BatchMetadataRequest {
            ids: ids.clone(),
            tag_mode: (*tag_mode).into(),
            patch: MetadataPatch {
                tags: tags.clone(),
                color: color.clone(),
                notes: notes.clone(),
                favorite: *favorite,
            },
        })?),
        Command::Relink { id, path } => {
            let target = audiolib_core::validate_wav_path(path, &config.allowed_scan_roots)?;
            json!(provider.relink_audio(*id, &target)?)
        }
        Command::Remove { ids, .. } => json!({"removed":provider.remove_audio(ids)?}),
        Command::RemoveFolder { id, .. } => json!({"removed":provider.remove_folder(*id)?}),
        Command::ScanHistory { limit, offset } => json!(provider.scan_history(*limit, *offset)?),
        Command::PlayHistory { limit, offset } => {
            let library_id =
                audiolib_db::local_play_history::local_library_id(provider.database_path())?;
            let history = audiolib_db::local_play_history::LocalPlayHistory::open(
                &config.data_root(),
                &library_id,
                config.play_history_limit,
            )?;
            json!(history.page(*limit, *offset)?)
        }
        Command::Export {
            destination,
            export_format,
        } => json!(provider.export_library(destination, (*export_format).into())?),
        Command::ImportAnnotations { source, .. } => json!(provider.import_annotations(source)?),
        Command::Db {
            command: DbCommand::Status,
        } => json!(provider.library_info()?),
        Command::Db {
            command: DbCommand::Backup { destination },
        } => json!(provider.backup_library(destination)?),
        Command::Db {
            command: DbCommand::Restore { source, .. },
        } => json!(provider.restore_library(source)?),
        Command::Db {
            command: DbCommand::Optimize,
        } => json!(provider.maintain_library(MaintenanceAction::Optimize)?),
        Command::Db {
            command: DbCommand::RebuildSearch { if_needed },
        } => rebuild_search(&provider, *if_needed, &mut |line| eprintln!("{line}"))?,
        Command::Db {
            command: DbCommand::Check,
        } => {
            let report = provider.check_library()?;
            if report.is_ok() {
                json!(report)
            } else {
                let mut failure = audiolib_core::failure("Library integrity check failed");
                failure["error_code"] = json!("INTEGRITY_FAILED");
                failure["data"] = json!(report);
                failure
            }
        }
        _ => unreachable!("transport and import operations returned before opening the library"),
    };
    Ok(value)
}

/// Rollback copies (`audiolib-rollback-*.db`) currently next to the library.
fn rollback_copies(database: &std::path::Path) -> Vec<PathBuf> {
    let directory = database
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    let mut found: Vec<PathBuf> = std::fs::read_dir(directory)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("audiolib-rollback-") && name.ends_with(".db"))
        })
        .collect();
    found.sort();
    found
}

/// `db rebuild-search`: backfill `source_tags_text` (batched, resumable), then bring the
/// search text and FTS index to the current revision with `ensure_source_search_current`
/// (exclusive lease, rollback copy, integrity check). An already-current library is rebuilt
/// anyway as a repair unless `if_needed`. `progress` receives human-readable lines (stderr).
fn rebuild_search(
    provider: &SqliteProvider,
    if_needed: bool,
    progress: &mut dyn FnMut(&str),
) -> anyhow::Result<Value> {
    let total = Instant::now();
    let rows = provider.stats()?.audio_files;
    let before = rollback_copies(provider.database_path());
    progress(&format!(
        "[1/2] Backfilling extracted tags ({rows} files)..."
    ));
    let step = Instant::now();
    let tags_filled = provider.ensure_source_tags_current()?;
    let tags_seconds = step.elapsed().as_secs_f64();
    progress(&format!(
        "[1/2] Tags: {tags_filled} rows updated in {tags_seconds:.1} s"
    ));
    progress(
        "[2/2] Rebuilding the search index (FTS); this can take several minutes on a large library and leaves an audiolib-rollback-*.db copy of the library next to it...",
    );
    let step = Instant::now();
    let mut value = if provider.ensure_source_search_current()? {
        json!({"action": "rebuild_search", "outdated": true, "search_rebuilt": true})
    } else if if_needed {
        json!({"action": "rebuild_search", "outdated": false, "search_rebuilt": false})
    } else {
        let report = provider.maintain_library(MaintenanceAction::RebuildSearch)?;
        let mut value = json!(report);
        value["outdated"] = json!(false);
        value["search_rebuilt"] = json!(true);
        value
    };
    let rebuild_seconds = step.elapsed().as_secs_f64();
    let created: Vec<PathBuf> = rollback_copies(provider.database_path())
        .into_iter()
        .filter(|path| !before.contains(path))
        .collect();
    if value.get("rollback_backup").is_none() {
        value["rollback_backup"] = json!(created.first());
    }
    value["rollback_copies"] = json!(created);
    value["audio_files"] = json!(rows);
    value["tags_backfilled"] = json!(tags_filled);
    value["tags_seconds"] = json!(tags_seconds);
    value["rebuild_seconds"] = json!(rebuild_seconds);
    value["total_seconds"] = json!(total.elapsed().as_secs_f64());
    progress(&if value["search_rebuilt"] == true {
        format!(
            "[2/2] Search index rebuilt in {rebuild_seconds:.1} s (total {:.1} s); delete the audiolib-rollback-*.db copy once the result is verified",
            total.elapsed().as_secs_f64()
        )
    } else {
        "[2/2] Search index is current; nothing to do".to_owned()
    });
    Ok(value)
}

fn csv_cell(value: &Value) -> String {
    let text = match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        _ => value.to_string(),
    };
    format!("\"{}\"", text.replace('"', "\"\""))
}
fn print_result(value: &Value, format: Format) {
    match format {
        Format::Json => println!("{}", serde_json::to_string_pretty(value).unwrap()),
        Format::Table | Format::Csv => {
            let data = &value["data"];
            if let Some(results) = data["results"].as_array() {
                if matches!(format, Format::Csv) {
                    println!(
                        "id,filename,sample_rate,bit_depth,channels,duration,tags,file_path,color,notes,favorite,missing"
                    );
                    for file in results {
                        println!(
                            "{}",
                            [
                                "id",
                                "filename",
                                "sample_rate",
                                "bit_depth",
                                "channels",
                                "duration",
                                "tags",
                                "file_path",
                                "color",
                                "notes",
                                "favorite",
                                "missing"
                            ]
                            .iter()
                            .map(|key| csv_cell(&file[*key]))
                            .collect::<Vec<_>>()
                            .join(",")
                        );
                    }
                } else {
                    println!("ID       RATE    BITS CH   SECONDS  NAME");
                    for file in results {
                        println!(
                            "{:<8} {:<7} {:<4} {:<4} {:<8.2} {}",
                            file["id"],
                            file["sample_rate"],
                            file["bit_depth"],
                            file["channels"],
                            file["duration"].as_f64().unwrap_or(0.0),
                            file["filename"].as_str().unwrap_or("")
                        );
                    }
                    println!("{} matches", data["total_count"]);
                }
            } else {
                println!("{}", serde_json::to_string_pretty(value).unwrap());
            }
        }
    }
}
/// `remote status|test|sync`. Secrets are loaded from the protected secrets file and
/// never printed; only whether they are set.
fn remote_operation(config: &AppConfig, command: &RemoteCommand) -> anyhow::Result<Value> {
    // `--url` overrides the saved address for this run only (normalised, https by default).
    let override_url = match command {
        RemoteCommand::Status { url }
        | RemoteCommand::Test { url }
        | RemoteCommand::Roots { url }
        | RemoteCommand::Capabilities { url }
        | RemoteCommand::Scans { url, .. }
        | RemoteCommand::History { url, .. } => url.as_deref(),
        _ => None,
    };
    let mut overridden = config.data_source.clone();
    if let Some(url) = override_url {
        overridden.server_url = audiolib_core::server_url::normalize_server_url(url)
            .map_err(|error| anyhow::anyhow!("Invalid --url: {error}"))?;
        overridden.mode = audiolib_core::config::DataSourceMode::Server;
    }
    let settings = &overridden;
    let data_root = config.data_root();
    // Per-profile tokens; the saved token belongs to the active (or last used) profile
    // and is also sent to a `--url` override, as before.
    let secrets = audiolib_remote::load_profile_secrets(&data_root, &config.data_source)?;
    let cache = audiolib_remote::RemoteCache::new(&data_root);
    let require_server = || {
        anyhow::ensure!(
            !settings.server_url.trim().is_empty(),
            "No server URL configured; set it in the desktop Data source settings"
        );
        // The access token is optional: without one, requests carry no Authorization header.
        Ok(())
    };
    match command {
        RemoteCommand::Status { .. } => {
            let active = cache.active();
            let address = if settings.server_url.trim().is_empty() {
                Value::Null
            } else {
                match audiolib_core::server_url::ServerAddress::parse(&settings.server_url) {
                    Ok(address) => json!({
                        "base_url": address.base_url(),
                        "api_url": address.api_url(),
                        "scheme": address.scheme.as_str(),
                        "host": address.host,
                        "port": address.effective_port(),
                        "path_prefix": address.prefix,
                    }),
                    Err(error) => json!({ "error": error.to_string() }),
                }
            };
            let https = settings
                .server_url
                .trim()
                .to_ascii_lowercase()
                .starts_with("https://");
            Ok(json!({
                "mode": settings.mode,
                "server_url": settings.server_url,
                "url_override": override_url.is_some(),
                "address": address,
                // Certificate policy for https:// servers and WebDAV (`remote test` reports the
                // actual handshake).
                "tls": {
                    "used": https,
                    "verification": "rustls, OS trust store via rustls-platform-verifier",
                    "trust_store": audiolib_remote::TRUST_STORE,
                    "self_signed_accepted": false,
                },
                "token_saved": !secrets.token.is_empty(),
                "active": config.data_source.active,
                "profiles": config.data_source.profiles.len(),
                // The saved active profile (not an --url override) and its "transcode
                // auditions" choice: off = always /stream, on = /preview pcm/flac.
                "active_profile": config.data_source.active_profile().map(|profile| json!({
                    "id": profile.id,
                    "name": profile.name,
                    "url": profile.server_url,
                    "kind": profile.kind,
                    "transcode": profile.transcode_enabled(),
                })),
                "audition_transcode": config.data_source.audition_transcode(),
                "webdav_username": settings.webdav.username,
                "webdav_password_saved": !secrets.webdav_password.is_empty(),
                "webdav_base_url": settings.webdav.base_url,
                "webdav_root_urls": settings.webdav.root_urls,
                "poll_interval_secs": settings.poll_interval_secs,
                "auto_sync": settings.auto_sync,
                "database": active.as_ref().map(|state| cache.database_path(state)),
                "cache": active,
            }))
        }
        RemoteCommand::Test { .. } => {
            require_server()?;
            let cached = cache.active().map(|state| state.roots).unwrap_or_default();
            let report =
                audiolib_remote::test_connection(settings, &secrets, &cached, Some(&cache), false);
            let mut reply = audiolib_core::success(json!(report));
            // Optional checks (WebDAV) that failed: reported, but the test still succeeds.
            let mut warnings = report.warnings(false);
            if secrets.token.trim().is_empty() {
                warnings.insert(
                    0,
                    "No access token saved; requests carry no Authorization header".to_owned(),
                );
            }
            reply["warnings"] = json!(warnings);
            if !report.ok() {
                reply["success"] = json!(false);
                reply["error"] = json!(report.summary(false));
                reply["error_code"] = json!("REMOTE_TEST_FAILED");
            }
            Ok(reply)
        }
        RemoteCommand::Sync { force, restart } => {
            require_server()?;
            let client = audiolib_remote::RemoteClient::new(&settings.server_url, &secrets.token)?;
            let control = audiolib_remote::SyncControl::new();
            let signal = control.clone();
            ctrlc::set_handler(move || signal.cancel())?;
            let outcome = match audiolib_remote::sync_library_with_options(
                &client,
                &cache,
                &mut |_| {},
                &control,
                audiolib_remote::SyncOptions {
                    force: *force,
                    restart: *restart,
                },
            )? {
                audiolib_remote::SyncRunOutcome::Completed(outcome) => *outcome,
                audiolib_remote::SyncRunOutcome::Paused(_) => {
                    anyhow::bail!("Sync paused; run remote sync to continue")
                }
            };
            prune_after_cli_sync(&cache, &outcome.state);
            Ok(json!({
                "library_id": outcome.state.library_id,
                "revision": outcome.state.revision,
                "downloaded": outcome.downloaded,
                "library_changed": outcome.library_changed,
                "audio_files": outcome.state.audio_files,
                "snapshot_bytes": outcome.state.snapshot_bytes,
                "database": cache.database_path(&outcome.state),
                "elapsed_ms": outcome.elapsed_ms,
            }))
        }
        RemoteCommand::Edit {
            ids,
            tags,
            tag_mode,
            color,
            notes,
            favorite,
        } => {
            require_server()?;
            let (client, state) = remote_writer_parts(settings, &secrets, &cache)?;
            let writer = audiolib_remote::RemoteWriter::new(
                client.clone(),
                cache.clone(),
                state.library_id.clone(),
            );
            let patch = MetadataPatch {
                tags: tags.clone(),
                color: color.clone(),
                notes: notes.clone(),
                favorite: *favorite,
            };
            let tag_mode = TagEditMode::from(*tag_mode);
            let result = if ids.len() == 1 && tag_mode == TagEditMode::Replace {
                writer.update_metadata(ids[0], &patch).map(|outcome| {
                    (
                        "patch",
                        json!({"files": [outcome.value]}),
                        outcome.revision_before,
                        outcome.revision,
                        outcome.cache,
                    )
                })
            } else {
                writer
                    .update_metadata_batch(&BatchMetadataRequest {
                        ids: ids.clone(),
                        patch,
                        tag_mode,
                    })
                    .map(|outcome| {
                        let (report, files) = outcome.value;
                        (
                            "batch",
                            json!({"report": report, "files": files}),
                            outcome.revision_before,
                            outcome.revision,
                            outcome.cache,
                        )
                    })
            };
            remote_write_reply(&client, &cache, result)
        }
        RemoteCommand::Download { ids } => {
            require_server()?;
            let (client, state) = remote_writer_parts(settings, &secrets, &cache)?;
            let snapshot = SqliteProvider::open_read_only(&cache.database_path(&state))?;
            let items = ids
                .iter()
                .map(|id| {
                    Ok(audiolib_remote::DownloadItem {
                        file: snapshot.get_audio(*id)?,
                        source: client.download_source(*id),
                    })
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            let downloads = audiolib_remote::DownloadCache::new(
                config.paths.cache.clone(),
                audiolib_remote::DEFAULT_DOWNLOAD_CACHE_BYTES,
            );
            let cancel = std::sync::atomic::AtomicBool::new(false);
            let paths = downloads.ensure_all(
                &state.library_id,
                &items,
                // The CLI auditions nothing: there is no in-memory copy to reuse.
                None,
                &mut |_| {},
                &cancel,
            )?;
            let files: Vec<Value> = items
                .iter()
                .zip(&paths)
                .map(|(item, path)| {
                    json!({
                        "id": item.file.id,
                        "path": path,
                        "bytes": std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
                    })
                })
                .collect();
            Ok(json!({"library_id": state.library_id, "files": files}))
        }
        RemoteCommand::Profiles => {
            let source = &config.data_source;
            let profiles: Vec<Value> = source
                .profiles
                .iter()
                .map(|profile| {
                    json!({
                        "id": profile.id,
                        "name": profile.name,
                        "url": profile.server_url,
                        "kind": profile.kind,
                        "group": profile.group_key(),
                        "library_id": profile.library_id,
                        "webdav_override": profile.webdav.is_some(),
                        "last_test": profile.last_test,
                        "transcode": profile.transcode_enabled(),
                        "active": source.active.as_deref() == Some(profile.id.as_str()),
                        "token_saved": !secrets.token_for(&profile.id).is_empty(),
                    })
                })
                .collect();
            Ok(json!({
                "active": source.active.as_deref().unwrap_or(match source.mode {
                    audiolib_core::config::DataSourceMode::Local => "local",
                    audiolib_core::config::DataSourceMode::Server => "",
                }),
                "mode": source.mode,
                "server_url": source.server_url,
                "profiles": profiles,
            }))
        }
        RemoteCommand::Roots { .. } => {
            require_server()?;
            let client = audiolib_remote::RemoteClient::new(&settings.server_url, &secrets.token)?;
            Ok(json!({ "roots": client.roots()? }))
        }
        RemoteCommand::Capabilities { .. } => {
            require_server()?;
            let client = audiolib_remote::RemoteClient::new(&settings.server_url, &secrets.token)?;
            let capabilities = client.capabilities()?;
            Ok(json!({
                "supported": capabilities.supported,
                "manage_mode": capabilities.manage_mode,
                "may_manage": capabilities.may_manage(),
                "capabilities": capabilities,
            }))
        }
        RemoteCommand::Scans { id, .. } => {
            require_server()?;
            let client = audiolib_remote::RemoteClient::new(&settings.server_url, &secrets.token)?;
            let scan = match id {
                Some(id) => Some(client.scan_status(*id)?),
                None => client.current_scan()?,
            };
            Ok(json!({ "scan": scan }))
        }
        RemoteCommand::History {
            kind,
            limit,
            offset,
            ..
        } => {
            require_server()?;
            let client = audiolib_remote::RemoteClient::new(&settings.server_url, &secrets.token)?;
            Ok(match kind {
                HistoryKind::Scans => json!({ "scans": client.scan_history(*limit, *offset)? }),
            })
        }
    }
}

/// Client plus the cached snapshot state that server writes and downloads work on.
fn remote_writer_parts(
    settings: &audiolib_core::config::DataSourceSettings,
    secrets: &audiolib_remote::RemoteSecrets,
    cache: &audiolib_remote::RemoteCache,
) -> anyhow::Result<(audiolib_remote::RemoteClient, audiolib_remote::CacheState)> {
    let client = audiolib_remote::RemoteClient::new(&settings.server_url, &secrets.token)?;
    let state = cache
        .active()
        .ok_or_else(|| anyhow::anyhow!("No synced server snapshot; run `remote sync` first"))?;
    let cached_address = audiolib_core::server_url::ServerAddress::parse(&state.server_url)
        .map_err(|_| {
            anyhow::anyhow!("Cached server address is invalid; run `remote sync` first")
        })?;
    let configured_address = audiolib_core::server_url::ServerAddress::parse(client.api_url())?;
    anyhow::ensure!(
        cached_address.scheme == configured_address.scheme
            && cached_address
                .host
                .eq_ignore_ascii_case(&configured_address.host)
            && cached_address.effective_port() == configured_address.effective_port()
            && cached_address.prefix == configured_address.prefix,
        "Cached snapshot belongs to another server; run `remote sync` for the configured server first"
    );
    // IDs belong to this snapshot's library, not merely to its server address. A server
    // can replace its library between CLI invocations while retaining the same URL.
    let library = client.library()?;
    anyhow::ensure!(
        library.library_id == state.library_id,
        "The server library changed since the cached snapshot; run `remote sync` first"
    );
    Ok((client, state))
}

type WriteResult = Result<
    (&'static str, Value, u64, u64, audiolib_remote::CacheUpdate),
    audiolib_remote::RemoteError,
>;

/// The CLI opens no snapshot: drop this library's older snapshot files. A file a running
/// desktop still has open cannot be deleted on Windows and stays; replaced libraries
/// (whose audio cache a desktop may still use) are left to the desktop's post-swap cleanup.
fn prune_after_cli_sync(cache: &audiolib_remote::RemoteCache, state: &audiolib_remote::CacheState) {
    cache.prune(&state.library_id, &[state.database_file.as_str()]);
    if let Err(error) = cache.finish_sync_transfer(state) {
        eprintln!("Published snapshot retained; transfer cleanup deferred: {error}");
    }
}

/// Report a server write; when the cache could not follow (or the outcome is uncertain)
/// run a normal sync so the local snapshot matches the server again.
fn remote_write_reply(
    client: &audiolib_remote::RemoteClient,
    cache: &audiolib_remote::RemoteCache,
    result: WriteResult,
) -> anyhow::Result<Value> {
    let resync = |reason: &str| -> Value {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        match audiolib_remote::sync_library(client, cache, &mut |_| {}, &cancel, false) {
            Ok(outcome) => {
                prune_after_cli_sync(cache, &outcome.state);
                json!({
                    "reason": reason,
                    "revision": outcome.state.revision,
                    "downloaded": outcome.downloaded,
                })
            }
            Err(error) => json!({"reason": reason, "error": error.to_string()}),
        }
    };
    match result {
        Ok((mode, mut reply, before, after, update)) => {
            reply["mode"] = json!(mode);
            reply["revision_before"] = json!(before);
            reply["revision"] = json!(after);
            match update {
                audiolib_remote::CacheUpdate::Applied { revision } => {
                    reply["cache"] = json!({"applied": true, "revision": revision});
                }
                audiolib_remote::CacheUpdate::Resync { reason } => {
                    reply["cache"] = json!({"applied": false, "sync": resync(&reason)});
                }
            }
            Ok(reply)
        }
        Err(error) => {
            if error.write_may_have_committed() {
                let sync = resync("write outcome uncertain");
                anyhow::bail!("{error}; cache re-sync: {sync}");
            }
            Err(error.into())
        }
    }
}

fn main() -> std::process::ExitCode {
    let args = Arguments::parse();
    let start = Instant::now();
    let (mut reply, succeeded) = match operation(&args) {
        Ok(value) if value.get("success").is_some() => {
            let success = value["success"] == true;
            (value, success)
        }
        Ok(value) => (audiolib_core::success(value), true),
        Err(error) => (audiolib_core::failure(&error.to_string()), false),
    };
    reply["execution_time_ms"] = json!(start.elapsed().as_secs_f64() * 1000.0);
    print_result(&reply, args.format);
    if succeeded {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audiolib_core::config::AppPaths;
    use rusqlite::Connection;

    fn isolated_config(root: &std::path::Path) -> AppConfig {
        AppConfig {
            paths: AppPaths {
                database: root.join("library.db"),
                settings: root.join("settings.json"),
                cache: root.join("cache"),
                logs: root.join("logs"),
                legacy_database: None,
                audio_terms: None,
            },
            language: "cn".into(),
            audition_play_on_single_click: false,
            play_history_limit: audiolib_core::config::PLAY_HISTORY_LIMIT_PER_LIBRARY,
            pregenerate_list_waveforms: false,
            hold_list_waveform_batches_after_audition: false,
            max_search_tasks: audiolib_core::config::DEFAULT_MAX_SEARCH_TASKS,
            ui_scale_percent: 0,
            theme: audiolib_core::config::DEFAULT_THEME.into(),
            ui_font_size: audiolib_core::config::DEFAULT_FONT_SIZE,
            list_font_size: audiolib_core::config::DEFAULT_FONT_SIZE,
            audio_table: Default::default(),
            spectral_peaks: Default::default(),
            window_layout: Default::default(),
            playback: json!({"backend":"wasapi","volume":0.25}),
            allowed_scan_roots: Vec::new(),
            data_source: Default::default(),
            data_source_fallback: None,
        }
    }

    #[test]
    fn server_history_reads_only_the_selected_client_store_without_opening_a_library() {
        let root = tempfile::tempdir().unwrap();
        let mut config = isolated_config(root.path());
        config.data_source.mode = audiolib_core::config::DataSourceMode::Server;
        config.data_source.active = Some("history-server".into());
        config
            .data_source
            .profiles
            .push(audiolib_core::profiles::ServerProfile {
                id: "history-server".into(),
                library_id: Some("library-id".into()),
                ..Default::default()
            });
        // A deliberately absent working library proves this path does not open/create it.
        let working = config.paths.database.clone();
        let history = audiolib_db::local_play_history::LocalPlayHistory::open(
            &config.data_root(),
            "server-library-id",
            config.play_history_limit,
        )
        .unwrap();
        history
            .record(Some(7), "remote/one.wav", "one.wav")
            .unwrap();
        let reply = server_local_play_history(&config, 10, 0).unwrap();
        assert_eq!(reply.as_array().unwrap().len(), 1);
        assert_eq!(reply[0]["audio_id"], 7);
        assert!(!working.exists());
    }

    #[test]
    fn desktop_profile_identity_distinguishes_isolated_settings_and_legacy_replies() {
        let directory = tempfile::tempdir().unwrap();
        let first_directory = directory.path().join("first");
        let second_directory = directory.path().join("second");
        std::fs::create_dir_all(&first_directory).unwrap();
        std::fs::create_dir_all(&second_directory).unwrap();
        let first = first_directory.join("settings.json");
        let second = second_directory.join("settings.json");
        std::fs::write(&first, []).unwrap();
        std::fs::write(&second, []).unwrap();
        let reply =
            json!({"success":true,"metadata":{"settings_path":first.canonicalize().unwrap()}});
        assert!(desktop_profile_running(&reply, &first));
        assert!(desktop_profile_running(
            &reply,
            &first_directory.join("..").join("first/settings.json")
        ));
        assert!(!desktop_profile_running(&reply, &second));
        assert!(desktop_profile_running(&json!({"success":true}), &first));
        assert!(!desktop_profile_running(&json!({"success":false}), &first));
        assert!(!desktop_profile_running(&json!({"success":false}), &second));
    }

    #[test]
    fn ordinary_open_rejects_foreign_and_original_legacy_databases_without_changing_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let config = isolated_config(directory.path());
        for (name, schema) in [
            ("foreign.db", "CREATE TABLE unrelated_notes(contents TEXT)"),
            (
                "legacy.db",
                "CREATE TABLE audio_files(id INTEGER PRIMARY KEY,file_path TEXT,filename TEXT)",
            ),
        ] {
            let path = directory.path().join(name);
            let connection = Connection::open(&path).unwrap();
            connection.execute_batch(schema).unwrap();
            drop(connection);
            let before = std::fs::read(&path).unwrap();
            assert!(open_provider(&config, &Some(path.clone())).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), before);
            let connection = Connection::open(&path).unwrap();
            let version: i64 = connection
                .pragma_query_value(None, "user_version", |row| row.get(0))
                .unwrap();
            assert_eq!(version, 0);
        }
        assert!(!config.paths.settings.exists());
    }

    #[test]
    fn missing_explicit_or_saved_library_is_not_created() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = isolated_config(directory.path());
        let explicit = directory.path().join("explicit.db");
        assert!(open_provider(&config, &Some(explicit.clone())).is_err());
        assert!(!explicit.exists());
        config.paths.database = directory.path().join("selected.db");
        // Even missing settings cannot turn a non-default selection into first-run creation.
        assert!(open_provider(&config, &None).is_err());
        assert!(!config.paths.database.exists());
        config.save().unwrap();
        let saved_before = std::fs::read(&config.paths.settings).unwrap();
        assert!(open_provider(&config, &None).is_err());
        assert!(!config.paths.database.exists());
        assert_eq!(std::fs::read(&config.paths.settings).unwrap(), saved_before);
        config.paths.database = directory.path().join("library.db");
        assert!(open_provider(&config, &None).is_err());
        assert!(!config.paths.database.exists());
    }

    #[test]
    fn default_library_and_empty_library_notices_only_apply_when_relevant() {
        let path = std::path::Path::new("D:/data/library.db");
        assert_eq!(
            default_database_notice(&None, path).as_deref(),
            Some("正在使用默认库：D:/data/library.db")
        );
        assert_eq!(default_database_notice(&Some(path.into()), path), None);
        assert_eq!(
            empty_library_notice(0, || Ok(true)),
            Some("当前库为空，可用 --database 指定库路径")
        );
        assert_eq!(empty_library_notice(0, || Ok(false)), None);
        assert_eq!(
            empty_library_notice(3, || panic!("a search with hits must not call stats")),
            None
        );
        assert_eq!(
            empty_library_notice(0, || Err(audiolib_core::Error::Database(
                "advisory lookup failed".into()
            ))),
            None
        );
    }

    #[test]
    fn search_without_matches_reports_only_an_actually_empty_library() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("library.db");
        let provider = SqliteProvider::open(&path).unwrap();
        let request = SearchRequest {
            query: "unmatched".into(),
            filters: SearchFilters {
                favorites_only: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let page = provider.search(&request).unwrap();
        assert_eq!(page.total_count, 0);
        assert!(
            empty_library_notice(page.total_count, || {
                provider.stats().map(|stats| stats.audio_files == 0)
            })
            .is_some()
        );
        let connection = Connection::open(&path).unwrap();
        audiolib_db::register_library_functions(&connection).unwrap();
        connection
            .execute_batch(
                "INSERT INTO audio_files(file_path,path_key,filename)
                 VALUES('populated.wav','populated.wav','populated.wav');",
            )
            .unwrap();
        let page = provider.search(&request).unwrap();
        assert_eq!(page.total_count, 0);
        assert_eq!(
            empty_library_notice(page.total_count, || {
                provider.stats().map(|stats| stats.audio_files == 0)
            }),
            None
        );
        let hit = provider
            .search(&SearchRequest {
                query: "populated".into(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hit.total_count, 1);
        assert_eq!(
            empty_library_notice(hit.total_count, || panic!("stats must remain unused")),
            None
        );
    }

    #[test]
    fn first_default_initialization_persists_preferences_and_valid_libraries_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let config = isolated_config(directory.path());
        let provider = open_provider(&config, &None).unwrap();
        assert_eq!(provider.stats().unwrap().audio_files, 0);
        drop(provider);
        let saved: Value =
            serde_json::from_slice(&std::fs::read(&config.paths.settings).unwrap()).unwrap();
        assert_eq!(saved["language"], "cn");
        assert_eq!(saved["playback"]["volume"], 0.25);
        assert!(open_provider(&config, &None).is_ok());
        assert!(open_provider(&config, &Some(config.paths.database.clone())).is_ok());
    }

    #[test]
    fn first_default_import_copies_legacy_without_modifying_source() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = isolated_config(directory.path());
        let source = directory.path().join("legacy.db");
        let connection = Connection::open(&source).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE audio_files(id INTEGER PRIMARY KEY,file_path TEXT,filename TEXT);
                 INSERT INTO audio_files VALUES(1,'C:/fixture/synthetic.wav','synthetic.wav');",
            )
            .unwrap();
        drop(connection);
        let before = std::fs::read(&source).unwrap();
        config.paths.legacy_database = Some(source.clone());
        let provider = open_provider(&config, &None).unwrap();
        assert_eq!(provider.stats().unwrap().audio_files, 1);
        assert_eq!(std::fs::read(&source).unwrap(), before);
        assert!(config.paths.settings.exists());
    }

    #[test]
    fn rebuild_search_backfills_rebuilds_reports_and_honours_if_needed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("library.db");
        let provider = SqliteProvider::open(&path).unwrap();
        let mut lines = Vec::new();
        // Current library: --if-needed is a no-op without a rollback copy.
        let value = rebuild_search(&provider, true, &mut |l| lines.push(l.to_owned())).unwrap();
        assert_eq!(value["search_rebuilt"], false);
        assert_eq!(value["rollback_copies"], json!([]));
        assert!(
            lines
                .iter()
                .any(|l| l.contains("[2/2]") && l.contains("current"))
        );
        // Without the flag a current library is repaired through the maintenance path.
        let value = rebuild_search(&provider, false, &mut |_| {}).unwrap();
        assert_eq!(value["search_rebuilt"], true);
        assert_eq!(value["outdated"], false);
        assert_eq!(value["integrity"]["sqlite_ok"], true);
        assert_eq!(value["integrity"]["fts_ok"], true);
        assert!(std::path::Path::new(value["rollback_backup"].as_str().unwrap()).is_file());
        // A library from before the search-text revision is migrated and timed.
        Connection::open(&path)
            .unwrap()
            .execute_batch("DROP TABLE library_meta;")
            .unwrap();
        let mut lines = Vec::new();
        let value = rebuild_search(&provider, true, &mut |l| lines.push(l.to_owned())).unwrap();
        assert_eq!(value["search_rebuilt"], true);
        assert_eq!(value["outdated"], true);
        assert_eq!(value["rollback_copies"].as_array().unwrap().len(), 1);
        assert!(value["rebuild_seconds"].as_f64().unwrap() >= 0.0);
        assert!(value["total_seconds"].as_f64().is_some());
        assert!(lines.iter().any(|l| l.contains("[1/2]")));
        assert!(lines.iter().any(|l| l.contains("rebuilt in")));
        assert_eq!(rollback_copies(&path).len(), 2);
    }
}
