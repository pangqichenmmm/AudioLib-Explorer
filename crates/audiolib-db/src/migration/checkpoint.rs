// crates/audiolib-db/src/migration/checkpoint.rs
//! Validate durable cursors against their committed rows before resuming any writes.
use super::{MigrationJob, MigrationPhase, plan::invalid, stage::copy_columns};
use crate::search_storage::{self, quote_identifier};
use audiolib_core::{Error, Result};
use rusqlite::Connection;
use std::collections::BTreeMap;

struct Checkpoint {
    phase: String,
    last: Option<i64>,
    completed: i64,
    done: i64,
}

fn rejected(detail: impl std::fmt::Display) -> Error {
    invalid(format!("Candidate checkpoint is inconsistent: {detail}"))
}

fn count(conn: &Connection, sql: &str) -> Result<i64> {
    conn.query_row(sql, [], |row| row.get(0)).map_err(rejected)
}

/// Source is already attached read-only as `input`; cancellation covers these scans.
/// This validates restart bookkeeping, not content hashes or FTS posting semantics.
/// The final business digest and index validation remain mandatory before publication.
pub(super) fn validate(
    job: &MigrationJob,
    candidate: &Connection,
    input: &Connection,
) -> Result<()> {
    let blueprint = search_storage::blueprint_with(input, &job.plan.schema_changes)?;
    let mut entries = BTreeMap::new();
    let mut statement = candidate
        .prepare("SELECT name,phase,last_id,completed,done FROM audiolib_migration_copy")
        .map_err(rejected)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                Checkpoint {
                    phase: row.get(1)?,
                    last: row.get(2)?,
                    completed: row.get(3)?,
                    done: row.get(4)?,
                },
            ))
        })
        .map_err(rejected)?;
    for row in rows {
        let (name, checkpoint) = row.map_err(rejected)?;
        if checkpoint.completed < 0 || !matches!(checkpoint.done, 0 | 1) {
            return Err(rejected(format!(
                "invalid counter or completion flag for {name:?}"
            )));
        }
        if entries.insert(name.clone(), checkpoint).is_some() {
            return Err(rejected(format!("duplicate table {name:?}")));
        }
    }
    let global = entries
        .remove("")
        .ok_or_else(|| rejected("missing global phase"))?;
    if global.done != 0 || !matches!(global.phase.as_str(), "copy" | "index") {
        return Err(rejected("invalid global phase or completion flag"));
    }
    if global.phase == "copy"
        && (global.last.is_some() || global.completed != 0 || job.phase == MigrationPhase::Indexing)
    {
        return Err(rejected("invalid or regressed copy phase"));
    }
    for (name, _) in &blueprint.tables {
        if !super::inspect::table_exists(input, name)? {
            continue;
        }
        let checkpoint = entries
            .remove(name)
            .ok_or_else(|| rejected(format!("missing table {name:?}")))?;
        if checkpoint.phase != "table" || (global.phase == "index" && checkpoint.done != 1) {
            return Err(rejected(format!("invalid table phase for {name:?}")));
        }
        validate_table(candidate, name, &checkpoint)?;
    }
    if !entries.is_empty() {
        return Err(rejected("unexpected table entries"));
    }
    if global.phase == "index" {
        validate_index(candidate, &global)?;
    }
    Ok(())
}

fn validate_table(conn: &Connection, name: &str, checkpoint: &Checkpoint) -> Result<()> {
    let table = quote_identifier(name);
    let actual = count(
        conn,
        &format!("SELECT COUNT(*) FROM main.{table} NOT INDEXED"),
    )?;
    if actual != checkpoint.completed {
        return Err(rejected(format!("row count for {name:?}")));
    }
    let (columns, has_rowid) = copy_columns(conn, name)?;
    if has_rowid {
        let rowid = quote_identifier(&columns[0]);
        let maximum: Option<i64> = conn
            .query_row(
                &format!("SELECT MAX({rowid}) FROM main.{table}"),
                [],
                |row| row.get(0),
            )
            .map_err(rejected)?;
        if maximum != checkpoint.last {
            return Err(rejected(format!("row cursor for {name:?}")));
        }
        if let Some(last) = checkpoint.last {
            let prefix: i64 = conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM input.{table} NOT INDEXED WHERE {rowid}<=?1"),
                    [last],
                    |row| row.get(0),
                )
                .map_err(rejected)?;
            let orphan = count(
                conn,
                &format!(
                    "SELECT EXISTS(SELECT 1 FROM main.{table} AS copied NOT INDEXED
                 WHERE NOT EXISTS(SELECT 1 FROM input.{table} AS original
                                  WHERE original.{rowid}=copied.{rowid}))",
                ),
            )?;
            if prefix != actual || orphan != 0 {
                return Err(rejected(format!("copied row identities for {name:?}")));
            }
        }
    } else if checkpoint.last.is_some() {
        return Err(rejected(format!(
            "rowid cursor for WITHOUT ROWID table {name:?}"
        )));
    }
    if checkpoint.completed != 0 || checkpoint.done != 0 {
        let total = count(
            conn,
            &format!("SELECT COUNT(*) FROM input.{table} NOT INDEXED"),
        )?;
        if actual > total || (checkpoint.done == 1 && actual != total) {
            return Err(rejected(format!("completion state for {name:?}")));
        }
    }
    Ok(())
}

fn validate_index(conn: &Connection, checkpoint: &Checkpoint) -> Result<()> {
    let (actual, maximum): (i64, Option<i64>) = conn
        .query_row(
            "SELECT COUNT(*),MAX(id) FROM audio_search_docsize",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(rejected)?;
    if actual != checkpoint.completed || maximum != checkpoint.last {
        return Err(rejected("indexed row count or cursor"));
    }
    if let Some(last) = checkpoint.last {
        let prefix: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM audio_files NOT INDEXED WHERE id<=?1",
                [last],
                |row| row.get(0),
            )
            .map_err(rejected)?;
        let orphan = count(
            conn,
            "SELECT EXISTS(SELECT 1 FROM audio_search_docsize AS indexed
             WHERE NOT EXISTS(SELECT 1 FROM audio_files WHERE id=indexed.id))",
        )?;
        if prefix != actual || orphan != 0 {
            return Err(rejected("indexed row identities"));
        }
    }
    Ok(())
}
