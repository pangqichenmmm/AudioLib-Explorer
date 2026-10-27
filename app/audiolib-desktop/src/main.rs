// app/audiolib-desktop/src/main.rs
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#[cfg(test)]
mod close_tests;
mod controller;
mod diagnostics;
mod dialogs;
mod gui_search_benchmark;
mod list_waveforms;
#[cfg(test)]
mod pane_review_tests;
mod progressive_waveform;
#[cfg(test)]
mod responsive_layout_tests;
#[cfg(test)]
mod search_task_ui_tests;
mod startup;
mod tag_chips;
mod theme;
#[cfg(test)]
mod ui_review_tests;
#[cfg(test)]
mod ui_scale_tests;
#[cfg(test)]
mod ui_tests;
mod updates;
mod waveform_view;
mod window_layout;
mod window_repaint;
use anyhow::{Context, Result};
use audiolib_audio::{AudioEngine, PlaybackConfig};
use audiolib_core::{DataProvider, config::AppConfig};
#[cfg(test)]
use audiolib_db::SqliteProvider;
use slint::ComponentHandle;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
slint::include_modules!();

/// Hold an OS file lock for this user's preferences throughout the desktop lifetime.
/// The sidecar remains on disk; the operating system releases the lock on process exit.
struct DesktopInstanceGuard {
    _file: std::fs::File,
}

impl DesktopInstanceGuard {
    fn acquire(settings: &std::path::Path) -> Result<Self> {
        let directory = settings.parent().unwrap_or(std::path::Path::new("."));
        let path = directory.join("desktop-instance.lock");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("Open desktop instance lock: {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) => anyhow::bail!(
                "AudioLib Explorer is already running for this user data directory: {}. Close the existing window before starting another instance.",
                directory.display()
            ),
            Err(std::fs::TryLockError::Error(error)) => {
                Err(error).with_context(|| format!("Lock desktop instance: {}", path.display()))
            }
        }
    }
}

fn main() {
    if let Err(error) = run() {
        report_fatal_error(&error);
        std::process::exit(1);
    }
}

fn report_fatal_error(error: &anyhow::Error) {
    let message = format!("AudioLib Explorer: {error:#}");
    eprintln!("{message}");
    // Explicit GUI diagnostics own an isolated report/log path. An invalid
    // diagnostic invocation must not append to normal user state or open a modal.
    if std::env::args().any(|argument| argument.starts_with("--gui-search-")) {
        return;
    }
    // The asynchronous tracing guard may already have been dropped by run(), and
    // invalid preferences can fail before tracing starts. Persist this synchronously.
    let root = std::env::var_os("AUDIOLIB_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("LOCALAPPDATA")
                .map(|path| PathBuf::from(path).join("AudioLibExplorer/v1"))
        })
        .unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_else(|_| std::env::temp_dir())
                .join(".audiolib-user")
        });
    let logs = root.join("logs");
    let log_path = logs.join("AudioLibExplorer-startup-error.log");
    let result = (|| -> std::io::Result<()> {
        std::fs::create_dir_all(&logs)?;
        let mut log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;
        use std::io::Write;
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        writeln!(log, "{timestamp}: {message}")?;
        log.sync_all()
    })();
    let chinese = std::fs::read(root.join("settings.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .is_some_and(|settings| settings["language"].as_str() == Some("cn"));
    let diagnostic = match (result, chinese) {
        (Ok(()), true) => format!(
            "启动失败，请查看以下诊断信息。\n{message}\n\n日志：{}",
            log_path.display()
        ),
        (Err(log_error), true) => {
            format!("启动失败，请查看以下诊断信息。\n{message}\n\n无法写入日志：{log_error}")
        }
        (Ok(()), false) => format!("{message}\n\nLog: {}", log_path.display()),
        (Err(log_error), false) => format!("{message}\n\nUnable to write log: {log_error}"),
    };
    if !std::env::args().any(|argument| argument == "--headless-smoke" || argument == "--smoke-ui")
    {
        audiolib_windows::show_error_dialog(&diagnostic);
    }
}

fn run() -> Result<()> {
    if audiolib_update::handle_update_helper()? {
        return Ok(());
    }
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if arguments
        .iter()
        .any(|argument| argument.starts_with("--gui-search-"))
    {
        return gui_search_benchmark::run(&arguments);
    }
    let config = AppConfig::load_default().context("Load AudioLib preferences")?;
    let _instance = DesktopInstanceGuard::acquire(&config.paths.settings)?;
    // Read the snapshot under the instance lock as a CLI library selection may have
    // completed between the initial path discovery and acquiring this guard.
    let mut config = AppConfig::load_default().context("Reload locked AudioLib preferences")?;
    let default_database = config
        .paths
        .settings
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join("library.db");
    // Server mode only with a usable active profile: otherwise open local for this
    // session while saves keep the persisted selection. Without an openable local
    // library the server selection stays (its placeholder lets the user fix the
    // profile) instead of refusing to start. Logged once tracing is ready.
    let local_usable = config.paths.database.exists() || config.paths.database == default_database;
    let server_fallback = if local_usable {
        config.guard_server_mode().map(|reason| (reason, true))
    } else {
        config
            .data_source
            .server_mode_problem()
            .map(|reason| (reason, false))
    };
    // Server mode never opens (or creates) the local working library.
    let server_mode = config.data_source.mode == audiolib_core::config::DataSourceMode::Server;
    anyhow::ensure!(
        server_mode || config.paths.database.exists() || config.paths.database == default_database,
        "The selected library no longer exists: {}. Use AudioLibCli db use <existing-library.db> to select an existing library.",
        config.paths.database.display()
    );
    let log = tracing_appender::rolling::daily(&config.paths.logs, "AudioLibExplorer.log");
    let (writer, _log_guard) = tracing_appender::non_blocking(log);
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(writer)
        .try_init()
        .ok();
    // Release builds have no console: record every panic, worker threads' included,
    // in the log before the default hook runs.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        tracing::error!(
            thread = thread.name().unwrap_or("unnamed"),
            backtrace = %std::backtrace::Backtrace::force_capture(),
            "Panic: {info}"
        );
        default_hook(info);
    }));
    // Warning-only SQLite self-check: log it and keep starting.
    match audiolib_db::sqlite_engine::identity() {
        Ok(engine) => {
            if let Some(warning) =
                audiolib_core::compatibility::check_linked_sqlite(&engine.version)
            {
                tracing::warn!(%warning, "Linked SQLite self-check");
            }
        }
        Err(error) => tracing::warn!(%error, "Could not read the linked SQLite version"),
    }
    match server_fallback {
        Some((reason, true)) => {
            tracing::warn!(%reason, "Server data source unusable; starting in local mode")
        }
        Some((reason, false)) => tracing::warn!(
            %reason,
            "Server data source unusable and no local library exists; keeping server mode"
        ),
        None => {}
    }
    if arguments
        .iter()
        .any(|argument| argument == "--headless-smoke")
    {
        let (sqlite, _) = startup::open_library(&config)?;
        let page = sqlite.search(&audiolib_core::SearchRequest::default())?;
        println!(
            "{}",
            serde_json::json!({"success":true,"total_count":page.total_count,"page_size":page.results.len(),"database":config.paths.database})
        );
        return Ok(());
    }
    let ui = AppWindow::new().context("Create desktop window")?;
    window_repaint::install(ui.window());
    ui.set_app_version(env!("CARGO_PKG_VERSION").into());
    ui.set_chinese(config.language == "cn");
    // Smoke runs keep the design layout so screenshots stay deterministic.
    let restore_layout = !arguments.iter().any(|argument| argument == "--smoke-ui");
    if restore_layout {
        window_layout::restore_before_show(&ui, &config.window_layout);
    }
    let Some((sqlite, remote)) = startup::show_while_opening(&ui, config.clone())? else {
        return Ok(());
    };
    if restore_layout {
        window_layout::keep_on_screen(&ui);
    }
    let playback =
        serde_json::from_value::<PlaybackConfig>(config.playback.clone()).unwrap_or_default();
    let engine = AudioEngine::new(playback.clone()).context("Initialize audio engine")?;
    let _diagnostics = diagnostics::install(&ui, config.paths.logs.clone(), engine.handle());
    let controller = controller::Controller::new_with_source(
        &ui,
        sqlite,
        engine.handle(),
        config,
        playback,
        remote,
    );
    let updates = updates::DesktopUpdates::install(&ui, controller.clone(), &arguments);
    let close_control = controller.clone();
    let close_ui = ui.as_weak();
    ui.window().on_close_requested(move || {
        if let Some(ui) = close_ui.upgrade() {
            close_control.request_close(&ui);
        }
        slint::CloseRequestResponse::KeepWindowShown
    });
    let control = controller.clone();
    let _ipc = audiolib_ipc::start_server(Arc::new(move |value| control.ipc(value)))
        .map_err(|error| {
            tracing::warn!(%error,"Local playback control unavailable");
            error
        })
        .ok();
    let timer = slint::Timer::default();
    let selection_timer = slint::Timer::default();
    let closing_timer = slint::Timer::default();
    let smoke_failed = Arc::new(AtomicBool::new(false));
    let smoke = arguments.iter().any(|argument| argument == "--smoke-ui");
    if smoke {
        let screenshot = arguments
            .windows(2)
            .find(|pair| pair[0] == "--screenshot")
            .map(|pair| PathBuf::from(&pair[1]))
            .unwrap_or_else(|| std::env::temp_dir().join("AudioLibExplorer-ui.png"));
        if let Some(language) = arguments
            .windows(2)
            .find(|pair| pair[0] == "--smoke-language")
            .map(|pair| pair[1].as_str())
        {
            ui.set_chinese(language == "cn");
            ui.invoke_language_requested(language == "cn");
        }
        let smoke_view = arguments
            .windows(2)
            .find(|pair| pair[0] == "--smoke-view")
            .map(|pair| pair[1].clone())
            .unwrap_or_default();
        {
            match smoke_view.as_str() {
                "output" => {
                    ui.set_settings_visible(true);
                    ui.invoke_devices_requested(ui.get_backend_index());
                }
                "about" => ui.set_about_visible(true),
                "update" => {
                    ui.set_update_visible(true);
                    ui.set_update_version("v0.7.0-beta.2".into());
                    ui.set_update_message("A new version is available. / 发现新版本。".into());
                    ui.set_update_available(true);
                }
                "help" => ui.set_help_visible(true),
                "debug" => {
                    ui.set_debug_visible(true);
                    ui.set_debug_title(
                        if ui.get_chinese() {
                            "播放后搜索结果丢失"
                        } else {
                            "Search results disappear after playback"
                        }
                        .into(),
                    );
                    ui.set_debug_steps(
                        if ui.get_chinese() {
                            "搜索 → 播放 → 检查结果是否保留。"
                        } else {
                            "Search → Play → Check that results remain."
                        }
                        .into(),
                    );
                    ui.invoke_debug_mode_requested(true);
                    ui.invoke_debug_action_requested(0);
                }
                "peaks" => ui.set_peaks_visible(true),
                "normal" => {
                    ui.set_peak_mode(0);
                    ui.invoke_peak_style_requested();
                }
                "database" | "library" => ui.invoke_open_library(0),
                "maintenance" => {
                    ui.invoke_open_library(0);
                    ui.set_advanced_maintenance_visible(true);
                }
                "batch" => {
                    ui.invoke_check_page_requested(true);
                    ui.set_batch_visible(true);
                }
                "filters" | "filter-tags" => ui.set_filters_visible(true),
                "import" => ui.set_import_visible(true),
                "data-source" => ui.invoke_open_data_source(),
                "server-settings" => {
                    ui.invoke_open_data_source();
                    ui.invoke_sm_select_row(-1);
                }
                "cache-export" | "server-cache" => {
                    // Synthetic presentation-only fixture of 缓存与导出; never connects to a
                    // server or changes a working library. Smoke runs use an isolated data dir.
                    ui.set_library_visible(true);
                    ui.set_remote_mode(true);
                    ui.set_sm_sources(slint::ModelRc::new(slint::VecModel::from(vec![
                        SourceRow {
                            name: "本地素材库".into(),
                            ..Default::default()
                        },
                        SourceRow {
                            name: "Sound server".into(),
                            subtitle: "https://example.invalid".into(),
                            kind: 1,
                            active: true,
                            ..Default::default()
                        },
                    ])));
                    ui.set_sm_selected(1);
                    ui.set_sm_page(2);
                    ui.set_sm_cache_available(true);
                    ui.set_sm_cache_summary(
                        if ui.get_chinese() {
                            "音效库 1a2b3c4d · 快照 r794 · 3009 个音效 · 41.7 MiB · 最近检查 10-21 00:10"
                        } else {
                            "Library 1a2b3c4d · snapshot r794 · 3009 sounds · 41.7 MiB · checked 10-21 00:10"
                        }
                        .into(),
                    );
                    ui.set_sm_cache_dir(
                        if ui.get_chinese() {
                            "缓存目录：D:/AudioLib/data/remote/1a2b3c4d-0000-4000-8000-000000000000"
                        } else {
                            "Cache directory: D:/AudioLib/data/remote/1a2b3c4d-0000-4000-8000-000000000000"
                        }
                        .into(),
                    );
                    ui.set_sm_cache_info(
                        "音效库 1a2b3c4d（1a2b3c4d-0000-4000-8000-000000000000）".into(),
                    );
                    ui.set_ds_sync_info("已是最新 · r794".into());
                    ui.set_sm_wave_supported(true);
                    ui.set_sm_wave_can_change(true);
                    ui.set_sm_wave_pregenerate(true);
                    ui.set_sm_wave_status("已生成 2870 / 3009".into());
                    ui.set_sm_wc_visible(true);
                    ui.set_sm_wc_jobs(true);
                    ui.set_sm_wc_can_manage(true);
                    ui.set_sm_wc_rate_setting(true);
                    ui.set_sm_wc_offers_missing(true);
                    ui.set_sm_wc_offers_failed(true);
                    ui.set_sm_wc_offers_rebuild(true);
                    ui.set_sm_wc_has_failed(true);
                    ui.set_sm_wc_run_state("空闲".into());
                    ui.set_sm_wc_progress(0.95);
                    ui.set_sm_wc_progress_text("2870 / 3009 · 失败 12".into());
                    ui.set_sm_wc_rate_draft("20".into());
                    ui.set_sm_cw_stats("418 条 · 23.6 MiB".into());
                }
                "server-roots" | "remote-folder" => {
                    // Synthetic presentation-only fixture; never connects to a server
                    // or changes a working library. Smoke runs use an isolated data dir.
                    ui.set_library_visible(true);
                    ui.set_remote_mode(true);
                    ui.set_sm_sources(slint::ModelRc::new(slint::VecModel::from(vec![
                        SourceRow {
                            name: "本地素材库".into(),
                            ..Default::default()
                        },
                        SourceRow {
                            name: "Sound server".into(),
                            subtitle: "https://example.invalid".into(),
                            kind: 1,
                            active: true,
                            ..Default::default()
                        },
                    ])));
                    ui.set_sm_selected(1);
                    ui.set_sm_page(1);
                    ui.set_sm_can_manage(true);
                    ui.set_sm_root_controls(true);
                    ui.set_sm_roots(slint::ModelRc::new(slint::VecModel::from(vec![
                        ServerRootRow {
                            name: "Foley".into(),
                            path: "/Volumes/Sounds/Foley".into(),
                            status: "在线 / Online".into(),
                            file_count: "1200".into(),
                            missing: "0".into(),
                            last_scan: "2026-10-13 09:30".into(),
                            ..Default::default()
                        },
                        ServerRootRow {
                            name: "Ambience".into(),
                            path: "/Volumes/Sounds/Ambience".into(),
                            status: "在线 / Online".into(),
                            file_count: "3400".into(),
                            missing: "2".into(),
                            last_scan: "2026-10-13 09:32".into(),
                            ..Default::default()
                        },
                    ])));
                    ui.set_sm_root_selected(0);
                    ui.set_sm_library_status("Sound server · 2 roots".into());
                    if smoke_view == "remote-folder" {
                        ui.set_sm_root_form(true);
                        ui.set_sm_root_name("New sounds".into());
                        ui.set_sm_fs_path("/Volumes/Sounds".into());
                        ui.set_sm_fs_parent("/Volumes".into());
                        ui.set_sm_fs_can_select(true);
                        ui.set_sm_fs_breadcrumbs(slint::ModelRc::new(slint::VecModel::from(vec![
                            RemoteBreadcrumbRow {
                                name: "/".into(),
                                path: "/".into(),
                            },
                            RemoteBreadcrumbRow {
                                name: "Volumes".into(),
                                path: "/Volumes".into(),
                            },
                            RemoteBreadcrumbRow {
                                name: "Sounds".into(),
                                path: "/Volumes/Sounds".into(),
                            },
                        ])));
                        ui.set_sm_fs_rows(slint::ModelRc::new(slint::VecModel::from(vec![
                            RemoteFolderRow {
                                name: "Foley".into(),
                                path: "/Volumes/Sounds/Foley".into(),
                            },
                            RemoteFolderRow {
                                name: "Ambience".into(),
                                path: "/Volumes/Sounds/Ambience".into(),
                            },
                            RemoteFolderRow {
                                name: "Vehicles".into(),
                                path: "/Volumes/Sounds/Vehicles".into(),
                            },
                            RemoteFolderRow {
                                name: "木门与脚步".into(),
                                path: "/Volumes/Sounds/木门与脚步".into(),
                            },
                        ])));
                        ui.set_sm_fs_visible(true);
                    }
                }
                "window-repaint" => {
                    ui.invoke_open_data_source();
                    ui.invoke_sm_select_row(-1);
                    // Synthetic unsaved form values for native repaint validation.
                    ui.set_ds_server_url("https://example.invalid/unsaved-draft".into());
                    ui.set_ds_token("synthetic-unsaved-token".into());
                    ui.set_ds_dirty(true);
                }
                "scan" | "scan-paused" => {
                    let paused = smoke_view == "scan-paused";
                    ui.set_scanning(true);
                    ui.set_scan_paused(paused);
                    ui.set_scan_summary(
                        match (paused, ui.get_chinese()) {
                            (true, true) => "扫描已暂停 · 已检查 100000 个音效 · 新增 128 · 更新 0 · 未变化 99872 · 问题 0",
                            (false, true) => "增量扫描中 · 已检查 100000 个音效 · 新增 128 · 更新 0 · 未变化 99872 · 问题 0",
                            (true, false) => "Scan paused · 100000 sounds checked · 128 added · 0 updated · 99872 unchanged · 0 issues",
                            (false, false) => "Incremental scan · 100000 sounds checked · 128 added · 0 updated · 99872 unchanged · 0 issues",
                        }.into(),
                    );
                    ui.set_scan_current_path("N:/Sounds/Scan smoke fixture/Aircraft.wav".into());
                }
                "wide" => ui.set_inspector_visible(false),
                "compact" => {
                    ui.window().set_size(slint::LogicalSize::new(1080.0, 700.0));
                    ui.set_filters_visible(true);
                }
                "selection-compact" => {
                    ui.window().set_size(slint::LogicalSize::new(1080.0, 700.0));
                }
                _ => {}
            }
        }
        let delay = arguments
            .windows(2)
            .find(|pair| pair[0] == "--smoke-delay-ms")
            .and_then(|pair| pair[1].parse::<u64>().ok())
            .unwrap_or(3600)
            .clamp(500, 60_000);
        let weak = ui.as_weak();
        let failure = smoke_failed.clone();
        let duration = arguments
            .windows(2)
            .find(|pair| pair[0] == "--smoke-duration-ms")
            .and_then(|pair| pair[1].parse::<u64>().ok())
            .unwrap_or(delay)
            .clamp(delay, 60_000);
        let close_after_screenshot = duration <= delay;
        if !close_after_screenshot {
            let close_control = controller.clone();
            let close_ui = ui.as_weak();
            closing_timer.start(
                slint::TimerMode::SingleShot,
                Duration::from_millis(duration),
                move || {
                    if let Some(ui) = close_ui.upgrade() {
                        close_control.request_close(&ui);
                    }
                },
            );
        }
        let selection_ui = ui.as_weak();
        selection_timer.start(
            slint::TimerMode::SingleShot,
            Duration::from_millis(900),
            move || {
                if let Some(ui) = selection_ui.upgrade() {
                    ui.set_selected_row(0);
                    ui.invoke_select_requested(0);
                    if matches!(
                        smoke_view.as_str(),
                        "batch" | "selection" | "selection-compact"
                    ) {
                        ui.invoke_check_page_requested(true);
                        ui.set_batch_visible(smoke_view == "batch");
                    }
                    if smoke_view == "context" {
                        use slint::Model;
                        if let Some(row) = ui.get_rows().row_data(1) {
                            ui.invoke_show_file_menu(
                                row,
                                slint::LogicalPosition::new(480.0, 310.0),
                            );
                        }
                    }
                    if smoke_view == "filter-tags" {
                        // Mixed include/exclude chips through the same path as typing + Enter.
                        ui.set_tag_draft("engine -interior -rain \"pass by\"".into());
                        ui.invoke_apply_filters();
                    }
                    if smoke_view == "sorted" {
                        // A header sort (duration, descending) shows its arrow.
                        ui.set_sort_index(2);
                        ui.set_sort_descending(true);
                        ui.invoke_filter_requested();
                    }
                    if smoke_view == "file-status" {
                        // Library > Show files > All records, flagged in the list footer.
                        ui.set_visibility_index(2);
                        ui.invoke_filter_requested();
                    }
                    if smoke_view == "folder-context" {
                        use slint::Model;
                        if let Some(folder) = ui.get_folders().row_data(0) {
                            ui.invoke_show_folder_menu(
                                folder,
                                slint::LogicalPosition::new(140.0, 280.0),
                            );
                        }
                    }
                }
            },
        );
        let screenshot_control = controller.clone();
        timer.start(
            slint::TimerMode::SingleShot,
            Duration::from_millis(delay),
            move || {
                if let Some(ui) = weak.upgrade() {
                    let result = (|| -> Result<()> {
                        let snapshot = ui
                            .window()
                            .take_snapshot()
                            .context("Capture software-rendered window")?;
                        if let Some(parent) = screenshot.parent() {
                            std::fs::create_dir_all(parent)?;
                        }
                        image::save_buffer_with_format(
                            &screenshot,
                            snapshot.as_bytes(),
                            snapshot.width(),
                            snapshot.height(),
                            image::ColorType::Rgba8,
                            image::ImageFormat::Png,
                        )?;
                        println!("Screenshot: {}", screenshot.display());
                        Ok(())
                    })();
                    if let Err(error) = result {
                        failure.store(true, Ordering::Release);
                        eprintln!("UI smoke failed: {error:#}");
                    }
                }
                if (close_after_screenshot || failure.load(Ordering::Acquire))
                    && let Some(ui) = weak.upgrade()
                {
                    screenshot_control.request_close(&ui);
                }
            },
        );
    }
    ui.show().context("Show desktop")?;
    window_repaint::install_native_redraw_guard(ui.window());
    // An event-loop failure must still stop the workers, flush preferences and
    // release the audio engine before returning the original platform error.
    let event_loop_result = slint::run_event_loop().context("Run desktop event loop");
    tracing::info!("Desktop event loop stopped; saving preferences and stopping audio");
    controller.shutdown();
    let hide_result = ui.hide().context("Hide desktop");
    drop(engine);
    tracing::info!("Desktop shutdown complete");
    event_loop_result?;
    hide_result?;
    anyhow::ensure!(
        !smoke_failed.load(Ordering::Acquire),
        "Software-rendered UI smoke screenshot failed"
    );
    if let Some(updates) = updates {
        updates.deploy_after_shutdown()?;
    }
    Ok(())
}

#[cfg(test)]
mod instance_tests {
    use super::DesktopInstanceGuard;
    use std::{path::PathBuf, process::Command, time::Duration};

    struct Directory(PathBuf);
    impl Directory {
        fn new(label: &str) -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "audiolib-instance-{label}-{}-{nonce}",
                std::process::id()
            ));
            std::fs::create_dir_all(&root).unwrap();
            Self(root.canonicalize().unwrap())
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn same_preferences_allow_only_one_instance_and_release_preserves_sidecar() {
        let directory = Directory::new("exclusive");
        let settings = directory.0.join("settings.json");
        let first = DesktopInstanceGuard::acquire(&settings).unwrap();
        let error = DesktopInstanceGuard::acquire(&settings).err().unwrap();
        assert!(error.to_string().contains("already running"));
        drop(first);
        let second = DesktopInstanceGuard::acquire(&settings).unwrap();
        assert!(directory.0.join("desktop-instance.lock").exists());
        drop(second);
    }

    #[test]
    fn lock_io_failure_does_not_claim_another_instance_is_running() {
        let directory = Directory::new("failure");
        let file = directory.0.join("not-a-directory");
        std::fs::write(&file, []).unwrap();
        let error = DesktopInstanceGuard::acquire(&file.join("settings.json"))
            .err()
            .unwrap();
        assert!(error.to_string().contains("Open desktop instance lock"));
        assert!(!error.to_string().contains("already running"));
    }

    #[test]
    fn abnormal_process_exit_releases_instance_lock() {
        let directory = Directory::new("abnormal-exit");
        let settings = directory.0.join("settings.json");
        let ready = directory.0.join("child-ready");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "instance_tests::child_hold_instance_lock",
                "--ignored",
                "--nocapture",
            ])
            .env("AUDIOLIB_INSTANCE_TEST_SETTINGS", &settings)
            .env("AUDIOLIB_INSTANCE_TEST_READY", &ready)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !ready.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let child_ready = ready.exists();
        let blocked = child_ready && DesktopInstanceGuard::acquire(&settings).is_err();
        let _ = child.kill();
        child.wait().unwrap();
        assert!(child_ready, "Child did not acquire its instance lock");
        assert!(blocked, "Child must prevent a second instance");
        assert!(DesktopInstanceGuard::acquire(&settings).is_ok());
    }

    #[test]
    #[ignore = "helper invoked by the abnormal-process-exit regression"]
    fn child_hold_instance_lock() {
        let settings = PathBuf::from(std::env::var_os("AUDIOLIB_INSTANCE_TEST_SETTINGS").unwrap());
        let ready = PathBuf::from(std::env::var_os("AUDIOLIB_INSTANCE_TEST_READY").unwrap());
        let _guard = DesktopInstanceGuard::acquire(&settings).unwrap();
        std::fs::write(ready, []).unwrap();
        std::thread::sleep(Duration::from_secs(10));
    }
}
