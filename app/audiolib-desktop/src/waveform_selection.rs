// app/audiolib-desktop/src/waveform_selection.rs
//! Source-bound Waveform ranges, ordered audition and durable native drag exports.
use super::*;
use audiolib_core::AudioFrameRange;
use std::io::{Read, Write};
#[cfg(test)]
#[path = "waveform_selection_tests.rs"]
mod integration_tests;

/// Captured with the displayed image, independent of inspector/task selection.
#[derive(Clone)]
pub(super) struct WaveSource {
    file: AudioFile,
    key: String,
    source_generation: u64,
    library_generation: u64,
    remote: bool,
    local_stamp: Option<SourceStamp>,
    pub(super) remote_version: Option<audiolib_remote::OriginalVersion>,
}

impl WaveSource {
    pub(super) fn capture(c: &Controller, file: &AudioFile) -> Self {
        let source_generation = c.source_generation.load(Ordering::Acquire);
        let library_generation = c.library_generation.load(Ordering::Acquire);
        let remote = c.remote_read_only() && file.id > 0;
        let version = if remote {
            c.remote
                .lock()
                .unwrap()
                .state
                .as_ref()
                .map(|state| format!("{}:{}", state.library_id, state.database_file))
                .unwrap_or_default()
        } else {
            String::new()
        };
        let mut file = file.clone();
        if file.filename.is_empty() {
            file.filename = Path::new(&file.file_path)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "audio".into());
        }
        let identifier = if remote {
            format!("{}:{}", file.id, file.file_size)
        } else {
            "local".into()
        };
        let path = if cfg!(windows) && !remote {
            file.file_path.replace('\\', "/").to_lowercase()
        } else {
            file.file_path.clone()
        };
        let key = format!(
            "{source_generation}:{library_generation}:{version}:{identifier}:{path}:{}:{}",
            file.sample_rate,
            file.duration.to_bits()
        );
        Self {
            file,
            source_generation,
            library_generation,
            remote,
            local_stamp: None,
            remote_version: None,
            key,
        }
    }

    fn total_frames(&self) -> u64 {
        (self.file.duration * f64::from(self.file.sample_rate)).round() as u64
    }

    /// Called by the Waveform worker, keeping filesystem queries off the UI.
    pub(super) fn bind_local_version(&mut self) -> Result<(), String> {
        if !self.remote && !audiolib_audio::is_stream_source(Path::new(&self.file.file_path)) {
            let stamp =
                SourceStamp::read(Path::new(&self.file.file_path)).map_err(|e| e.to_string())?;
            self.key.push_str(&format!(
                ":{}:{:?}:{:?}",
                stamp.bytes, stamp.modified, stamp.created
            ));
            self.local_stamp = Some(stamp);
        }
        Ok(())
    }

    pub(super) fn check_local_version(&self) -> Result<(), String> {
        if let Some(expected) = &self.local_stamp
            && SourceStamp::read(Path::new(&self.file.file_path))
                .ok()
                .as_ref()
                != Some(expected)
        {
            return Err(
                "The original changed since this Waveform was rendered; reload the sound".into(),
            );
        }
        Ok(())
    }

    pub(super) fn bind_source_version(
        &mut self,
        c: &Controller,
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        self.bind_local_version()?;
        if self.remote {
            let version = c.remote_original_version(self.file.id, cancel)?;
            self.key
                .push_str(&format!(":{}:{}", version.etag, version.length));
            self.remote_version = Some(version);
        }
        Ok(())
    }

    fn check_remote_version(&self, c: &Controller, cancel: &AtomicBool) -> Result<(), String> {
        if self.remote {
            let expected = self.remote_version.as_ref().ok_or_else(|| {
                c.localized(
                    "该波形没有已验证的原素材版本，请重新载入音频",
                    "This Waveform has no verified original version; reload the sound",
                )
                .to_string()
            })?;
            c.check_remote_original_version(self.file.id, expected, cancel)?;
        }
        Ok(())
    }

    fn check_preview_version(
        &self,
        c: &Controller,
        path: &Path,
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        if !self.remote {
            return self.check_local_version();
        }
        self.check_remote_version(c, cancel)?;
        let stream = c
            .remote_stream
            .lock()
            .unwrap()
            .as_ref()
            .filter(|playback| playback.file_id == self.file.id && playback.path == path)
            .map(|playback| playback.stream.clone())
            .ok_or_else(|| {
                c.localized(
                    "选中的试听来源已改变，请重新载入音频",
                    "The selected audition source changed; reload the sound",
                )
                .to_string()
            })?;
        let probe = audiolib_remote::probe_download_with_cancel(stream.source(), cancel)
            .map_err(|e| e.to_string())?;
        let expected = probe
            .etag
            .filter(|etag| etag.starts_with('"') && etag.ends_with('"'))
            .ok_or_else(|| {
                c.localized(
                    "无法验证试听流版本，请重新载入音频",
                    "The audition representation has no verified strong ETag; reload the sound",
                )
                .to_string()
            })?;
        if stream.etag().as_deref() != Some(expected.as_str()) {
            return Err(c
                .localized(
                    "试听流已改变，请重新载入音频",
                    "The audition representation changed; reload the sound",
                )
                .into());
        }
        self.check_remote_version(c, cancel)
    }
}

#[derive(Clone)]
struct Selection {
    source: WaveSource,
    range: AudioFrameRange,
    revision: u64,
    generation: u64,
    cancel: Arc<AtomicBool>,
    render_cancel: Arc<AtomicBool>,
    preview_cancel: Arc<AtomicBool>,
    preview_ready: Arc<AtomicBool>,
}

impl Selection {
    fn invalidate(&self) {
        self.cancel.store(true, Ordering::Release);
        self.render_cancel.store(true, Ordering::Release);
        self.preview_cancel.store(true, Ordering::Release);
    }
}

#[derive(Clone, PartialEq, Eq)]
struct SourceStamp {
    bytes: u64,
    modified: Option<std::time::SystemTime>,
    created: Option<std::time::SystemTime>,
}
impl SourceStamp {
    fn read(path: &Path) -> std::io::Result<Self> {
        let metadata = std::fs::metadata(path)?;
        Ok(Self {
            bytes: metadata.len(),
            modified: metadata.modified().ok(),
            created: metadata.created().ok(),
        })
    }
}

struct PreparedSlice {
    _directory: crate::slice_staging::SliceStaging,
    path: PathBuf,
    export_directory: PathBuf,
    stem: String,
    local_source: Option<(PathBuf, SourceStamp)>,
}

#[derive(Default)]
pub(super) struct SliceState {
    source: Option<WaveSource>,
    selection: Option<Selection>,
    revision: u64,
    ready: Option<Arc<PreparedSlice>>,
    /// A complete numbered copy prepared after the pointer was released. Reuse
    /// it on the next drag instead of copying a large slice on every attempt.
    published: Option<PublishedSlice>,
    workers: usize,
    drag_requested: bool,
    publishing: bool,
}

/// Worker ownership is released even on error or a rejected UI notification.
struct Worker(Arc<Controller>);
impl Worker {
    fn new(c: &Arc<Controller>) -> Self {
        c.slice.lock().unwrap().workers += 1;
        Self(c.clone())
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let mut state = self.0.slice.lock().unwrap();
        state.workers -= 1;
        self.0.slice_wake.notify_all();
    }
}

struct DragBusy(Arc<Controller>);
impl Drop for DragBusy {
    fn drop(&mut self) {
        let mut state = self.0.slice.lock().unwrap();
        state.publishing = false;
        state.drag_requested = false;
        self.0.drag_busy.store(false, Ordering::Release);
    }
}

/// Presentation phases do not change the selected source/range or audio state.
#[repr(i32)]
enum ExportFeedback {
    Idle = 0,
    Preparing = 1,
    Ready = 2,
    NativeDrag = 3,
    Cancelled = 4,
    Error = 5,
}

fn show_export_feedback(ui: &AppWindow, phase: ExportFeedback) {
    ui.set_wave_selection_preparing(matches!(phase, ExportFeedback::Preparing));
    ui.set_wave_selection_export_state(phase as i32);
}

fn selection_ui_clear(ui: &AppWindow) {
    ui.set_wave_selection_active(false);
    ui.set_wave_selection_start(0.0);
    ui.set_wave_selection_end(0.0);
    show_export_feedback(ui, ExportFeedback::Idle);
    ui.set_wave_selection_progress("".into());
}

impl Controller {
    pub(super) fn slice_original_etag(&self, file: &AudioFile) -> Option<String> {
        self.slice
            .lock()
            .unwrap()
            .selection
            .as_ref()
            .filter(|selection| {
                selection.source.file.id == file.id
                    && selection.source.file.file_path == file.file_path
            })
            .and_then(|selection| {
                selection
                    .source
                    .remote_version
                    .as_ref()
                    .map(|version| version.etag.clone())
            })
    }
    pub(super) fn bind_wave_selection(self: &Arc<Self>, ui: &AppWindow) {
        for preview in [true, false] {
            let c = self.clone();
            std::thread::Builder::new()
                .name(
                    if preview {
                        "audiolib-slice-preview"
                    } else {
                        "audiolib-slice-render"
                    }
                    .into(),
                )
                .spawn(move || {
                    let queue = if preview {
                        &c.slice_preview_jobs
                    } else {
                        &c.slice_render_jobs
                    };
                    queue.run(&c.shutdown);
                })
                .expect("spawn bounded Waveform worker");
        }
        let c = self.clone();
        ui.on_wave_selection_committed(move |start, end| {
            c.commit_wave_selection(f64::from(start), f64::from(end))
        });
        let c = self.clone();
        ui.on_wave_selection_cleared(move || c.clear_wave_selection());
        let c = self.clone();
        ui.on_wave_selection_cancel_requested(move || c.cancel_slice_preparation());
        let c = self.clone();
        ui.on_wave_selection_drag_requested(move || c.drag_wave_selection());
        let c = self.clone();
        ui.on_wave_selection_drag_ended(move || c.end_slice_drag());
        let c = self.clone();
        ui.on_wave_selection_gesture_changed(move |active| {
            c.wave_selection_gesture.store(active, Ordering::Release);
        });
        let c = self.clone();
        ui.on_slice_export_directory_requested(move || c.browse_slice_directory());
        let configured = self.config.lock().unwrap().slice_export_directory.clone();
        if let Some(path) = configured {
            ui.set_slice_export_directory(path.display().to_string().into());
        } else {
            let c = self.clone();
            std::thread::spawn(move || {
                if let Ok(path) = c.slice_directory() {
                    post(c.ui.clone(), move |ui| {
                        if ui.get_slice_export_directory().is_empty() {
                            ui.set_slice_export_directory(path.display().to_string().into());
                        }
                    });
                }
            });
        }
    }

    pub(super) fn publish_wave_source(&self, ui: &AppWindow, source: &WaveSource) -> bool {
        if self.source_generation.load(Ordering::Acquire) != source.source_generation
            || self.library_generation.load(Ordering::Acquire) != source.library_generation
        {
            return false;
        }
        let _admission = self.latest_selection.lock().unwrap();
        let mut state = self.slice.lock().unwrap();
        if state
            .source
            .as_ref()
            .is_some_and(|old| old.key == source.key)
        {
            return false;
        }
        if let Some(old) = state.selection.take() {
            old.invalidate();
        }
        state.ready = None;
        state.published = None;
        state.revision += 1;
        state.drag_requested = false;
        state.source = Some(source.clone());
        drop(state);
        self.slice_preview_jobs.clear_pending();
        self.slice_render_jobs.clear_pending();
        self.wave_selection_gesture.store(false, Ordering::Release);
        selection_ui_clear(ui);
        ui.set_wave_selection_identity(source.key.clone().into());
        self.command(AudioCommand::ClearRange);
        true
    }

    pub(super) fn cancel_all_slices(&self) {
        let _admission = self.latest_selection.lock().unwrap();
        self.selection_generation.fetch_add(1, Ordering::AcqRel);
        let mut state = self.slice.lock().unwrap();
        if let Some(selection) = state.selection.take() {
            selection.invalidate();
        }
        state.ready = None;
        state.published = None;
        state.source = None;
        state.revision += 1;
        state.drag_requested = false;
        drop(state);
        self.slice_preview_jobs.clear_pending();
        self.slice_render_jobs.clear_pending();
        self.wave_selection_gesture.store(false, Ordering::Release);
        post(self.ui.clone(), |ui| {
            selection_ui_clear(&ui);
            ui.set_wave_selection_identity("".into());
        });
    }

    pub(super) fn wait_slice_workers(&self) {
        let mut state = self.slice.lock().unwrap();
        while state.workers > 0 {
            state = self.slice_wake.wait(state).unwrap();
        }
    }

    fn selection_current(&self, selection: &Selection) -> bool {
        self.available()
            && self.drag_request_current(
                selection.source.source_generation,
                selection.source.library_generation,
                &selection.cancel,
            )
            && self
                .slice
                .lock()
                .unwrap()
                .selection
                .as_ref()
                .is_some_and(|current| {
                    current.revision == selection.revision
                        && current.source.key == selection.source.key
                })
    }

    fn slice_directory(&self) -> audiolib_core::Result<PathBuf> {
        let configured = self.config.lock().unwrap().slice_export_directory.clone();
        match configured {
            Some(path) if path.is_absolute() => Ok(path),
            Some(_) => Err(audiolib_core::Error::Validation(
                self.localized(
                    "请选择绝对路径的切片保存目录",
                    "Choose an absolute slice export directory in Settings",
                )
                .to_string(),
            )),
            None => audiolib_windows::documents_directory()
                .map(|path| path.join("AudioLib Explorer").join("Exports")),
        }
    }

    fn browse_slice_directory(self: &Arc<Self>) {
        if !self.available() {
            return;
        }
        let owner = self
            .ui
            .upgrade()
            .map_or(0, |ui| crate::dialogs::owner_handle(ui.window()));
        let c = self.clone();
        std::thread::spawn(move || {
            match crate::dialogs::choose_folder(c.chinese.load(Ordering::Acquire), owner) {
                Ok(Some(path)) => {
                    let c2 = c.clone();
                    post(c.ui.clone(), move |ui| {
                        if !c2.available() {
                            return;
                        }
                        c2.cancel_slice_preparation();
                        c2.config.lock().unwrap().slice_export_directory = Some(path.clone());
                        ui.set_slice_export_directory(path.display().to_string().into());
                        *c2.single_click_play_pending.lock().unwrap() = true;
                        c2.single_click_play_wake.notify_one();
                    });
                }
                Ok(None) => {}
                Err(error) => c.status(error),
            }
        });
    }

    fn commit_wave_selection(self: &Arc<Self>, start: f64, end: f64) {
        if !self.available() {
            return;
        }
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        if !ui.get_waveform_ready() {
            return;
        }
        let source = self.slice.lock().unwrap().source.clone();
        let Some(source) = source else {
            return;
        };
        let range = match AudioFrameRange::from_seconds(
            start.min(end),
            start.max(end),
            source.file.sample_rate,
            source.total_frames(),
        ) {
            Ok(range) => range,
            Err(error) => {
                ui.set_status(error.to_string().into());
                return;
            }
        };
        let barrier = match self.enqueue_current_metadata(&ui) {
            Ok(barrier) => barrier,
            Err(error) => {
                ui.set_status(error.into());
                return;
            }
        };
        // The same admission lock as Stop/Load prevents a stale deferred Play.
        let mut remembered = self.latest_selection.lock().unwrap();
        let generation = self.selection_generation.fetch_add(1, Ordering::AcqRel) + 1;
        *remembered = None;
        *self.selection_job.lock().unwrap() = None;
        *self.selection_audio_job.lock().unwrap() = None;
        let cancel = Arc::new(AtomicBool::new(false));
        let preview_cancel = Arc::new(AtomicBool::new(false));
        std::mem::replace(
            &mut *self.remote_audio_cancel.lock().unwrap(),
            preview_cancel.clone(),
        )
        .store(true, Ordering::Release);
        let selection = {
            let mut state = self.slice.lock().unwrap();
            if let Some(old) = state.selection.take() {
                old.invalidate();
            }
            state.revision += 1;
            state.ready = None;
            state.published = None;
            state.drag_requested = false;
            let selection = Selection {
                source,
                range,
                revision: state.revision,
                generation,
                cancel,
                render_cancel: Arc::new(AtomicBool::new(false)),
                preview_cancel,
                preview_ready: Arc::new(AtomicBool::new(false)),
            };
            state.selection = Some(selection.clone());
            selection
        };
        drop(remembered);
        ui.set_wave_selection_active(true);
        ui.set_wave_selection_start(range.start_seconds() as f32);
        ui.set_wave_selection_end(range.end_seconds() as f32);
        show_export_feedback(&ui, ExportFeedback::Preparing);
        ui.set_wave_selection_progress(
            if self.chinese.load(Ordering::Acquire) {
                "正在准备 WAV 切片…"
            } else {
                "Preparing WAV slice…"
            }
            .into(),
        );
        self.wave_follow_paused.store(true, Ordering::Release);
        self.metadata_wake.notify_all();
        self.preview_wave_selection(selection.clone(), barrier);
        self.prepare_wave_slice(selection);
    }

    fn preview_wave_selection(self: &Arc<Self>, selection: Selection, barrier: u64) {
        let guard = Worker::new(self);
        let c = self.clone();
        self.slice_preview_jobs.submit(Box::new(move || {
            let _guard = guard;
            if c.wait_selection_metadata(barrier, selection.generation)
                .is_none()
            {
                return;
            }
            let _prepare = c.remote_audio_prepare.lock().unwrap();
            if !c.selection_current(&selection)
                || c.selection_generation.load(Ordering::Acquire) != selection.generation
            {
                return;
            }
            if let Err(error) = selection.source.check_local_version() {
                selection.preview_ready.store(true, Ordering::Release);
                c.status(error);
                return;
            }
            if let Err(error) = selection
                .source
                .check_remote_version(&c, &selection.preview_cancel)
            {
                selection.preview_ready.store(true, Ordering::Release);
                if c.selection_current(&selection) {
                    c.status(error);
                }
                return;
            }
            let prepared = if selection.source.remote {
                c.playable_path(
                    &selection.source.file,
                    &selection.preview_cancel,
                    selection.generation,
                )
            } else {
                Ok(PathBuf::from(&selection.source.file.file_path))
            };
            let path = match prepared {
                Ok(path) => path,
                Err(error) => {
                    selection.preview_ready.store(true, Ordering::Release);
                    if c.selection_current(&selection) {
                        c.status(error);
                    }
                    return;
                }
            };
            let result = (|| -> audiolib_core::Result<()> {
                if c.audio.status().file_path.as_ref() != Some(&path) {
                    let completion = {
                        let _admission = c.latest_selection.lock().unwrap();
                        if !c.selection_current(&selection)
                            || c.selection_generation.load(Ordering::Acquire)
                                != selection.generation
                        {
                            return Ok(());
                        }
                        c.audio.enqueue_with_ack(AudioCommand::Load(path.clone()))?
                    };
                    completion.wait(Duration::from_secs(4))?;
                }
                selection
                    .source
                    .check_local_version()
                    .map_err(audiolib_core::Error::Validation)?;
                selection
                    .source
                    .check_preview_version(&c, &path, &selection.preview_cancel)
                    .map_err(audiolib_core::Error::Validation)?;
                let completion = {
                    let _admission = c.latest_selection.lock().unwrap();
                    if !c.selection_current(&selection)
                        || c.selection_generation.load(Ordering::Acquire) != selection.generation
                    {
                        return Ok(());
                    }
                    let status = c.audio.status();
                    c.audio.enqueue_with_ack(AudioCommand::PlayRange {
                        range: selection.range,
                        expected_generation: status.generation,
                    })?
                };
                completion.wait(Duration::from_secs(4))?;
                c.list_waveforms.note_audition_start();
                c.clear_audio_reload(selection.source.file.id);
                Ok(())
            })();
            selection.preview_ready.store(true, Ordering::Release);
            if let Err(error) = result {
                if c.selection_current(&selection) {
                    c.status(error.to_string());
                }
            } else {
                let c2 = c.clone();
                post(c.ui.clone(), move |ui| {
                    if c2.selection_current(&selection)
                        && c2.selection_generation.load(Ordering::Acquire) == selection.generation
                    {
                        ui.set_audio_ready(true);
                    }
                });
            }
        }));
    }

    /// Replaying a selected Waveform must never reload the inspector's other file.
    pub(super) fn play_wave_selection(self: &Arc<Self>) -> bool {
        let selected = self.slice.lock().unwrap().selection.clone();
        let Some(selection) =
            selected.filter(|selection| !selection.cancel.load(Ordering::Acquire))
        else {
            return false;
        };
        let status = self.audio.status();
        if status.playback_range == Some(selection.range) {
            self.command(AudioCommand::Play);
        } else {
            self.commit_wave_selection(
                selection.range.start_seconds(),
                selection.range.end_seconds(),
            );
        }
        true
    }

    pub(super) fn seek_wave_selection(self: &Arc<Self>, position: f64) {
        let range = self
            .slice
            .lock()
            .unwrap()
            .selection
            .as_ref()
            .map(|selection| selection.range);
        if range.is_some_and(|range| {
            position < range.start_seconds() || position >= range.end_seconds()
        }) {
            self.clear_wave_selection();
        }
        let source = self.slice.lock().unwrap().source.clone();
        let Some(source) = source else {
            return;
        };
        let status = self.audio.status();
        let matches = status.file_path.as_deref().is_some_and(|path| {
            if source.remote {
                self.remote_wave_file(path).is_some_and(|file| {
                    file.id == source.file.id && file.file_path == source.file.file_path
                })
            } else {
                same_path(path, Path::new(&source.file.file_path))
            }
        });
        if !matches || self.audio_reload.lock().unwrap().is_some() {
            self.load_wave_source_at(source, position);
            return;
        }
        self.command(AudioCommand::SeekAndPlay(position));
    }

    fn load_wave_source_at(self: &Arc<Self>, source: WaveSource, position: f64) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let barrier = match self.enqueue_current_metadata(&ui) {
            Ok(barrier) => barrier,
            Err(error) => {
                ui.set_status(error.into());
                return;
            }
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let generation = {
            let mut remembered = self.latest_selection.lock().unwrap();
            let generation = self.selection_generation.fetch_add(1, Ordering::AcqRel) + 1;
            *remembered = None;
            *self.selection_job.lock().unwrap() = None;
            *self.selection_audio_job.lock().unwrap() = None;
            std::mem::replace(
                &mut *self.remote_audio_cancel.lock().unwrap(),
                cancel.clone(),
            )
            .store(true, Ordering::Release);
            generation
        };
        let guard = Worker::new(self);
        let c = self.clone();
        self.slice_preview_jobs.submit(Box::new(move || {
            let _guard = guard;
            let current = || {
                c.available()
                    && !cancel.load(Ordering::Acquire)
                    && c.selection_generation.load(Ordering::Acquire) == generation
                    && c.slice
                        .lock()
                        .unwrap()
                        .source
                        .as_ref()
                        .is_some_and(|shown| shown.key == source.key)
            };
            if c.wait_selection_metadata(barrier, generation).is_none() {
                return;
            }
            let _prepare = c.remote_audio_prepare.lock().unwrap();
            if !current() {
                return;
            }
            let result = (|| -> Result<(), String> {
                source.check_remote_version(&c, &cancel)?;
                let path = if source.remote {
                    c.playable_path(&source.file, &cancel, generation)?
                } else {
                    PathBuf::from(&source.file.file_path)
                };
                let completion = {
                    let _admission = c.latest_selection.lock().unwrap();
                    if !current() {
                        return Ok(());
                    }
                    c.audio
                        .enqueue_with_ack(AudioCommand::Load(path.clone()))
                        .map_err(|e| e.to_string())?
                };
                completion
                    .wait(Duration::from_secs(4))
                    .map_err(|e| e.to_string())?;
                source.check_preview_version(&c, &path, &cancel)?;
                let range_completion = {
                    let _admission = c.latest_selection.lock().unwrap();
                    if !current() {
                        return Ok(());
                    }
                    let range = c.slice.lock().unwrap().selection.as_ref().map(|s| s.range);
                    range
                        .map(|range| {
                            c.audio.enqueue_with_ack(AudioCommand::PlayRange {
                                range,
                                expected_generation: c.audio.status().generation,
                            })
                        })
                        .transpose()
                        .map_err(|e| e.to_string())?
                };
                if let Some(completion) = range_completion {
                    completion
                        .wait(Duration::from_secs(4))
                        .map_err(|e| e.to_string())?;
                }
                let _admission = c.latest_selection.lock().unwrap();
                if !current() {
                    return Ok(());
                }
                c.audio
                    .send(AudioCommand::SeekAndPlay(position))
                    .map_err(|e| e.to_string())?;
                c.clear_audio_reload(source.file.id);
                Ok(())
            })();
            if current()
                && let Err(error) = result
            {
                c.status(error);
            }
        }));
    }

    fn clear_wave_selection(&self) {
        let _admission = self.latest_selection.lock().unwrap();
        self.selection_generation.fetch_add(1, Ordering::AcqRel);
        let mut state = self.slice.lock().unwrap();
        if let Some(selection) = state.selection.take() {
            selection.invalidate();
        }
        state.revision += 1;
        state.ready = None;
        state.published = None;
        state.drag_requested = false;
        drop(state);
        self.slice_preview_jobs.clear_pending();
        self.slice_render_jobs.clear_pending();
        self.command(AudioCommand::ClearRange);
        if let Some(ui) = self.ui.upgrade() {
            selection_ui_clear(&ui);
        }
    }

    pub(super) fn cancel_slice_preparation(&self) {
        let mut state = self.slice.lock().unwrap();
        if let Some(selection) = state.selection.as_ref() {
            selection.render_cancel.store(true, Ordering::Release);
        }
        state.ready = None;
        state.published = None;
        state.drag_requested = false;
        drop(state);
        if let Some(ui) = self.ui.upgrade() {
            show_export_feedback(&ui, ExportFeedback::Cancelled);
            ui.set_wave_selection_progress(
                if self.chinese.load(Ordering::Acquire) {
                    "已取消；再次拖动可重新准备切片"
                } else {
                    "Cancelled; drag the selection to prepare again"
                }
                .into(),
            );
        }
    }

    fn end_slice_drag(&self) {
        let mut state = self.slice.lock().unwrap();
        state.drag_requested = false;
    }

    pub(super) fn slice_needs_original(&self, file: &AudioFile, rate: u32) -> bool {
        let state = self.slice.lock().unwrap();
        state.selection.as_ref().is_some_and(|selection| {
            selection.source.file.id == file.id
                && selection.source.file.file_path == file.file_path
                && selection
                    .range
                    .map_inward(rate, (file.duration * f64::from(rate)).round() as u64)
                    .is_err()
        })
    }

    /// Transport refreshes retain a selected Waveform even when a search task has
    /// restored a different inspector. An explicit Load of another source wins.
    pub(super) fn slice_wave_file(
        &self,
        status: &audiolib_audio::PlaybackStatus,
    ) -> Option<AudioFile> {
        let selection = self.slice.lock().unwrap().selection.clone()?;
        if !self.selection_current(&selection) {
            return None;
        }
        let path = status.file_path.as_deref()?;
        let matches = if selection.source.remote {
            self.remote_wave_file(path).is_some_and(|file| {
                file.id == selection.source.file.id
                    && file.file_path == selection.source.file.file_path
            })
        } else {
            same_path(path, Path::new(&selection.source.file.file_path))
        };
        matches.then_some(selection.source.file)
    }

    fn slice_progress(self: &Arc<Self>, selection: &Selection, text: String) {
        let c = self.clone();
        let selection = selection.clone();
        post(self.ui.clone(), move |ui| {
            if c.selection_current(&selection) && !selection.render_cancel.load(Ordering::Acquire) {
                ui.set_wave_selection_progress(text.into());
            }
        });
    }

    fn prepare_wave_slice(self: &Arc<Self>, selection: Selection) {
        let remote = if selection.source.remote {
            self.remote_drag_source().map(Some)
        } else {
            Ok(None)
        };
        let guard = Worker::new(self);
        let c = self.clone();
        self.slice_render_jobs.submit(Box::new(move || {
            let _guard = guard;
            let result = (|| -> Result<Arc<PreparedSlice>, String> {
                let deadline = Instant::now() + Duration::from_secs(3);
                while !selection.preview_ready.load(Ordering::Acquire) && Instant::now() < deadline
                {
                    if !c.selection_current(&selection)
                        || selection.render_cancel.load(Ordering::Acquire)
                    {
                        return Err("Cancelled".into());
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                if !c.selection_current(&selection)
                    || selection.render_cancel.load(Ordering::Acquire)
                {
                    return Err("Cancelled".into());
                }
                selection.source.check_local_version()?;
                let export_directory = c.slice_directory().map_err(|e| e.to_string())?;
                std::fs::create_dir_all(&export_directory)
                    .map_err(|e| format!("{}: {e}", export_directory.display()))?;
                let export_directory =
                    export_directory.canonicalize().map_err(|e| e.to_string())?;
                let directory = crate::slice_staging::prepare_staging(&export_directory)
                    .map_err(|e| e.to_string())?;
                let original = if let Some(remote) = remote? {
                    let mut last = Instant::now() - Duration::from_secs(1);
                    Some(c.prepare_remote_slice(
                        &remote,
                        &selection.source.file,
                        &directory.directory.path().join("original.audio"),
                        selection.source.remote_version.as_ref(),
                        &mut |progress| {
                            if last.elapsed() >= Duration::from_millis(100)
                                || progress.total == Some(progress.received)
                            {
                                last = Instant::now();
                                let percent = progress
                                    .total
                                    .filter(|n| *n > 0)
                                    .map_or(0, |n| progress.received.saturating_mul(100) / n);
                                c.slice_progress(
                                    &selection,
                                    if c.chinese.load(Ordering::Acquire) {
                                        format!("下载原素材 {percent}%")
                                    } else {
                                        format!("Downloading original {percent}%")
                                    },
                                );
                            }
                        },
                        &selection.render_cancel,
                    )?)
                } else {
                    None
                };
                let source = if let Some(original) = original.as_ref() {
                    let expected = selection.source.remote_version.as_ref()
                        .ok_or_else(|| c.localized("该波形没有已验证的原素材版本，请重新载入音频", "This Waveform has no verified original version; reload the sound").to_string())?;
                    if original.etag.as_deref() != Some(expected.etag.as_str()) || original.length != expected.length {
                        return Err(c.localized("原素材已在波形绘制后改变，请重新载入音频", "The original changed since this Waveform was rendered; reload the sound").into());
                    }
                    original.path.clone()
                } else {
                    let roots = c.config.lock().unwrap().allowed_scan_roots.clone();
                    audiolib_core::validate_audio_path(
                        Path::new(&selection.source.file.file_path),
                        &roots,
                    )
                    .map_err(|e| e.to_string())?
                };
                if !c.selection_current(&selection)
                    || selection.render_cancel.load(Ordering::Acquire)
                {
                    return Err("Cancelled".into());
                }
                let local_source = (!selection.source.remote)
                    .then(|| SourceStamp::read(&source).map(|stamp| (source.clone(), stamp)))
                    .transpose()
                    .map_err(|e| e.to_string())?;
                let path = directory.directory.path().join("slice.wav");
                let mut last = Instant::now() - Duration::from_secs(1);
                audiolib_audio::export_audio_slice(
                    &source,
                    &path,
                    selection.range,
                    &selection.render_cancel,
                    &mut |done, total| {
                        if last.elapsed() >= Duration::from_millis(100) || done == total {
                            last = Instant::now();
                            let percent = done.saturating_mul(100).checked_div(total).unwrap_or(0);
                            c.slice_progress(
                                &selection,
                                if c.chinese.load(Ordering::Acquire) {
                                    format!("裁剪 WAV {percent}%")
                                } else {
                                    format!("Rendering WAV {percent}%")
                                },
                            );
                        }
                    },
                )
                .map_err(|e| e.to_string())?;
                selection.source.check_local_version()?;
                if !c.selection_current(&selection)
                    || selection.render_cancel.load(Ordering::Acquire)
                {
                    return Err("Cancelled".into());
                }
                Ok(Arc::new(PreparedSlice {
                    _directory: directory,
                    path,
                    export_directory,
                    stem: slice_stem(&selection.source.file.filename),
                    local_source,
                }))
            })();
            let c2 = c.clone();
            post(c.ui.clone(), move |ui| {
                if !c2.selection_current(&selection)
                    || selection.render_cancel.load(Ordering::Acquire)
                {
                    return;
                }
                show_export_feedback(&ui, ExportFeedback::Ready);
                match result {
                    Ok(ready) => {
                        c2.slice.lock().unwrap().ready = Some(ready);
                        ui.set_wave_selection_progress(
                            if c2.chinese.load(Ordering::Acquire) {
                                "切片已就绪，拖动选区导出"
                            } else {
                                "Slice ready; drag the selection to export"
                            }
                            .into(),
                        );
                        if c2.slice.lock().unwrap().drag_requested {
                            c2.publish_slice_drag(selection);
                        }
                    }
                    Err(error) => {
                        show_export_feedback(&ui, ExportFeedback::Error);
                        ui.invoke_cancel_wave_selection_gesture();
                        ui.set_wave_selection_progress(error.clone().into());
                        ui.set_status(error.into());
                    }
                }
            });
        }));
    }

    fn drag_wave_selection(self: &Arc<Self>) {
        if !self.available() {
            return;
        }
        let mut state = self.slice.lock().unwrap();
        let Some(mut selection) = state.selection.clone() else {
            return;
        };
        if selection.cancel.load(Ordering::Acquire) || state.publishing {
            return;
        }
        state.drag_requested = true;
        let ready = state.ready.is_some();
        let preparing = self
            .ui
            .upgrade()
            .is_some_and(|ui| ui.get_wave_selection_preparing());
        let retry = !ready && !preparing;
        if retry {
            selection.render_cancel.store(true, Ordering::Release);
            state.revision += 1;
            selection.revision = state.revision;
            selection.render_cancel = Arc::new(AtomicBool::new(false));
            state.published = None;
            state.selection = Some(selection.clone());
        }
        drop(state);
        if ready {
            self.publish_slice_drag(selection);
        } else if retry {
            if let Some(ui) = self.ui.upgrade() {
                show_export_feedback(&ui, ExportFeedback::Preparing);
            }
            self.prepare_wave_slice(selection);
        }
    }

    fn publish_slice_drag(self: &Arc<Self>, selection: Selection) {
        let (ready, published) = {
            let mut state = self.slice.lock().unwrap();
            if !state.drag_requested || state.publishing {
                return;
            }
            let Some(ready) = state.ready.clone() else {
                return;
            };
            if self.drag_busy.swap(true, Ordering::AcqRel) {
                return;
            }
            state.publishing = true;
            (ready, state.published.take())
        };
        let guard = Worker::new(self);
        let busy = DragBusy(self.clone());
        if let Some(ui) = self.ui.upgrade() {
            show_export_feedback(&ui, ExportFeedback::Preparing);
            ui.set_wave_selection_progress(
                if self.chinese.load(Ordering::Acquire) {
                    "正在准备拖出 WAV…"
                } else {
                    "Preparing WAV drag…"
                }
                .into(),
            );
        }
        let c = self.clone();
        std::thread::spawn(move || {
            let _guard = guard;
            let result =
                if ready.local_source.as_ref().is_some_and(|(path, stamp)| {
                    SourceStamp::read(path).ok().as_ref() != Some(stamp)
                }) {
                    Err(if c.chinese.load(Ordering::Acquire) {
                        "原素材已更改，请重新框选".into()
                    } else {
                        "The original changed; select the range again".into()
                    })
                } else if let Some(published) = published {
                    Ok(published)
                } else {
                    publish_numbered_slice(
                        &ready.path,
                        &ready.export_directory,
                        &ready.stem,
                        &selection.render_cancel,
                    )
                };
            let c2 = c.clone();
            post(c.ui.clone(), move |ui| {
                let _busy = busy;
                if !c2.selection_current(&selection)
                    || selection.render_cancel.load(Ordering::Acquire)
                {
                    return;
                }
                let requested = c2.slice.lock().unwrap().drag_requested;
                show_export_feedback(&ui, ExportFeedback::Ready);
                ui.set_wave_selection_progress(
                    if c2.chinese.load(Ordering::Acquire) {
                        "切片已就绪，拖动选区可再次导出"
                    } else {
                        "Slice ready; drag the selection to export again"
                    }
                    .into(),
                );
                match result {
                    Ok(mut published)
                        if requested
                            && !ui.get_background_blocked()
                            && audiolib_windows::primary_button_down() =>
                    {
                        show_export_feedback(&ui, ExportFeedback::NativeDrag);
                        ui.set_wave_selection_progress(
                            if c2.chinese.load(Ordering::Acquire) {
                                "拖放到目标程序 · Esc 取消"
                            } else {
                                "Drop into the target app · Esc cancels"
                            }
                            .into(),
                        );
                        crate::waveform_cursor::suspend_for_native_drag(&ui);
                        let outcome =
                            audiolib_windows::drag_files(std::slice::from_ref(&published.path));
                        // Native loops may consume the button release. Always
                        // finish this gesture before accepting another one.
                        ui.invoke_cancel_wave_selection_gesture();
                        crate::waveform_cursor::resume_after_native_drag(&ui);
                        match outcome {
                            Ok(audiolib_windows::DragOutcome::Copied) => {
                                published.keep = true;
                                if c2.selection_current(&selection) {
                                    show_export_feedback(&ui, ExportFeedback::Ready);
                                    ui.set_wave_selection_progress(
                                        if c2.chinese.load(Ordering::Acquire) {
                                            "已导出 · 可再次拖出 WAV"
                                        } else {
                                            "Exported · Drag out again for another WAV"
                                        }
                                        .into(),
                                    );
                                }
                                ui.set_status(
                                    format!(
                                        "{} {}",
                                        if c2.chinese.load(Ordering::Acquire) {
                                            "已导出"
                                        } else {
                                            "Exported"
                                        },
                                        published.path.display()
                                    )
                                    .into(),
                                );
                            }
                            Ok(audiolib_windows::DragOutcome::Cancelled) => {
                                if c2.selection_current(&selection) {
                                    show_export_feedback(&ui, ExportFeedback::Cancelled);
                                    ui.set_wave_selection_progress(
                                        if c2.chinese.load(Ordering::Acquire) {
                                            "已取消拖出 · 再次拖动可重试"
                                        } else {
                                            "Drag cancelled · Drag again to retry"
                                        }
                                        .into(),
                                    );
                                }
                            }
                            Err(error) => {
                                if c2.selection_current(&selection) {
                                    show_export_feedback(&ui, ExportFeedback::Error);
                                    ui.set_wave_selection_progress(error.to_string().into());
                                }
                                ui.set_status(error.to_string().into());
                            }
                        }
                    }
                    Ok(published) => {
                        c2.slice.lock().unwrap().published = Some(published);
                        ui.set_status(
                            if c2.chinese.load(Ordering::Acquire) {
                                "切片已就绪，请再次拖出"
                            } else {
                                "Slice ready; drag again to export"
                            }
                            .into(),
                        );
                    }
                    Err(error) => {
                        let mut state = c2.slice.lock().unwrap();
                        state.ready = None;
                        state.published = None;
                        drop(state);
                        show_export_feedback(&ui, ExportFeedback::Error);
                        ui.invoke_cancel_wave_selection_gesture();
                        ui.set_wave_selection_progress(error.clone().into());
                        ui.set_status(error.into());
                    }
                }
            });
        });
    }
}

/// A published filename is durable only after the native target accepted the copy.
struct PublishedSlice {
    path: PathBuf,
    keep: bool,
}
impl Drop for PublishedSlice {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn slice_stem(name: &str) -> String {
    let stem = Path::new(name)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("audio");
    let safe: String = stem
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| if "<>:\"/\\|?*".contains(c) { '_' } else { c })
        .take(100)
        .collect();
    let safe = safe.trim().trim_end_matches(['.', ' ']);
    if safe.is_empty() {
        "audio".into()
    } else {
        safe.to_string()
    }
}

fn publish_numbered_slice(
    source: &Path,
    directory: &Path,
    stem: &str,
    cancel: &AtomicBool,
) -> Result<PublishedSlice, String> {
    for sequence in 1..=999_999_u32 {
        if cancel.load(Ordering::Acquire) {
            return Err("Cancelled".into());
        }
        let path = directory.join(format!("{stem}_slice_{sequence:03}.wav"));
        // Each durable file owns its bytes: a DAW may edit it in place without
        // changing the prepared slice reused by a subsequent export.
        let mut output = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("{}: {error}", path.display())),
        };
        let published = PublishedSlice { path, keep: false };
        let mut input = std::fs::File::open(source).map_err(|e| e.to_string())?;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            if cancel.load(Ordering::Acquire) {
                return Err("Cancelled".into());
            }
            let read = input.read(&mut buffer).map_err(|e| e.to_string())?;
            if read == 0 {
                break;
            }
            output
                .write_all(&buffer[..read])
                .map_err(|e| e.to_string())?;
        }
        output.sync_all().map_err(|e| e.to_string())?;
        return Ok(published);
    }
    Err("No available slice sequence number".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbered_exports_never_replace_and_only_accepted_files_persist() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("prepared.wav");
        std::fs::write(&source, b"complete WAV fixture").unwrap();
        let cancel = AtomicBool::new(false);
        let mut first = publish_numbered_slice(&source, root.path(), "音效", &cancel).unwrap();
        first.keep = true;
        let second = publish_numbered_slice(&source, root.path(), "音效", &cancel).unwrap();
        assert!(first.path.ends_with("音效_slice_001.wav"));
        assert!(second.path.ends_with("音效_slice_002.wav"));
        let cancelled = second.path.clone();
        drop(second);
        assert!(!cancelled.exists());
        assert_eq!(std::fs::read(&first.path).unwrap(), b"complete WAV fixture");
        assert_eq!(std::fs::read(&source).unwrap(), b"complete WAV fixture");
        cancel.store(true, Ordering::Release);
        assert!(publish_numbered_slice(&source, root.path(), "音效", &cancel).is_err());
    }

    #[test]
    fn slice_names_keep_unicode_and_cannot_escape_the_export_directory() {
        assert_eq!(slice_stem("音效.wav"), "音效");
        assert!(!slice_stem("../bad:na?me.wav").contains(['/', '\\', ':', '?']));
        assert_eq!(slice_stem(".wav"), ".wav");
    }
}
