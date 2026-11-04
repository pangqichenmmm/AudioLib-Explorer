// app/audiolib-desktop/src/server_removal.rs
//! Server-mode record removal outside the 服务器音效库 page: the menu / context-menu gate
//! (the active server's `/capabilities`) and the optimistic overlay that hides removed
//! records at once.
//!
//! After a 200 from `POST /audio/remove` or `DELETE /folders/{id}` the open snapshot still
//! holds the records until the revision-bump resync swaps it. [`HiddenRecords`] hides them
//! from the list, the folder tree and the record count in memory only: the snapshot file
//! is never modified, so a resync that never comes leaves nothing behind after a restart.
//! The overlay is dropped as soon as a snapshot past the removal (or of another library)
//! opens; from then on the snapshot wins, whatever it says.
use super::source_manager::capabilities_note;
use super::{Controller, cn_or, post};
use crate::AppWindow;
use audiolib_core::{AudioFile, DataProvider, Folder, SearchPage};
use audiolib_remote::{CacheState, Capabilities, MemoryCache, RemoteClient};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// Folder levels walked when collecting a removed folder's subtree from the snapshot.
const MAX_SUBTREE_DEPTH: usize = 64;

/// Records the server removed that the open snapshot still lists.
#[derive(Debug, Default)]
pub(super) struct HiddenRecords {
    library_id: String,
    /// Snapshot revision the latest removal was made against: a snapshot past it already
    /// reflects every removal recorded here.
    since: u64,
    ids: HashSet<i64>,
    folders: HashSet<i64>,
    /// Records the server reported removed (for the library count).
    removed: u64,
}

impl HiddenRecords {
    pub(super) fn is_empty(&self) -> bool {
        self.ids.is_empty() && self.folders.is_empty()
    }

    fn add(&mut self, state: &CacheState, ids: &[i64], folders: &HashSet<i64>, removed: u64) {
        if self.library_id != state.library_id {
            *self = Self {
                library_id: state.library_id.clone(),
                ..Self::default()
            };
        }
        self.since = self.since.max(state.revision);
        self.ids.extend(ids.iter().copied());
        self.folders.extend(folders.iter().copied());
        self.removed = self.removed.saturating_add(removed);
    }

    fn hides(&self, file: &AudioFile) -> bool {
        self.ids.contains(&file.id)
            || file
                .folder_id
                .is_some_and(|folder| self.folders.contains(&folder))
    }

    /// Drop hidden rows from a result page. The total only loses the rows of this page
    /// (other pages are not read); the resync corrects it.
    fn filter_page(&self, page: &mut SearchPage) {
        let before = page.results.len();
        page.results.retain(|file| !self.hides(file));
        let dropped = before - page.results.len();
        page.returned_count = page.returned_count.saturating_sub(dropped);
        page.total_count = page.total_count.saturating_sub(dropped as u64);
    }

    fn filter_folders(&self, folders: &mut Vec<Folder>) {
        folders.retain(|folder| !self.folders.contains(&folder.id));
    }

    /// A snapshot past the removals, of another library, or none at all settles them.
    fn settled_by(&self, state: Option<&CacheState>) -> bool {
        !self.is_empty()
            && state.is_none_or(|state| {
                state.library_id != self.library_id || state.revision > self.since
            })
    }
}

/// Removal gate of the active server for the menus (from its `/capabilities`).
#[derive(Clone, Debug, Default)]
pub(super) struct ServerManage {
    /// Server the answer came from: it is ignored once another server is active.
    url: String,
    can_manage: bool,
    /// Why not, in the server's terms; empty while unknown.
    note: String,
    /// The `/capabilities` answer itself (list Waveforms read it); `None` while unknown
    /// or unreadable.
    capabilities: Option<Capabilities>,
}

impl ServerManage {
    #[cfg(test)]
    pub(super) fn allowed_for_test(url: &str) -> Self {
        Self {
            url: url.into(),
            can_manage: true,
            note: String::new(),
            capabilities: None,
        }
    }

    /// `/capabilities` of `server_url`, once read after the latest sync.
    pub(super) fn capabilities_for(&self, server_url: &str) -> Option<&Capabilities> {
        (self.url == server_url)
            .then_some(self.capabilities.as_ref())
            .flatten()
    }

    /// The answer for `server_url`, or "unknown" if it came from another server.
    fn for_server(&self, server_url: &str) -> Self {
        if self.url == server_url {
            self.clone()
        } else {
            Self::default()
        }
    }
}

/// The removal's folder and its descendants as listed by the snapshot.
fn folder_subtree(provider: &dyn DataProvider, id: i64) -> HashSet<i64> {
    let mut subtree = HashSet::from([id]);
    let mut level = vec![id];
    for _ in 0..MAX_SUBTREE_DEPTH {
        let mut next = Vec::new();
        for parent in level {
            for child in provider.folders(Some(parent)).unwrap_or_default() {
                if subtree.insert(child.id) {
                    next.push(child.id);
                }
            }
        }
        if next.is_empty() {
            break;
        }
        level = next;
    }
    subtree
}

/// Record id of an audition-stream memory key
/// (`<library>/<id>/<size>/<route>/<source>/<snapshot filename>`).
fn stream_key_id(key: &str) -> Option<i64> {
    key.split('/').nth(1)?.parse().ok()
}

/// Record id of a waveform memory key (`<library>/<revision>/<id>-<start>-…`).
fn waveform_key_id(key: &str) -> Option<i64> {
    key.split('/').nth(2)?.split('-').next()?.parse().ok()
}

impl Controller {
    /// The folders a server folder removal takes, from the open snapshot (worker thread).
    pub(crate) fn removed_folder_subtree(&self, id: i64) -> HashSet<i64> {
        folder_subtree(self.provider().as_ref(), id)
    }

    /// Hide what the server just removed (worker thread, before the view reloads).
    pub(crate) fn hide_removed(&self, ids: &[i64], folders: &HashSet<i64>, removed: u64) {
        let mut remote = self.remote.lock().unwrap();
        let Some(state) = remote.state.clone() else {
            return;
        };
        remote.hidden.add(&state, ids, folders, removed);
    }

    /// Drop the overlay once the open snapshot reflects the removals; true if dropped
    /// (the caller reloads the view).
    pub(crate) fn settle_hidden(&self) -> bool {
        let mut remote = self.remote.lock().unwrap();
        if remote.hidden.settled_by(remote.state.as_ref()) {
            remote.hidden = HiddenRecords::default();
            true
        } else {
            false
        }
    }

    pub(crate) fn hide_removed_in_page(&self, page: &mut SearchPage) {
        if self.remote_read_only() {
            let remote = self.remote.lock().unwrap();
            if !remote.hidden.is_empty() {
                remote.hidden.filter_page(page);
            }
        }
    }

    pub(crate) fn hide_removed_folders(&self, folders: &mut Vec<Folder>) {
        if self.remote_read_only() {
            self.remote.lock().unwrap().hidden.filter_folders(folders);
        }
    }

    /// Records removed on the server but still counted by the open snapshot.
    pub(crate) fn hidden_record_count(&self) -> u64 {
        if !self.remote_read_only() {
            return 0;
        }
        self.remote.lock().unwrap().hidden.removed
    }

    /// Drop id-keyed memory after a server removal: the ids no longer exist, and a rescan
    /// re-adds the files under new ones. With the removed ids known only those entries go;
    /// a folder removal (ids unknown here) drops everything. Called on the UI thread.
    pub(crate) fn invalidate_server_ids(&self, ids: Option<&HashSet<i64>>) {
        let mut remote = self.remote.lock().unwrap();
        let Some(ids) = ids else {
            MemoryCache::global().clear();
            remote.audio_ids.clear();
            remote.audio_names.clear();
            remote.audio_files.clear();
            remote.preview_refused.clear();
            remote.waveforms.clear();
            return;
        };
        MemoryCache::global()
            .remove_where(|key| stream_key_id(key).is_some_and(|id| ids.contains(&id)));
        let gone: Vec<_> = remote
            .audio_ids
            .iter()
            .filter(|(_, id)| ids.contains(id))
            .map(|(path, _)| path.clone())
            .collect();
        for path in gone {
            remote.audio_ids.remove(&path);
            remote.audio_names.remove(&path);
            remote.audio_files.remove(&path);
        }
        remote.preview_refused.retain(|id, _| !ids.contains(id));
        remote
            .waveforms
            .retain(|key, _| waveform_key_id(key).is_none_or(|id| !ids.contains(&id)));
    }

    /// Menu removals in server mode: allowed when the active server lets this PC manage;
    /// otherwise the status line says why (in the server's terms) and true is returned.
    pub(crate) fn refuse_server_removal(&self) -> bool {
        let manage = self.current_server_manage();
        if manage.can_manage {
            return false;
        }
        let chinese = self.chinese.load(Ordering::Acquire);
        let reason = if manage.note.is_empty() {
            cn_or(
                chinese,
                "尚未取得服务器的管理权限信息（/capabilities）",
                "no management information from the server (/capabilities) yet",
            )
        } else {
            manage.note
        };
        self.status(if chinese {
            format!("服务器模式：本机不能移除服务器音效库的记录（{reason}）。")
        } else {
            format!("Server mode: this PC may not remove server library records ({reason}).")
        });
        true
    }

    /// Ask the active server's `/capabilities` for the menu gate (after each sync).
    pub(crate) fn refresh_server_manage(self: &Arc<Self>) {
        let settings = self.config.lock().unwrap().data_source.clone();
        let secrets = self.secrets();
        let chinese = self.chinese.load(Ordering::Acquire);
        let c = self.clone();
        std::thread::spawn(move || {
            let manage = match RemoteClient::new(&settings.server_url, &secrets.token)
                .and_then(|client| client.capabilities())
            {
                Ok(capabilities) => ServerManage {
                    url: settings.server_url.clone(),
                    can_manage: capabilities.may_manage(),
                    note: capabilities_note(&capabilities, chinese),
                    capabilities: Some(capabilities),
                },
                Err(error) => ServerManage {
                    url: settings.server_url.clone(),
                    can_manage: false,
                    capabilities: None,
                    note: if chinese {
                        format!("未能读取服务器的管理权限：{}", error.localized(true))
                    } else {
                        format!(
                            "could not read the server's management setting: {}",
                            error.localized(false)
                        )
                    },
                },
            };
            let c2 = c.clone();
            post(c.ui.clone(), move |ui| {
                c2.apply_server_manage(&ui, &settings.server_url, manage);
            });
        });
    }

    /// Apply the active server's asynchronous capability reply. A selection made
    /// before the optional artwork marker arrived needs one fresh cover job.
    fn apply_server_manage(&self, ui: &AppWindow, expected_url: &str, manage: ServerManage) {
        let current = self.config.lock().unwrap().data_source.server_url.clone();
        if current != expected_url {
            return;
        }
        let artwork_became_available = {
            let mut remote = self.remote.lock().unwrap();
            let usable = |manage: &ServerManage| {
                manage
                    .capabilities_for(&current)
                    .is_some_and(|capabilities| {
                        capabilities.album_artwork_usable()
                            || capabilities.album_entity_artwork_usable()
                    })
            };
            let became_available = !usable(&remote.manage) && usable(&manage);
            remote.manage = manage;
            became_available && !remote.library_changed
        };
        self.publish_server_manage(ui);
        if artwork_became_available && self.remote_read_only() && self.available() {
            let selected = self.selected.lock().unwrap().clone();
            if let Some(file) = selected {
                self.queue_artwork(&file);
            }
        }
    }

    /// The menu gate of the server that is active now.
    fn current_server_manage(&self) -> ServerManage {
        if let Err(note) = self.require_remote_write_identity() {
            return ServerManage {
                note,
                ..Default::default()
            };
        }
        let url = self.config.lock().unwrap().data_source.server_url.clone();
        self.remote.lock().unwrap().manage.for_server(&url)
    }

    /// Show the gate on the menus (server mode only; local removals are not gated).
    pub(crate) fn publish_server_manage(&self, ui: &AppWindow) {
        let manage = if self.remote_read_only() {
            self.current_server_manage()
        } else {
            ServerManage::default()
        };
        ui.set_server_can_manage(manage.can_manage);
        ui.set_server_manage_note(manage.note.into());
    }

    /// Confirmation of a menu removal in server mode: the 服务器音效库 page's text.
    pub(crate) fn server_removal_confirmation(&self, ids: Option<usize>, folder: i64) -> String {
        let chinese = self.chinese.load(Ordering::Acquire);
        match ids {
            Some(count) => removal_records_text(count, chinese),
            None => {
                let path = self
                    .tree
                    .lock()
                    .unwrap()
                    .known
                    .get(&folder)
                    .map(|folder| folder.path.clone())
                    .unwrap_or_default();
                removal_folder_text(&path, chinese)
            }
        }
    }
}

/// Same wording as the 服务器音效库 page's inline confirmation (app-window.slint).
pub(super) fn removal_records_text(count: usize, chinese: bool) -> String {
    if chinese {
        format!(
            "从服务器音效库移除所选的 {count} 个记录？只删除库记录及其标注，服务器上的 WAV 文件不会改动；重新扫描会以新的 id 把它们加回（标注不会恢复）。"
        )
    } else {
        format!(
            "Remove the {count} selected records from the server library? Only the library records and their annotations are removed; WAV files on the server are untouched, and a rescan adds them back with new ids (annotations are not restored)."
        )
    }
}

/// Same wording as the 服务器音效库 page's inline confirmation (app-window.slint).
pub(super) fn removal_folder_text(path: &str, chinese: bool) -> String {
    if chinese {
        format!(
            "删除目录「{path}」及其子目录在服务器音效库中的全部记录？只删除库记录及其标注，服务器上的目录和 WAV 文件不会改动；重新扫描会以新的 id 把它们加回（标注不会恢复）。"
        )
    } else {
        format!(
            "Remove all records of the folder \"{path}\" and its subfolders from the server library? Only the library records and their annotations are removed; folders and WAV files on the server are untouched, and a rescan adds them back with new ids (annotations are not restored)."
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(library: &str, revision: u64) -> CacheState {
        CacheState {
            library_id: library.into(),
            revision,
            ..CacheState::default()
        }
    }

    fn file(id: i64, folder: Option<i64>) -> AudioFile {
        AudioFile {
            id,
            folder_id: folder,
            ..AudioFile::default()
        }
    }

    #[test]
    fn hidden_records_filter_pages_and_folders_until_a_later_snapshot() {
        let mut hidden = HiddenRecords::default();
        assert!(
            !hidden.settled_by(None),
            "an empty overlay never needs settling"
        );
        hidden.add(&state("lib", 12), &[2, 3], &HashSet::from([20, 21]), 9);
        let mut page = SearchPage {
            results: vec![
                file(1, Some(10)),
                file(2, Some(10)),
                file(3, None),
                file(4, Some(21)),
                file(5, Some(22)),
            ],
            total_count: 120,
            returned_count: 5,
            ..SearchPage::default()
        };
        hidden.filter_page(&mut page);
        let ids: Vec<i64> = page.results.iter().map(|file| file.id).collect();
        assert_eq!(ids, [1, 5]);
        assert_eq!((page.returned_count, page.total_count), (2, 117));
        let folder = |id| Folder {
            id,
            path: format!("/f{id}"),
            name: format!("f{id}"),
            parent_id: None,
            file_count: 1,
        };
        let mut folders = vec![folder(10), folder(20), folder(21), folder(22)];
        hidden.filter_folders(&mut folders);
        let ids: Vec<i64> = folders.iter().map(|folder| folder.id).collect();
        assert_eq!(ids, [10, 22]);
        // The same revision (a sync that started before the removal) keeps hiding.
        assert!(!hidden.settled_by(Some(&state("lib", 12))));
        // A second removal before the resync: only a snapshot past both settles.
        hidden.add(&state("lib", 13), &[7], &HashSet::new(), 1);
        assert!(!hidden.settled_by(Some(&state("lib", 13))));
        assert_eq!(hidden.removed, 10);
        assert!(hidden.settled_by(Some(&state("lib", 14))));
        assert!(hidden.settled_by(Some(&state("other", 1))));
        assert!(hidden.settled_by(None));
        // Another library starts a new overlay.
        hidden.add(&state("other", 1), &[9], &HashSet::new(), 1);
        assert!(!hidden.ids.contains(&2) && hidden.ids.contains(&9));
        assert_eq!((hidden.since, hidden.removed), (1, 1));
    }

    #[test]
    fn the_menu_gate_belongs_to_the_server_it_came_from() {
        let manage = ServerManage {
            url: "http://server.example.lan:8799".into(),
            can_manage: true,
            note: String::new(),
            capabilities: None,
        };
        assert!(
            manage
                .for_server("http://server.example.lan:8799")
                .can_manage
        );
        let other = manage.for_server("http://server.example.lan:8790");
        assert!(!other.can_manage && other.note.is_empty());
    }

    #[test]
    fn artwork_capability_arrival_requeues_the_current_selection_only_once() {
        for marker in ["album_artwork", "album_entity_artwork"] {
            let directory = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "controller::remote_mode::server_removal::tests::child_artwork_capability_arrival",
                "--ignored",
                "--test-threads=1",
            ])
            .env("AUDIOLIB_ARTWORK_GATE_TEST_ROOT", directory.path())
            .env("AUDIOLIB_ARTWORK_GATE_MARKER", marker)
            .env("AUDIOLIB_DATA_DIR", directory.path())
            .output()
            .unwrap();
            assert!(
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
                "Artwork capability regression ({marker}) failed: {}\n{}\n{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
    }

    #[test]
    #[ignore = "isolated artwork capability regression"]
    fn child_artwork_capability_arrival() {
        use audiolib_audio::{AudioEngine, PlaybackConfig};
        use audiolib_core::config::AppConfig;
        use audiolib_db::SqliteProvider;

        i_slint_backend_testing::init_no_event_loop();
        let directory =
            std::path::PathBuf::from(std::env::var_os("AUDIOLIB_ARTWORK_GATE_TEST_ROOT").unwrap());
        // Explicit preferences prevent discovery of any working/legacy library.
        std::fs::write(directory.join("settings.json"), b"{}").unwrap();
        let config = AppConfig::load_in(directory).unwrap();
        let provider = Arc::new(SqliteProvider::open(&config.paths.database).unwrap());
        let playback = PlaybackConfig::default();
        let engine = AudioEngine::new(playback.clone()).unwrap();
        let ui = AppWindow::new().unwrap();
        let controller = Controller::new(&ui, provider, engine.handle(), config, playback);
        let url = "http://127.0.0.1:9";
        controller.config.lock().unwrap().data_source.server_url = url.into();
        controller.remote_mode.store(true, Ordering::Release);
        controller.remote.lock().unwrap().state = Some(state("artwork-library", 1));
        let mut selected = file(42, None);
        selected.album_id = Some(7);
        selected.album_entity = Some(audiolib_core::albums::Album {
            id: 7,
            name: "Shared library".into(),
            kind: audiolib_core::albums::AlbumKind::Metadata,
            metadata_key: Some("Shared library".into()),
            folder_id: None,
            folder_path: None,
            representative_audio_id: Some(42),
        });
        *controller.selected.lock().unwrap() = Some(selected);

        let marker = std::env::var("AUDIOLIB_ARTWORK_GATE_MARKER").unwrap();
        let reply = |url: &str, artwork: bool| {
            let mut manifest = audiolib_core::compatibility::compatibility_manifest();
            // A modern server may advertise only shared-entity artwork. Its
            // capability arrival must admit the same coalesced album request.
            manifest.features.remove(if marker == "album_artwork" {
                "album_entity_artwork"
            } else {
                "album_artwork"
            });
            if !artwork {
                manifest.features.remove(marker.as_str());
            }
            ServerManage {
                url: url.into(),
                capabilities: Some(Capabilities {
                    compatibility: Some(manifest),
                    ..Capabilities::default()
                }),
                ..ServerManage::default()
            }
        };
        // Keep the independent worker before disk/HTTP while observing real queue
        // admission via its cancellation token. No server or audio device is used.
        let cache_guard = controller.artwork_cache_lock.lock().unwrap();
        let initial = controller.artwork_cancel.lock().unwrap().clone();
        controller.apply_server_manage(&ui, url, reply(url, false));
        assert!(!initial.load(Ordering::Acquire));
        controller.apply_server_manage(&ui, "http://127.0.0.1:8", reply(url, true));
        assert!(
            !initial.load(Ordering::Acquire),
            "a stale server cannot queue artwork"
        );

        controller.apply_server_manage(&ui, url, reply(url, true));
        assert!(
            initial.load(Ordering::Acquire),
            "new capability must retry the selected cover"
        );
        let queued = controller.artwork_cancel.lock().unwrap().clone();
        controller.apply_server_manage(&ui, url, reply(url, true));
        assert!(
            !queued.load(Ordering::Acquire),
            "repeated capability does not restart the request"
        );
        controller.apply_server_manage(&ui, url, reply(url, false));
        assert!(
            !queued.load(Ordering::Acquire),
            "a legacy reply does not queue artwork"
        );

        controller.closing.store(true, Ordering::Release);
        controller.apply_server_manage(&ui, url, reply(url, true));
        assert!(
            !queued.load(Ordering::Acquire),
            "closing cannot admit a new cover request"
        );
        controller.shutdown();
        drop(cache_guard);
        drop(controller);
        drop(ui);
        drop(engine);
    }

    #[test]
    fn memory_keys_name_their_record() {
        assert_eq!(
            stream_key_id("11111111-aaaa/42/1024/pcm-48000/server"),
            Some(42)
        );
        assert_eq!(stream_key_id("lib"), None);
        assert_eq!(
            waveform_key_id("11111111-aaaa/12/42-0-18446744073709551615-1024-1"),
            Some(42)
        );
        assert_eq!(waveform_key_id("lib/12"), None);
    }

    #[test]
    fn menu_confirmations_use_the_server_library_page_wording() {
        let records = removal_records_text(3, true);
        assert!(records.contains("移除所选的 3 个记录"), "{records}");
        assert!(records.contains("服务器上的 WAV 文件不会改动"), "{records}");
        assert!(records.contains("以新的 id"), "{records}");
        let folder = removal_folder_text("/lib/Ambience", true);
        assert!(folder.contains("「/lib/Ambience」"), "{folder}");
        assert!(
            folder.contains("服务器上的目录和 WAV 文件不会改动"),
            "{folder}"
        );
        let english = removal_records_text(1, false);
        assert!(
            english.contains("WAV files on the server are untouched"),
            "{english}"
        );
    }
}
