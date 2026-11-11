// crates/audiolib-core/src/local_upgrade.rs
//! Prepare directory-segment path search for a local library.
//!
//! This builds a derived index without publishing schema 9 or changing the local
//! writable format. Batches are checkpointed so interrupted preparation resumes
//! without asking again or making a second backup.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Free space needed relative to the library size: rollback backup 1x + WAL/index 0.3x (§5.3).
pub const SPACE_FACTOR_NUM: u64 = 13;
pub const SPACE_FACTOR_DEN: u64 = 10;

/// What opening this library needs before path search can be used.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalUpgradePlan {
    /// A checkpoint exists: continue without asking (§6 "本地库迁移中断").
    pub resuming: bool,
    pub completed: u64,
    pub total: u64,
    pub database_bytes: u64,
    pub required_bytes: u64,
    /// `None` when the volume does not report free space.
    pub available_bytes: Option<u64>,
    pub estimated_secs: u64,
}

impl LocalUpgradePlan {
    pub fn required_for(database_bytes: u64) -> u64 {
        database_bytes.saturating_mul(SPACE_FACTOR_NUM) / SPACE_FACTOR_DEN
    }

    /// A first run needs room for the backup; a resume already has its backup.
    pub fn insufficient_space(&self) -> bool {
        !self.resuming
            && self
                .available_bytes
                .is_some_and(|available| available < self.required_bytes)
    }
}

/// Phases reported while upgrading, matching the server's names (§5.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LocalUpgradePhase {
    Backup,
    PathBuild,
    Optimize,
    Switch,
}

impl LocalUpgradePhase {
    pub fn label(self, chinese: bool) -> &'static str {
        match (self, chinese) {
            (Self::Backup, true) => "备份",
            (Self::PathBuild, true) => "建立路径索引",
            (Self::Optimize, true) => "优化索引",
            (Self::Switch, true) => "切换",
            (Self::Backup, false) => "Backing up",
            (Self::PathBuild, false) => "Building path index",
            (Self::Optimize, false) => "Optimizing index",
            (Self::Switch, false) => "Switching",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalUpgradeProgress {
    pub phase: LocalUpgradePhase,
    pub completed: u64,
    pub total: u64,
}

impl LocalUpgradeProgress {
    pub fn percent(&self) -> u8 {
        if self.total == 0 {
            return 0;
        }
        (self.completed.min(self.total).saturating_mul(100) / self.total) as u8
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalUpgradeOutcome {
    Done {
        backup: Option<PathBuf>,
    },
    /// Stopped between batches; the checkpoint is kept for the next open.
    Cancelled,
}

fn duration_text(secs: u64, chinese: bool) -> String {
    match (secs, chinese) {
        (0..=59, true) => "不到 1 分钟".into(),
        (0..=59, false) => "under a minute".into(),
        (s, true) => format!("约 {} 分钟", s.div_ceil(60)),
        (s, false) => format!("about {} min", s.div_ceil(60)),
    }
}

/// Binary size text (KiB/MiB/GiB), one decimal above KiB.
pub fn size_text(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// §7.1 confirm dialog title.
pub fn confirm_title(chinese: bool) -> &'static str {
    if chinese {
        "建立路径搜索索引"
    } else {
        "Prepare path search"
    }
}

/// §7.1 confirm dialog body.
pub fn confirm_body(plan: &LocalUpgradePlan, chinese: bool) -> String {
    let time = duration_text(plan.estimated_secs, chinese);
    let space = size_text(plan.required_bytes);
    if chinese {
        format!(
            "这一步为本地音效库建立路径搜索索引，资料库格式保持不变。开始前会自动备份到同目录。预计需要 {time}，需要可用空间 {space}。构建中可以取消，下次打开会从中断处继续。"
        )
    } else {
        format!(
            "This prepares a path-search index for the local sound library and keeps its format unchanged. A backup is created in the same folder first. The estimated time is {time}, with {space} of free space required. You can cancel during preparation and resume on the next open."
        )
    }
}

pub fn confirm_buttons(chinese: bool) -> (&'static str, &'static str) {
    if chinese {
        ("开始构建", "暂不，打开其它音效库")
    } else {
        ("Build index", "Not now, open another library")
    }
}

/// §7.1 progress line.
pub fn progress_text(progress: &LocalUpgradeProgress, chinese: bool) -> String {
    let phase = progress.phase.label(chinese);
    let percent = progress.percent();
    if chinese {
        format!(
            "正在准备路径搜索：{phase} {}/{}（{percent}%）",
            progress.completed, progress.total
        )
    } else {
        format!(
            "Preparing path search: {phase} {}/{} ({percent}%)",
            progress.completed, progress.total
        )
    }
}

pub fn cancel_confirm_text(chinese: bool) -> &'static str {
    if chinese {
        "已完成的部分会保留，下次打开会继续。确定取消吗？"
    } else {
        "Finished work is kept and the next open continues. Cancel index preparation?"
    }
}

pub fn resume_text(chinese: bool) -> &'static str {
    if chinese {
        "继续上次未完成的路径索引构建…"
    } else {
        "Continuing unfinished path-index preparation…"
    }
}

pub fn insufficient_space_text(plan: &LocalUpgradePlan, chinese: bool) -> String {
    let need = size_text(plan.required_bytes);
    let free = plan.available_bytes.map(size_text).unwrap_or_else(|| {
        if chinese {
            "未知".into()
        } else {
            "unknown".into()
        }
    });
    if chinese {
        format!("磁盘空间不足：需要约 {need}，当前可用 {free}。请清理后重试。")
    } else {
        format!(
            "Not enough disk space: about {need} needed, {free} free. Free some space and try again."
        )
    }
}

pub fn failed_text(backup: Option<&std::path::Path>, chinese: bool) -> String {
    let path = backup
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| {
            if chinese {
                "（无）".into()
            } else {
                "(none)".into()
            }
        });
    if chinese {
        format!(
            "路径索引构建未完成，原有音效记录与标注保留。已提交的路径索引进度可能已保存，下次打开会继续构建。备份位置为 {path}。"
        )
    } else {
        format!(
            "Path-index preparation is incomplete. Existing audio records and annotations are retained. Committed path-index progress may be saved for the next open to resume. Backup at {path}."
        )
    }
}

pub fn copy_diagnostics_label(chinese: bool) -> &'static str {
    if chinese {
        "复制诊断信息"
    } else {
        "Copy diagnostics"
    }
}

pub fn cancelled_text(chinese: bool) -> &'static str {
    if chinese {
        "路径索引构建已取消，已完成的部分会保留，下次打开会继续。"
    } else {
        "Path-index preparation cancelled; finished work is kept and the next open continues."
    }
}

pub fn done_text(chinese: bool) -> &'static str {
    if chinese {
        "路径索引构建完成，路径搜索已可用。"
    } else {
        "Path index built. Path search is available."
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> LocalUpgradePlan {
        LocalUpgradePlan {
            resuming: false,
            completed: 0,
            total: 10,
            database_bytes: 1000,
            required_bytes: LocalUpgradePlan::required_for(1000),
            available_bytes: Some(1200),
            estimated_secs: 70,
        }
    }

    #[test]
    fn space_needs_one_point_three_times_on_first_run_only() {
        let mut plan = plan();
        assert_eq!(plan.required_bytes, 1300);
        assert!(plan.insufficient_space());
        plan.resuming = true;
        assert!(!plan.insufficient_space());
        plan.resuming = false;
        plan.available_bytes = None;
        assert!(!plan.insufficient_space());
    }

    #[test]
    fn texts_follow_design_wording() {
        let plan = plan();
        let body = confirm_body(&plan, true);
        assert!(
            body.contains("路径搜索索引") && body.contains("约 2 分钟"),
            "{body}"
        );
        assert!(body.contains("下次打开会从中断处继续"));
        assert_eq!(confirm_buttons(true), ("开始构建", "暂不，打开其它音效库"));
        let progress = LocalUpgradeProgress {
            phase: LocalUpgradePhase::PathBuild,
            completed: 5,
            total: 10,
        };
        assert_eq!(
            progress_text(&progress, true),
            "正在准备路径搜索：建立路径索引 5/10（50%）"
        );
        assert!(insufficient_space_text(&plan, true).starts_with("磁盘空间不足：需要约 1.3 KB"));
        assert!(failed_text(None, true).contains("已提交的路径索引进度可能已保存"));
    }

    #[test]
    fn percent_clamps_and_handles_empty_total() {
        let mut progress = LocalUpgradeProgress {
            phase: LocalUpgradePhase::Switch,
            completed: 3,
            total: 0,
        };
        assert_eq!(progress.percent(), 0);
        progress.total = 2;
        assert_eq!(progress.percent(), 100);
    }
}
