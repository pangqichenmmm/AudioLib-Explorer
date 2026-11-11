// app/audiolib-desktop/src/gui_search_benchmark.rs
//! Explicit, isolated own-window search-to-compositor diagnostics. Normal mode is inert.
//! Completion requires decoded WGC pixels; a model callback is never a capture fallback.
use crate::{AppWindow, controller::Controller};
use anyhow::{Context, Result, anyhow, bail, ensure};
use audiolib_audio::{AudioEngine, PlaybackConfig};
use audiolib_core::{
    DataProvider, FileVisibility, Folder, SearchRequest, SortOrder, config::AppConfig,
};
use audiolib_db::SqliteProvider;
use audiolib_windows::gui_capture::{
    self as capture, CapturedMarker, MarkerGeometry, Observer, ObserverStatus, QpcStamp,
};
use serde_json::{Value, json};
use slint::{ComponentHandle, ModelRc, VecModel};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

static ENABLED: AtomicBool = AtomicBool::new(false);
static STATE: Mutex<Option<Measurement>> = Mutex::new(None);

/// Fast guard used before constructing diagnostic-only IDs on production paths.
pub(super) fn enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}

struct Measurement {
    nonce: u32,
    expected: SearchRequest,
    actual: Option<SearchRequest>,
    startup_generation: Option<u64>,
    startup_page: bool,
    startup_folders: bool,
    startup_library_info: Option<bool>,
    calibration_generation: Option<u64>,
    visibility_epoch: u64,
    interactive: bool,
    armed: bool,
    finished: bool,
    input: Option<QpcStamp>,
    input_kind: Option<&'static str>,
    generation: Option<u64>,
    stages: BTreeMap<String, QpcStamp>,
    page: Option<Value>,
    folder_counts: Option<Vec<(i64, u64)>>,
    invalid: Option<String>,
    native_text_events: u64,
    intermediate_requests: u64,
    geometry_events: ArmedGeometryEvents,
}

#[derive(Clone, Copy, Debug)]
struct ArmedGeometry {
    physical_client_size: [u32; 2],
    outer_position: [i32; 2],
    scale_factor: f64,
}

impl ArmedGeometry {
    fn report(self) -> Value {
        json!({"physical_client_size":self.physical_client_size,
            "outer_position_physical":self.outer_position,"scale_factor":self.scale_factor})
    }
}

#[derive(Clone, Copy, Debug)]
enum NativeGeometryEvent {
    Moved([i32; 2]),
    Resized([u32; 2]),
    ScaleFactorChanged(f64),
}

#[derive(Default)]
struct ArmedGeometryEvents {
    baseline: Option<ArmedGeometry>,
    client_layout_guarded: bool,
    total: u64,
    ignored_duplicates: u64,
    delegated_client_layout: u64,
    overflow: u64,
    events: Vec<Value>,
}

impl ArmedGeometryEvents {
    fn record(
        &mut self,
        event: NativeGeometryEvent,
        timestamp: Result<QpcStamp, String>,
    ) -> Option<String> {
        self.total += 1;
        let equal_payload = self.baseline.is_some_and(|baseline| match event {
            NativeGeometryEvent::Moved(position) => position == baseline.outer_position,
            NativeGeometryEvent::Resized(size) => size == baseline.physical_client_size,
            // Preserve conservative rejection even when this payload repeats.
            NativeGeometryEvent::ScaleFactorChanged(_) => false,
        });
        let ignored_duplicate = equal_payload && timestamp.is_ok();
        // WM_SIZE reports client layout, including native menu reconstruction.
        // Only the synchronous own-HWND guard can establish unchanged outer
        // geometry before winit buffers these events. Never infer this from a
        // later GetWindowRect snapshot or discard the original event payload.
        let delegated_client_layout = self.client_layout_guarded
            && self.baseline.is_some()
            && matches!(event, NativeGeometryEvent::Resized(_))
            && !equal_payload
            && timestamp.is_ok();
        if ignored_duplicate {
            self.ignored_duplicates += 1;
        }
        if delegated_client_layout {
            self.delegated_client_layout += 1;
        }
        let (kind, payload) = match event {
            NativeGeometryEvent::Moved(position) => {
                ("Moved", json!({"outer_position_physical":position}))
            }
            NativeGeometryEvent::Resized(size) => ("Resized", json!({"physical_client_size":size})),
            NativeGeometryEvent::ScaleFactorChanged(scale) => {
                ("ScaleFactorChanged", json!({"scale_factor":scale}))
            }
        };
        if self.events.len() < 32 {
            self.events.push(json!({
                "index":self.total,"event":kind,"payload":payload,
                "qpc_ticks":timestamp.as_ref().ok().map(|time| time.ticks),
                "qpc_frequency":timestamp.as_ref().ok().map(|time| time.frequency),
                "qpc_error":timestamp.as_ref().err(),
                "armed_baseline":self.baseline.map(ArmedGeometry::report),
                "ignored_duplicate":ignored_duplicate,
                "delegated_client_layout":delegated_client_layout
            }));
        } else {
            self.overflow += 1;
        }
        (!ignored_duplicate && !delegated_client_layout).then(|| format!(
            "native geometry event rejected during sample: {event:?}; armed baseline={:?}; QPC error={:?}",
            self.baseline, timestamp.err()
        ))
    }
}

fn state(action: impl FnOnce(&mut Measurement)) {
    if !enabled() {
        return;
    }
    if let Some(value) = STATE
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .as_mut()
    {
        action(value);
    }
}

fn invalidate(reason: impl Into<String>) {
    state(|value| {
        if value.invalid.is_none() && !value.finished {
            value.invalid = Some(reason.into());
        }
    });
}

fn stamp(value: &mut Measurement, name: &str) {
    match capture::qpc_now() {
        Ok(time) => {
            value.stages.entry(name.into()).or_insert(time);
        }
        Err(error) => {
            value.invalid.get_or_insert(error);
        }
    }
}

fn marker(ui: &AppWindow, nonce: u32, generation: u64, phase: u8) {
    match capture::encode_marker(nonce, generation, phase) {
        Ok(token) => {
            ui.set_perf_marker_bits(ModelRc::new(VecModel::from(
                capture::marker_bits(&token).to_vec(),
            )));
            ui.window().request_redraw();
        }
        Err(error) => invalidate(error),
    }
}

fn same_request(left: &SearchRequest, right: &SearchRequest) -> bool {
    left.query == right.query
        && left.folder_id == right.folder_id
        && left.include_subfolders == right.include_subfolders
        && left.filters == right.filters
        && left.limit == right.limit
        && left.offset == right.offset
        && left.order_by == right.order_by
        && left.descending == right.descending
}

/// Called before the real native input handler, and for visibility/geometry changes.
pub(super) fn window_event(
    _window: &slint::Window,
    event: &slint::winit_030::winit::event::WindowEvent,
) {
    if !enabled() {
        return;
    }
    use slint::winit_030::winit::{event::ElementState, event::WindowEvent};
    state(|value| {
        if value.finished {
            return;
        }
        if !value.armed {
            if matches!(
                event,
                WindowEvent::Focused(false)
                    | WindowEvent::Occluded(true)
                    | WindowEvent::Moved(_)
                    | WindowEvent::Resized(_)
                    | WindowEvent::ScaleFactorChanged { .. }
            ) {
                value.visibility_epoch = value.visibility_epoch.saturating_add(1);
            }
            return;
        }
        match event {
            WindowEvent::KeyboardInput {
                event,
                is_synthetic: false,
                ..
            } if event.state == ElementState::Pressed
                && event.text.as_ref().is_some_and(|text| !text.is_empty()) =>
            {
                value.native_text_events += 1;
                if !value.interactive {
                    value.invalid.get_or_insert(
                        "native text input interfered with the scripted Slint-dispatch sample"
                            .into(),
                    );
                    return;
                }
                if value.generation.is_some() {
                    value.invalid.get_or_insert(
                        "additional native text arrived after the expected final request".into(),
                    );
                    return;
                }
                if value.input.is_none() {
                    match capture::qpc_now() {
                        Ok(time) => {
                            value.input = Some(time);
                            value.input_kind = Some("native KeyboardInput Pressed text");
                            value.stages.insert("input".into(), time);
                        }
                        Err(error) => {
                            value.invalid.get_or_insert(error);
                        }
                    }
                }
            }
            WindowEvent::Ime(slint::winit_030::winit::event::Ime::Commit(text))
                if !text.is_empty() =>
            {
                value.native_text_events += 1;
                if !value.interactive {
                    value.invalid.get_or_insert(
                        "native IME input interfered with the scripted Slint-dispatch sample"
                            .into(),
                    );
                    return;
                }
                if value.generation.is_some() {
                    value.invalid.get_or_insert(
                        "additional IME text arrived after the expected final request".into(),
                    );
                    return;
                }
                if value.input.is_none() {
                    match capture::qpc_now() {
                        Ok(time) => {
                            value.input = Some(time);
                            value.input_kind = Some("native Ime Commit");
                            value.stages.insert("input".into(), time);
                        }
                        Err(error) => {
                            value.invalid.get_or_insert(error);
                        }
                    }
                }
            }
            WindowEvent::Focused(false) | WindowEvent::Occluded(true) => {
                value
                    .invalid
                    .get_or_insert("window lost focus or became occluded during the sample".into());
            }
            WindowEvent::Moved(position) => {
                if let Some(reason) = value.geometry_events.record(
                    NativeGeometryEvent::Moved([position.x, position.y]),
                    capture::qpc_now(),
                ) {
                    value.invalid.get_or_insert(reason);
                }
            }
            WindowEvent::Resized(size) => {
                if let Some(reason) = value.geometry_events.record(
                    NativeGeometryEvent::Resized([size.width, size.height]),
                    capture::qpc_now(),
                ) {
                    value.invalid.get_or_insert(reason);
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                if let Some(reason) = value.geometry_events.record(
                    NativeGeometryEvent::ScaleFactorChanged(*scale_factor),
                    capture::qpc_now(),
                ) {
                    value.invalid.get_or_insert(reason);
                }
            }
            WindowEvent::CloseRequested | WindowEvent::Destroyed => {
                value
                    .invalid
                    .get_or_insert("window closed before compositor completion".into());
            }
            _ => {}
        }
    });
}

/// Startup statistics must publish before initial WGC calibration or measured input.
pub(super) fn library_info_published(succeeded: bool) {
    state(|value| {
        if !value.armed && !value.finished {
            value.startup_library_info = Some(succeeded);
            stamp(value, "startup_library_info_published");
            if !succeeded {
                value
                    .invalid
                    .get_or_insert("startup library information failed".into());
            }
        }
    });
}

/// Record the validated production request before its debounce/worker dispatch.
pub(super) fn request(ui: &AppWindow, generation: u64, request: &SearchRequest) {
    let mut next = None;
    state(|value| {
        if value.finished {
            return;
        }
        if !value.armed {
            if request.query.is_empty() && value.startup_generation.is_none() {
                value.startup_generation = Some(generation);
                next = Some((value.nonce, generation, capture::PHASE_PENDING));
            }
            return;
        }
        if same_request(request, &value.expected) {
            if value.input.is_none() {
                value
                    .invalid
                    .get_or_insert("request arrived without a measured text input event".into());
            } else if value.generation.replace(generation).is_some() {
                value.invalid.get_or_insert(
                    "the expected final request was submitted more than once".into(),
                );
            }
            value.actual = Some(request.clone());
            stamp(value, "request_validated");
            next = Some((value.nonce, generation, capture::PHASE_PENDING));
        } else {
            value.intermediate_requests += 1;
            if value.generation.is_some() {
                value.invalid.get_or_insert("a different GUI request superseded the expected generation before compositor completion".into());
                value.actual = Some(request.clone());
            } else if request.query == value.expected.query {
                value.invalid.get_or_insert(
                    "actual GUI filters, scope, paging or sort differ from the expected request"
                        .into(),
                );
                value.actual = Some(request.clone());
            }
        }
    });
    if let Some((nonce, generation, phase)) = next {
        marker(ui, nonce, generation, phase);
    }
}

/// Record actual worker entry, after debounce and source/save barriers.
pub(super) fn worker_started(generation: u64, facets: bool) {
    state(|value| {
        if value.generation == Some(generation) && !value.finished {
            stamp(
                value,
                if facets {
                    "facets_worker_started"
                } else {
                    "page_worker_started"
                },
            );
        }
    });
}

/// Record worker completion independently of later event-loop model publication.
pub(super) fn worker_completed(generation: u64, facets: bool) {
    state(|value| {
        if value.generation == Some(generation) && !value.finished {
            stamp(
                value,
                if facets {
                    "facets_worker_completed"
                } else {
                    "page_worker_completed"
                },
            );
        }
    });
}

/// Called only after assigning the real result-page model on the event loop.
pub(super) fn page_published(
    ui: &AppWindow,
    generation: u64,
    total_count: u64,
    returned_count: usize,
    ids: Vec<i64>,
    execution_ms: f64,
    cache_hit: bool,
) {
    let mut next = None;
    state(|value| {
        if value.finished {
            return;
        }
        if !value.armed && value.startup_generation == Some(generation) {
            value.startup_page = true;
            if value.calibration_generation.is_none() {
                next = Some((
                    value.nonce,
                    generation,
                    if value.startup_folders {
                        capture::PHASE_ALL
                    } else {
                        capture::PHASE_PAGE
                    },
                ));
            }
        } else if value.generation == Some(generation) {
            stamp(value, "page_model_published");
            value.page = Some(
                json!({"total_count":total_count,"returned_count":returned_count,"result_ids":ids,"search_execution_ms":execution_ms,"search_ranking_cache_hit":cache_hit}),
            );
            if cache_hit {
                value
                    .invalid
                    .get_or_insert("first final query unexpectedly hit the ranking cache".into());
            }
            next = Some((
                value.nonce,
                generation,
                if value.folder_counts.is_some() {
                    capture::PHASE_ALL
                } else {
                    capture::PHASE_PAGE
                },
            ));
        }
    });
    if let Some((nonce, generation, phase)) = next {
        marker(ui, nonce, generation, phase);
    }
}

/// Called after the matching directory model is assigned, not merely queued.
pub(super) fn folders_published(ui: &AppWindow, generation: u64, folders: Arc<Vec<Folder>>) {
    let mut next = None;
    state(|value| {
        if value.finished {
            return;
        }
        if !value.armed && value.startup_generation == Some(generation) {
            value.startup_folders = true;
            if value.startup_page && value.calibration_generation.is_none() {
                next = Some((value.nonce, generation, capture::PHASE_ALL));
            }
        } else if value.generation == Some(generation) {
            stamp(value, "folder_model_published");
            let mut counts = folders
                .iter()
                .map(|folder| (folder.id, folder.file_count))
                .collect::<Vec<_>>();
            counts.sort_unstable_by_key(|&(id, _)| id);
            value.folder_counts = Some(counts);
            if value.page.is_some() {
                next = Some((value.nonce, generation, capture::PHASE_ALL));
            }
        }
    });
    if let Some((nonce, generation, phase)) = next {
        marker(ui, nonce, generation, phase);
    }
}

struct Options {
    expected: SearchRequest,
    source: PathBuf,
    report: PathBuf,
    interactive: bool,
    chinese: bool,
    pointer_overlay_process: Option<u32>,
}

impl Options {
    fn parse(arguments: &[String]) -> Result<Self> {
        let mut request = None;
        let mut source = None;
        let mut report = None;
        let mut interactive = false;
        let mut chinese = true;
        let mut pointer_overlay_process = None;
        let mut index = 0;
        while index < arguments.len() {
            let argument = arguments[index].as_str();
            match argument {
                "--gui-search-benchmark"
                | "--gui-search-database"
                | "--gui-search-report"
                | "--gui-search-language"
                | "--gui-search-pointer-overlay-process" => {
                    index += 1;
                    let value = arguments
                        .get(index)
                        .ok_or_else(|| anyhow!("{argument} requires a value"))?;
                    match argument {
                        "--gui-search-benchmark" => {
                            ensure!(
                                request.replace(value.clone()).is_none(),
                                "duplicate benchmark request"
                            );
                        }
                        "--gui-search-database" => {
                            ensure!(
                                source.replace(PathBuf::from(value)).is_none(),
                                "duplicate source database"
                            );
                        }
                        "--gui-search-report" => {
                            ensure!(
                                report.replace(PathBuf::from(value)).is_none(),
                                "duplicate report path"
                            );
                        }
                        "--gui-search-pointer-overlay-process" => {
                            let process_id = value
                                .parse::<u32>()
                                .context("pointer overlay process must be a u32 PID")?;
                            ensure!(
                                process_id != 0,
                                "pointer overlay process PID must be nonzero"
                            );
                            ensure!(
                                pointer_overlay_process.replace(process_id).is_none(),
                                "duplicate pointer overlay process"
                            );
                        }
                        _ => {
                            ensure!(value == "cn" || value == "en", "language must be cn or en");
                            chinese = value == "cn";
                        }
                    }
                }
                "--gui-search-interactive" => {
                    ensure!(!interactive, "duplicate interactive flag");
                    interactive = true;
                }
                _ => bail!("unknown GUI search diagnostic argument: {argument}"),
            }
            index += 1;
        }
        let raw = request.context("--gui-search-benchmark requires a JSON SearchRequest")?;
        ensure!(raw.len() <= 65_536, "benchmark request exceeds 64 KiB");
        let expected: SearchRequest =
            serde_json::from_str(&raw).context("Parse benchmark SearchRequest")?;
        validate_request(&expected)?;
        Ok(Self {
            expected,
            source: source.context("--gui-search-database is required")?,
            report: report.context("--gui-search-report is required")?,
            interactive,
            chinese,
            pointer_overlay_process,
        })
    }
}

fn validate_request(request: &SearchRequest) -> Result<()> {
    request.validate()?;
    ensure!(
        !request.query.trim().is_empty(),
        "a text-input benchmark requires a nonempty query"
    );
    ensure!(
        request.folder_id.is_none()
            && request.include_subfolders
            && request.offset == 0
            && request.limit == 200,
        "GUI benchmark requires cross-folder scope, include_subfolders=true, offset=0 and limit=200"
    );
    ensure!(
        request
            .filters
            .sample_rate
            .is_none_or(|value| [44_100, 48_000, 88_200, 96_000, 192_000].contains(&value)),
        "sample rate is not represented by the production filter control"
    );
    ensure!(
        request
            .filters
            .bit_depth
            .is_none_or(|value| [16, 24, 32].contains(&value)),
        "bit depth is not represented by the production filter control"
    );
    ensure!(
        request
            .filters
            .channels
            .is_none_or(|value| [1, 2, 4, 6, 8].contains(&value)),
        "channel count is not represented by the production filter control"
    );
    Ok(())
}

fn preset_filters(ui: &AppWindow, request: &SearchRequest) {
    ui.set_visibility_index(match request.filters.visibility {
        FileVisibility::Present => 0,
        FileVisibility::Missing => 1,
        FileVisibility::All => 2,
    });
    ui.set_sample_filter(
        request
            .filters
            .sample_rate
            .and_then(|value| {
                [0, 44_100, 48_000, 88_200, 96_000, 192_000]
                    .iter()
                    .position(|&item| item == value)
            })
            .unwrap_or(0) as i32,
    );
    ui.set_bit_filter(
        request
            .filters
            .bit_depth
            .and_then(|value| [0, 16, 24, 32].iter().position(|&item| item == value))
            .unwrap_or(0) as i32,
    );
    ui.set_channel_filter(
        request
            .filters
            .channels
            .and_then(|value| [0, 1, 2, 4, 6, 8].iter().position(|&item| item == value))
            .unwrap_or(0) as i32,
    );
    ui.set_duration_min(
        request
            .filters
            .duration_min
            .map(|value| value.to_string())
            .unwrap_or_default()
            .into(),
    );
    ui.set_duration_max(
        request
            .filters
            .duration_max
            .map(|value| value.to_string())
            .unwrap_or_default()
            .into(),
    );
    ui.set_favorites_only(request.filters.favorites_only);
    ui.set_tagged_only(request.filters.tagged_only);
    // The canonical term text quotes separators, so the GUI rebuilds the same tag fields.
    ui.set_tag_filter(
        audiolib_core::tag_filter::format_tag_terms(&audiolib_core::tag_filter::tag_terms_of(
            &request.filters,
        ))
        .into(),
    );
    ui.set_color_filter(request.filters.color.clone().unwrap_or_default().into());
    ui.set_sort_index(match request.order_by {
        SortOrder::Relevance => 0,
        SortOrder::Filename => 1,
        SortOrder::Duration => 2,
        SortOrder::SampleRate => 3,
        SortOrder::Channels => 4,
        SortOrder::Tags => 5,
        SortOrder::Modified => 6,
    });
    ui.set_sort_descending(request.descending);
}

#[derive(Default)]
struct Observation {
    observer: Option<Observer>,
    #[cfg(windows)]
    native_geometry_guard: Option<audiolib_windows::gui_geometry::NativeGeometryGuard>,
    native_geometry_history: Option<Value>,
    final_native_geometry: Option<Value>,
    deferred_layout_frames: u64,
    hwnd: usize,
    geometry: Option<MarkerGeometry>,
    ready: bool,
    calibrated: Option<CapturedMarker>,
    calibration_generation: Option<u64>,
    calibration_stamp: Option<QpcStamp>,
    calibration_epoch: u64,
    visibility_epoch_seen: u64,
    foreground_since: Option<Instant>,
    armed_at: Option<Instant>,
    input_seen_at: Option<Instant>,
    dropped_when_armed: u64,
    final_frame: Option<CapturedMarker>,
    previous_generation_frames: u64,
    close_at: Option<Instant>,
    scale_factor: f64,
    logical_size: [f64; 2],
    outer_position: [i32; 2],
    last_inspection: Option<capture::WindowInspection>,
    armed_raw_intersecting_windows_max: u32,
    armed_recognized_cursor_overlays_max: u32,
    armed_potential_occluders_max: u32,
    hold_until: Option<Instant>,
    post_measurement_hold_ms: u64,
    observer_restarts: u64,
    telemetry: CaptureTelemetry,
    marker_parent_offset: [f32; 2],
    marker_actual_client_position: [f32; 2],
    memory: MemoryObservations,
    work_area_adjustment: Option<Value>,
    prearm_progress_at: Option<Instant>,
    prearm_progress_gates: Option<PrearmGates>,
}

#[derive(Debug, PartialEq, Eq)]
struct PrearmGates {
    startup_page: bool,
    startup_folders: bool,
    startup_library_info: Option<bool>,
    ready: bool,
    calibration_generation: Option<u64>,
    calibrated: bool,
    // own process, foreground, visible, noniconic, uncloaked, input desktop
    visibility_checks: Option<[bool; 6]>,
    coverage_complete: Option<bool>,
    // frame/client monitor, then frame/client workarea uncovered physical pixels
    uncovered_pixels: Option<[u64; 4]>,
    unknown_occluders: Option<u32>,
}

fn log_prearm_progress(observation: &mut Observation, started: Instant) {
    if observation.armed_at.is_some() {
        return;
    }
    let mut startup = (false, false, None);
    state(|value| {
        startup = (
            value.startup_page,
            value.startup_folders,
            value.startup_library_info,
        )
    });
    let inspection = observation.last_inspection;
    let gates = PrearmGates {
        startup_page: startup.0,
        startup_folders: startup.1,
        startup_library_info: startup.2,
        ready: observation.ready,
        calibration_generation: observation.calibration_generation,
        calibrated: observation.calibrated.is_some(),
        visibility_checks: inspection.map(|value| {
            let visibility = value.visibility;
            [
                visibility.own_process,
                visibility.foreground,
                visibility.visible,
                !visibility.iconic,
                visibility.cloaked == Some(0),
                visibility.input_desktop_accessible,
            ]
        }),
        coverage_complete: inspection.map(|value| {
            value.desktop_coverage.enumeration_complete
                && value.desktop_coverage.calculation_complete
        }),
        uncovered_pixels: inspection.map(|value| {
            let coverage = value.desktop_coverage;
            [
                coverage.frame_uncovered_monitor_pixels,
                coverage.client_uncovered_monitor_pixels,
                coverage.frame_uncovered_work_area_pixels,
                coverage.client_uncovered_work_area_pixels,
            ]
        }),
        unknown_occluders: inspection.map(|value| value.potential_occluders),
    };
    let changed = observation.prearm_progress_gates.as_ref() != Some(&gates);
    let should_log = observation.prearm_progress_at.is_none_or(|last| {
        last.elapsed() >= Duration::from_secs(2)
            || (changed && last.elapsed() >= Duration::from_millis(250))
    });
    if should_log {
        eprintln!(
            "GUI calibration prearm: elapsed_ms={} gates={gates:?} decoded_frames={} rejected_frames={} last_rejection={:?}",
            started.elapsed().as_millis(),
            observation.telemetry.decoded_frames,
            observation.telemetry.prearm_rejected_frames,
            observation.telemetry.last_prearm_rejected_frame
        );
        observation.prearm_progress_at = Some(Instant::now());
        observation.prearm_progress_gates = Some(gates);
    }
}

fn adjust_initial_work_area(ui: &AppWindow, observation: &mut Observation) -> Result<bool> {
    if observation.work_area_adjustment.is_some() {
        return Ok(false);
    }
    let (hwnd, scale, size) = own_window(ui)?;
    let before = ui.window().position();
    observation.scale_factor = scale;
    observation.logical_size = [f64::from(size[0]) / scale, f64::from(size[1]) / scale];
    observation.outer_position = [before.x, before.y];
    observation.work_area_adjustment = Some(json!({
        "attempted":true,"outer_position_before_physical":[before.x,before.y],
        "physical_client_size_before":size,"scale_factor_before":scale,
        "resized":false,"applied_once_before_observer":true
    }));
    let adjustment = match capture::window_work_area_adjustment(hwnd) {
        Ok(adjustment) => adjustment,
        Err(error) => {
            observation
                .work_area_adjustment
                .as_mut()
                .expect("placement record initialized")["error"] = json!(error);
            bail!("initial window cannot fit the workarea at its preferred size: {error}");
        }
    };
    let x = before
        .x
        .checked_add(adjustment.delta[0])
        .context("initial workarea x adjustment overflow")?;
    let y = before
        .y
        .checked_add(adjustment.delta[1])
        .context("initial workarea y adjustment overflow")?;
    let record = observation
        .work_area_adjustment
        .as_mut()
        .expect("placement record initialized");
    record["basis"] = json!(adjustment);
    record["requested_outer_position_physical"] = json!([x, y]);
    let moved = adjustment.delta != [0, 0];
    record["move_requested"] = moved.into();
    if moved {
        // Own diagnostic window only, before observer/input. Do not resize or
        // weaken the independent strict frame/client workarea coverage checks.
        ui.window().set_position(slint::PhysicalPosition::new(x, y));
        ui.window().request_redraw();
    }
    Ok(moved)
}

#[derive(Default)]
struct MemoryObservations {
    baseline_before_arming: Option<Value>,
    last_sample_at: Option<Instant>,
    samples_after_input: u64,
    sampled_current_working_set_max_bytes: Option<u64>,
    sampled_private_usage_max_bytes: Option<u64>,
    errors: u64,
    last_error: Option<String>,
}

impl MemoryObservations {
    fn read(&mut self) -> Option<capture::ProcessMemoryUsage> {
        match capture::process_memory_usage() {
            Ok(memory) => Some(memory),
            Err(error) => {
                self.errors += 1;
                self.last_error = Some(error);
                None
            }
        }
    }

    fn sample_after_input(&mut self, force: bool) {
        if !force
            && self
                .last_sample_at
                .is_some_and(|time| time.elapsed() < Duration::from_millis(200))
        {
            return;
        }
        self.last_sample_at = Some(Instant::now());
        if let Some(memory) = self.read() {
            self.samples_after_input += 1;
            self.sampled_current_working_set_max_bytes = Some(
                self.sampled_current_working_set_max_bytes
                    .unwrap_or(0)
                    .max(memory.current_working_set_bytes),
            );
            self.sampled_private_usage_max_bytes = Some(
                self.sampled_private_usage_max_bytes
                    .unwrap_or(0)
                    .max(memory.private_usage_bytes),
            );
        }
    }
}

#[derive(Default)]
struct CaptureTelemetry {
    ready_events: u64,
    ready_content_size: Option<[u32; 2]>,
    ready_qpc_frequency: Option<i64>,
    prearm_rejected_frames: u64,
    last_prearm_rejected_frame: Option<Value>,
    decoded_frames: u64,
    last_decoded_frame: Option<Value>,
}

impl CaptureTelemetry {
    fn status(&mut self, status: &ObserverStatus, armed: bool) {
        match status {
            ObserverStatus::Ready {
                content_size,
                qpc_frequency,
            } => {
                self.ready_events += 1;
                self.ready_content_size = Some(*content_size);
                self.ready_qpc_frequency = Some(*qpc_frequency);
            }
            ObserverStatus::RejectedFrame {
                frame_time_100ns,
                reason,
            } if !armed => {
                self.prearm_rejected_frames += 1;
                self.last_prearm_rejected_frame = Some(json!({
                    "frame_system_relative_time_100ns":frame_time_100ns,"reason":reason
                }));
            }
            _ => {}
        }
    }

    fn decoded(&mut self, frame: &CapturedMarker) {
        self.decoded_frames += 1;
        self.last_decoded_frame = Some(frame_json(frame));
    }
}

fn clear_calibration(observation: &mut Observation) {
    observation.calibrated = None;
    observation.calibration_generation = None;
    observation.calibration_stamp = None;
    observation.foreground_since = None;
    state(|value| value.calibration_generation = None);
}

fn stop_observer(observation: &mut Observation) {
    if let Some(observer) = observation.observer.take() {
        // Retain queued diagnostic evidence even when setup geometry changed.
        for status in observer.status.try_iter() {
            observation
                .telemetry
                .status(&status, observation.armed_at.is_some());
        }
        for frame in observer.frames.try_iter() {
            observation.telemetry.decoded(&frame);
        }
        drop(observer);
    }
}

fn own_window(ui: &AppWindow) -> Result<(usize, f64, [u32; 2])> {
    use slint::winit_030::{
        WinitWindowAccessor,
        winit::raw_window_handle::{HasWindowHandle, RawWindowHandle},
    };
    ui.window()
        .with_winit_window(|window| {
            let handle = window
                .window_handle()
                .map_err(|error| anyhow!(error.to_string()))?;
            let RawWindowHandle::Win32(handle) = handle.as_raw() else {
                bail!("GUI capture requires a native Win32 window");
            };
            let size = window.inner_size();
            Ok((
                handle.hwnd.get() as usize,
                window.scale_factor(),
                [size.width, size.height],
            ))
        })
        .context("native winit window is not available")?
}

fn start_observer(ui: &AppWindow, observation: &mut Observation) -> Result<()> {
    let (hwnd, scale, size) = own_window(ui)?;
    let cell_width = (3.0 * scale).ceil().max(2.0);
    let cell_height = (6.0 * scale).ceil().max(2.0);
    let left = f64::from(size[0]) - 64.0 * cell_width - 4.0;
    let top = f64::from(size[1]) - 2.0 * cell_height - 4.0;
    ensure!(
        left >= 0.0 && top >= 0.0,
        "native client area cannot contain the marker"
    );
    ui.set_perf_marker_cell_width((cell_width / scale) as f32);
    ui.set_perf_marker_cell_height((cell_height / scale) as f32);
    ui.set_perf_marker_x((left / scale) as f32);
    ui.set_perf_marker_y((top / scale) as f32);
    let geometry = capture::client_marker_roi(
        hwnd,
        left / scale,
        top / scale,
        cell_width / scale,
        cell_height / scale,
        scale,
    )
    .map_err(|error| anyhow!(error))?;
    observation.observer =
        Some(Observer::start(hwnd, geometry.roi).map_err(|error| anyhow!(error))?);
    observation.hwnd = hwnd;
    observation.geometry = Some(geometry);
    observation.scale_factor = scale;
    observation.logical_size = [f64::from(size[0]) / scale, f64::from(size[1]) / scale];
    let position = ui.window().position();
    observation.outer_position = [position.x, position.y];
    if let Some(adjustment) = observation.work_area_adjustment.as_mut() {
        adjustment["actual_outer_position_at_observer_setup_physical"] =
            json!([position.x, position.y]);
        adjustment["actual_coverage_at_observer_setup"] =
            json!(geometry.inspection.desktop_coverage);
    }
    ui.window().request_redraw();
    Ok(())
}

fn geometry_unchanged(original: MarkerGeometry, current: capture::WindowInspection) -> bool {
    original.inspection.client_size == current.client_size
        && original.inspection.client_origin_in_capture == current.client_origin_in_capture
        && original.inspection.expected_capture_size == current.expected_capture_size
}

fn arm_native_geometry(observation: &mut Observation) -> Result<()> {
    #[cfg(windows)]
    {
        let guard = audiolib_windows::gui_geometry::NativeGeometryGuard::arm(observation.hwnd, 256)
            .map_err(|error| anyhow!(error))?;
        let history = guard.history().map_err(|error| anyhow!(error))?;
        let calibrated = observation.geometry.context("capture geometry missing")?;
        let client = history.baseline.client_screen;
        let frame = history.baseline.frame_rect;
        ensure!(
            [
                i64::from(client.right) - i64::from(client.left),
                i64::from(client.bottom) - i64::from(client.top)
            ] == calibrated.inspection.client_size.map(i64::from)
                && [
                    i64::from(frame.right) - i64::from(frame.left),
                    i64::from(frame.bottom) - i64::from(frame.top)
                ] == calibrated.inspection.expected_capture_size.map(i64::from),
            "synchronous native geometry differs from compositor calibration"
        );
        ensure!(
            [
                i64::from(client.left) - i64::from(frame.left),
                i64::from(client.top) - i64::from(frame.top)
            ] == calibrated
                .inspection
                .client_origin_in_capture
                .map(i64::from),
            "synchronous native client origin differs from compositor calibration"
        );
        observation.native_geometry_guard = Some(guard);
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = observation;
        bail!("synchronous native geometry diagnostics require Windows")
    }
}

fn check_native_geometry(observation: &Observation) -> Result<()> {
    #[cfg(windows)]
    {
        let guard = observation
            .native_geometry_guard
            .as_ref()
            .context("synchronous native geometry guard missing")?;
        let history = guard.history().map_err(|error| anyhow!(error))?;
        ensure!(
            history.fatal.is_none(),
            "native geometry history failed: {:?}",
            history.fatal
        );
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = observation;
        bail!("synchronous native geometry diagnostics require Windows")
    }
}

fn final_native_geometry(observation: &Observation, frame_time: i64) -> Result<Option<Value>> {
    #[cfg(windows)]
    {
        use audiolib_windows::gui_geometry::own_window_geometry;
        let guard = observation
            .native_geometry_guard
            .as_ref()
            .context("synchronous native geometry guard missing")?;
        let before = own_window_geometry(observation.hwnd)
            .map_err(|error| anyhow!("native final geometry before: {error:?}"))?;
        if !guard
            .frame_after_restore(frame_time)
            .map_err(|error| anyhow!(error))?
        {
            return Ok(None);
        }
        let after = own_window_geometry(observation.hwnd)
            .map_err(|error| anyhow!("native final geometry after: {error:?}"))?;
        let history = guard.history().map_err(|error| anyhow!(error))?;
        if !history
            .frame_after_restore(frame_time)
            .map_err(|error| anyhow!("latest native frame geometry: {error:?}"))?
        {
            return Ok(None);
        }
        ensure!(
            history.fatal.is_none() && before == history.baseline && after == history.baseline,
            "native geometry changed around final frame qualification"
        );
        Ok(Some(json!({"before":before,"after":after,
            "frame_at_or_after_last_client_restore":true,
            "last_client_restore":history.last_client_restore,
            "snapshot_boundary":"qualification time, not historical frame metadata"})))
    }
    #[cfg(not(windows))]
    {
        let _ = (observation, frame_time);
        bail!("synchronous native geometry diagnostics require Windows")
    }
}

fn stop_native_geometry(observation: &mut Observation) -> Result<()> {
    #[cfg(windows)]
    if let Some(mut guard) = observation.native_geometry_guard.take() {
        match guard.stop() {
            Ok(history) => {
                observation.native_geometry_history = Some(json!(history));
                ensure!(
                    history.fatal.is_none(),
                    "native geometry history failed on stop: {:?}",
                    history.fatal
                );
                ensure!(
                    !history.client_layout_pending,
                    "native client layout was not restored before stop"
                );
                if let Some(frame) = observation.final_frame.as_ref() {
                    ensure!(
                        history
                            .frame_after_restore(frame.frame_time_100ns)
                            .map_err(|error| anyhow!("stopped native frame geometry: {error:?}"))?,
                        "final frame predates the last native client-layout restoration"
                    );
                }
            }
            Err(error) => {
                observation.native_geometry_history = Some(json!({
                    "stop_error":error,"history":guard.history().ok()
                }));
                bail!(error);
            }
        }
    }
    #[cfg(not(windows))]
    let _ = observation;
    Ok(())
}

fn tick(
    ui: &AppWindow,
    observation: &mut Observation,
    expected: &SearchRequest,
    interactive: bool,
    source_text: &str,
    started: Instant,
    pointer_policy: Option<&capture::PointerOverlayPolicy>,
) -> Result<bool> {
    log_prearm_progress(observation, started);
    if observation.observer.is_none() {
        let mut startup_ready = false;
        let mut failure = None;
        state(|value| {
            startup_ready = value.startup_library_info == Some(true)
                && value.startup_page
                && value.startup_folders;
            failure = value.invalid.clone();
        });
        if let Some(error) = failure {
            bail!(error);
        }
        if !startup_ready {
            ensure!(
                started.elapsed() < Duration::from_secs(120),
                "startup library information or empty models timed out"
            );
            return Ok(false);
        }
        if adjust_initial_work_area(ui, observation)? {
            // Give the native move/DPI events an event-loop turn before setting
            // the fixed capture ROI. This adjustment is never repeated.
            return Ok(false);
        }
        start_observer(ui, observation)?;
    }
    let geometry = observation.geometry.context("capture geometry missing")?;
    let inspection = capture::inspect_window_with_pointer_policy(observation.hwnd, pointer_policy)
        .map_err(|error| anyhow!(error))?;
    observation.last_inspection = Some(inspection);
    observation.marker_parent_offset =
        [ui.get_perf_marker_parent_x(), ui.get_perf_marker_parent_y()];
    observation.marker_actual_client_position = [
        ui.get_perf_marker_actual_client_x(),
        ui.get_perf_marker_actual_client_y(),
    ];
    let armed = observation.armed_at.is_some();
    let mut input_started = false;
    state(|value| input_started = value.input.is_some());
    if armed && input_started {
        observation.memory.sample_after_input(false);
    }
    let (hwnd, scale, _) = own_window(ui)?;
    let position = ui.window().position();
    if hwnd != observation.hwnd
        || scale.to_bits() != observation.scale_factor.to_bits()
        || [position.x, position.y] != observation.outer_position
        || geometry.inspection.expected_capture_size != inspection.expected_capture_size
        || (!armed && !geometry_unchanged(geometry, inspection))
    {
        ensure!(
            !armed,
            "native window geometry or DPI changed during the sample"
        );
        stop_observer(observation);
        clear_calibration(observation);
        observation.ready = false;
        observation.observer_restarts += 1;
        start_observer(ui, observation)?;
        return Ok(false);
    }
    if armed {
        check_native_geometry(observation)?;
    }
    let observer = observation
        .observer
        .as_ref()
        .context("capture observer missing")?;
    if let Some(error) = observer.failure() {
        bail!("WGC capture failed: {error}");
    }
    ensure!(observer.is_running(), "WGC capture worker stopped");
    if armed {
        observation.armed_raw_intersecting_windows_max = observation
            .armed_raw_intersecting_windows_max
            .max(inspection.raw_intersecting_windows);
        observation.armed_recognized_cursor_overlays_max = observation
            .armed_recognized_cursor_overlays_max
            .max(inspection.recognized_cursor_overlays);
        observation.armed_potential_occluders_max = observation
            .armed_potential_occluders_max
            .max(inspection.potential_occluders);
        ensure!(
            inspection.necessary_checks_pass(),
            "foreground/visibility/input-desktop/occlusion eligibility failed during the sample"
        );
        ensure!(
            ui.get_database_path().as_str() == source_text,
            "GUI data source changed during the sample"
        );
        ensure!(
            observer.dropped_messages() == observation.dropped_when_armed,
            "WGC output queue overflowed during the sample"
        );
    }
    for status in observer.status.try_iter() {
        observation.telemetry.status(&status, armed);
        match status {
            ObserverStatus::Ready { content_size, .. } => {
                ensure!(
                    content_size == geometry.inspection.expected_capture_size,
                    "WGC capture origin/size did not match native calibration"
                );
                observation.ready = true;
            }
            ObserverStatus::RejectedFrame { reason, .. } if armed => {
                bail!("WGC rejected an armed frame: {reason}")
            }
            ObserverStatus::Failed(error) => bail!("WGC capture failed: {error}"),
            ObserverStatus::Stopped => bail!("WGC capture stopped before completion"),
            ObserverStatus::RejectedFrame { .. } => {}
        }
    }
    if !armed {
        let mut epoch = None;
        state(|value| epoch = Some((value.nonce, value.visibility_epoch)));
        let (nonce, visibility_epoch) = epoch.context("measurement state missing")?;
        if !inspection.necessary_checks_pass()
            || visibility_epoch != observation.visibility_epoch_seen
        {
            // Keep the observer and cumulative telemetry, but require fresh pixels.
            observation.calibrated = None;
            observation.calibration_generation = None;
            observation.calibration_stamp = None;
            observation.foreground_since = None;
            observation.visibility_epoch_seen = visibility_epoch;
            state(|value| value.calibration_generation = None);
        }
        if inspection.necessary_checks_pass()
            && observation.ready
            && observation.calibration_generation.is_none()
        {
            observation.calibration_epoch = observation
                .calibration_epoch
                .checked_add(1)
                .context("calibration epoch overflow")?;
            let generation = u64::MAX
                .checked_sub(observation.calibration_epoch)
                .context("calibration generation overflow")?;
            let timestamp = capture::qpc_now().map_err(|error| anyhow!(error))?;
            observation.calibration_generation = Some(generation);
            observation.calibration_stamp = Some(timestamp);
            state(|value| {
                value.calibration_generation = Some(generation);
                value
                    .stages
                    .insert("foreground_calibration_epoch".into(), timestamp);
            });
            // A new token changes actual pixels even if becoming foreground caused no damage.
            marker(ui, nonce, generation, capture::PHASE_ALL);
        }
    }
    for frame in observer.frames.try_iter() {
        observation.telemetry.decoded(&frame);
        ensure!(
            frame.content_size == geometry.inspection.expected_capture_size,
            "captured frame changed size"
        );
        let mut identity = None;
        state(|value| identity = Some((value.nonce, value.generation, value.input)));
        let (nonce, generation, input) = identity.context("measurement state missing")?;
        ensure!(
            frame.nonce == nonce,
            "captured marker nonce does not belong to this run"
        );
        if armed {
            ensure!(
                frame.visibility_before.necessary_checks_pass()
                    && frame.visibility_after.necessary_checks_pass(),
                "captured frame failed foreground/desktop/visibility checks"
            );
            ensure!(
                frame.dropped_output_messages == observation.dropped_when_armed,
                "captured frame reports a dropped WGC message"
            );
            if Some(frame.generation) == generation && frame.phase == capture::PHASE_ALL {
                let input = input.context("completion marker arrived without input QPC")?;
                capture::input_to_frame_ms(input.ticks, input.frequency, frame.frame_time_100ns)
                    .map_err(|error| anyhow!(error))?;
                let mut models_published = false;
                state(|value| {
                    models_published = value.page.is_some() && value.folder_counts.is_some()
                });
                ensure!(
                    models_published,
                    "completion pixels lack matching page and folder publication evidence"
                );
                let Some(final_geometry) =
                    final_native_geometry(observation, frame.frame_time_100ns)?
                else {
                    observation.deferred_layout_frames += 1;
                    continue;
                };
                ensure!(
                    geometry_unchanged(geometry, inspection),
                    "final compositor frame has a changed client/capture translation"
                );
                observation.final_native_geometry = Some(final_geometry);
                observation.final_frame = Some(frame);
                observation.memory.sample_after_input(true);
                return Ok(true);
            }
            if Some(frame.generation) != generation {
                observation.previous_generation_frames += 1;
            }
        } else if calibration_matches(observation, &frame) {
            observation.calibrated = Some(frame);
        }
    }
    let mut failure = None;
    let mut startup_complete = false;
    let mut has_input = false;
    state(|value| {
        failure = value.invalid.clone();
        startup_complete = value.startup_page && value.startup_folders;
        has_input = value.input.is_some();
    });
    if let Some(error) = failure {
        bail!(error);
    }
    if !armed {
        ensure!(
            started.elapsed() < Duration::from_secs(120),
            "startup/initial WGC calibration timed out"
        );
        if inspection.necessary_checks_pass()
            && observation.ready
            && observation.calibrated.is_some()
            && startup_complete
        {
            let since = observation
                .foreground_since
                .get_or_insert_with(Instant::now);
            if since.elapsed() >= Duration::from_millis(200) {
                preset_filters(ui, expected);
                ui.invoke_perf_focus_search();
                let (armed_hwnd, armed_scale, armed_size) = own_window(ui)?;
                let armed_position = ui.window().position();
                if armed_hwnd != observation.hwnd
                    || armed_size != inspection.client_size
                    || armed_scale.to_bits() != observation.scale_factor.to_bits()
                    || [armed_position.x, armed_position.y] != observation.outer_position
                {
                    // Still prearm: the next tick observes and recalibrates the
                    // changed setup. Never promote a changed geometry to input.
                    observation.foreground_since = None;
                    return Ok(false);
                }
                let armed_geometry = ArmedGeometry {
                    physical_client_size: inspection.client_size,
                    outer_position: [armed_position.x, armed_position.y],
                    scale_factor: armed_scale,
                };
                observation.dropped_when_armed = observer.dropped_messages();
                observation.memory.baseline_before_arming =
                    observation.memory.read().map(|memory| json!(memory));
                arm_native_geometry(observation)?;
                observation.armed_at = Some(Instant::now());
                state(|value| {
                    value.geometry_events.baseline = Some(armed_geometry);
                    value.geometry_events.client_layout_guarded = true;
                    value.armed = true;
                    stamp(value, "armed_after_initial_frame");
                });
                if !interactive {
                    state(|value| match capture::qpc_now() {
                        Ok(time) => {
                            value.input = Some(time);
                            value.input_kind = Some("Slint KeyPressed dispatch, complete text");
                            value.stages.insert("input".into(), time);
                        }
                        Err(error) => {
                            value.invalid.get_or_insert(error);
                        }
                    });
                    ui.window()
                        .dispatch_event(slint::platform::WindowEvent::KeyPressed {
                            text: expected.query.clone().into(),
                        });
                    ui.window()
                        .dispatch_event(slint::platform::WindowEvent::KeyReleased {
                            text: expected.query.clone().into(),
                        });
                } else {
                    eprintln!(
                        "GUI search benchmark armed after decoded calibration; enter the expected query in the focused search field"
                    );
                }
            }
        } else {
            observation.foreground_since = None;
        }
    } else {
        let elapsed = if has_input {
            observation
                .input_seen_at
                .get_or_insert_with(Instant::now)
                .elapsed()
        } else {
            observation.armed_at.expect("armed state checked").elapsed()
        };
        ensure!(
            elapsed < Duration::from_secs(if interactive && !has_input { 120 } else { 30 }),
            "armed GUI search/compositor observation timed out"
        );
    }
    Ok(false)
}

fn frame_json(frame: &CapturedMarker) -> Value {
    json!({"nonce":frame.nonce,"generation":frame.generation,"phase":frame.phase,"token_hex":frame.token.iter().map(|byte| format!("{byte:02x}")).collect::<String>(),"frame_system_relative_time_100ns":frame.frame_time_100ns,"previous_observed_frame_100ns":frame.previous_observed_frame_time_100ns,"last_same_generation_nonfinal_frame_100ns":frame.last_nonfinal_time_100ns,"content_size":frame.content_size,"roi_digest":format!("{:016x}",frame.roi_digest),"dropped_output_messages":frame.dropped_output_messages,"visibility_before_passed":frame.visibility_before.necessary_checks_pass(),"visibility_after_passed":frame.visibility_after.necessary_checks_pass()})
}

fn preferred_size_used(logical_size: [f64; 2]) -> bool {
    (logical_size[0] - 1440.0).abs() < 0.01 && (logical_size[1] - 900.0).abs() < 0.01
}

fn inspection_json(inspection: capture::WindowInspection) -> Value {
    let candidates = inspection
        .occluder_candidates
        .iter()
        .take(
            (inspection.occluder_candidate_count as usize)
                .min(inspection.occluder_candidates.len()),
        )
        .map(|candidate| candidate.report())
        .collect::<Vec<_>>();
    json!({
        "raw_intersecting_windows":inspection.raw_intersecting_windows,
        "recognized_cursor_overlays":inspection.recognized_cursor_overlays,
        "potential_occluders":inspection.potential_occluders,
        "scan_complete":inspection.occlusion_scan_complete,
        "desktop_coverage":inspection.desktop_coverage,
        "occluder_candidates":candidates,
        "occluder_candidates_overflow":inspection.occluder_candidates_overflow,
        "foreign_window_titles_or_images_saved":false
    })
}

fn calibration_matches(observation: &Observation, frame: &CapturedMarker) -> bool {
    observation.calibration_generation == Some(frame.generation)
        && frame.phase == capture::PHASE_ALL
        && frame.visibility_before.necessary_checks_pass()
        && frame.visibility_after.necessary_checks_pass()
        && observation.calibration_stamp.is_some_and(|stamp| {
            capture::input_to_frame_ms(stamp.ticks, stamp.frequency, frame.frame_time_100ns).is_ok()
        })
}

struct OracleResults {
    total_count: u64,
    returned_count: usize,
    ids: Vec<i64>,
    folder_counts: Vec<(i64, u64)>,
}

fn oracle_results(source: &Path, request: &SearchRequest) -> Result<OracleResults> {
    let oracle = SqliteProvider::open_read_only(source)?;
    ensure!(
        oracle.database_path().canonicalize()? == source,
        "oracle source changed or required an implicit upgrade"
    );
    let page = oracle.search(request)?;
    let ids = page.results.iter().map(|audio| audio.id).collect();
    let mut folder_counts = oracle
        .search_folders(request)?
        .into_iter()
        .map(|folder| (folder.id, folder.file_count))
        .collect::<Vec<_>>();
    folder_counts.sort_unstable_by_key(|&(id, _)| id);
    Ok(OracleResults {
        total_count: page.total_count,
        returned_count: page.returned_count,
        ids,
        folder_counts,
    })
}

struct ActiveGuard;
impl Drop for ActiveGuard {
    fn drop(&mut self) {
        ENABLED.store(false, Ordering::Release);
        *STATE.lock().unwrap_or_else(|error| error.into_inner()) = None;
    }
}

fn measure(options: &Options) -> Result<Value> {
    ensure!(cfg!(windows), "GUI compositor diagnostics require Windows");
    let pointer_policy = options
        .pointer_overlay_process
        .map(capture::PointerOverlayPolicy::for_sky_process)
        .transpose()
        .map_err(|error| anyhow!(error))?;
    let pointer_policy_report = pointer_policy.as_ref().map(|policy| json!(policy.report()));
    let source = options
        .source
        .canonicalize()
        .context("Resolve existing benchmark source")?;
    ensure!(
        SqliteProvider::has_current_search_state(&source)?,
        "benchmark source must already have current v3 search state; implicit upgrades are refused"
    );
    let source_before = std::fs::metadata(&source)?;
    let modified_before = source_before.modified()?;
    let provider = Arc::new(SqliteProvider::open_read_only(&source)?);
    ensure!(
        provider.database_path().canonicalize()? == source,
        "benchmark refuses an implicitly upgraded owned source copy"
    );
    let profile = tempfile::Builder::new()
        .prefix("AudioLibGuiSearch-")
        .tempdir()?;
    std::fs::write(profile.path().join("settings.json"), b"{}")?;
    let mut config = AppConfig::load_in(profile.path().to_path_buf())?;
    config.paths.database = source.clone();
    config.paths.legacy_database = None;
    config.language = if options.chinese { "cn" } else { "en" }.into();
    let playback = PlaybackConfig::default();
    let engine =
        AudioEngine::new(playback.clone()).context("Initialize isolated GUI audio engine")?;
    let ui = AppWindow::new().context("Create actual benchmark window")?;
    crate::window_repaint::install(ui.window());
    ui.set_app_version(env!("CARGO_PKG_VERSION").into());
    ui.set_perf_marker_enabled(true);
    let nonce =
        (SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos() as u32) ^ std::process::id();
    ensure!(
        !ENABLED.swap(true, Ordering::AcqRel),
        "a GUI search diagnostic is already active"
    );
    let _active = ActiveGuard;
    *STATE.lock().unwrap_or_else(|error| error.into_inner()) = Some(Measurement {
        nonce,
        expected: options.expected.clone(),
        actual: None,
        startup_generation: None,
        startup_page: false,
        startup_folders: false,
        startup_library_info: None,
        calibration_generation: None,
        visibility_epoch: 0,
        interactive: options.interactive,
        armed: false,
        finished: false,
        input: None,
        input_kind: None,
        generation: None,
        stages: BTreeMap::new(),
        page: None,
        folder_counts: None,
        invalid: None,
        native_text_events: 0,
        intermediate_requests: 0,
        geometry_events: ArmedGeometryEvents::default(),
    });
    marker(&ui, nonce, 0, capture::PHASE_PENDING);
    let controller = Controller::new(&ui, provider.clone(), engine.handle(), config, playback);
    let close_control = controller.clone();
    let close_ui = ui.as_weak();
    ui.window().on_close_requested(move || {
        if let Some(ui) = close_ui.upgrade() {
            close_control.request_close(&ui);
        }
        slint::CloseRequestResponse::KeepWindowShown
    });
    let observation = Rc::new(RefCell::new(Observation::default()));
    let observed = observation.clone();
    let weak = ui.as_weak();
    let control = controller.clone();
    let expected = options.expected.clone();
    let interactive = options.interactive;
    let source_text = source.display().to_string();
    let started = Instant::now();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(5),
        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let mut observation = observed.borrow_mut();
            if let Some(close_at) = observation.close_at {
                if close_at.elapsed() > Duration::from_secs(10) {
                    state(|value| {
                        value
                            .invalid
                            .get_or_insert("normal controller close timed out".into());
                    });
                    let _ = slint::quit_event_loop();
                }
                return;
            }
            if let Some(until) = observation.hold_until {
                if Instant::now() >= until {
                    observation.hold_until = None;
                    observation.close_at = Some(Instant::now());
                    control.request_close(&ui);
                }
                return;
            }
            let outcome = tick(
                &ui,
                &mut observation,
                &expected,
                interactive,
                &source_text,
                started,
                pointer_policy.as_ref(),
            );
            let captured_successfully = matches!(&outcome, Ok(true));
            let should_close = match outcome {
                Ok(complete) => complete,
                Err(error) => {
                    invalidate(format!("{error:#}"));
                    true
                }
            };
            if should_close {
                // Detach before controller close/hide can destroy the HWND. A
                // detach/history failure belongs to this sample, even after an ALL.
                if let Err(error) = stop_native_geometry(&mut observation) {
                    invalidate(format!("{error:#}"));
                }
                let mut hold = false;
                state(|value| {
                    hold = interactive && captured_successfully && value.invalid.is_none();
                    value.finished = true;
                });
                if hold {
                    observation.post_measurement_hold_ms = 2000;
                    observation.hold_until = Some(Instant::now() + Duration::from_millis(2000));
                } else {
                    observation.close_at = Some(Instant::now());
                    control.request_close(&ui);
                }
            }
        },
    );
    // Explicit diagnostic initialization only. Keep the production preferred size,
    // while avoiding an inherited cross-screen initial placement for this new window.
    ui.window()
        .set_position(slint::PhysicalPosition::new(40, 40));
    if let Err(error) = ui.show() {
        timer.stop();
        stop_observer(&mut observation.borrow_mut());
        controller.shutdown();
        return Err(error).context("Show native benchmark window");
    }
    let event_loop_result = slint::run_event_loop();
    timer.stop();
    if let Err(error) = stop_native_geometry(&mut observation.borrow_mut()) {
        state(|value| {
            value.invalid.get_or_insert(format!("{error:#}"));
        });
    }
    stop_observer(&mut observation.borrow_mut());
    controller.shutdown();
    let hide_result = ui.hide();
    drop(engine);
    if let Err(error) = event_loop_result {
        state(|value| {
            value
                .invalid
                .get_or_insert(format!("event loop failed: {error}"));
        });
    }
    if let Err(error) = hide_result {
        state(|value| {
            value
                .invalid
                .get_or_insert(format!("window hide failed: {error}"));
        });
    }
    let memory_before_oracle = observation
        .borrow_mut()
        .memory
        .read()
        .map(|memory| json!(memory));
    let observation = observation.borrow();
    let guard = STATE.lock().unwrap_or_else(|error| error.into_inner());
    let measurement = guard.as_ref().context("measurement state disappeared")?;
    let mut invalid = measurement.invalid.clone();
    let total_ms = match (measurement.input, observation.final_frame.as_ref()) {
        (Some(input), Some(frame)) => {
            match capture::input_to_frame_ms(input.ticks, input.frequency, frame.frame_time_100ns) {
                Ok(time) => Some(time),
                Err(error) => {
                    invalid.get_or_insert(error);
                    None
                }
            }
        }
        _ => {
            invalid.get_or_insert("no decoded final compositor frame for measured input".into());
            None
        }
    };
    // These raw process-wide counters must precede the independent oracle, which
    // can itself use parallel ranking. They include the initial browse/startup.
    let ranking_diagnostics_before_oracle = audiolib_db::parallel_ranking_diagnostics();
    let auxiliary_readers_configured = audiolib_db::auxiliary_search_reader_count();
    let mmap_requested_bytes = audiolib_db::sqlite_reader_mmap_bytes(&source);
    // Independent provider oracle runs only after the measured window/capture has stopped.
    let mut oracle_error = None;
    let oracle_matches = match oracle_results(&source, &options.expected) {
        Ok(oracle) => {
            measurement
                .actual
                .as_ref()
                .is_some_and(|actual| same_request(actual, &options.expected))
                && measurement.page.as_ref().is_some_and(|page| {
                    page["result_ids"] == json!(oracle.ids)
                        && page["total_count"] == oracle.total_count
                        && page["returned_count"] == oracle.returned_count
                })
                && measurement.folder_counts.as_ref() == Some(&oracle.folder_counts)
        }
        Err(error) => {
            oracle_error = Some(format!("{error:#}"));
            false
        }
    };
    if !oracle_matches {
        invalid.get_or_insert(if let Some(error) = &oracle_error {
            format!("post-timing provider oracle failed: {error}")
        } else {
            "GUI request/results/folder counts differ from the post-timing provider oracle".into()
        });
    }
    let mut source_stat_error = None;
    let source_unchanged = match std::fs::metadata(&source).and_then(|after| {
        Ok(after.len() == source_before.len() && after.modified()? == modified_before)
    }) {
        Ok(unchanged) => unchanged,
        Err(error) => {
            source_stat_error = Some(error.to_string());
            false
        }
    };
    if !source_unchanged {
        invalid.get_or_insert("source size or modification time changed".into());
    }
    let stages = measurement.stages.iter().map(|(name, time)| {
        let since_input_ms = measurement.input.filter(|input| input.frequency == time.frequency).map(|input| (time.ticks as f64 - input.ticks as f64) * 1000.0 / input.frequency as f64);
        (name.clone(), json!({"qpc_ticks":time.ticks,"qpc_frequency":time.frequency,"since_input_ms":since_input_ms}))
    }).collect::<serde_json::Map<_, _>>();
    let valid = invalid.is_none();
    let mut report = json!({"schema_version":1,"valid":valid,"invalid_reason":invalid,"meets_target":valid && total_ms.is_some_and(|time| time < 500.0),"target_ms":500,"total_ms":total_ms});
    report["endpoint"] = if options.pointer_overlay_process.is_some() {
        "first observed WGC compositor frame with matching nonce/generation phase ALL under an explicit verified Sky cursor-decoration exception; application/unknown occluders remain blocking; not an absolute unobscured-display, panel-scanout or photon-time claim"
    } else {
        "first observed WGC compositor frame with matching nonce/generation phase ALL under strict window inspection; not an absolute continuous-occlusion, panel-scanout or photon-time claim"
    }.into();
    report["input_mode"] = if options.interactive {
        "native KeyboardInput Pressed or Ime Commit"
    } else {
        "Slint WindowEvent KeyPressed/Released with complete text through the production LineEdit"
    }
    .into();
    report["input_timestamp_source"] = json!(measurement.input_kind);
    report["native_text_events"] = measurement.native_text_events.into();
    report["intermediate_requests"] = measurement.intermediate_requests.into();
    report["expected_request"] = json!(options.expected);
    report["actual_request"] = json!(measurement.actual);
    report["page"] = json!(measurement.page);
    report["folder_counts"] = json!(measurement.folder_counts);
    report["oracle_matches"] = oracle_matches.into();
    report["oracle_error"] = json!(oracle_error);
    report["source_stat_error"] = json!(source_stat_error);
    report["source_same"] = true.into();
    report["source_current"] = true.into();
    report["owned_copy"] = false.into();
    report["source_read_only"] = true.into();
    report["source_database_bytes"] = source_before.len().into();
    report["source_size_mtime_unchanged"] = source_unchanged.into();
    report["normal_data_directory_environment_changed"] = false.into();
    report["isolated_profile"] = true.into();
    report["network_started"] = false.into();
    report["language"] = if options.chinese { "cn" } else { "en" }.into();
    report["interactive_post_measurement_hold_ms"] = observation.post_measurement_hold_ms.into();
    report["pointer_overlay_policy"] = json!({"enabled":options.pointer_overlay_process.is_some(),"verified_process":pointer_policy_report,"scope":"Only explicitly identified Sky cursor decorations may be excepted; application and unknown occluders remain blocking","absolute_unobscured_display_claimed":false});
    report["occlusion_observations"] = json!({"last":observation.last_inspection.map(inspection_json),"armed_raw_intersecting_windows_max":observation.armed_raw_intersecting_windows_max,"armed_recognized_cursor_overlays_max":observation.armed_recognized_cursor_overlays_max,"armed_potential_occluders_max":observation.armed_potential_occluders_max});
    report["window"] = json!({
        "physical_client_size":observation.geometry.map(|value| value.inspection.client_size),
        "logical_client_size":observation.logical_size,"scale_factor":observation.scale_factor,
        "normal_preferred_size_used":preferred_size_used(observation.logical_size),
        "initial_outer_position_request":{"physical_x":40,"physical_y":40},
        "actual_outer_position_at_capture_setup_physical":observation.outer_position,
        "work_area_adjustment":observation.work_area_adjustment,
        "client_origin_in_capture":observation.geometry.map(|value| value.inspection.client_origin_in_capture),
        "expected_capture_size":observation.geometry.map(|value| value.inspection.expected_capture_size),
        "marker_parent_offset_logical":observation.marker_parent_offset,
        "marker_actual_client_position_logical":observation.marker_actual_client_position,
        "marker_roi":observation.geometry.map(|value| json!({"x":value.roi.x,"y":value.roi.y,"cell_width":value.roi.cell_width,"cell_height":value.roi.cell_height}))
    });
    report["armed_native_geometry"] = json!({
        "baseline":measurement.geometry_events.baseline.map(ArmedGeometry::report),
        "events_total":measurement.geometry_events.total,
        "ignored_duplicates":measurement.geometry_events.ignored_duplicates,
        "client_layout_guarded":measurement.geometry_events.client_layout_guarded,
        "delegated_client_layout":measurement.geometry_events.delegated_client_layout,
        "events_overflow":measurement.geometry_events.overflow,
        "retained_limit":32,"events":measurement.geometry_events.events,
        "scale_factor_events_always_rejected":true
    });
    report["synchronous_native_geometry_history"] = json!(observation.native_geometry_history);
    report["final_native_geometry"] = json!(observation.final_native_geometry);
    report["deferred_client_layout_final_frames"] = json!(observation.deferred_layout_frames);
    report["capture_observations"] = json!({
        "observer_restarts_before_arming":observation.observer_restarts,
        "ready_events":observation.telemetry.ready_events,
        "last_ready_content_size":observation.telemetry.ready_content_size,
        "last_ready_qpc_frequency":observation.telemetry.ready_qpc_frequency,
        "prearm_rejected_frames":observation.telemetry.prearm_rejected_frames,
        "last_prearm_rejected_frame":observation.telemetry.last_prearm_rejected_frame,
        "decoded_frames":observation.telemetry.decoded_frames,
        "last_decoded_frame":observation.telemetry.last_decoded_frame
    });
    report["process_memory_observations"] = json!({
        "baseline_before_arming":observation.memory.baseline_before_arming,
        "after_timing_before_oracle":memory_before_oracle,
        "process_lifetime_peak_includes_startup_and_closing":true,
        "sample_interval_ms":200,"samples_after_input":observation.memory.samples_after_input,
        "sampled_current_working_set_max_bytes":observation.memory.sampled_current_working_set_max_bytes,
        "sampled_private_usage_max_bytes":observation.memory.sampled_private_usage_max_bytes,
        "sampled_maxima_are_true_peaks":false,
        "read_errors":observation.memory.errors,"last_read_error":observation.memory.last_error
    });
    report["startup_empty_models_complete"] =
        (measurement.startup_page && measurement.startup_folders).into();
    report["startup_library_info_published"] = json!(measurement.startup_library_info);
    report["calibration_epoch_generation"] = json!(observation.calibration_generation);
    report["foreground_calibration_epochs"] = observation.calibration_epoch.into();
    report["startup_wgc_calibration"] = json!(observation.calibrated.as_ref().map(frame_json));
    report["final_frame"] = json!(observation.final_frame.as_ref().map(frame_json));
    report["previous_generation_frames_ignored_for_completion"] =
        observation.previous_generation_frames.into();
    report["stages"] = stages.into();
    report["resource_conditions"] = json!({"os_page_cache_cleared":false,"startup_empty_browse_complete_before_input":true,"startup_library_stats_completion_gated":true,"ordinary_reader_pool_max":4,"sqlite_cache_kib_per_ordinary_reader":65536,"auxiliary_readers_configured":auxiliary_readers_configured,"auxiliary_readers_max":audiolib_db::SQLITE_AUXILIARY_SEARCH_READERS,"sqlite_cache_kib_per_auxiliary_reader":audiolib_db::SQLITE_AUXILIARY_READER_CACHE_KB,"sqlite_mmap_requested_ceiling_bytes":mmap_requested_bytes,"sqlite_mmap_fixed_volume_maximum_bytes":audiolib_db::SQLITE_READER_MMAP_BYTES,"mmap_is_resident_memory":false,"parallel_ranking_raw_before_oracle":ranking_diagnostics_before_oracle,"parallel_ranking_counters_include_startup":true,"foreground_stable_before_input_ms":200,"visibility_and_conservative_occlusion_polled_ms":5,"full_window_images_saved":false,"oracle_runs_after_timing":true});
    Ok(report)
}

/// Run only for the explicit CLI route, before loading ordinary user preferences.
pub(super) fn run(arguments: &[String]) -> Result<()> {
    let options = Options::parse(arguments)?;
    let mut output: File = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&options.report)
        .context("Create a new GUI benchmark report without overwriting existing evidence")?;
    let result = measure(&options);
    let report = match &result {
        Ok(value) => value.clone(),
        Err(error) => {
            json!({"schema_version":1,"valid":false,"meets_target":false,"invalid_reason":format!("{error:#}"),"expected_request":options.expected,"full_window_images_saved":false})
        }
    };
    serde_json::to_writer_pretty(&mut output, &report)?;
    writeln!(output)?;
    output.sync_all()?;
    result?;
    ensure!(
        report["valid"] == true,
        "GUI benchmark invalid: {}",
        report["invalid_reason"]
    );
    // Valid slow samples remain successful process runs, with meets_target=false.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn armed_geometry_accepts_only_exact_duplicate_move_or_resize_payloads() {
        let baseline = ArmedGeometry {
            physical_client_size: [2880, 1800],
            outer_position: [102, 238],
            scale_factor: 2.0,
        };
        let timestamp = QpcStamp {
            ticks: 123,
            frequency: 10_000_000,
        };
        let mut events = ArmedGeometryEvents {
            baseline: Some(baseline),
            ..Default::default()
        };
        for event in [
            NativeGeometryEvent::Resized([2880, 1800]),
            NativeGeometryEvent::Moved([102, 238]),
        ] {
            assert!(events.record(event, Ok(timestamp)).is_none());
            assert_eq!(events.events.last().unwrap()["ignored_duplicate"], true);
        }
        for event in [
            NativeGeometryEvent::Resized([2881, 1800]),
            NativeGeometryEvent::Moved([103, 238]),
            NativeGeometryEvent::ScaleFactorChanged(2.0),
        ] {
            assert!(events.record(event, Ok(timestamp)).is_some());
            assert_eq!(events.events.last().unwrap()["ignored_duplicate"], false);
        }
        assert!(
            events
                .record(
                    NativeGeometryEvent::Moved([102, 238]),
                    Err("QPC failed".into())
                )
                .is_some()
        );
        let mut missing = ArmedGeometryEvents::default();
        assert!(
            missing
                .record(NativeGeometryEvent::Resized([2880, 1800]), Ok(timestamp))
                .is_some()
        );
        // A real change remains a recorded rejection even if a later event
        // returns exactly to the original geometry; no history is discarded.
        assert!(
            events
                .record(NativeGeometryEvent::Moved([102, 238]), Ok(timestamp))
                .is_none()
        );
        assert_eq!(events.events[3]["ignored_duplicate"], false);
    }

    #[test]
    fn buffered_client_layout_requires_the_synchronous_native_guard() {
        let timestamp = QpcStamp {
            ticks: 123,
            frequency: 10_000_000,
        };
        let mut events = ArmedGeometryEvents {
            baseline: Some(ArmedGeometry {
                physical_client_size: [2880, 1800],
                outer_position: [40, 104],
                scale_factor: 2.0,
            }),
            client_layout_guarded: true,
            ..Default::default()
        };
        assert!(
            events
                .record(NativeGeometryEvent::Resized([2880, 1839]), Ok(timestamp))
                .is_none()
        );
        assert_eq!(events.delegated_client_layout, 1);
        assert_eq!(events.events[0]["delegated_client_layout"], true);
        assert_eq!(events.events[0]["ignored_duplicate"], false);
        for event in [
            NativeGeometryEvent::Moved([40, 105]),
            NativeGeometryEvent::ScaleFactorChanged(2.0),
        ] {
            assert!(events.record(event, Ok(timestamp)).is_some());
        }
        assert!(
            events
                .record(
                    NativeGeometryEvent::Resized([2880, 1839]),
                    Err("QPC failed".into())
                )
                .is_some()
        );
        events.client_layout_guarded = false;
        assert!(
            events
                .record(NativeGeometryEvent::Resized([2880, 1839]), Ok(timestamp))
                .is_some()
        );
    }

    #[test]
    fn armed_geometry_event_report_is_bounded_and_counts_every_overflow() {
        let mut events = ArmedGeometryEvents {
            baseline: Some(ArmedGeometry {
                physical_client_size: [2880, 1800],
                outer_position: [102, 238],
                scale_factor: 2.0,
            }),
            ..Default::default()
        };
        for _ in 0..35 {
            assert!(
                events
                    .record(
                        NativeGeometryEvent::Resized([2880, 1800]),
                        Ok(QpcStamp {
                            ticks: 123,
                            frequency: 10_000_000
                        })
                    )
                    .is_none()
            );
        }
        assert_eq!(events.events.len(), 32);
        assert_eq!(events.total, 35);
        assert_eq!(events.overflow, 3);
        assert_eq!(events.ignored_duplicates, 35);
        assert_eq!(events.events[0]["qpc_ticks"], 123);
        assert_eq!(
            events.events[0]["armed_baseline"]["physical_client_size"],
            json!([2880, 1800])
        );
    }

    #[test]
    fn request_parser_rejects_silent_scope_or_paging_changes() {
        for request in [
            SearchRequest {
                query: "metal".into(),
                folder_id: Some(1),
                ..Default::default()
            },
            SearchRequest {
                query: "metal".into(),
                limit: 10,
                ..Default::default()
            },
            SearchRequest {
                query: String::new(),
                ..Default::default()
            },
        ] {
            assert!(validate_request(&request).is_err());
        }
        validate_request(&SearchRequest {
            query: "ambient".into(),
            ..Default::default()
        })
        .unwrap();
    }

    #[test]
    fn cli_requires_explicit_source_and_new_report_destination() {
        let request = serde_json::to_string(&SearchRequest {
            query: "plos".into(),
            ..Default::default()
        })
        .unwrap();
        let mut arguments = vec![
            "--gui-search-benchmark".into(),
            request,
            "--gui-search-database".into(),
            "source.db".into(),
        ];
        assert!(Options::parse(&arguments).is_err());
        arguments.extend(["--gui-search-report".into(), "new-report.json".into()]);
        assert!(!Options::parse(&arguments).unwrap().interactive);
        assert!(Options::parse(&arguments).unwrap().chinese);
        arguments.push("--gui-search-interactive".into());
        assert!(Options::parse(&arguments).unwrap().interactive);
        arguments.push("--unknown".into());
        assert!(Options::parse(&arguments).is_err());
    }

    #[test]
    fn existing_report_is_preserved_before_any_source_open() {
        let directory = tempfile::tempdir().unwrap();
        let report = directory.path().join("preserved.json");
        std::fs::write(&report, b"preserved evidence").unwrap();
        let source = directory.path().join("must-not-create.db");
        let arguments = vec![
            "--gui-search-benchmark".into(),
            serde_json::to_string(&SearchRequest {
                query: "metal".into(),
                ..Default::default()
            })
            .unwrap(),
            "--gui-search-database".into(),
            source.display().to_string(),
            "--gui-search-report".into(),
            report.display().to_string(),
        ];
        assert!(run(&arguments).is_err());
        assert_eq!(std::fs::read(report).unwrap(), b"preserved evidence");
        assert!(!source.exists());
    }

    #[test]
    fn calibration_requires_the_fresh_epoch_and_a_frame_after_its_qpc_stamp() {
        let visibility = capture::Visibility {
            own_process: true,
            foreground: true,
            visible: true,
            iconic: false,
            cloaked: Some(0),
            input_desktop_accessible: true,
        };
        let observation = Observation {
            calibration_generation: Some(17),
            calibration_stamp: Some(QpcStamp {
                ticks: 100,
                frequency: 10_000_000,
            }),
            ..Default::default()
        };
        let mut frame = CapturedMarker {
            token: [0; 16],
            nonce: 1,
            generation: 17,
            phase: capture::PHASE_ALL,
            frame_time_100ns: 100,
            previous_observed_frame_time_100ns: None,
            last_nonfinal_time_100ns: None,
            content_size: [1000, 600],
            roi_digest: 0,
            visibility_before: visibility,
            visibility_after: visibility,
            dropped_output_messages: 0,
        };
        assert!(calibration_matches(&observation, &frame));
        frame.generation = 16;
        assert!(!calibration_matches(&observation, &frame));
        frame.generation = 17;
        frame.frame_time_100ns = 99;
        assert!(!calibration_matches(&observation, &frame));
        frame.frame_time_100ns = 100;
        frame.visibility_after.foreground = false;
        assert!(!calibration_matches(&observation, &frame));
    }

    #[test]
    fn setup_reset_preserves_rejection_evidence_but_discards_calibration() {
        let mut observation = Observation {
            calibration_generation: Some(17),
            calibration_stamp: Some(QpcStamp {
                ticks: 100,
                frequency: 10_000_000,
            }),
            foreground_since: Some(Instant::now()),
            ..Default::default()
        };
        observation.telemetry.status(
            &ObserverStatus::RejectedFrame {
                frame_time_100ns: 123,
                reason: "marker CRC mismatch".into(),
            },
            false,
        );
        clear_calibration(&mut observation);
        assert!(observation.calibration_generation.is_none());
        assert!(observation.calibration_stamp.is_none());
        assert!(observation.foreground_since.is_none());
        assert_eq!(observation.telemetry.prearm_rejected_frames, 1);
        assert_eq!(
            observation
                .telemetry
                .last_prearm_rejected_frame
                .as_ref()
                .unwrap()["frame_system_relative_time_100ns"],
            123
        );
        assert!(preferred_size_used([1440.0, 900.0]));
        assert!(!preferred_size_used([1080.0, 700.0]));
    }

    #[test]
    fn marker_pixels_use_client_coordinates_with_software_menu_at_fractional_dpi() {
        use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
        struct Platform(Rc<MinimalSoftwareWindow>);
        impl slint::platform::Platform for Platform {
            fn create_window_adapter(
                &self,
            ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
                Ok(self.0.clone())
            }
        }
        // No native window, database, audio engine or user profile is created.
        // The real AppWindow and software MenuBar exercise the compiler's menu
        // wrapper; the expected pixels are sampled in physical client space.
        let adapter = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(Platform(adapter.clone()))).unwrap();
        let ui = AppWindow::new().unwrap();
        let token = capture::encode_marker(0x12345678, 17, capture::PHASE_ALL).unwrap();
        let bits = capture::marker_bits(&token);
        ui.set_perf_marker_enabled(true);
        ui.set_perf_marker_bits(ModelRc::new(VecModel::from(bits.to_vec())));
        for native_menu in [false, true] {
            // Exercise both platform variants through the testing backend's
            // software menu; the client-coordinate compensation stays identical.
            ui.set_native_macos(native_menu);
            for scale in [1.0_f32, 1.25, 1.5, 2.0] {
                ui.window()
                    .dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged {
                        scale_factor: scale,
                    });
                ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
                let width = (1440.0 * scale) as usize;
                let height = (900.0 * scale) as usize;
                let cell_width = (3.0 * scale).ceil() as usize;
                let cell_height = (6.0 * scale).ceil() as usize;
                let left = width - 64 * cell_width - 4;
                let top = height - 2 * cell_height - 4;
                ui.set_perf_marker_cell_width(cell_width as f32 / scale);
                ui.set_perf_marker_cell_height(cell_height as f32 / scale);
                ui.set_perf_marker_x(left as f32 / scale);
                ui.set_perf_marker_y(top as f32 / scale);
                ui.show().unwrap();
                slint::platform::update_timers_and_animations();
                let mut pixels = vec![slint::Rgb8Pixel::default(); width * height];
                assert!(adapter.draw_if_needed(|renderer| {
                    renderer.render(&mut pixels, width);
                }));
                slint::platform::update_timers_and_animations();
                assert!(
                    ui.get_perf_marker_parent_y() > 0.0,
                    "software menu must exercise a nonzero parent offset"
                );
                for (index, bit) in bits.iter().enumerate() {
                    let x = left + (index % 64) * cell_width;
                    let y = top + (index / 64) * cell_height;
                    let expected = if *bit { 255 } else { 0 };
                    for row in y..y + cell_height {
                        for column in x..x + cell_width {
                            let pixel = pixels[row * width + column];
                            assert_eq!(
                                [pixel.r, pixel.g, pixel.b],
                                [expected; 3],
                                "scale={scale}, bit={index}, pixel=({column},{row})"
                            );
                        }
                    }
                }
                assert!((ui.get_perf_marker_actual_client_x() * scale - left as f32).abs() < 0.01);
                assert!((ui.get_perf_marker_actual_client_y() * scale - top as f32).abs() < 0.01);
            }
        }
        ui.hide().unwrap();
    }
}
