// app/audiolib-desktop/src/diagnostics.rs
//! Opt-in, bounded diagnostics. Only explicit safe fields cross the report boundary.
use crate::AppWindow;
use audiolib_audio::{AudioHandle, PlaybackStatus};
use serde_json::{Value, json};
use slint::{ComponentHandle, Model};
use std::{
    collections::VecDeque,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, SyncSender},
    },
    time::Duration,
};

const ISSUE_URL: &str = "https://github.com/Freedragonlancer/AudioLib-Explorer/issues/new";
const MAX_EVENTS: usize = 128;
const MAX_LOG_BYTES: u64 = 256 * 1024;

/// Fixed event vocabulary: never pass error text, paths or remote credentials here.
#[derive(Clone, Copy, Debug)]
pub enum Event {
    RemoteResyncRequested,
    SnapshotRefreshed,
    LibraryViewReset,
    PlaybackHistoryRecorded,
    PlaybackHistoryFailed,
    CacheRevisionMismatch,
    CachePatchFailed,
    CacheMissing,
    CacheRevisionSaveFailed,
    RemoteWriteOutcomeUnknown,
}

#[derive(Clone)]
struct Sink {
    sender: SyncSender<Command>,
    enabled: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
}
static SINK: OnceLock<Sink> = OnceLock::new();

/// Record a control/UI event without blocking. Never call from an audio callback.
pub fn record(event: Event) {
    if let Some(sink) = SINK.get()
        && sink.enabled.load(Ordering::Relaxed)
        && sink.sender.try_send(Command::Event(event)).is_err()
    {
        sink.dropped.fetch_add(1, Ordering::Relaxed);
    }
}

enum Command {
    Enable(bool),
    Sample(Value),
    Event(Event),
    Report { title: String, steps: String },
    OpenGithub(String),
    OpenLogs,
    Shutdown,
}

/// Own the worker and UI sampler; teardown drains local diagnostic writes.
pub struct Runtime {
    _timer: slint::Timer,
    sink: Sink,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Drop for Runtime {
    fn drop(&mut self) {
        self._timer.stop();
        self.sink.enabled.store(false, Ordering::Relaxed);
        let _ = self.sink.sender.send(Command::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Bind session-only Debug actions. All diagnostic disk/shell work stays on a worker.
pub fn install(ui: &AppWindow, logs: PathBuf, audio: AudioHandle) -> Runtime {
    let (sender, receiver) = mpsc::sync_channel(128);
    let sink = Sink {
        sender,
        enabled: Arc::new(AtomicBool::new(false)),
        dropped: Arc::new(AtomicU64::new(0)),
    };
    let _ = SINK.set(sink.clone());
    let weak = ui.as_weak();
    let dropped = sink.dropped.clone();
    let worker = std::thread::spawn(move || {
        let directory = logs.join("debug");
        let mut events = VecDeque::new();
        let mut enabled = false;
        while let Ok(command) = receiver.recv() {
            let result = match command {
                Command::Shutdown => break,
                Command::Enable(value) => {
                    enabled = value;
                    if value {
                        events.clear();
                        dropped.store(0, Ordering::Relaxed);
                        let _ = std::fs::create_dir_all(&directory);
                        let _ = std::fs::write(directory.join("Debug-session.txt"), b"");
                    }
                    append(&directory, &mut events, json!({"event": if value {"DebugEnabled"} else {"DebugDisabled"}}))
                }
                Command::Sample(value) if enabled => append(&directory, &mut events, value),
                Command::Event(event) if enabled => append(&directory, &mut events, json!({"event": format!("{event:?}")})),
                Command::Sample(_) | Command::Event(_) => Ok(()),
                Command::Report { title, steps } => {
                    let report = make_report(&title, &steps, &events, dropped.load(Ordering::Relaxed));
                    let url = issue_url(&title, &report);
                    let save = std::fs::create_dir_all(&directory)
                        .and_then(|()| std::fs::write(directory.join("Debug-report.txt"), &report));
                    let saved = save.is_ok();
                    let _ = weak.upgrade_in_event_loop(move |ui| {
                        ui.set_debug_report(report.into());
                        ui.set_debug_issue_url(url.into());
                        ui.set_debug_busy(false);
                        ui.set_debug_feedback(if saved {
                            if ui.get_chinese() { "报告已保存到日志目录下 debug/Debug-report.txt。请检查预览后再公开提交。" } else { "Saved as debug/Debug-report.txt in the log folder. Review before public submission." }
                        } else if ui.get_chinese() { "报告已生成，但无法写入日志目录。可复制报告后提交。" } else { "Report ready, but saving failed. You can copy it to submit." }.into());
                    });
                    Ok(())
                }
                Command::OpenGithub(url) => {
                    open_github(&url).map(|()| feedback(&weak, "已打开 GitHub。请登录并点击创建 Issue；长报告请粘贴已复制的完整内容。", "GitHub opened. Sign in and create the issue; paste the copied full report if needed."))
                }
                Command::OpenLogs => open_logs(&logs),
            };
            if result.is_err() {
                feedback(
                    &weak,
                    "诊断操作失败。报告预览仍可复制；请检查日志目录权限或默认浏览器。",
                    "Diagnostic action failed. You can still copy the preview; check log folder access or the default browser.",
                );
            }
        }
    });
    let toggle = sink.clone();
    let weak = ui.as_weak();
    ui.on_debug_mode_requested(move |enabled| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_debug_busy() {
            return;
        }
        if toggle.sender.try_send(Command::Enable(enabled)).is_ok() {
            toggle.enabled.store(enabled, Ordering::Relaxed);
            ui.set_debug_enabled(enabled);
            ui.set_debug_report("".into());
            ui.set_debug_issue_url("".into());
        } else {
            ui.set_debug_feedback(
                if ui.get_chinese() {
                    "诊断队列繁忙，Debug 开关未改变，请稍后重试。"
                } else {
                    "Diagnostic queue busy; Debug is unchanged. Try again shortly."
                }
                .into(),
            );
        }
    });
    let actions = sink.clone();
    let weak = ui.as_weak();
    ui.on_debug_action_requested(move |action| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if action == 0 && ui.get_debug_busy() {
            return;
        }
        let command = match action {
            0 => {
                ui.set_debug_busy(true);
                Command::Report {
                    title: ui.get_debug_title().chars().take(120).collect(),
                    steps: ui.get_debug_steps().chars().take(4000).collect(),
                }
            }
            1 => Command::OpenGithub(ui.get_debug_issue_url().to_string()),
            2 => Command::OpenLogs,
            _ => return,
        };
        if actions.sender.try_send(command).is_err() {
            if action == 0 {
                ui.set_debug_busy(false);
            }
            ui.set_debug_feedback(
                if ui.get_chinese() {
                    "诊断队列繁忙，请稍后重试。"
                } else {
                    "Diagnostic queue busy; try again shortly."
                }
                .into(),
            );
        }
    });
    let timer = slint::Timer::default();
    let sampling = sink.clone();
    let weak = ui.as_weak();
    let mut previous = None;
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(250),
        move || {
            if !sampling.enabled.load(Ordering::Relaxed) {
                previous = None;
                return;
            }
            let Some(ui) = weak.upgrade() else {
                return;
            };
            // Compare query privately to detect same-length changes; never serialize it.
            let query = ui.get_query().to_string();
            let sample = snapshot(&ui, &audio.status());
            if previous.as_ref() != Some(&(query.clone(), sample.clone())) {
                if sampling
                    .sender
                    .try_send(Command::Sample(sample.clone()))
                    .is_err()
                {
                    sampling.dropped.fetch_add(1, Ordering::Relaxed);
                } else {
                    previous = Some((query, sample));
                }
            }
        },
    );
    Runtime {
        _timer: timer,
        sink,
        worker: Some(worker),
    }
}

fn snapshot(ui: &AppWindow, audio: &PlaybackStatus) -> Value {
    json!({
        "event": "StateChanged", "query_chars": ui.get_query().chars().count(),
        "result_rows": ui.get_rows().row_count(), "selected_row": ui.get_selected_row(),
        "task_restoring": ui.get_task_restoring(),
        "workspace_loading": ui.get_search_workspace_loading(),
        "result_scroll_y": ui.get_task_result_scroll_y(),
        "folder_scroll_y": ui.get_task_folder_scroll_y(),
        "folder_scoped": !ui.get_selected_folder_path().is_empty(),
        "searching": ui.get_searching(), "scanning": ui.get_scanning(),
        "closing": ui.get_closing(), "library_busy": ui.get_library_busy(), "metadata_dirty": ui.get_metadata_dirty(),
        "server_mode": ui.get_remote_mode(), "syncing": ui.get_ds_syncing(),
        "applying": ui.get_ds_applying(),
        "favorites": ui.get_favorites_only(), "tagged": ui.get_tagged_only(),
        "tag_filter": !ui.get_tag_filter().is_empty(), "color_filter": !ui.get_color_filter().is_empty(),
        "sample_filter": ui.get_sample_filter(), "bit_filter": ui.get_bit_filter(),
        "channel_filter": ui.get_channel_filter(), "visibility": ui.get_visibility_index(),
        "sort": ui.get_sort_index(), "sort_descending": ui.get_sort_descending(),
        "table_widths": [ui.get_table_name_width(), ui.get_table_duration_width(), ui.get_table_format_width(), ui.get_table_channels_width(), ui.get_table_tags_width()],
        "page": ui.get_page_label().split('/').next().unwrap_or("").trim().parse::<u64>().ok(),
        "audio": audio_fields(audio)
    })
}

fn audio_fields(audio: &PlaybackStatus) -> Value {
    json!({"state": audio.state, "generation": audio.generation,
        "transport_revision": audio.transport_revision, "session": audio.play_session_id,
        "backend": audio.active_backend, "source_rate": audio.sample_rate,
        "output_rate": audio.device_sample_rate, "buffer_frames": audio.actual_buffer_frames,
        "underruns": audio.underruns, "buffering": audio.buffering,
        "has_error": audio.error.is_some()})
}

fn append(directory: &Path, events: &mut VecDeque<Value>, mut value: Value) -> std::io::Result<()> {
    value["utc"] = chrono::Utc::now().to_rfc3339().into();
    if events.len() == MAX_EVENTS {
        events.pop_front();
    }
    events.push_back(value.clone());
    std::fs::create_dir_all(directory)?;
    let path = directory.join("Debug-session.txt");
    if std::fs::metadata(&path).is_ok_and(|metadata| metadata.len() >= MAX_LOG_BYTES) {
        // Two fixed bounded files; never enumerate or include ordinary raw logs.
        std::fs::copy(&path, directory.join("Debug-session-previous.txt"))?;
        std::fs::write(&path, b"")?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{value}")
}

fn make_report(title: &str, steps: &str, events: &VecDeque<Value>, dropped: u64) -> String {
    let event_log = events
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "## Problem\n{title}\n\n## Reproduction / expected / actual\n{steps}\n\n## Diagnostics\nAudioLib Explorer {} · {} / {} · ASIO build: {}\nRetained diagnostic events: {} (limit {}); dropped queue events: {dropped}\nSearch text, paths, names, Metadata, server URLs, device identity and credentials are excluded. User-written description above is included as entered.\nCapture is sampled every 250ms; short transitions may be absent.\n\n```jsonl\n{event_log}\n```\n",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        cfg!(feature = "asio"),
        events.len(),
        MAX_EVENTS
    )
}

fn encode(value: &str) -> String {
    use std::fmt::Write;
    let mut result = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            result.push(byte as char);
        } else {
            let _ = write!(result, "%{byte:02X}");
        }
    }
    result
}

fn issue_url(title: &str, report: &str) -> String {
    let title = if title.trim().is_empty() {
        "AudioLib Explorer bug report"
    } else {
        title
    };
    let prefix = format!("{ISSUE_URL}?title={}&body=", encode(title));
    let full = format!("{prefix}{}", encode(report));
    if full.len() <= 7600 {
        full
    } else {
        format!(
            "{prefix}{}",
            encode(&format!(
                "AudioLib Explorer {}\n\nPlease paste the full diagnostic report copied by the application here before submitting.\n",
                env!("CARGO_PKG_VERSION")
            ))
        )
    }
}

fn feedback(weak: &slint::Weak<AppWindow>, chinese: &'static str, english: &'static str) {
    let _ = weak.upgrade_in_event_loop(move |ui| {
        ui.set_debug_feedback(if ui.get_chinese() { chinese } else { english }.into());
    });
}

fn open_logs(logs: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        std::process::Command::new("explorer.exe")
            .arg(logs)
            .creation_flags(0x0800_0000)
            .spawn()?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = logs;
        Err(std::io::Error::other(
            "Windows log folder integration unavailable",
        ))
    }
}

fn open_github(url: &str) -> std::io::Result<()> {
    if !url.starts_with(&format!("{ISSUE_URL}?title="))
        || url.len() > 7600
        || url.contains(['\r', '\n', '\0'])
    {
        return Err(std::io::Error::other("Invalid GitHub issue link"));
    }
    #[cfg(windows)]
    {
        use std::ffi::c_void;
        #[link(name = "shell32")]
        unsafe extern "system" {
            fn ShellExecuteW(
                window: *mut c_void,
                operation: *const u16,
                file: *const u16,
                parameters: *const u16,
                directory: *const u16,
                show: i32,
            ) -> isize;
        }
        let operation = "open".encode_utf16().chain(Some(0)).collect::<Vec<_>>();
        let target = url.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
        // SAFETY: Both UTF-16 strings are NUL-terminated and retained for this call.
        // Only the fixed HTTPS GitHub issue endpoint is accepted; no shell command
        // or parameter is supplied. The returned pseudo-handle is only an error code.
        let result = unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                operation.as_ptr(),
                target.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
            )
        };
        if result <= 32 {
            return Err(std::io::Error::other(
                "Default browser could not open GitHub",
            ));
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        Err(std::io::Error::other(
            "Windows browser integration unavailable",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reports_exclude_raw_audio_identity_and_errors() {
        let audio = PlaybackStatus {
            file_path: Some(PathBuf::from(r"C:\private\secret.wav")),
            error: Some("https://user:password@internal/token=secret".into()),
            active_device_id: Some("private-driver".into()),
            ..Default::default()
        };
        let report = make_report(
            "Playback refresh",
            "Play then results disappear",
            &VecDeque::from([audio_fields(&audio)]),
            3,
        );
        for forbidden in [
            "secret.wav",
            "password",
            "internal",
            "private-driver",
            "file_path",
            "active_device_id",
        ] {
            assert!(!report.contains(forbidden));
        }
        assert!(report.contains("has_error"));
        assert!(report.contains("dropped queue events: 3"));
    }
    #[test]
    fn issue_links_encode_unicode_and_bound_large_reports() {
        let link = issue_url("声音 & #刷新", "query & body=other\n中文");
        assert!(link.starts_with(ISSUE_URL));
        assert!(link.contains("%26"));
        assert!(!link.contains("中文"));
        let long = issue_url("Bug", &"中".repeat(10000));
        assert!(long.len() < 7600);
        assert!(long.contains("Please%20paste"));
        assert!(open_github("file:///private").is_err());
    }
    #[test]
    fn capture_is_bounded_and_persists_only_safe_events() {
        let path = std::env::temp_dir().join(format!(
            "audiolib-debug-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let mut events = VecDeque::new();
        for index in 0..(MAX_EVENTS + 3) {
            append(&path, &mut events, json!({"sequence":index})).unwrap();
        }
        assert_eq!(events.len(), MAX_EVENTS);
        assert_eq!(events.front().unwrap()["sequence"], 3);
        let report = make_report("Bug", "Steps", &events, 0);
        assert!(!report.contains(&path.to_string_lossy().to_string()));
        std::fs::write(
            path.join("Debug-session.txt"),
            vec![b'x'; MAX_LOG_BYTES as usize],
        )
        .unwrap();
        append(&path, &mut events, json!({"event":"SnapshotRefreshed"})).unwrap();
        assert_eq!(
            std::fs::metadata(path.join("Debug-session-previous.txt"))
                .unwrap()
                .len(),
            MAX_LOG_BYTES
        );
        assert!(
            std::fs::metadata(path.join("Debug-session.txt"))
                .unwrap()
                .len()
                < 1024
        );
        std::fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn ui_capture_excludes_search_text_metadata_and_paths() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().unwrap();
        ui.set_query("private customer query".into());
        ui.set_selected_path(r"C:\customer\asset.wav".into());
        ui.set_edit_notes("confidential notes".into());
        ui.set_tag_filter("customer label".into());
        ui.set_debug_visible(true);
        let report = make_report(
            "Bug",
            "Steps",
            &VecDeque::from([snapshot(&ui, &PlaybackStatus::default())]),
            0,
        );
        for forbidden in [
            "private customer query",
            "asset.wav",
            "confidential notes",
            "customer label",
        ] {
            assert!(!report.contains(forbidden));
        }
        assert!(report.contains("query_chars"));
        assert!(!ui.get_debug_enabled());
        ui.invoke_debug_mode_requested(true);
        assert_eq!(ui.get_query(), "private customer query");
    }

    #[test]
    fn ordinary_sync_changes_the_sample_without_source_application() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().unwrap();
        ui.set_remote_mode(true);
        let audio = PlaybackStatus::default();
        let idle = snapshot(&ui, &audio);
        ui.set_ds_syncing(true);
        let syncing = snapshot(&ui, &audio);
        // The sampler compares snapshots, so this ordinary sync must enqueue a
        // change even when query, playback and source-application state are stable.
        assert_ne!(syncing, idle);
        assert_eq!(syncing["syncing"], true);
        assert_eq!(syncing["applying"], false);
        ui.set_ds_syncing(false);
        ui.set_ds_applying(true);
        let applying = snapshot(&ui, &audio);
        assert_ne!(applying, syncing);
        assert_eq!(applying["syncing"], false);
        assert_eq!(applying["applying"], true);
    }
}
