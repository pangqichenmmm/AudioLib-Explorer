// app/audiolib-desktop/src/controller_artwork.rs
//! Independent artwork worker and client-local cover management, outside the audio path.
use super::*;

#[derive(Default)]
pub(super) struct ArtworkActions {
    pub(super) pending: usize,
    failure: Option<String>,
}

/// Destinations are captured while admitting the action on the event loop. An
/// accepted edit belongs to that source even if the user switches sources later.
pub(super) struct ArtworkActionContext {
    cache_root: PathBuf,
    thumbnail_path: Option<PathBuf>,
    identity: Option<(PathBuf, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ArtworkActionError {
    Input(String),
    Transient(String),
    Persistence(String),
}

impl ArtworkActionError {
    fn into_message(self) -> String {
        match self {
            Self::Input(message) | Self::Transient(message) | Self::Persistence(message) => message,
        }
    }
}

pub(super) struct ArtworkActionGuard {
    controller: Arc<Controller>,
    result: Option<Result<bool, ArtworkActionError>>,
}

impl ArtworkActionGuard {
    pub(super) fn complete(mut self, result: Result<bool, ArtworkActionError>) {
        self.result = Some(result);
    }
}

impl Drop for ArtworkActionGuard {
    fn drop(&mut self) {
        let mut actions = self.controller.artwork_actions.lock().unwrap();
        match self.result.take() {
            Some(Ok(true)) => actions.failure = None,
            Some(
                Ok(false) | Err(ArtworkActionError::Input(_) | ArtworkActionError::Transient(_)),
            ) => {}
            Some(Err(ArtworkActionError::Persistence(error))) => actions.failure = Some(error),
            None => {
                actions.failure = Some(internal_error(
                    self.controller.chinese.load(Ordering::Acquire),
                    "保存封面设置",
                    "Saving artwork settings",
                ));
            }
        }
        actions.pending -= 1;
        self.controller.artwork_actions_wake.notify_all();
    }
}

impl Controller {
    pub(super) fn begin_artwork_action(self: &Arc<Self>) -> ArtworkActionGuard {
        self.artwork_actions.lock().unwrap().pending += 1;
        ArtworkActionGuard {
            controller: self.clone(),
            result: None,
        }
    }

    /// Drain admitted persistent edits, including while the close gate is held.
    pub(super) fn wait_artwork_actions(&self) -> Result<(), String> {
        let mut actions = self.artwork_actions.lock().unwrap();
        while actions.pending != 0 {
            actions = self.artwork_actions_wake.wait(actions).unwrap();
        }
        actions.failure.clone().map_or(Ok(()), |error| {
            Err(format!(
                "{}: {error}",
                cn_or(
                    self.chinese.load(Ordering::Acquire),
                    "封面设置未保存",
                    "Artwork settings were not saved",
                )
            ))
        })
    }

    pub(super) fn artwork_action_context(&self, file: Option<&AudioFile>) -> ArtworkActionContext {
        let cache_root = self.config.lock().unwrap().paths.cache.clone();
        ArtworkActionContext {
            cache_root,
            thumbnail_path: file.and_then(|file| self.cached_artwork_path(file)),
            identity: file.and_then(|file| self.artwork_identity(file)),
        }
    }

    pub(super) fn start_artwork_worker(self: &Arc<Self>) {
        let c = self.clone();
        std::thread::Builder::new()
            .name("audiolib-artwork".into())
            .spawn(move || {
                loop {
                    let mut pending = c.artwork_job.lock().unwrap();
                    while pending.is_none() && !c.shutdown.load(Ordering::Acquire) {
                        pending = c.artwork_wake.wait(pending).unwrap();
                    }
                    if c.shutdown.load(Ordering::Acquire) {
                        return;
                    }
                    let job = pending.take().unwrap();
                    drop(pending);
                    if !c.artwork_current(&job) {
                        c.artwork_job_finished(&job);
                        continue;
                    }
                    let pixels = c.load_artwork(&job);
                    let notice = job.notice.lock().unwrap().take();
                    c.artwork_job_finished(&job);
                    let confirmed = c.clone();
                    post(c.ui.clone(), move |ui| {
                        if !confirmed.artwork_current(&job) {
                            return;
                        }
                        if let Some(notice) = notice {
                            ui.set_artwork_message(notice.into());
                        }
                        let ready = pixels.is_some();
                        let image = pixels
                            .map(|pixels| {
                                Image::from_rgba8(
                                    SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
                                        pixels.as_raw(),
                                        pixels.width(),
                                        pixels.height(),
                                    ),
                                )
                            })
                            .unwrap_or_default();
                        ui.set_album_cover(image.clone());
                        ui.set_album_cover_ready(ready);
                        if !ready {
                            return;
                        }
                        confirmed.remember_album_artwork(&job);
                        let album = job.album.id.to_string();
                        for index in 0..ui.get_albums().row_count() {
                            if let Some(mut row) = ui.get_albums().row_data(index)
                                && row.id.as_str() == album
                            {
                                row.cover = image.clone();
                                ui.get_albums().set_row_data(index, row);
                            }
                        }
                    });
                }
            })
            .expect("spawn artwork worker");
    }

    pub(super) fn perform_artwork_action(
        &self,
        context: ArtworkActionContext,
        action: i32,
        chosen: Option<PathBuf>,
        chinese: bool,
    ) -> Result<bool, ArtworkActionError> {
        // Decode a chosen source before taking the cache lock, just as automatic
        // extraction does. All subsequent mutations use admission-time paths.
        let chosen_pixels = if action == 2 {
            let Some(path) = chosen else { return Ok(false) };
            Some(crate::artwork::read_thumbnail(&path, 512).ok_or_else(|| {
                ArtworkActionError::Input(cn_or(
                    chinese,
                    "请选择有效 PNG/JPEG（不超过 8 MiB、4096×4096）",
                    "Choose a valid PNG/JPEG (up to 8 MiB, 4096×4096)",
                ))
            })?)
        } else {
            None
        };
        let _cache = self.artwork_cache_lock.lock().unwrap();
        if action == 0 {
            if let Some(path) = context.thumbnail_path {
                remove_if_present(&path).map_err(ArtworkActionError::Transient)?;
                remove_if_present(&path.with_extension("etag"))
                    .map_err(ArtworkActionError::Transient)?;
            }
            return Ok(true);
        }
        if action == 1 {
            crate::artwork::clean_cache(&context.cache_root, true)
                .map_err(|error| ArtworkActionError::Transient(error.to_string()))?;
            return Ok(true);
        }
        let (directory, key) = context
            .identity
            .ok_or_else(|| ArtworkActionError::Input("No active library".to_string()))?;
        let image_path = directory.join(format!("{key}.png"));
        let hidden_path = directory.join(format!("{key}.hidden"));
        match action {
            2 => {
                crate::artwork::save_thumbnail(&image_path, &chosen_pixels.unwrap())
                    .map_err(|error| ArtworkActionError::Persistence(error.to_string()))?;
                crate::album_override_migration::mark_completed(&directory, &key)
                    .map_err(|error| ArtworkActionError::Persistence(error.to_string()))?;
                remove_if_present(&hidden_path).map_err(ArtworkActionError::Persistence)?;
            }
            3 => {
                std::fs::create_dir_all(&directory)
                    .map_err(|error| ArtworkActionError::Persistence(error.to_string()))?;
                std::fs::write(&hidden_path, [])
                    .map_err(|error| ArtworkActionError::Persistence(error.to_string()))?;
                crate::album_override_migration::mark_completed(&directory, &key)
                    .map_err(|error| ArtworkActionError::Persistence(error.to_string()))?;
                remove_if_present(&image_path).map_err(ArtworkActionError::Persistence)?;
            }
            4 => {
                crate::album_override_migration::mark_completed(&directory, &key)
                    .map_err(|error| ArtworkActionError::Persistence(error.to_string()))?;
                remove_if_present(&image_path).map_err(ArtworkActionError::Persistence)?;
                remove_if_present(&hidden_path).map_err(ArtworkActionError::Persistence)?;
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    pub(super) fn finish_artwork_action(
        self: &Arc<Self>,
        ui: &AppWindow,
        source_generation: u64,
        chinese: bool,
        result: Result<bool, String>,
    ) {
        ui.set_artwork_busy(false);
        if self.source_generation.load(Ordering::Acquire) != source_generation || !self.available()
        {
            return;
        }
        let refresh_search = match result {
            Ok(false) => false,
            Ok(true) => {
                ui.set_artwork_message(
                    cn_or(chinese, "封面设置已更新", "Artwork settings updated").into(),
                );
                ui.set_album_cover_ready(false);
                ui.set_album_cover(Image::default());
                true
            }
            Err(error) => {
                ui.set_artwork_message(error.into());
                true
            }
        };
        // Cancellation and errors must resume the automatic request invalidated
        // at admission. Otherwise cancelling the picker can leave a blank cover.
        let file = self.selected.lock().unwrap().clone();
        if let Some(file) = file {
            self.queue_artwork(&file);
        }
        if refresh_search {
            self.request_search(ui, true, false);
        }
    }

    pub(super) fn bind_artwork(self: &Arc<Self>, ui: &AppWindow) {
        let c = self.clone();
        ui.on_artwork_enabled_requested(move |enabled| {
            if !c.available() {
                return;
            }
            c.cancel_artwork();
            c.show_album_artwork.store(enabled, Ordering::Release);
            *c.single_click_play_pending.lock().unwrap() = true;
            c.single_click_play_wake.notify_one();
            if let Some(ui) = c.ui.upgrade() {
                ui.set_artwork_enabled(enabled);
                ui.set_album_cover_ready(false);
                ui.set_album_cover(Image::default());
                ui.set_artwork_message("".into());
                let file = c.selected.lock().unwrap().clone();
                if let Some(file) = file {
                    c.queue_artwork(&file);
                }
                c.request_search(&ui, true, false);
            }
        });
        let c = self.clone();
        ui.on_artwork_action_requested(move |action| {
            if !c.available() {
                return;
            }
            let Some(ui) = c.ui.upgrade() else { return };
            if ui.get_artwork_busy() {
                return;
            }
            let file = c.selected.lock().unwrap().clone();
            if action != 1 && file.as_ref().is_none_or(|file| file.album_entity.is_none()) {
                return;
            }
            let context = c.artwork_action_context(file.as_ref());
            let source_generation = c.source_generation.load(Ordering::Acquire);
            let owner = crate::dialogs::owner_handle(ui.window());
            let chinese = c.chinese.load(Ordering::Acquire);
            c.cancel_artwork();
            ui.set_artwork_busy(true);
            ui.set_artwork_message("".into());
            let guard = c.begin_artwork_action();
            let worker = c.clone();
            std::thread::spawn(move || {
                // Native selection happens before taking the cache lock; playback and
                // selection remain independent while the user chooses a picture.
                let result = survive_panic(
                    || {
                        let chosen = if action == 2 {
                            crate::dialogs::choose(
                                false,
                                crate::dialogs::FileKind::Image,
                                chinese,
                                owner,
                            )
                        } else {
                            Ok(None)
                        }
                        .map_err(ArtworkActionError::Input)?;
                        worker.perform_artwork_action(context, action, chosen, chinese)
                    },
                    || {
                        ArtworkActionError::Persistence(internal_error(
                            chinese,
                            "保存封面设置",
                            "Saving artwork settings",
                        ))
                    },
                );
                guard.complete(result.clone());
                let confirmed = worker.clone();
                post(worker.ui.clone(), move |ui| {
                    confirmed.finish_artwork_action(
                        &ui,
                        source_generation,
                        chinese,
                        result.map_err(ArtworkActionError::into_message),
                    );
                });
            });
        });
    }
}

fn remove_if_present(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

fn cn_or(chinese: bool, chinese_text: &str, english_text: &str) -> String {
    if chinese { chinese_text } else { english_text }.to_string()
}
