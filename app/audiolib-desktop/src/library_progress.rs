// app/audiolib-desktop/src/library_progress.rs
//! Worker-side local opens and bounded stage publication on the Slint event loop.
use super::{Controller, post};
use crate::{AppWindow, startup::StartupProgress};
use audiolib_core::DataProvider;
use audiolib_db::{SnapshotUpgradeProgress, SqliteProvider};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// Logs durations without paths or credentials. Dropping the reporter invalidates
/// pending posts so they cannot overwrite progress belonging to a later operation.
pub(super) struct LibraryProgress {
    ui: slint::Weak<AppWindow>,
    chinese: bool,
    started: Instant,
    stage_started: Instant,
    stage: &'static str,
    last_post: Option<Instant>,
    active: Arc<AtomicBool>,
}

impl LibraryProgress {
    pub(super) fn new(controller: &Controller) -> Self {
        Self {
            ui: controller.ui.clone(),
            chinese: controller.chinese.load(Ordering::Acquire),
            started: Instant::now(),
            stage_started: Instant::now(),
            stage: "",
            last_post: None,
            active: Arc::new(AtomicBool::new(true)),
        }
    }

    pub(super) fn stage(&mut self, stage: &'static str, cn: &str, en: &str) {
        self.publish(
            stage,
            if self.chinese {
                cn.to_owned()
            } else {
                en.to_owned()
            },
        );
    }

    fn publish(&mut self, stage: &'static str, label: String) {
        if stage != self.stage {
            self.log_stage();
            self.stage = stage;
            self.stage_started = Instant::now();
            self.last_post = None;
            tracing::info!(stage, "Library operation stage started");
        }
        if self
            .last_post
            .is_some_and(|last| last.elapsed() < Duration::from_millis(100))
        {
            return;
        }
        self.last_post = Some(Instant::now());
        let active = self.active.clone();
        post(self.ui.clone(), move |ui| {
            if active.load(Ordering::Acquire) && ui.get_library_busy() {
                ui.set_library_operation_stage(label.into());
            }
        });
    }

    fn migration(&mut self, stage: SnapshotUpgradeProgress) {
        use SnapshotUpgradeProgress::*;
        if matches!(stage, Preparing) {
            self.stage(
                "preparing_local",
                "正在检查本地音效库格式…",
                "Checking the local library format…",
            );
            return;
        }
        let key = match stage {
            Preparing => unreachable!(),
            BackingUp => "migration_backup",
            Copying { .. } => "migration_copy",
            Tokenizing { .. } => "migration_tokenizing",
            BuildingFts => "migration_fts",
            Indexing { .. } => "migration_indexing",
            SourceTags => "migration_tags",
            Validating => "migration_validation",
            Compacting => "migration_compacting",
            Publishing => "migration_publishing",
            Checkpoint => "migration_checkpoint",
        };
        self.publish(key, StartupProgress::Local(stage).label(self.chinese));
    }

    fn log_stage(&self) {
        if !self.stage.is_empty() {
            tracing::info!(
                stage = self.stage,
                elapsed_ms = self.stage_started.elapsed().as_millis() as u64,
                "Library operation stage finished"
            );
        }
    }
}

impl Drop for LibraryProgress {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
        self.log_stage();
        tracing::info!(
            elapsed_ms = self.started.elapsed().as_millis() as u64,
            "Library operation stages finished"
        );
    }
}

impl Controller {
    /// Ordinary opens retain identity, supported format, migration and lease checks.
    /// Full integrity checks (including an owned backup) are reserved for admission
    /// of a new source and explicit maintenance, never every registered-source switch.
    pub(super) fn open_local_for_switch(
        &self,
        path: &Path,
        create: bool,
        full_check: bool,
        progress: &mut LibraryProgress,
    ) -> Result<SqliteProvider, String> {
        progress.stage(
            "opening_local",
            "正在验证并打开本地音效库…",
            "Validating and opening the local library…",
        );
        let opening_started = Instant::now();
        let config = self.config.lock().unwrap().clone();
        let result = (|| {
            let mut report = |stage| progress.migration(stage);
            let provider = if create {
                SqliteProvider::open_local_with_progress_cancellable(
                    path,
                    &config.data_root(),
                    config.play_history_limit,
                    &self.shutdown,
                    &mut report,
                )
            } else {
                SqliteProvider::open_existing_local_with_progress_cancellable(
                    path,
                    &config.data_root(),
                    config.play_history_limit,
                    &self.shutdown,
                    &mut report,
                )
            }
            .map_err(|error| error.local_display_text(progress.chinese))?;
            if full_check {
                progress.stage(
                    "admission_integrity",
                    "正在完整校验新添加的音效库（需要复制数据库）…",
                    "Checking the newly added library (copies the database)…",
                );
                let integrity = provider
                    .check_library()
                    .map_err(|error| error.to_string())?;
                if !integrity.is_ok() {
                    return Err(if progress.chinese {
                        "所选音效库未通过校验。"
                    } else {
                        "Selected library failed validation."
                    }
                    .into());
                }
            }
            progress.stage(
                "search_preparation",
                "正在准备搜索索引…",
                "Preparing the search index…",
            );
            crate::startup::prepare_local_search(&provider);
            Ok(provider)
        })();
        tracing::info!(
            success = result.is_ok(),
            full_check,
            elapsed_ms = opening_started.elapsed().as_millis() as u64,
            "Local library open finished"
        );
        result
    }
}
