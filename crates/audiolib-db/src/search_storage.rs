// crates/audiolib-db/src/search_storage.rs
//! Plan B projection and schema construction. Business Metadata is always authoritative.
//! Writers register the shared tokenizer; triggers keep business and FTS changes atomic.
use crate::{db_error, schema, sqlite_engine};
use audiolib_core::{Error, Result};
use rusqlite::{Connection, OptionalExtension};

pub(crate) const DERIVED_COLUMNS: &[&str] = &[
    "filename_tokenized",
    "user_tags_tokenized",
    "bwf_description_tokenized",
    "ixml_scene_tokenized",
    "ixml_take_tokenized",
    "user_notes_tokenized",
];

/// SQLite identifiers fold ASCII case; retained spelling is not a new field.
pub(crate) fn is_derived_column(name: &str) -> bool {
    DERIVED_COLUMNS
        .iter()
        .any(|column| column.eq_ignore_ascii_case(name))
}

pub(crate) const FTS_COLUMNS: &str = "filename,filename_tokenized,user_tags,user_tags_tokenized,\
    bwf_description,bwf_description_tokenized,ixml_scene,ixml_scene_tokenized,\
    ixml_take,ixml_take_tokenized,user_notes,user_notes_tokenized";

pub(crate) const CREATE_FTS: &str = "
CREATE VIRTUAL TABLE audio_search USING fts5(
 filename,filename_tokenized,user_tags,user_tags_tokenized,
 bwf_description,bwf_description_tokenized,ixml_scene,ixml_scene_tokenized,
 ixml_take,ixml_take_tokenized,user_notes,user_notes_tokenized,
 content='',contentless_delete=1,detail=full,columnsize=1,
 tokenize='unicode61 remove_diacritics 2');";

/// Same expression list for initial indexing and every incremental replacement.
/// `alias` comes exclusively from internal SQL builders, never from a request.
fn projection(alias: &str) -> String {
    format!(
        "{alias}.filename,audiolib_tokenize({alias}.filename),\
         {alias}.user_tags,audiolib_tokenize({alias}.user_tags),\
         {alias}.bwf_description,audiolib_tokenize(audiolib_source_text({alias}.bwf_description,{alias}.metadata_json)),\
         {alias}.ixml_scene,audiolib_tokenize({alias}.ixml_scene),\
         {alias}.ixml_take,audiolib_tokenize({alias}.ixml_take),\
         {alias}.user_notes,audiolib_tokenize({alias}.user_notes)"
    )
}

pub(crate) fn triggers() -> String {
    let values = projection("new");
    format!(
        "CREATE TRIGGER audio_files_ai AFTER INSERT ON audio_files BEGIN
           INSERT INTO audio_search(rowid,{FTS_COLUMNS}) VALUES(new.id,{values});
         END;
         CREATE TRIGGER audio_files_au AFTER UPDATE OF
           id,filename,user_tags,bwf_description,metadata_json,ixml_scene,ixml_take,user_notes
           ON audio_files BEGIN
           DELETE FROM audio_search WHERE rowid=old.id;
           INSERT INTO audio_search(rowid,{FTS_COLUMNS}) VALUES(new.id,{values});
         END;
         CREATE TRIGGER audio_files_ad AFTER DELETE ON audio_files BEGIN
           DELETE FROM audio_search WHERE rowid=old.id;
         END;"
    )
}

pub(crate) fn quote_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

pub(crate) fn transaction_required(conn: &Connection) -> Result<()> {
    if conn.is_autocommit() {
        return Err(Error::Database(
            "Search storage changes require the caller's transaction".into(),
        ));
    }
    Ok(())
}

/// Install only after copied business rows and complete FTS validation. Existing
/// SQL writers without the tokenizer cannot commit text edits with an empty index.
pub(crate) fn install_triggers(conn: &Connection) -> Result<()> {
    transaction_required(conn)?;
    conn.execute_batch(&triggers()).map_err(db_error)
}

pub(crate) fn create_index(conn: &Connection) -> Result<()> {
    transaction_required(conn)?;
    sqlite_engine::require_plan_b()?;
    conn.execute_batch(CREATE_FTS).map_err(db_error)
}

/// Insert one committed migration batch. Nullable cursor includes i64::MIN;
/// exclusive subsequent bounds avoid overflow and duplicate boundary records.
pub(crate) fn index_batch(
    conn: &Connection,
    after: Option<i64>,
    limit: u32,
) -> Result<(Option<i64>, u64)> {
    transaction_required(conn)?;
    if limit == 0 {
        return Err(Error::Validation(
            "Index batch size must be positive".into(),
        ));
    }
    let lower = if after.is_some() { "id>?" } else { "id>=?" };
    let high: Option<i64> = conn
        .query_row(
            &format!(
                "SELECT MAX(id) FROM (SELECT id FROM audio_files WHERE {lower} ORDER BY id LIMIT ?)"
            ),
            rusqlite::params![after.unwrap_or(i64::MIN), limit],
            |row| row.get(0),
        )
        .map_err(db_error)?;
    let Some(high) = high else {
        return Ok((after, 0));
    };
    let values = projection("af");
    let lower = if after.is_some() {
        "af.id>?"
    } else {
        "af.id>=?"
    };
    let rows = conn
        .execute(
            &format!(
                "INSERT INTO audio_search(rowid,{FTS_COLUMNS})
                 SELECT af.id,{values} FROM audio_files af WHERE {lower} AND af.id<=? ORDER BY af.id"
            ),
            rusqlite::params![after.unwrap_or(i64::MIN), high],
        )
        .map_err(db_error)?;
    Ok((Some(high), rows as u64))
}

/// An owned staging blueprint preserves unknown business columns and constraints.
/// SQLite itself rewrites the schema; no regex parser rewrites user SQL.
#[derive(Debug)]
pub(crate) struct Blueprint {
    pub tables: Vec<(String, String)>,
    pub objects: Vec<(String, String)>,
}

/// Execute one stored CREATE statement, never a batch of SQL obtained from a file.
pub(crate) fn execute_schema(conn: &Connection, sql: &str) -> Result<()> {
    if !sql
        .trim_start()
        .get(..6)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("CREATE"))
    {
        return Err(Error::Validation(
            "Unsupported stored schema statement".into(),
        ));
    }
    conn.execute(sql, []).map(drop).map_err(db_error)
}

#[derive(Debug)]
enum TriggerToken {
    Word(String),
    Quoted(String),
    Punctuation(char),
}

impl TriggerToken {
    fn is_keyword(&self, keyword: &str) -> bool {
        matches!(self, Self::Word(word) if word.eq_ignore_ascii_case(keyword))
    }

    fn identifier(&self) -> Option<&str> {
        match self {
            Self::Word(value) | Self::Quoted(value) => Some(value),
            Self::Punctuation(_) => None,
        }
    }
}

/// Read SQLite's CREATE TRIGGER header, rather than searching its body or SQL
/// strings. SQLite silently accepts unknown names in UPDATE OF, so preparing a
/// trigger or EXPLAIN UPDATE cannot prove those dependencies are still valid.
fn trigger_update_columns(sql: &str) -> Result<Vec<String>> {
    let invalid = || Error::Validation("Unsupported stored trigger header".into());
    let mut input = sql.chars().peekable();
    let mut tokens = Vec::new();
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
                if input.next().ok_or_else(invalid)? == '*' && input.peek() == Some(&'/') {
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
                let next = input.next().ok_or_else(invalid)?;
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
            tokens.push(TriggerToken::Quoted(value));
        } else if ch.is_alphanumeric() || matches!(ch, '_' | '$') || !ch.is_ascii() {
            let mut word = ch.to_string();
            while input.peek().is_some_and(|next| {
                next.is_alphanumeric() || matches!(next, '_' | '$') || !next.is_ascii()
            }) {
                word.push(input.next().expect("peeked character"));
            }
            tokens.push(TriggerToken::Word(word));
        } else {
            tokens.push(TriggerToken::Punctuation(ch));
        }
    }
    let mut cursor = 0;
    let take = |cursor: &mut usize, keyword: &str| {
        if tokens
            .get(*cursor)
            .is_some_and(|token| token.is_keyword(keyword))
        {
            *cursor += 1;
            true
        } else {
            false
        }
    };
    if !take(&mut cursor, "CREATE") {
        return Err(invalid());
    }
    if !take(&mut cursor, "TEMP") {
        take(&mut cursor, "TEMPORARY");
    }
    if !take(&mut cursor, "TRIGGER") {
        return Err(invalid());
    }
    if take(&mut cursor, "IF") && !(take(&mut cursor, "NOT") && take(&mut cursor, "EXISTS")) {
        return Err(invalid());
    }
    tokens
        .get(cursor)
        .and_then(TriggerToken::identifier)
        .ok_or_else(invalid)?;
    cursor += 1;
    if matches!(tokens.get(cursor), Some(TriggerToken::Punctuation('.'))) {
        cursor += 1;
        tokens
            .get(cursor)
            .and_then(TriggerToken::identifier)
            .ok_or_else(invalid)?;
        cursor += 1;
    }
    if !take(&mut cursor, "BEFORE")
        && !take(&mut cursor, "AFTER")
        && take(&mut cursor, "INSTEAD")
        && !take(&mut cursor, "OF")
    {
        return Err(invalid());
    }
    if !take(&mut cursor, "UPDATE") || !take(&mut cursor, "OF") {
        return Ok(Vec::new());
    }
    let mut columns = Vec::new();
    loop {
        let column = tokens
            .get(cursor)
            .and_then(TriggerToken::identifier)
            .ok_or_else(invalid)?;
        columns.push(column.to_owned());
        cursor += 1;
        if take(&mut cursor, "ON") {
            return Ok(columns);
        }
        if !matches!(tokens.get(cursor), Some(TriggerToken::Punctuation(','))) {
            return Err(invalid());
        }
        cursor += 1;
    }
}

fn check_trigger_column_dependencies(
    conn: &Connection,
    changes: &[crate::migration::SchemaChange],
) -> Result<()> {
    let mut statement = conn
        .prepare(
            "SELECT name,tbl_name,sql FROM sqlite_schema WHERE type='trigger' AND sql IS NOT NULL",
        )
        .map_err(db_error)?;
    let triggers = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(db_error)?;
    for trigger in triggers {
        let (name, owner, sql) = trigger.map_err(db_error)?;
        let columns = trigger_update_columns(&sql)?;
        for change in changes {
            if let crate::migration::SchemaChange::DropDerivedColumn { table, column }
            | crate::migration::SchemaChange::RetireColumn { table, column } = change
                && owner.eq_ignore_ascii_case(table)
                && columns.iter().any(|name| name.eq_ignore_ascii_case(column))
            {
                return Err(Error::Validation(format!(
                    "Trigger {name} still depends on removed field {table}.{column} through UPDATE OF"
                )));
            }
        }
    }
    Ok(())
}

/// Transform only an empty in-memory schema. A view/index/constraint that depends
/// on a removed column makes SQLite reject the plan before any source mutation.
#[cfg(test)]
pub(crate) fn blueprint(source: &Connection) -> Result<Blueprint> {
    let changes = DERIVED_COLUMNS
        .iter()
        .map(|column| crate::migration::SchemaChange::DropDerivedColumn {
            table: "audio_files".into(),
            column: (*column).into(),
        })
        .collect::<Vec<_>>();
    blueprint_with(source, &changes)
}

pub(crate) fn blueprint_with(
    source: &Connection,
    changes: &[crate::migration::SchemaChange],
) -> Result<Blueprint> {
    if schema::pending(source)? {
        return Err(Error::Busy(
            "Complete or recover the legacy search migration before preparing storage B".into(),
        ));
    }
    schema::definition::check_repair_source(source)?;
    if schema::feature_version(source, "album_entities")? != Some(1) {
        let collision:bool=source.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name COLLATE NOCASE IN ('albums','audiolib_album_id_sequence','audiolib_album_id_high_water','audiolib_album_audio_insert','audiolib_album_audio_update','audiolib_album_folder_delete','idx_albums_folder','idx_audio_album_members'))",[],|row|row.get(0)).map_err(db_error)?;
        let old_index: Option<String> = source
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE name='idx_audio_visible_album' COLLATE NOCASE",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_error)?;
        if collision
            || old_index.is_some_and(|sql| {
                !schema::definition::equivalent(
                    &sql,
                    "CREATE INDEX idx_audio_visible_album ON audio_files(album,id) WHERE missing=0",
                )
            })
        {
            return Err(Error::Validation(
                "Shared album object name conflicts with an unknown extension".into(),
            ));
        }
    }
    let unsupported_virtual: bool = source
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_list WHERE schema='main'
             AND type='virtual' AND name<>'audio_search' COLLATE NOCASE)",
            [],
            |row| row.get(0),
        )
        .map_err(db_error)?;
    if unsupported_virtual {
        return Err(Error::Validation(
            "A custom virtual table requires a supported migration adapter".into(),
        ));
    }
    let mut statement = source
        .prepare(
            "SELECT s.name,s.sql FROM sqlite_schema s JOIN pragma_table_list t ON t.name=s.name
             AND t.schema='main' WHERE s.type='table' AND t.type='table'
             AND s.name NOT GLOB 'sqlite_*' ORDER BY s.name",
        )
        .map_err(db_error)?;
    let tables = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(db_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(db_error)?;
    let mut statement = source
        .prepare(
            "SELECT name,sql FROM sqlite_schema WHERE type IN ('index','view','trigger')
             AND sql IS NOT NULL AND name COLLATE NOCASE NOT IN ('audio_files_ai','audio_files_au','audio_files_ad')
             ORDER BY CASE type WHEN 'index' THEN 0 WHEN 'view' THEN 1 ELSE 2 END,name",
        )
        .map_err(db_error)?;
    let objects = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(db_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(db_error)?;
    let memory = Connection::open_in_memory().map_err(db_error)?;
    schema::register_tokenizer(&memory, crate::load_tokenizer())?;
    memory
        .execute_batch("PRAGMA foreign_keys=OFF;")
        .map_err(db_error)?;
    for (_, sql) in &tables {
        execute_schema(&memory, sql)?;
    }
    for (name, sql) in &objects {
        if name == "idx_audio_visible_album"
            && schema::definition::equivalent(
                sql,
                "CREATE INDEX idx_audio_visible_album ON audio_files(album,id) WHERE missing=0",
            )
        {
            continue;
        }
        if [
            "audiolib_album_id_high_water",
            "audiolib_album_audio_insert",
            "audiolib_album_audio_update",
            "audiolib_album_folder_delete",
        ]
        .contains(&name.as_str())
        {
            continue;
        }
        if !schema::definition::needs_owned_repair(name, sql) {
            execute_schema(&memory, sql)?;
        }
    }
    schema::definition::repair_owned(&memory)?;
    check_trigger_column_dependencies(&memory, changes)?;
    for change in changes {
        use crate::migration::SchemaChange;
        let sql = match change {
            SchemaChange::CreateAlbumEntities => {
                memory
                    .execute_batch(crate::albums::SCHEMA)
                    .map_err(db_error)?;
                continue;
            }
            SchemaChange::AddColumn {
                table,
                column,
                definition,
            } => {
                let exists:bool=memory.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_xinfo(?1) WHERE name=?2 COLLATE NOCASE)",rusqlite::params![table,column],|row|row.get(0)).map_err(db_error)?;
                if exists {
                    continue;
                }
                format!(
                    "ALTER TABLE {} ADD COLUMN {} {definition}",
                    quote_identifier(table),
                    quote_identifier(column)
                )
            }
            SchemaChange::DropDerivedColumn { table, column }
            | SchemaChange::RetireColumn { table, column } => {
                let exists: bool = memory
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM pragma_table_xinfo(?1) WHERE name=?2 COLLATE NOCASE)",
                        rusqlite::params![table, column],
                        |row| row.get(0),
                    )
                    .map_err(db_error)?;
                if !exists {
                    continue;
                }
                format!(
                    "ALTER TABLE {} DROP COLUMN {}",
                    quote_identifier(table),
                    quote_identifier(column)
                )
            }
            SchemaChange::RetireTable { table } => {
                format!("DROP TABLE IF EXISTS {}", quote_identifier(table))
            }
        };
        memory.execute(&sql, []).map_err(|error| {
            Error::Validation(format!(
                "Schema evolution conflicts with an existing dependency: {error}",
            ))
        })?;
    }
    // DROP TABLE does not reject orphan views/triggers/FKs by itself. Compile all
    // dependent operations without running them, and inspect every FK explicitly.
    for (name, _) in &tables {
        let exists: bool = memory
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1 COLLATE NOCASE)",
                [name],
                |row| row.get(0),
            )
            .map_err(db_error)?;
        if !exists {
            continue;
        }
        let parents = memory
            .prepare("SELECT DISTINCT \"table\" FROM pragma_foreign_key_list(?1)")
            .map_err(db_error)?
            .query_map([name], |row| row.get::<_, String>(0))
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?;
        for parent in parents {
            let exists: bool = memory
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1 COLLATE NOCASE)",
                    [&parent],
                    |row| row.get(0),
                )
                .map_err(db_error)?;
            if !exists {
                return Err(Error::Validation(format!(
                    "Table {name} still references retired table {parent}"
                )));
            }
        }
        let quote = quote_identifier(name);
        for sql in [
            format!("EXPLAIN INSERT INTO {quote} DEFAULT VALUES"),
            format!("EXPLAIN DELETE FROM {quote}"),
        ] {
            memory.prepare(&sql).map_err(db_error)?;
        }
        let setters = memory
            .prepare("SELECT name FROM pragma_table_xinfo(?1) WHERE hidden=0")
            .map_err(db_error)?
            .query_map([name], |row| row.get::<_, String>(0))
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?
            .iter()
            .map(|column| {
                let quoted = quote_identifier(column);
                format!("{quoted}={quoted}")
            })
            .collect::<Vec<_>>()
            .join(",");
        memory
            .prepare(&format!("EXPLAIN UPDATE {quote} SET {setters}"))
            .map_err(db_error)?;
    }
    let views = memory
        .prepare("SELECT name FROM sqlite_schema WHERE type='view'")
        .map_err(db_error)?
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(db_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(db_error)?;
    for view in views {
        memory
            .prepare(&format!(
                "SELECT * FROM {} LIMIT 0",
                quote_identifier(&view)
            ))
            .map_err(db_error)?;
    }
    let tables_after = memory.prepare("SELECT name,sql FROM sqlite_schema WHERE type='table' AND name NOT GLOB 'sqlite_*' ORDER BY name")
        .map_err(db_error)?.query_map([], |row| Ok((row.get(0)?,row.get(1)?))).map_err(db_error)?
        .collect::<rusqlite::Result<_>>().map_err(db_error)?;
    let objects_after = memory.prepare("SELECT name,sql FROM sqlite_schema WHERE type IN ('index','view','trigger') AND sql IS NOT NULL ORDER BY CASE type WHEN 'index' THEN 0 WHEN 'view' THEN 1 ELSE 2 END,name")
        .map_err(db_error)?.query_map([], |row| Ok((row.get(0)?,row.get(1)?))).map_err(db_error)?
        .collect::<rusqlite::Result<_>>().map_err(db_error)?;
    Ok(Blueprint {
        tables: tables_after,
        objects: objects_after,
    })
}

pub(crate) fn columns(conn: &Connection) -> Result<Vec<String>> {
    conn.prepare("SELECT name FROM pragma_table_xinfo('audio_files') ORDER BY cid")
        .map_err(db_error)?
        .query_map([], |row| row.get(0))
        .map_err(db_error)?
        .collect::<rusqlite::Result<_>>()
        .map_err(db_error)
}

/// Structural validation rejects misleading format markers and lost synchronization.
pub(crate) fn structure_matches(conn: &Connection) -> Result<bool> {
    if columns(conn)?.iter().any(|name| is_derived_column(name)) {
        return Ok(false);
    }
    let expected = Connection::open_in_memory().map_err(db_error)?;
    expected
        .execute_batch(
            "CREATE TABLE audio_files(id INTEGER PRIMARY KEY,filename,user_tags,bwf_description,
         metadata_json,ixml_scene,ixml_take,user_notes);",
        )
        .map_err(db_error)?;
    expected.execute_batch(CREATE_FTS).map_err(db_error)?;
    expected.execute_batch(&triggers()).map_err(db_error)?;
    for name in [
        "audio_search",
        "audio_files_ai",
        "audio_files_au",
        "audio_files_ad",
    ] {
        let (kind, table, canonical): (String, String, String) = expected
            .query_row(
                "SELECT type,tbl_name,sql FROM sqlite_schema WHERE name=?1",
                [name],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(db_error)?;
        if !schema::definition::matches_sql(conn, name, &kind, &table, &canonical)? {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    fn fixture() -> Connection {
        let mut source = Connection::open_in_memory().unwrap();
        schema::register_tokenizer(&source, crate::load_tokenizer()).unwrap();
        schema::migrate(&mut source).unwrap();
        source
    }

    #[test]
    fn blueprint_keeps_unknown_columns_constraints_and_source_schema() {
        let source = fixture();
        source.execute_batch(
            "ALTER TABLE audio_files ADD COLUMN extension_value TEXT CHECK(length(extension_value)<100);
             CREATE INDEX custom_extension_index ON audio_files(extension_value);
             CREATE VIEW custom_view AS SELECT id,extension_value FROM audio_files;",
        ).unwrap();
        let before = columns(&source).unwrap();
        let plan = blueprint(&source).unwrap();
        assert_eq!(before, columns(&source).unwrap());
        let target = Connection::open_in_memory().unwrap();
        target.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();
        for (_, sql) in plan.tables.iter().chain(plan.objects.iter()) {
            target.execute_batch(sql).unwrap();
        }
        let after = columns(&target).unwrap();
        assert!(after.contains(&"extension_value".into()));
        assert!(
            !after
                .iter()
                .any(|name| DERIVED_COLUMNS.contains(&name.as_str()))
        );
        assert!(target.execute(
            "INSERT INTO audio_files(file_path,filename,extension_value) VALUES('x','x',?1)",
            ["x".repeat(100)]
        ).is_err());
    }

    #[test]
    fn custom_dependency_on_removed_text_fails_without_changing_source() {
        let source = fixture();
        source
            .execute_batch(
                "CREATE VIEW custom_tokens AS SELECT filename_tokenized FROM audio_files;",
            )
            .unwrap();
        assert!(blueprint(&source).is_err());
        assert!(
            columns(&source)
                .unwrap()
                .contains(&"filename_tokenized".into())
        );
    }

    #[test]
    fn projection_triggers_are_atomic_and_index_metadata_not_only_bwf() {
        let source = fixture();
        let plan = blueprint(&source).unwrap();
        let mut target = Connection::open_in_memory().unwrap();
        schema::register_tokenizer(&target, crate::load_tokenizer()).unwrap();
        for (_, sql) in &plan.tables {
            target.execute_batch(sql).unwrap();
        }
        {
            let tx = target.transaction().unwrap();
            create_index(&tx).unwrap();
            install_triggers(&tx).unwrap();
            tx.commit().unwrap();
        }
        assert!(structure_matches(&target).unwrap());
        for id in [i64::MIN, -7, i64::MAX] {
            target.execute(
                "INSERT INTO audio_files(id,file_path,filename,user_notes,metadata_json)
                 VALUES(?1,?2,'metal.wav','wood',?3)",
                params![id, id.to_string(), r#"{"wav_metadata":{"ixml":{"SCENE":"secretunique"}},"private":"ignoredprivateunique"}"#],
            ).unwrap();
        }
        let count = |query: &str| -> i64 {
            let compiled =
                crate::search_text::compile_query(&crate::load_tokenizer(), query).unwrap();
            target
                .query_row(
                    "SELECT COUNT(*) FROM audio_search WHERE audio_search MATCH ?1",
                    [compiled],
                    |r| r.get(0),
                )
                .unwrap()
        };
        assert_eq!(count("secretunique"), 3);
        assert_eq!(count("ignoredprivateunique"), 0);
        target
            .execute_batch("BEGIN; UPDATE audio_files SET user_notes='changedunique'; ROLLBACK;")
            .unwrap();
        assert_eq!(count("changedunique"), 0);
        target.execute_batch("UPDATE audio_files SET user_notes='changedunique'; DELETE FROM audio_files WHERE id=-7;").unwrap();
        assert_eq!(count("changedunique"), 2);
        assert_eq!(count("wood"), 0);
        // No caller-provided tokenized text can get out of sync with Metadata.
        assert!(
            !columns(&target)
                .unwrap()
                .contains(&"filename_tokenized".into())
        );
    }

    #[test]
    fn index_batches_include_extreme_ids_once_and_require_a_transaction() {
        let source = fixture();
        let plan = blueprint(&source).unwrap();
        let mut target = Connection::open_in_memory().unwrap();
        schema::register_tokenizer(&target, crate::load_tokenizer()).unwrap();
        for (_, sql) in &plan.tables {
            target.execute_batch(sql).unwrap();
        }
        for id in [i64::MIN, -7, 1, i64::MAX] {
            target
                .execute(
                    "INSERT INTO audio_files(id,file_path,filename) VALUES(?1,?2,'metal.wav')",
                    params![id, id.to_string()],
                )
                .unwrap();
        }
        assert!(create_index(&target).is_err());
        let tx = target.transaction().unwrap();
        create_index(&tx).unwrap();
        let (a, rows) = index_batch(&tx, None, 2).unwrap();
        assert_eq!((a, rows), (Some(-7), 2));
        let (b, rows) = index_batch(&tx, a, 2).unwrap();
        assert_eq!((b, rows), (Some(i64::MAX), 2));
        assert_eq!(index_batch(&tx, b, 2).unwrap(), (b, 0));
        tx.commit().unwrap();
    }
}
