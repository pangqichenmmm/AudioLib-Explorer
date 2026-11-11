// crates/audiolib-db/src/migration/diagnose.rs
//! Dedicated read-only diagnosis separates unreadable business rows from repairable
//! derived structures, unsupported formats and incomplete observations.
use super::inspect::{
    LibraryInspection, check_cancel, inspect_connection, observe_format, ordinary_tables,
};
use crate::{db_error, schema, search_storage};
use audiolib_core::{
    Result,
    compatibility::{
        CompatibilityDecision, CompatibilityReason, ExecutionState, LibraryAccess,
        MigrationEvidence, assess_library,
    },
};
use rusqlite::{Connection, ErrorCode, OpenFlags};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

/// Stable diagnostic classes. Neither unavailable reads nor unknown formats mean rebuild.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DiagnosticCode {
    /// Physical FTS storage or document membership is damaged; retained business
    /// data is readable and the registered migration can regenerate this data.
    DerivedDataInvalid,
    /// A positively identified ordinary business table returned SQLite corruption.
    BusinessRowsUnreadable,
    /// An explicitly retired table is damaged; use a dedicated preservation adapter,
    /// not a full-library rebuild verdict for otherwise readable retained data.
    RetiredDataUnreadable,
    /// Readable business rows contain broken foreign-key relationships.
    BusinessReferencesInvalid,
    /// The tool cannot prove that repairing the observed structure preserves identities.
    UnsafeSchemaRepair,
    /// Permissions, I/O, unsupported SQL or another failure prevented a conclusion.
    InspectionUnavailable,
}

/// Local inspection evidence; messages contain diagnostics, not exported row values.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DiagnosticFinding {
    /// Stable failure class for client/server presentation.
    pub code: DiagnosticCode,
    /// Known schema object, without source row contents.
    pub object: Option<String>,
    /// Present when the pinned SQLite engine supplied a native failure code.
    pub sqlite_extended_code: Option<i32>,
    /// Local diagnostic detail; never executable advice or SQL.
    pub message: String,
}

/// Recovery advice is evidence-based; this report never grants permission to delete/rescan.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LibraryDiagnosis {
    /// Canonical source selected by the caller.
    pub source: PathBuf,
    /// Evidence-based write/migration disposition, independent of application version.
    pub decision: CompatibilityDecision,
    /// Full source evidence only when the reads/checks needed for it succeeded.
    pub inspection: Option<LibraryInspection>,
    /// Repairable or blocking observations, in check order.
    pub findings: Vec<DiagnosticFinding>,
    /// A backup or explicit salvage/rebuild workflow is required for unreadable rows.
    pub recovery: Vec<String>,
    /// Always false: diagnosis never authorizes destructive reconstruction.
    pub automatic_rebuild_allowed: bool,
}

fn unavailable(object: Option<String>, error: impl std::fmt::Display) -> DiagnosticFinding {
    DiagnosticFinding {
        code: DiagnosticCode::InspectionUnavailable,
        object,
        sqlite_extended_code: None,
        message: error.to_string(),
    }
}

fn blocked(reason: CompatibilityReason) -> CompatibilityDecision {
    CompatibilityDecision {
        disposition: None,
        execution_state: ExecutionState::Blocked,
        reasons: vec![reason],
        direct_use: false,
    }
}

// Force the table B-tree instead of letting a covering secondary index hide damaged
// records. A WITHOUT ROWID primary tree is itself the authoritative table.
fn read_table(conn: &Connection, table: &str, cancel: &AtomicBool) -> rusqlite::Result<()> {
    let mut statement = conn.prepare(&format!(
        "SELECT * FROM {} NOT INDEXED",
        search_storage::quote_identifier(table)
    ))?;
    let columns = statement.column_count();
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        if cancel.load(Ordering::Acquire) {
            return Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_INTERRUPT),
                None,
            ));
        }
        for column in 0..columns {
            row.get_ref(column)?;
        }
    }
    Ok(())
}

fn derived_finding(conn: &Connection, cancel: &AtomicBool) -> Result<Option<DiagnosticFinding>> {
    let names = conn.prepare("SELECT name FROM pragma_table_list WHERE schema='main' AND type='shadow' AND name GLOB 'audio_search_*'")
        .map_err(db_error)?.query_map([], |row| row.get::<_, String>(0)).map_err(db_error)?
        .collect::<rusqlite::Result<Vec<_>>>().map_err(db_error)?;
    for name in names {
        check_cancel(cancel)?;
        if let Err(error) = read_table(conn, &name, cancel) {
            check_cancel(cancel)?;
            let native = error.sqlite_error();
            let corrupt = native.is_some_and(|error| {
                matches!(
                    error.code,
                    ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase
                )
            });
            return Ok(Some(DiagnosticFinding {
                code: if corrupt {
                    DiagnosticCode::DerivedDataInvalid
                } else {
                    DiagnosticCode::InspectionUnavailable
                },
                object: Some(name),
                sqlite_extended_code: native.map(|error| error.extended_code),
                message: error.to_string(),
            }));
        }
    }
    if super::inspect::table_exists(conn, "audio_search_docsize")? {
        match conn.query_row("SELECT
            (SELECT COUNT(*) FROM audio_files af LEFT JOIN audio_search_docsize ds ON af.id=ds.id WHERE ds.id IS NULL)
            +(SELECT COUNT(*) FROM audio_search_docsize ds LEFT JOIN audio_files af ON af.id=ds.id WHERE af.id IS NULL)", [], |row| row.get::<_, i64>(0)) {
            Ok(0) => {},
            Ok(count) => return Ok(Some(DiagnosticFinding { code: DiagnosticCode::DerivedDataInvalid,
                object: Some("audio_search_docsize".into()), sqlite_extended_code: None,
                message: format!("{count} unmatched FTS documents; regenerate the derived index from retained Metadata") })),
            Err(error) => return Ok(Some(unavailable(Some("audio_search_docsize".into()), error))),
        }
    }
    Ok(None)
}

/// Inspect a pinned source without a writer, migration job or automatic reconstruction.
pub fn diagnose(path: &Path, cancel: &AtomicBool) -> Result<LibraryDiagnosis> {
    check_cancel(cancel)?;
    let source = path.canonicalize()?;
    let conn =
        Connection::open_with_flags(&source, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(db_error)?;
    conn.execute_batch("PRAGMA query_only=ON; BEGIN;")
        .map_err(db_error)?;
    diagnose_connection(&conn, source, cancel)
}

pub(super) fn diagnose_connection(
    conn: &Connection,
    source: PathBuf,
    cancel: &AtomicBool,
) -> Result<LibraryDiagnosis> {
    check_cancel(cancel)?;
    let mut report = LibraryDiagnosis {
        source: source.clone(),
        decision: blocked(CompatibilityReason::InvalidLibrary),
        inspection: None,
        findings: Vec::new(),
        recovery: Vec::new(),
        automatic_rebuild_allowed: false,
    };
    let (format, identified, pending) = match observe_format(conn) {
        Ok(observation) => observation,
        Err(error) => {
            report.findings.push(unavailable(None, error));
            report.recovery.push("Establish the source identity and restore readable schema metadata before choosing a migration or recovery path".into());
            return Ok(report);
        }
    };
    let mut evidence = MigrationEvidence {
        identified_library: identified,
        business_integrity: None,
        can_regenerate_search: false,
        search_ready: false,
        pending,
    };
    let preliminary = assess_library(&format, &evidence, LibraryAccess::Write);
    if !identified
        || preliminary
            .reasons
            .contains(&CompatibilityReason::ToolUpgradeRequired)
    {
        report.decision = preliminary;
        report.recovery.push("Use an identified AudioLib source and a tool supporting its declared format; no rebuild is inferred from version differences".into());
        return Ok(report);
    }
    let retired: Vec<_> = super::recipe::changes()
        .into_iter()
        .filter_map(|change| match change {
            super::SchemaChange::RetireTable { table } => Some(table),
            _ => None,
        })
        .collect();
    let tables = match ordinary_tables(conn) {
        Ok(tables) => tables,
        Err(error) => {
            check_cancel(cancel)?;
            report.findings.push(unavailable(None, error));
            report.recovery.push(
                "Restore access to the source schema before diagnosing its business tables".into(),
            );
            return Ok(report);
        }
    };
    for table in tables {
        check_cancel(cancel)?;
        if let Err(error) = read_table(conn, &table, cancel) {
            check_cancel(cancel)?;
            let native = error.sqlite_error();
            let corrupt = native.is_some_and(|native| {
                matches!(
                    native.code,
                    ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase
                )
            });
            let retired = retired.iter().any(|name| name.eq_ignore_ascii_case(&table));
            report.findings.push(DiagnosticFinding {
                code: if corrupt && retired {
                    DiagnosticCode::RetiredDataUnreadable
                } else if corrupt {
                    DiagnosticCode::BusinessRowsUnreadable
                } else {
                    DiagnosticCode::InspectionUnavailable
                },
                object: Some(table),
                sqlite_extended_code: native.map(|native| native.extended_code),
                message: error.to_string(),
            });
            if corrupt && !retired {
                evidence.business_integrity = Some(false);
            }
            // One conclusive failed table prevents automatic migration; do not return
            // an unbounded dump of errors or continue scanning after a read failure.
            break;
        }
    }
    if !report.findings.is_empty() {
        report.decision = if evidence.business_integrity == Some(false) {
            assess_library(&format, &evidence, LibraryAccess::Write)
        } else {
            blocked(CompatibilityReason::InvalidFormat)
        };
        report.recovery.push("Preserve this source and its sidecars; prefer a verified backup or an explicit salvage workflow before any source-audio rebuild".into());
        return Ok(report);
    }
    check_cancel(cancel)?;
    // Outgoing relationships of explicitly retired tables are removed with those
    // tables. Retained references to a retired table are still rejected by blueprint.
    let retired =
        serde_json::to_string(&retired).map_err(|error| super::plan::invalid(error.to_string()))?;
    let references = conn.query_row(
        "SELECT COUNT(*) FROM pragma_foreign_key_check
        WHERE \"table\" COLLATE NOCASE NOT IN (SELECT value FROM json_each(?1))",
        [&retired],
        |row| row.get::<_, i64>(0),
    );
    match references {
        Ok(0) => {}
        Ok(count) => report.findings.push(DiagnosticFinding {
            code: DiagnosticCode::BusinessReferencesInvalid,
            object: None,
            sqlite_extended_code: None,
            message: format!(
                "{count} foreign-key relationships require repair; no rows were removed"
            ),
        }),
        Err(error) => report.findings.push(unavailable(None, error)),
    }
    // The current target format is repaired in place, so its identity/relationship evidence
    // must be inspected too; a damaged one stays Blocked instead of becoming migratable.
    if matches!(
        format.schema_version,
        6 | audiolib_core::compatibility::TARGET_SCHEMA
    ) && evidence.pending.is_none()
        && let Err(error) = schema::definition::check_repair_source(conn)
    {
        report.findings.push(DiagnosticFinding {
            code: DiagnosticCode::UnsafeSchemaRepair,
            object: None,
            sqlite_extended_code: None,
            message: error.to_string(),
        });
    }
    if !report.findings.is_empty() {
        report.decision = blocked(CompatibilityReason::InvalidFormat);
        report.recovery.push("Restore or repair the reported identity/relationship evidence before planning; a full library rebuild has not been established as necessary".into());
        return Ok(report);
    }
    let derived = match derived_finding(conn, cancel) {
        Ok(finding) => finding,
        Err(error) => {
            check_cancel(cancel)?;
            Some(unavailable(Some("audio_search".into()), error))
        }
    };
    if let Some(finding) = derived {
        let repairable = finding.code == DiagnosticCode::DerivedDataInvalid;
        report.findings.push(finding);
        if !repairable {
            report.decision = blocked(CompatibilityReason::InvalidFormat);
            return Ok(report);
        }
        report.recovery.push("Regenerate the derived FTS index in the owned migration candidate; retain the source and rollback backup".into());
    }
    match inspect_connection(conn, source, LibraryAccess::Write, true, cancel) {
        Ok(mut inspection) => {
            if report
                .findings
                .iter()
                .any(|finding| finding.code == DiagnosticCode::DerivedDataInvalid)
            {
                inspection.evidence.search_ready = false;
                inspection.decision = assess_library(
                    &inspection.format,
                    &inspection.evidence,
                    LibraryAccess::Write,
                );
            }
            report.decision = inspection.decision.clone();
            report.inspection = Some(inspection);
        }
        Err(error) => {
            check_cancel(cancel)?;
            report.decision = blocked(CompatibilityReason::InvalidFormat);
            report.findings.push(unavailable(None, error));
        }
    }
    Ok(report)
}
