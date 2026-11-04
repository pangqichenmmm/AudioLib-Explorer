// app/audiolib-desktop/src/updates.rs
//! Background release discovery and a consented, drained desktop update handoff.
use crate::{AppWindow, controller::Controller};
use anyhow::{Context, Result};
use audiolib_update::{PreparedUpdate, Product, ReleaseUpdate, UpdateClient};
use slint::ComponentHandle;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

#[derive(Default)]
struct State {
    busy: bool,
    release: Option<ReleaseUpdate>,
    prepared: Option<PreparedUpdate>,
}

/// Retains downloaded files until the existing Metadata/scan close barrier succeeds.
pub struct DesktopUpdates {
    state: Arc<Mutex<State>>,
    stopped: Arc<AtomicBool>,
}

impl DesktopUpdates {
    /// Bind manual actions and check once per ordinary startup without blocking Slint.
    pub fn install(
        ui: &AppWindow,
        controller: Arc<Controller>,
        arguments: &[String],
    ) -> Option<Self> {
        ui.set_app_version(env!("CARGO_PKG_VERSION").into());
        ui.set_update_supported(!cfg!(target_os = "macos"));
        if cfg!(target_os = "macos") {
            ui.set_update_enabled(false);
            ui.set_update_message(
                text(
                    ui,
                    "Download a new Mac client from GitHub Releases, then close the app and replace it.",
                    "请从 GitHub Releases 下载新版 Mac 客户端，关闭应用后替换。",
                )
                .into(),
            );
            return None;
        }
        let executable = match std::env::current_exe() {
            Ok(path) => path,
            Err(error) => {
                tracing::warn!(%error, "Locate application for release check");
                ui.set_update_enabled(false);
                ui.set_update_message(format!("Update configuration / 更新配置: {error}").into());
                return None;
            }
        };
        let directory = executable.parent()?.to_path_buf();
        let client = match UpdateClient::for_install(
            Product::Desktop,
            env!("CARGO_PKG_VERSION"),
            &directory,
        ) {
            Ok(client) => Arc::new(client),
            Err(error) => {
                tracing::warn!(%error, "Configure release check");
                ui.set_update_enabled(false);
                ui.set_update_message(format!("Update configuration / 更新配置: {error}").into());
                return None;
            }
        };
        if !apply_update_policy(
            ui,
            &client,
            std::env::var("AUDIOLIB_UPDATE_DISABLE").as_deref() == Ok("1"),
        ) {
            // Leave startup and manual check/install callbacks unbound for local
            // development builds, including direct EXE launches without a launcher.
            return None;
        }
        if arguments.iter().any(|arg| {
            arg == "--smoke-ui" || arg == "--headless-smoke" || arg.starts_with("--gui-search-")
        }) {
            return None;
        }
        let state = Arc::new(Mutex::new(State::default()));
        let stopped = Arc::new(AtomicBool::new(false));
        let weak = ui.as_weak();
        let check_client = client.clone();
        let check_state = state.clone();
        let check_stopped = stopped.clone();
        ui.on_update_check_requested(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_update_visible(true);
                check(
                    &ui,
                    check_client.clone(),
                    check_state.clone(),
                    check_stopped.clone(),
                );
            }
        });
        let weak = ui.as_weak();
        let install_state = state.clone();
        let install_stopped = stopped.clone();
        let startup_client = client.clone();
        ui.on_update_install_requested(move || {
            let Some(ui) = weak.upgrade() else { return };
            if ui.get_closing() || ui.get_library_busy() {
                return;
            }
            let release = {
                let mut state = install_state.lock().unwrap();
                if state.busy {
                    return;
                }
                if state.prepared.is_some() {
                    ui.set_update_visible(false);
                    controller.request_close(&ui);
                    return;
                }
                let Some(release) = state.release.clone() else {
                    return;
                };
                state.busy = true;
                release
            };
            ui.set_update_busy(true);
            ui.set_update_message(
                text(
                    &ui,
                    "Downloading and verifying the update…",
                    "正在下载并校验更新…",
                )
                .into(),
            );
            let client = client.clone();
            let state = install_state.clone();
            let stopped = install_stopped.clone();
            let directory = directory.clone();
            let weak = ui.as_weak();
            let controller = controller.clone();
            std::thread::spawn(move || {
                let result = client.prepare(&release, &directory);
                if stopped.load(Ordering::Acquire) {
                    return;
                }
                let message = match result {
                    Ok(prepared) => {
                        state.lock().unwrap().prepared = Some(prepared);
                        None
                    }
                    Err(error) => Some(error.to_string()),
                };
                state.lock().unwrap().busy = false;
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    if stopped.load(Ordering::Acquire) {
                        return;
                    }
                    ui.set_update_busy(false);
                    if let Some(error) = message {
                        ui.set_update_message(
                            format!(
                                "{}: {error}",
                                text(
                                    &ui,
                                    "Update failed; retry when ready",
                                    "更新失败，可稍后重试"
                                )
                            )
                            .into(),
                        );
                    } else {
                        ui.set_update_message(
                            text(
                                &ui,
                                "Verified. Saving changes before restarting…",
                                "校验完成，正在保存更改并重启…",
                            )
                            .into(),
                        );
                        ui.set_update_visible(false);
                        controller.request_close(&ui);
                    }
                });
            });
        });
        check(ui, startup_client, state.clone(), stopped.clone());
        Some(Self { state, stopped })
    }

    /// Called only after the normal event-loop exit, shutdown and audio teardown.
    pub fn deploy_after_shutdown(self) -> Result<()> {
        self.stopped.store(true, Ordering::Release);
        if let Some(prepared) = self.state.lock().unwrap().prepared.take() {
            prepared
                .deploy_and_restart(&std::env::args_os().skip(1).collect::<Vec<_>>())
                .context("Deploy verified application update")?;
        }
        Ok(())
    }
}

impl Drop for DesktopUpdates {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
    }
}

fn text<'a>(ui: &AppWindow, english: &'a str, chinese: &'a str) -> &'a str {
    if ui.get_chinese() { chinese } else { english }
}

/// Show the full installed identity before applying local update restrictions.
fn apply_update_policy(ui: &AppWindow, client: &UpdateClient, environment_disabled: bool) -> bool {
    ui.set_app_version(client.current_version().into());
    let development = client.is_development();
    ui.set_update_enabled(!development && !environment_disabled);
    if development {
        ui.set_update_message(
            text(
                ui,
                "Local development build. Install newer test builds manually; published releases will not replace this build.",
                "本地开发测试版，请手动安装新的测试构建；已发布版本不会覆盖此构建。",
            )
            .into(),
        );
    } else if environment_disabled {
        ui.set_update_message(
            text(
                ui,
                "Update checks are disabled for this installation.",
                "此安装已关闭更新检查。",
            )
            .into(),
        );
    }
    !development && !environment_disabled
}

fn check(
    ui: &AppWindow,
    client: Arc<UpdateClient>,
    state: Arc<Mutex<State>>,
    stopped: Arc<AtomicBool>,
) {
    {
        let mut state = state.lock().unwrap();
        if state.busy || state.prepared.is_some() {
            return;
        }
        state.busy = true;
        state.release = None;
    }
    ui.set_update_busy(true);
    ui.set_update_available(false);
    ui.set_update_message(text(ui, "Checking GitHub Releases…", "正在检查 GitHub Release…").into());
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        let result = client.check();
        if stopped.load(Ordering::Acquire) {
            return;
        }
        let mut shared = state.lock().unwrap();
        shared.busy = false;
        let message = match result {
            Ok(release) => {
                shared.release = release.clone();
                Ok(release)
            }
            Err(error) => Err(error.to_string()),
        };
        drop(shared);
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if stopped.load(Ordering::Acquire) {
                return;
            }
            ui.set_update_busy(false);
            match message {
                Ok(Some(release)) => {
                    ui.set_update_version(release.tag.into());
                    ui.set_update_available(true);
                    ui.set_update_message(
                        text(&ui, "A new version is available.", "发现新版本。").into(),
                    );
                    ui.set_update_visible(true);
                }
                Ok(None) => ui.set_update_message(
                    text(&ui, "You are up to date.", "当前已是最新版本。").into(),
                ),
                Err(error) => {
                    tracing::warn!(%error, "Startup release check failed");
                    ui.set_update_message(
                        format!(
                            "{}: {error}",
                            text(&ui, "Unable to check updates", "暂时无法检查更新")
                        )
                        .into(),
                    );
                }
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use audiolib_update::Channel;

    #[test]
    fn development_identity_blocks_desktop_updates_without_a_launcher_environment() {
        i_slint_backend_testing::init_no_event_loop();
        for chinese in [false, true] {
            for version in ["0.8.6-0", "0.8.6-0+local.0123456789ab"] {
                let ui = AppWindow::new().unwrap();
                ui.set_chinese(chinese);
                let client = UpdateClient::new(Product::Desktop, Channel::Beta, version).unwrap();
                assert!(!apply_update_policy(&ui, &client, false));
                assert_eq!(ui.get_app_version(), version);
                assert!(!ui.get_update_enabled());
                assert!(!ui.get_update_message().is_empty());
            }
        }
    }

    #[test]
    fn published_desktop_versions_keep_the_normal_update_policy_and_identity() {
        i_slint_backend_testing::init_no_event_loop();
        for version in ["0.8.6", "0.8.7-beta.1", "0.8.7-rc.1"] {
            for environment_disabled in [false, true] {
                let ui = AppWindow::new().unwrap();
                let client = UpdateClient::new(Product::Desktop, Channel::Beta, version).unwrap();
                assert_eq!(
                    apply_update_policy(&ui, &client, environment_disabled),
                    !environment_disabled
                );
                assert_eq!(ui.get_app_version(), version);
                assert_eq!(ui.get_update_enabled(), !environment_disabled);
            }
        }
    }
}
