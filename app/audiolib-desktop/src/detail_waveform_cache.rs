// app/audiolib-desktop/src/detail_waveform_cache.rs
//! 「本机缓存」 in global settings: the client's on-demand colored detail Waveform
//! cache, as distinct from the server's list thumbnail Waveforms.
//!
//! Layout under `<cache>/waveforms/` (shared parent with the list cache):
//! * local mode: `<16 hex>.json` files written by `audiolib_audio::cached_waveform`;
//! * server mode: `server/<library dir>/<id>-<start>-<end>-<buckets>-<colors>.json` and
//!   its `.etag`, beside the list cache's `list/` directory and `waveforms.db`.
//!
//! Only those files are counted or removed: never `list/`, `waveforms.db`, any other
//! directory, the library database, settings or source WAVs. All I/O runs on a worker
//! thread, never on the UI thread or in the audio callback.
use super::{Controller, cn_or, post};
use crate::AppWindow;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// Totals of the colored detail Waveform cache.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct DetailCacheStats {
    pub entries: u64,
    pub bytes: u64,
}

fn is_local_entry(name: &str) -> bool {
    // `<16 hex>.json` or its `<16 hex>.<nanos>.tmp` staging file.
    let Some((stem, rest)) = name.split_once('.') else {
        return false;
    };
    stem.len() == 16
        && stem.bytes().all(|b| b.is_ascii_hexdigit())
        && (rest == "json" || (rest.ends_with(".tmp") || rest == "tmp"))
}

fn is_server_entry(name: &str) -> bool {
    // `<id>-<start>-<end>-<buckets>-<0|1>.json|.etag`
    let Some(stem) = name
        .strip_suffix(".json")
        .or_else(|| name.strip_suffix(".etag"))
    else {
        return false;
    };
    let parts: Vec<&str> = stem.split('-').collect();
    parts.len() == 5
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

fn files_in(directory: &Path, accept: fn(&str) -> bool, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_file() && entry.file_name().to_str().is_some_and(accept) {
            out.push(entry.path());
        }
    }
}

/// Every colored detail Waveform cache file under `<cache_root>/waveforms`.
pub(super) fn detail_cache_files(cache_root: &Path) -> Vec<PathBuf> {
    let root = cache_root.join("waveforms");
    let mut files = Vec::new();
    files_in(&root, is_local_entry, &mut files);
    if let Ok(libraries) = std::fs::read_dir(root.join("server")) {
        for library in libraries.flatten() {
            if library.file_type().is_ok_and(|kind| kind.is_dir()) {
                files_in(&library.path(), is_server_entry, &mut files);
            }
        }
    }
    files
}

/// Entries are the `.json` payloads; bytes include their `.etag` sidecars.
pub(super) fn detail_cache_stats(cache_root: &Path) -> DetailCacheStats {
    let mut stats = DetailCacheStats::default();
    for file in detail_cache_files(cache_root) {
        if file.extension().is_some_and(|ext| ext == "json") {
            stats.entries += 1;
        }
        stats.bytes += std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
    }
    stats
}

/// Remove the colored detail cache files; returns what remains afterwards.
pub(super) fn clear_detail_cache(cache_root: &Path) -> (u64, DetailCacheStats) {
    let mut removed = 0;
    for file in detail_cache_files(cache_root) {
        if std::fs::remove_file(&file).is_ok() {
            removed += 1;
        }
    }
    (removed, detail_cache_stats(cache_root))
}

fn stats_text(stats: DetailCacheStats, chinese: bool) -> String {
    let mib = stats.bytes as f64 / 1_048_576.0;
    if chinese {
        format!("{} 条 · {mib:.1} MiB", stats.entries)
    } else {
        format!("{} entries · {mib:.1} MiB", stats.entries)
    }
}

impl Controller {
    pub(super) fn bind_detail_waveform_cache(self: &Arc<Self>, ui: &AppWindow) {
        let c = self.clone();
        ui.on_sm_cw_refresh_requested(move || c.refresh_detail_cache(false));
        let c = self.clone();
        ui.on_sm_cw_clear_requested(move || c.refresh_detail_cache(true));
    }

    /// Read (or clear, then read) the colored cache on a worker thread.
    pub(super) fn refresh_detail_cache(self: &Arc<Self>, clear: bool) {
        let Some(ui) = self.ui.upgrade() else { return };
        if ui.get_sm_cw_busy() {
            return;
        }
        ui.set_sm_cw_busy(true);
        ui.set_sm_cw_confirm(false);
        let chinese = self.chinese.load(Ordering::Acquire);
        let cache_root = self.config.lock().unwrap().paths.cache.clone();
        if clear {
            // Drop the in-memory copies too so the next draw refetches/regenerates.
            self.remote.lock().unwrap().waveforms.clear();
        }
        let weak = self.ui.clone();
        let spawned = std::thread::Builder::new()
            .name("detail-waveform-cache".into())
            .spawn(move || {
                let (removed, stats) = if clear {
                    let (removed, stats) = clear_detail_cache(&cache_root);
                    (Some(removed), stats)
                } else {
                    (None, detail_cache_stats(&cache_root))
                };
                post(weak, move |ui| {
                    ui.set_sm_cw_busy(false);
                    ui.set_sm_cw_stats(stats_text(stats, chinese).into());
                    ui.set_sm_cw_message(
                        removed
                            .map(|removed| {
                                if chinese {
                                    format!("已删除 {removed} 个本地彩色波形缓存文件。")
                                } else {
                                    format!("Removed {removed} local colored Waveform cache files.")
                                }
                            })
                            .unwrap_or_default()
                            .into(),
                    );
                });
            });
        if spawned.is_err() {
            ui.set_sm_cw_busy(false);
            ui.set_sm_cw_message(
                cn_or(
                    chinese,
                    "无法启动缓存统计。",
                    "Could not start the cache scan.",
                )
                .into(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clearing_removes_only_colored_detail_waveform_files() {
        let root = std::env::temp_dir().join(format!(
            "audiolib-detail-cache-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let waveforms = root.join("waveforms");
        let library = waveforms.join("server").join("lib1");
        std::fs::create_dir_all(library.join("list")).unwrap();
        let write = |path: PathBuf, bytes: usize| std::fs::write(path, vec![b'x'; bytes]).unwrap();
        write(waveforms.join("0123456789abcdef.json"), 100);
        write(waveforms.join("0123456789abcdef.123.tmp"), 7);
        write(library.join("42-0-1000-2048-1.json"), 50);
        write(library.join("42-0-1000-2048-1.etag"), 10);
        // Kept: list cache, waveforms.db, unrelated files.
        write(library.join("list").join("shard-0.bin"), 300);
        write(library.join("waveforms.db"), 400);
        write(waveforms.join("notes.json"), 5);
        write(root.join("library.db"), 9);
        let stats = detail_cache_stats(&root);
        assert_eq!(
            stats,
            DetailCacheStats {
                entries: 2,
                bytes: 167
            }
        );
        let (removed, after) = clear_detail_cache(&root);
        assert_eq!((removed, after), (4, DetailCacheStats::default()));
        assert!(library.join("list").join("shard-0.bin").exists());
        assert!(library.join("waveforms.db").exists());
        assert!(waveforms.join("notes.json").exists());
        assert!(root.join("library.db").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
