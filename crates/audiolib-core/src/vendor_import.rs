// crates/audiolib-core/src/vendor_import.rs
//! Vendor CSV / XLS(X) import (design §4): client-side pre-checks, the server's
//! preview/commit/rollback replies, and the wording the desktop shows.
//!
//! Parsing, matching and writing happen on the server (§4.7); the client only refuses
//! files that are certain to exceed the advertised limits (§4.6) before uploading.

use crate::server_upgrade::{
    VENDOR_IMPORT_MAX_FILE_BYTES, VENDOR_IMPORT_MAX_ROW_TEXT_BYTES, VENDOR_IMPORT_MAX_ROWS,
    VendorImportInfo,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
#[cfg(test)]
use std::io::BufRead;
use std::io::Read;
use std::path::Path;

/// Automatic table Metadata collected during a WAV scan. Counts are separate from
/// audio-header probing: a changed table can update an otherwise unchanged WAV.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VendorScanSummary {
    /// Eligible CSV/TSV/XLS/XLSX files examined within the scanned directory tree.
    pub tables: u64,
    /// Data rows examined across the tables' sheets.
    pub rows: u64,
    /// Rows associated with exactly one same-directory WAV.
    pub matched: u64,
    /// Audio projections changed, counted once per (table, audio record).
    /// Different tables contributing to the same WAV are separate operations.
    pub updated: u64,
    /// Unchanged audio projections, counted once per (table, audio record).
    pub unchanged: u64,
    /// Rows with multiple same-directory candidates; no automatic write occurs.
    pub ambiguous: u64,
    /// Rows without a safe same-directory WAV match.
    pub unmatched: u64,
}

/// Compact live/final scan status shared by local and server-mode desktop views.
/// No table text or source paths are included in this presentation summary.
pub fn automatic_scan_summary(summary: &VendorScanSummary, chinese: bool) -> String {
    if summary.tables == 0 {
        return String::new();
    }
    if chinese {
        format!(
            "表格 {} · 匹配 {} 行 · Metadata 更新 {} · 未变化 {} · 未匹配 {} · 歧义 {}",
            summary.tables,
            summary.matched,
            summary.updated,
            summary.unchanged,
            summary.unmatched,
            summary.ambiguous
        )
    } else {
        format!(
            "{} tables · {} matched rows · {} Metadata updated · {} unchanged · {} unmatched · {} ambiguous",
            summary.tables,
            summary.matched,
            summary.updated,
            summary.unchanged,
            summary.unmatched,
            summary.ambiguous
        )
    }
}

/// Limits used for one import: the server's `/capabilities.vendor_import`, falling back to
/// the design defaults for fields an older server omits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VendorLimits {
    pub max_file_bytes: u64,
    pub max_rows: u64,
    pub max_row_text_bytes: u64,
}

impl Default for VendorLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: VENDOR_IMPORT_MAX_FILE_BYTES,
            max_rows: VENDOR_IMPORT_MAX_ROWS,
            max_row_text_bytes: VENDOR_IMPORT_MAX_ROW_TEXT_BYTES,
        }
    }
}

impl From<&VendorImportInfo> for VendorLimits {
    fn from(info: &VendorImportInfo) -> Self {
        Self {
            max_file_bytes: info.max_file_bytes,
            max_rows: info.max_rows,
            max_row_text_bytes: info
                .max_row_text_bytes
                .unwrap_or(VENDOR_IMPORT_MAX_ROW_TEXT_BYTES),
        }
    }
}

/// Container detected from the file header, never from the extension (§4.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VendorFormat {
    Xls,
    Xlsx,
    Text,
}

pub fn sniff_format(head: &[u8]) -> VendorFormat {
    if head.starts_with(&[0xD0, 0xCF, 0x11, 0xE0]) {
        VendorFormat::Xls
    } else if head.starts_with(b"PK\x03\x04") {
        VendorFormat::Xlsx
    } else {
        VendorFormat::Text
    }
}

/// AppleDouble, Office lock files, Synology metadata and Finder files are skipped (§4.2).
pub fn is_skipped_name(name: &str) -> bool {
    name.starts_with("._")
        || name.starts_with("~$")
        || name.starts_with("@syno")
        || name.eq_ignore_ascii_case(".DS_Store")
}

/// Why a file was refused before upload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VendorPrecheckError {
    Skipped,
    TooLarge { bytes: u64, max: u64 },
    TooManyRows { max: u64 },
    RowTooLong { row: u64, max: u64 },
    Empty,
}

impl VendorPrecheckError {
    pub fn text(&self, chinese: bool) -> String {
        match (self, chinese) {
            (Self::Skipped, true) => {
                "这是系统或锁定文件（._*、~$*、@syno*、.DS_Store），已跳过。".into()
            }
            (Self::Skipped, false) => {
                "This is a system or lock file (._*, ~$*, @syno*, .DS_Store); skipped.".into()
            }
            (Self::TooLarge { bytes, max }, true) => format!(
                "文件过大：{}，上限 {}。整文件拒绝。",
                crate::local_upgrade::size_text(*bytes),
                crate::local_upgrade::size_text(*max)
            ),
            (Self::TooLarge { bytes, max }, false) => format!(
                "File too large: {}, limit {}. The whole file is refused.",
                crate::local_upgrade::size_text(*bytes),
                crate::local_upgrade::size_text(*max)
            ),
            (Self::TooManyRows { max }, true) => {
                format!("行数超过上限 {max} 行。整文件拒绝。")
            }
            (Self::TooManyRows { max }, false) => {
                format!("More than {max} rows. The whole file is refused.")
            }
            (Self::RowTooLong { row, max }, true) => format!(
                "第 {row} 行文本超过 {}。整文件拒绝。",
                crate::local_upgrade::size_text(*max)
            ),
            (Self::RowTooLong { row, max }, false) => format!(
                "Row {row} is longer than {}. The whole file is refused.",
                crate::local_upgrade::size_text(*max)
            ),
            (Self::Empty, true) => "文件是空的。".into(),
            (Self::Empty, false) => "The file is empty.".into(),
        }
    }
}

/// Refuse only skipped/empty/oversized inputs before upload. Logical CSV records and
/// decoded row sizes must be checked by the real parser: physical newlines inside
/// quotes and UTF-16 byte widths are not safe reasons to reject a valid table here.
pub fn precheck(
    path: &Path,
    limits: VendorLimits,
) -> std::io::Result<Result<VendorFormat, VendorPrecheckError>> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    if is_skipped_name(&name) {
        return Ok(Err(VendorPrecheckError::Skipped));
    }
    let bytes = std::fs::metadata(path)?.len();
    if bytes == 0 {
        return Ok(Err(VendorPrecheckError::Empty));
    }
    if bytes > limits.max_file_bytes {
        return Ok(Err(VendorPrecheckError::TooLarge {
            bytes,
            max: limits.max_file_bytes,
        }));
    }
    let mut file = std::fs::File::open(path)?;
    let mut head = [0u8; 8];
    let read = file.read(&mut head)?;
    let format = sniff_format(&head[..read]);
    Ok(Ok(format))
}

#[cfg(test)]
fn check_text(mut reader: impl BufRead, limits: VendorLimits) -> Result<(), VendorPrecheckError> {
    // UTF-16 doubles the byte count of every character; compare against twice the limit.
    let utf16 = match reader.fill_buf() {
        Ok(head) => head.starts_with(&[0xFF, 0xFE]) || head.starts_with(&[0xFE, 0xFF]),
        Err(_) => false,
    };
    let line_limit = if utf16 {
        limits.max_row_text_bytes.saturating_mul(2)
    } else {
        limits.max_row_text_bytes
    };
    let mut rows = 0u64;
    let mut line = Vec::new();
    loop {
        line.clear();
        let n = match reader
            .by_ref()
            .take(line_limit.saturating_add(4))
            .read_until(b'\n', &mut line)
        {
            Ok(n) => n,
            Err(_) => return Ok(()),
        };
        if n == 0 {
            break;
        }
        let ended = line.last() == Some(&b'\n');
        let mut text = line.len() as u64;
        if ended {
            text -= 1;
            if line.len() >= 2 && line[line.len() - 2] == b'\r' {
                text -= 1;
            }
        }
        if utf16 && ended {
            // The second byte of a UTF-16LE newline follows the 0x0A byte.
            text = text.saturating_sub(1);
        }
        rows += 1;
        if text > line_limit {
            return Err(VendorPrecheckError::RowTooLong {
                row: rows,
                max: limits.max_row_text_bytes,
            });
        }
        if rows > limits.max_rows.saturating_add(1) {
            return Err(VendorPrecheckError::TooManyRows {
                max: limits.max_rows,
            });
        }
    }
    Ok(())
}

/// One table in a preview (`files[]`); a failed file does not stop the batch (§4.2).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VendorFileResult {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub rows: u64,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub format: String,
    #[serde(default)]
    pub encoding: String,
    #[serde(default)]
    pub delimiter: Option<String>,
    #[serde(default)]
    pub truncated_rows: u64,
}

/// Canonical field -> source header mapping; filename/path identify the audio, not annotations.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VendorColumnMapping {
    pub filename: Option<String>,
    pub relative_path: Option<String>,
    #[serde(default)]
    pub fields: BTreeMap<String, String>,
}

/// A per-sheet override; header_row is one based in the original spreadsheet/text records.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VendorSheetMapping {
    pub source_file: String,
    pub sheet: String,
    pub header_row: Option<u64>,
    pub mapping: VendorColumnMapping,
}

/// Recoverable preview/commit settings. Confirmations are explicit row_id -> audio id choices.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VendorImportOptions {
    pub vendor_root: Option<String>,
    pub fuzzy: bool,
    pub overwrite: bool,
    pub encoding: Option<String>,
    pub delimiter: Option<String>,
    pub preferred_languages: Vec<String>,
    pub mapping: Option<VendorColumnMapping>,
    pub sheet_mappings: Vec<VendorSheetMapping>,
    pub confirmations: BTreeMap<String, i64>,
    pub template: Option<String>,
}

/// A named reusable mapping persisted in the owned preview store.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VendorMappingTemplate {
    pub id: String,
    pub name: String,
    pub mapping: VendorColumnMapping,
    pub encoding: Option<String>,
    pub delimiter: Option<String>,
}

/// One source sheet and its detected header/mapping, including sheets with no matching key.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VendorSheetPreview {
    pub source_file: String,
    pub sheet: String,
    pub header_row: u64,
    pub headers: Vec<String>,
    pub mapping: VendorColumnMapping,
    pub rows: u64,
}

/// An explicit candidate offered for ambiguous or fuzzy matching; never an implicit write.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VendorMatchCandidate {
    pub id: i64,
    pub path: String,
}

/// A bounded row sample. Full row details can be paged by row_id in the preview store.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VendorRowPreview {
    pub row_id: String,
    pub source_file: String,
    pub sheet: String,
    pub row: u64,
    pub values: BTreeMap<String, String>,
    pub matched_id: Option<i64>,
    pub candidates: Vec<VendorMatchCandidate>,
    /// Exact full candidate-set size; candidates embeds only its bounded first page.
    #[serde(default)]
    pub candidate_count: u64,
    #[serde(default)]
    pub candidates_truncated: bool,
    pub suggested: bool,
    pub user_conflict: bool,
}

/// A bounded page from the full disk-backed candidate set; any page can be confirmed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VendorCandidatePage {
    pub offset: u64,
    pub total: u64,
    pub candidates: Vec<VendorMatchCandidate>,
}

/// `POST /api/vendor-import/preview` and `GET /api/vendor-import/{id}` replies. Fields
/// an older or newer server omits default to zero.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VendorImportPreview {
    pub import_id: String,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub rows: u64,
    #[serde(default)]
    pub matched: u64,
    #[serde(default)]
    pub unmatched: u64,
    #[serde(default)]
    pub ambiguous: u64,
    #[serde(default)]
    pub suggested: u64,
    #[serde(default)]
    pub files: Vec<VendorFileResult>,
    #[serde(default)]
    pub sheets: Vec<VendorSheetPreview>,
    #[serde(default)]
    pub samples: Vec<VendorRowPreview>,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub options: VendorImportOptions,
    #[serde(default)]
    pub can_commit: bool,
}

/// Commit / rollback reply.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VendorImportResult {
    #[serde(default)]
    pub import_id: String,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub written: u64,
    #[serde(default)]
    pub restored: u64,
}

/// Durable progress of a preview or a batched commit/rollback; state survives process exit.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VendorImportStatus {
    pub preview: VendorImportPreview,
    pub result: Option<VendorImportResult>,
    pub completed: u64,
    pub total: u64,
    pub cancellable: bool,
    pub error: Option<String>,
    /// Stable operation identity for resuming this library's authorized import history.
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

/// A bounded page of durable import history.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VendorImportHistory {
    pub imports: Vec<VendorImportStatus>,
}

pub fn preview_summary(preview: &VendorImportPreview, chinese: bool) -> String {
    let failed: Vec<&VendorFileResult> = preview
        .files
        .iter()
        .filter(|file| file.error.is_some())
        .collect();
    let mut text = if chinese {
        format!(
            "预览：{} 行 · 匹配 {} · 未匹配 {} · 歧义 {}（需确认）· 建议 {}",
            preview.rows, preview.matched, preview.unmatched, preview.ambiguous, preview.suggested
        )
    } else {
        format!(
            "Preview: {} rows · matched {} · unmatched {} · ambiguous {} (needs review) · suggested {}",
            preview.rows, preview.matched, preview.unmatched, preview.ambiguous, preview.suggested
        )
    };
    if !failed.is_empty() {
        let names: Vec<String> = failed
            .iter()
            .take(3)
            .map(|file| {
                format!(
                    "{}：{}",
                    file.name,
                    file.error.as_deref().unwrap_or_default()
                )
            })
            .collect();
        if chinese {
            text.push_str(&format!(
                "\n{} 个文件失败：{}",
                failed.len(),
                names.join("；")
            ));
        } else {
            text.push_str(&format!(
                "\n{} file(s) failed: {}",
                failed.len(),
                names.join("; ")
            ));
        }
    }
    text.push_str(if chinese {
        "\n预览不写库；未匹配和歧义的行不会写入。用户标注永远不被覆盖。"
    } else {
        "\nPreview does not write; unmatched and ambiguous rows are not written. Your annotations are never overwritten."
    });
    text
}

pub fn committed_text(result: &VendorImportResult, chinese: bool) -> String {
    if chinese {
        format!(
            "厂商资料已导入 {} 条（批次 {}），可回滚。",
            result.written, result.import_id
        )
    } else {
        format!(
            "Imported {} vendor rows (batch {}); can be rolled back.",
            result.written, result.import_id
        )
    }
}

pub fn rolled_back_text(result: &VendorImportResult, chinese: bool) -> String {
    if chinese {
        format!("已回滚批次 {}。", result.import_id)
    } else {
        format!("Batch {} rolled back.", result.import_id)
    }
}

/// Quote one report/export cell as literal CSV text, including spreadsheet formula prefixes.
/// Parsing/import never executes formulas; export must also keep these strings as data.
pub fn report_csv_cell(value: &str) -> String {
    let dangerous = value.trim_start().starts_with(['=', '+', '-', '@']);
    let prefix = if dangerous { "'" } else { "" };
    format!("\"{prefix}{}\"", value.replace('"', "\"\""))
}

/// Entry point disabled reason: local libraries do not import yet (§4.7 covers server
/// mode only); a server without `vendor_import` or without manage rights hides it.
pub fn unavailable_text(remote: bool, chinese: bool) -> &'static str {
    match (remote, chinese) {
        (false, true) => "本地音效库暂不支持厂商资料导入，请在服务器模式下由管理员导入。",
        (false, false) => {
            "Vendor metadata import is not available for local libraries yet; import it on the server."
        }
        (true, true) => crate::server_upgrade::old_server_explanation(true),
        (true, false) => crate::server_upgrade::old_server_explanation(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_replies_preserve_automatic_counts_and_accept_older_servers() {
        let old: crate::ScanProgress = serde_json::from_str(
            r#"{"session_id":7,"state":"completed","processed":3,"updated":0}"#,
        )
        .unwrap();
        assert_eq!(old.vendor_metadata, VendorScanSummary::default());
        let summary = VendorScanSummary {
            tables: 2,
            rows: 9,
            matched: 5,
            updated: 3,
            unchanged: 1,
            ambiguous: 1,
            unmatched: 3,
        };
        let encoded = serde_json::to_value(crate::ScanProgress {
            vendor_metadata: summary,
            ..old
        })
        .unwrap();
        let decoded: crate::ScanProgress = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded.vendor_metadata, summary);
        assert_eq!(decoded.processed, 3);
        assert_eq!(
            decoded.updated, 0,
            "Table writes are separate from WAV probing"
        );
        let future: crate::ScanProgress = serde_json::from_value(serde_json::json!({
            "vendor_metadata": {"tables": 1, "future_count": 99},
            "future_scan_option": true
        }))
        .unwrap();
        assert_eq!(future.vendor_metadata.tables, 1);
        assert_eq!(future.vendor_metadata.updated, 0);
    }

    fn limits(rows: u64, line: u64) -> VendorLimits {
        VendorLimits {
            max_file_bytes: 1024,
            max_rows: rows,
            max_row_text_bytes: line,
        }
    }

    #[test]
    fn format_comes_from_magic_not_extension() {
        assert_eq!(
            sniff_format(&[0xD0, 0xCF, 0x11, 0xE0, 0]),
            VendorFormat::Xls
        );
        assert_eq!(sniff_format(b"PK\x03\x04rest"), VendorFormat::Xlsx);
        assert_eq!(sniff_format(b"Filename,Desc"), VendorFormat::Text);
    }

    #[test]
    fn skip_list_matches_design() {
        for name in ["._a.csv", "~$b.xlsx", "@syno.csv", ".DS_Store"] {
            assert!(is_skipped_name(name), "{name}");
        }
        assert!(!is_skipped_name("Cannons.xlsx"));
    }

    #[test]
    fn rows_are_checked_at_the_boundary() {
        let text = "h\n1\n2\n";
        assert!(check_text(text.as_bytes(), limits(2, 10)).is_ok());
        assert_eq!(
            check_text(text.as_bytes(), limits(1, 10)),
            Err(VendorPrecheckError::TooManyRows { max: 1 })
        );
    }

    #[test]
    fn line_length_is_checked_at_the_boundary() {
        assert!(check_text("abcd\r\nx\n".as_bytes(), limits(10, 4)).is_ok());
        assert_eq!(
            check_text("ok\nabcde\n".as_bytes(), limits(10, 4)),
            Err(VendorPrecheckError::RowTooLong { row: 2, max: 4 })
        );
    }

    #[test]
    fn file_size_limit_refuses_whole_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("厂商.csv");
        std::fs::write(&path, vec![b'a'; 1025]).unwrap();
        assert_eq!(
            precheck(&path, limits(10, 4096)).unwrap(),
            Err(VendorPrecheckError::TooLarge {
                bytes: 1025,
                max: 1024
            })
        );
        std::fs::write(&path, b"Filename\nfoo.wav\n").unwrap();
        assert_eq!(
            precheck(&path, limits(10, 4096)).unwrap(),
            Ok(VendorFormat::Text)
        );
    }

    #[test]
    fn preview_reply_tolerates_missing_fields() {
        let preview: VendorImportPreview = serde_json::from_str(
            r#"{"import_id":"b1","matched":3,"files":[{"name":"a.xls","error":"encrypted"}]}"#,
        )
        .unwrap();
        let text = preview_summary(&preview, true);
        assert!(
            text.contains("匹配 3") && text.contains("a.xls：encrypted"),
            "{text}"
        );
    }

    #[test]
    fn quoted_newlines_are_left_to_the_real_parser_and_report_formulas_are_literal() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("quoted.csv");
        std::fs::write(&path, "Filename,Description\nfoo.wav,\"one\ntwo\nthree\"\n").unwrap();
        assert!(precheck(&path, limits(1, 100)).unwrap().is_ok());
        assert_eq!(report_csv_cell("=SUM(A1:A2)"), "\"'=SUM(A1:A2)\"");
        assert_eq!(report_csv_cell("one,\"two\""), "\"one,\"\"two\"\"\"");
    }
}
