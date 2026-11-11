// crates/audiolib-db/src/schema/definition.rs
//! Conservative schema comparison and repair of explicitly owned derived objects.
//! SQL is never rewritten from these comparison tokens; repairs execute only constants.
use crate::{db_error, search_storage};
use audiolib_core::{Error, Result};
use rusqlite::Connection;

struct Definition {
    name: &'static str,
    kind: &'static str,
    table: &'static str,
    sql: &'static str,
}

const SEQUENCE: Definition = Definition {
    name: "audiolib_audio_id_sequence",
    kind: "table",
    table: "audiolib_audio_id_sequence",
    sql: "CREATE TABLE audiolib_audio_id_sequence (
        singleton INTEGER PRIMARY KEY CHECK(singleton=1),
        high_water INTEGER NOT NULL CHECK(high_water>=0))",
};

// Only these names are reserved for repair. Optional accelerators and extension
// indexes/triggers retain their source definitions and existing fallback behavior.
const DERIVED: &[Definition] = &[
    Definition { name: "audiolib_audio_id_high_water", kind: "trigger", table: "audio_files",
        sql: "CREATE TRIGGER audiolib_audio_id_high_water AFTER INSERT ON audio_files BEGIN
          UPDATE audiolib_audio_id_sequence SET high_water=MAX(high_water,new.id) WHERE singleton=1;
         END" },
    Definition { name: "idx_audio_visible_name", kind: "index", table: "audio_files",
        sql: "CREATE INDEX idx_audio_visible_name ON audio_files(filename COLLATE NOCASE,id) WHERE missing=0" },
    Definition { name: "idx_playlist_items_audio_file", kind: "index", table: "playlist_items",
        sql: "CREATE INDEX idx_playlist_items_audio_file ON playlist_items(audio_file_id)" },
    Definition { name: "idx_project_references_audio_file", kind: "index", table: "project_references",
        sql: "CREATE INDEX idx_project_references_audio_file ON project_references(audio_file_id)" },
    Definition { name: "idx_waveform_cache_audio_file", kind: "index", table: "waveform_cache",
        sql: "CREATE INDEX idx_waveform_cache_audio_file ON waveform_cache(audio_file_id)" },
    Definition { name: "idx_closure_descendant", kind: "index", table: "folder_closure",
        sql: "CREATE INDEX idx_closure_descendant ON folder_closure(descendant_id)" },
    Definition { name: "idx_audio_files_not_present", kind: "index", table: "audio_files",
        sql: "CREATE INDEX idx_audio_files_not_present ON audio_files(id) WHERE missing IS NOT 0" },
    Definition { name: "idx_audio_search_facets", kind: "index", table: "audio_files",
        sql: "CREATE INDEX idx_audio_search_facets ON audio_files(id,missing,folder_id)" },
    Definition { name: "idx_audio_visible_tagged_folders", kind: "index", table: "audio_files",
        sql: "CREATE INDEX idx_audio_visible_tagged_folders ON audio_files(folder_id,id,user_tags,source_tags_text)
          WHERE missing=0 AND (COALESCE(trim(user_tags),'')<>'' OR source_tags_text<>'')" },
    // The album index is created only where the v7 album column already exists (new
    // libraries and the post-backfill step), so it is not a required derived object.
];

/// Compare SQLite token boundaries, preserving literal bytes and identifier case
/// rules. This is deliberately conservative, not a claim of general SQL equivalence.
pub(crate) fn equivalent(left: &str, right: &str) -> bool {
    match (tokens(left), tokens(right)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

fn tokens(sql: &str) -> Option<Vec<String>> {
    let mut input = sql.chars().peekable();
    let mut result = Vec::new();
    while let Some(ch) = input.next() {
        if ch.is_ascii_whitespace() {
            continue;
        }
        if ch == '-' && input.peek() == Some(&'-') {
            for ch in input.by_ref() {
                if ch == '\n' {
                    break;
                }
            }
            continue;
        }
        if ch == '/' && input.peek() == Some(&'*') {
            input.next();
            loop {
                if input.next()? == '*' && input.peek() == Some(&'/') {
                    input.next();
                    break;
                }
            }
            continue;
        }
        if matches!(ch, '\'' | '"' | '`' | '[') {
            let end = if ch == '[' { ']' } else { ch };
            let mut value = String::new();
            loop {
                let next = input.next()?;
                if next == end {
                    if ch != '[' && input.peek() == Some(&end) {
                        input.next();
                        value.push(end);
                        continue;
                    }
                    break;
                }
                value.push(next);
            }
            result.push(if ch == '\'' {
                format!("literal:{value}")
            } else {
                format!("word:{}", value.to_ascii_lowercase())
            });
        } else if ch.is_alphanumeric() || ch == '_' || !ch.is_ascii() {
            let mut word = ch.to_string();
            while input.peek().is_some_and(|next| {
                next.is_alphanumeric() || matches!(next, '_' | '$') || !next.is_ascii()
            }) {
                word.push(input.next()?);
            }
            result.push(format!("word:{}", word.to_ascii_lowercase()));
        } else {
            result.push(ch.to_string());
        }
    }
    while result.last().is_some_and(|token| token == ";") {
        result.pop();
    }
    Some(result)
}

fn observed(conn: &Connection, name: &str) -> Result<Vec<(String, String, Option<String>)>> {
    conn.prepare("SELECT type,tbl_name,sql FROM sqlite_schema WHERE name=?1 COLLATE NOCASE")
        .map_err(db_error)?
        .query_map([name], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .map_err(db_error)?
        .collect::<rusqlite::Result<_>>()
        .map_err(db_error)
}

fn matches(conn: &Connection, expected: &Definition) -> Result<bool> {
    matches_sql(
        conn,
        expected.name,
        expected.kind,
        expected.table,
        expected.sql,
    )
}

pub(crate) fn matches_sql(
    conn: &Connection,
    name: &str,
    kind: &str,
    table: &str,
    sql: &str,
) -> Result<bool> {
    Ok(
        matches!(observed(conn, name)?.as_slice(), [(actual_kind,actual_table,Some(actual_sql))]
        if actual_kind == kind && actual_table.eq_ignore_ascii_case(table)
            && equivalent(actual_sql, sql)),
    )
}

pub(crate) fn required_match(conn: &Connection) -> Result<bool> {
    if !matches(conn, &SEQUENCE)? {
        return Ok(false);
    }
    for expected in DERIVED {
        if !matches(conn, expected)? {
            return Ok(false);
        }
    }
    conn.query_row(
        "SELECT COUNT(*)=1 AND COALESCE(MIN(singleton=1 AND typeof(high_water)='integer'
         AND high_water>=MAX(0,COALESCE((SELECT MAX(id) FROM audio_files),0))),0)
         FROM audiolib_audio_id_sequence",
        [],
        |row| row.get(0),
    )
    .map_err(db_error)
}

/// Refuse to invent a lost issued-ID watermark or replace a reserved name which
/// actually belongs to a different object/table. No writes occur during this check.
fn check_structure(conn: &Connection) -> Result<()> {
    if !matches(conn, &SEQUENCE)? {
        return Err(Error::Validation("Audio ID sequence structure is missing or unsupported; restore verified identity metadata before migration".into()));
    }
    for expected in DERIVED {
        let actual = observed(conn, expected.name)?;
        if !actual.is_empty()
            && !matches!(actual.as_slice(), [(kind, table, Some(_))]
            if kind == expected.kind && table.eq_ignore_ascii_case(expected.table))
        {
            return Err(Error::Validation(format!(
                "Reserved schema object {} belongs to an unexpected type or table; automatic repair is refused",
                expected.name
            )));
        }
    }
    Ok(())
}

pub(crate) fn check_repair_source(conn: &Connection) -> Result<()> {
    check_structure(conn)?;
    let valid: bool = conn
        .query_row(
            "SELECT COUNT(*)=1 AND COALESCE(MIN(singleton=1 AND typeof(high_water)='integer'
         AND high_water>=MAX(0,COALESCE((SELECT MAX(id) FROM audio_files),0))),0)
         FROM audiolib_audio_id_sequence",
            [],
            |row| row.get(0),
        )
        .map_err(db_error)?;
    if !valid {
        return Err(Error::Validation("Audio ID high-water evidence is invalid; restore verified identity metadata before migration".into()));
    }
    Ok(())
}

pub(crate) fn needs_owned_repair(name: &str, sql: &str) -> bool {
    DERIVED
        .iter()
        .any(|expected| expected.name.eq_ignore_ascii_case(name) && !equivalent(sql, expected.sql))
}

/// Caller owns this in-memory schema or transaction; source libraries are never
/// passed here by the planner. Business tables and stored watermarks are untouched.
pub(crate) fn repair_owned(conn: &Connection) -> Result<()> {
    check_structure(conn)?;
    for expected in DERIVED {
        if matches(conn, expected)? {
            continue;
        }
        conn.execute(
            &format!(
                "DROP {} IF EXISTS {}",
                expected.kind,
                search_storage::quote_identifier(expected.name)
            ),
            [],
        )
        .map_err(db_error)?;
        search_storage::execute_schema(conn, expected.sql)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comparison_preserves_literals_and_token_boundaries() {
        assert!(equivalent(
            "CREATE TABLE \"audio_search\" (id INTEGER);",
            "create /*comment*/ table audio_search(id integer)"
        ));
        assert!(!equivalent("CHECK(x='Tag')", "CHECK(x='tag')"));
        assert!(!equivalent("CHECK(x='a b')", "CHECK(x='ab')"));
        assert!(!equivalent("CHECK(x='')", "CHECK(x=' ')"));
        assert!(!equivalent("SELECT a b", "SELECT ab"));
        assert!(!equivalent("CHECK(x='a--b')", "CHECK(x='a')"));
        assert!(!equivalent("SELECT 'unterminated", "SELECT 'unterminated"));
        assert!(!equivalent(
            "SELECT /*unterminated",
            "SELECT /*unterminated"
        ));
        assert!(equivalent("CHECK(x='a''b')", "check (x = 'a''b'); --end"));
    }
}
