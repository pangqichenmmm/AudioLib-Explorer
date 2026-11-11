// crates/audiolib-db/src/vendor_import/automatic.rs
//! Source-owned supplementary Metadata from successfully scanned WAVs in the
//! table's own subtree or, for a `{stem}--Docs` / `{stem}--Sounds*` table
//! directory, the subtrees of its same-stem sibling pack folders.
//! Parsing is isolated; each durable batch replaces only this source's events.
mod rooted;

use super::*;
use audiolib_core::server_upgrade::VENDOR_IMPORT_MAX_FILE_BYTES;
use audiolib_core::vendor_import::VendorScanSummary;

const BATCH: i64 = 128;

struct PreviousEvent {
    cursor: i64,
    audio: i64,
    field: String,
    value: String,
    source: String,
    sheet: String,
    row: i64,
    language: String,
}

fn checkpoint(
    summary: &VendorScanSummary,
    callback: &mut impl FnMut(&VendorScanSummary) -> bool,
) -> Result<()> {
    if callback(summary) {
        Ok(())
    } else {
        Err(Error::Cancelled)
    }
}

/// An explicit path is not permission to fall back to a coincident basename.
fn sibling_name(value: &str) -> Option<String> {
    let value = value.trim().replace('\\', "/");
    if value.starts_with('/') || value.contains(':') {
        return None;
    }
    let value = value.strip_prefix("./").unwrap_or(&value);
    if value.is_empty() || value == "." || value == ".." || value.contains('/') {
        return None;
    }
    Some(value.to_owned())
}

fn identity(sheet: &VendorSheetPreview, cells: &[String]) -> Option<String> {
    let cell = |header: &Option<String>| {
        header
            .as_ref()
            .and_then(|header| sheet.headers.iter().position(|value| value == header))
            .and_then(|index| cells.get(index))
            .map(String::as_str)
            .filter(|value| !value.trim().is_empty())
    };
    let filename = cell(&sheet.mapping.filename);
    let path = cell(&sheet.mapping.relative_path);
    let path = match path {
        Some(value) => Some(sibling_name(value)?),
        None => None,
    };
    let filename = match filename {
        Some(value) => Some(sibling_name(value)?),
        None => None,
    };
    match (filename, path) {
        (Some(left), Some(right)) if matching::fold(&left) != matching::fold(&right) => None,
        (Some(name), _) | (_, Some(name)) => Some(name),
        _ => None,
    }
}

/// The case-folded pack stem of a `{stem}--Docs` or `{stem}--Sounds*` folder.
fn pack_stem(name: &str) -> Option<String> {
    let lower = name.to_lowercase();
    let stem = match lower.strip_suffix("--docs") {
        Some(stem) => stem,
        None => &lower[..lower.rfind("--sounds")?],
    };
    (!stem.is_empty()).then(|| stem.to_owned())
}

fn mtime(metadata: &fs::Metadata) -> Result<i64> {
    i64::try_from(
        metadata
            .modified()?
            .duration_since(UNIX_EPOCH)
            .map_err(invalid)?
            .as_nanos(),
    )
    .map_err(|_| invalid("Source timestamp exceeds SQLite range"))
}

/// Sorted streaming fingerprints avoid retaining all contributions for one WAV.
fn event_hash(conn: &Connection, sql: &str, id: i64, import: &str) -> Result<String> {
    let mut query = conn.prepare_cached(sql).map_err(db_error)?;
    let mut rows = query.query(params![id, import]).map_err(db_error)?;
    let mut hash = Sha256::new();
    while let Some(row) = rows.next().map_err(db_error)? {
        let event = (
            row.get::<_, String>(0).map_err(db_error)?,
            row.get::<_, String>(1).map_err(db_error)?,
            row.get::<_, String>(2).map_err(db_error)?,
            row.get::<_, String>(3).map_err(db_error)?,
            row.get::<_, i64>(4).map_err(db_error)?,
            row.get::<_, String>(5).map_err(db_error)?,
        );
        let encoded = json(&event)?;
        hash.update((encoded.len() as u64).to_le_bytes());
        hash.update(encoded.as_bytes());
    }
    Ok(format!("{:x}", hash.finalize()))
}

impl SqliteProvider {
    /// Refresh one table without taking another library lease. ScanService owns
    /// that lease and invokes this after its audio workers have joined. Counts
    /// are cumulative within this table and published only outside DB guards.
    pub(crate) fn scan_vendor_table(
        &self,
        path: &Path,
        root: &Path,
        session_id: i64,
        callback: &mut impl FnMut(&VendorScanSummary) -> bool,
    ) -> Result<VendorScanSummary> {
        let mut summary = VendorScanSummary {
            tables: 1,
            ..Default::default()
        };
        checkpoint(&summary, callback)?;
        if self.read_only {
            return Err(invalid("Automatic Metadata requires a writable library"));
        }
        let name = path
            .file_name()
            .ok_or_else(|| invalid("Missing table filename"))?
            .to_string_lossy();
        if is_skipped_name(&name) {
            return Err(invalid("System/lock table is skipped"));
        }
        let mut source = rooted::open(root, path)?;
        let parent = source
            .path
            .parent()
            .ok_or_else(|| invalid("Table has no parent"))?
            .to_path_buf();
        let original = source.file.metadata()?;
        if original.len() == 0 || original.len() > VENDOR_IMPORT_MAX_FILE_BYTES {
            return Err(invalid(
                "Automatic table must contain 1 byte through 50 MiB",
            ));
        }
        let source_identity =
            crate::normalize_source_path_identity(&crate::display_path(&source.path));
        let digest = Sha256::digest(
            format!(
                "audiolib-sidecar-v1\0{}\0{source_identity}",
                self.album_namespace()
            )
            .as_bytes(),
        );
        let id = format!("{digest:x}")[..32].to_owned();
        let directory = self.vendor_root_directory(true)?.join(&id);
        fs::create_dir_all(&directory)?;
        unlinked(&directory)?;
        let owner = directory.join("owner");
        if owner.exists() {
            if !owner_matches(&owner, &self.album_namespace())? {
                return Err(invalid("Foreign automatic preview"));
            }
        } else {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&owner)?
                .write_all(self.album_namespace().as_bytes())?;
        }
        let operation_path = directory.join("operation.lock");
        unlinked(&operation_path)?;
        let operation = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(operation_path)?;
        operation
            .try_lock()
            .map_err(|_| Error::Busy("Table Metadata refresh is already running".into()))?;
        // The RAII token owns exactly this newly created subtree. Raw durable
        // events and before-images support rollback; resumption reparses the
        // source, so successful/no-op scans need no accumulating source copies.
        let run = tempfile::Builder::new()
            .prefix("automatic-")
            .tempdir_in(&directory)?;
        let input = run.path().join("input.bin");
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&input)?;
        let mut hash = Sha256::new();
        let mut copied = 0_u64;
        let mut buffer = [0_u8; 65536];
        loop {
            let count = source.file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            copied = copied
                .checked_add(count as u64)
                .ok_or_else(|| invalid("Source size overflow"))?;
            if copied > VENDOR_IMPORT_MAX_FILE_BYTES {
                return Err(invalid("Automatic source grew beyond 50 MiB"));
            }
            output.write_all(&buffer[..count])?;
            hash.update(&buffer[..count]);
            checkpoint(&summary, callback)?;
        }
        output.sync_all()?;
        let after = source.file.metadata()?;
        if copied != original.len()
            || after.len() != original.len()
            || after.modified()? != original.modified()?
        {
            return Err(invalid(
                "Table changed while copying; last good Metadata retained",
            ));
        }
        let content_hash = format!("{:x}", hash.finalize());
        drop(output);
        drop(source);
        let stage = Connection::open(run.path().join("data.db")).map_err(db_error)?;
        stage.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA max_page_count=262144;
          CREATE TABLE auto_rows(ordinal INTEGER PRIMARY KEY,sheet TEXT NOT NULL,row_number INTEGER NOT NULL,cells TEXT NOT NULL);
          CREATE TABLE auto_sheets(name TEXT PRIMARY KEY,value TEXT NOT NULL);
          CREATE TABLE auto_candidates(id INTEGER PRIMARY KEY,path TEXT NOT NULL,name TEXT NOT NULL,size INTEGER NOT NULL,mtime INTEGER NOT NULL,dir TEXT NOT NULL);
          CREATE INDEX auto_candidate_name ON auto_candidates(name,id);
          CREATE TABLE desired(audio_id INTEGER NOT NULL,field TEXT NOT NULL,value TEXT NOT NULL,source_file TEXT NOT NULL,sheet TEXT NOT NULL,row_number INTEGER NOT NULL,language TEXT NOT NULL,PRIMARY KEY(audio_id,field,value,language,source_file,sheet,row_number)) WITHOUT ROWID;
          CREATE TABLE work(id INTEGER PRIMARY KEY);
          BEGIN IMMEDIATE;").map_err(db_error)?;
        let mut remaining = resources::TOTAL_DECODED_BYTES;
        let file_result = parser::parse(
            &input,
            &name,
            &VendorImportOptions::default(),
            VendorLimits::default(),
            &mut remaining,
            |event| {
                match event {
                    parser::Event::Sheet(sheet) => {
                        stage.execute("INSERT INTO auto_sheets(name,value) VALUES(?1,?2) ON CONFLICT(name) DO UPDATE SET value=excluded.value",params![sheet.sheet,json(&sheet)?]).map_err(db_error)?;
                    }
                    parser::Event::Row { sheet, row, cells } => {
                        stage
                            .execute(
                                "INSERT INTO auto_rows(sheet,row_number,cells) VALUES(?1,?2,?3)",
                                params![sheet, sql_u64(row)?, json(&cells)?],
                            )
                            .map_err(db_error)?;
                        summary.rows += 1;
                        if summary.rows.is_multiple_of(256) {
                            checkpoint(&summary, callback)?;
                        }
                    }
                }
                Ok(())
            },
        )?;
        if file_result.truncated_rows != 0 {
            return Err(invalid(
                "Automatic table contains truncated rows; last good Metadata retained",
            ));
        }
        let sheets = stage
            .prepare("SELECT value FROM auto_sheets ORDER BY name")
            .map_err(db_error)?
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(db_error)?
            .map(|row| decode::<VendorSheetPreview>(&row.map_err(db_error)?))
            .collect::<Result<Vec<_>>>()?;
        if !sheets.iter().any(|sheet| {
            (sheet.mapping.filename.is_some() || sheet.mapping.relative_path.is_some())
                && !sheet.mapping.fields.is_empty()
        }) {
            return Err(invalid(
                "No recognized identity and Metadata columns; last good Metadata retained",
            ));
        }
        stage.execute_batch("COMMIT").map_err(db_error)?;
        checkpoint(&summary, callback)?;
        let scope = self.automatic_scope(&parent, root)?;
        self.automatic_candidates(&stage, &scope, session_id, &summary, callback)?;
        let mut query = stage.prepare("SELECT r.sheet,r.row_number,r.cells,s.value FROM auto_rows r JOIN auto_sheets s ON s.name=r.sheet ORDER BY r.ordinal").map_err(db_error)?;
        let mut rows = query.query([]).map_err(db_error)?;
        stage.execute_batch("BEGIN IMMEDIATE").map_err(db_error)?;
        let mut processed = 0_u64;
        while let Some(row) = rows.next().map_err(db_error)? {
            processed += 1;
            if processed.is_multiple_of(256) {
                checkpoint(&summary, callback)?;
            }
            let sheet: VendorSheetPreview = decode(&row.get::<_, String>(3).map_err(db_error)?)?;
            let cells: Vec<String> = decode(&row.get::<_, String>(2).map_err(db_error)?)?;
            let Some(filename) = identity(&sheet, &cells) else {
                summary.unmatched += 1;
                continue;
            };
            let count: i64 = stage
                .query_row(
                    "SELECT count(*) FROM auto_candidates WHERE name=?1",
                    [matching::fold(&filename)],
                    |row| row.get(0),
                )
                .map_err(db_error)?;
            if count != 1 {
                if count > 1 {
                    summary.ambiguous += 1;
                } else {
                    summary.unmatched += 1;
                }
                continue;
            }
            let audio_id: i64 = stage
                .query_row(
                    "SELECT id FROM auto_candidates WHERE name=?1",
                    [matching::fold(&filename)],
                    |row| row.get(0),
                )
                .map_err(db_error)?;
            let values: BTreeMap<String, String> = sheet
                .mapping
                .fields
                .iter()
                .filter_map(|(field, header)| {
                    sheet
                        .headers
                        .iter()
                        .position(|value| value == header)
                        .and_then(|index| cells.get(index))
                        .map(|value| value.trim())
                        .filter(|value| !value.is_empty())
                        .map(|value| (field.clone(), value.to_owned()))
                })
                .collect();
            if values.is_empty() {
                summary.unmatched += 1;
                continue;
            }
            for (field, value) in &values {
                stage.execute("INSERT OR IGNORE INTO desired(audio_id,field,value,source_file,sheet,row_number,language) VALUES(?1,?2,?3,?4,?5,?6,?7)",params![audio_id,field,value,source_identity,sheet.sheet,row.get::<_,i64>(1).map_err(db_error)?,values.get("language").map_or("",String::as_str)]).map_err(db_error)?;
            }
            summary.matched += 1;
            if (summary.matched + summary.unmatched + summary.ambiguous).is_multiple_of(256) {
                checkpoint(&summary, callback)?;
            }
        }
        drop(rows);
        drop(query);
        stage
            .execute_batch(
                "INSERT OR IGNORE INTO work SELECT DISTINCT audio_id FROM desired; COMMIT",
            )
            .map_err(db_error)?;
        let mut preview = VendorImportPreview {
            import_id: id.clone(),
            state: Some("running".into()),
            rows: summary.rows,
            matched: summary.matched,
            unmatched: summary.unmatched,
            ambiguous: summary.ambiguous,
            files: vec![file_result],
            sheets,
            ..Default::default()
        };
        preview
            .warnings
            .push("Automatic table-subtree WAV Metadata; resume by rescanning.".into());
        if summary.unmatched > 0 || summary.ambiguous > 0 {
            preview.warnings.push("This table is not fully associated: all previous source contributions are retained alongside safe new matches. Correct the table or explicitly roll back its source to converge.".into());
        }
        let mut encoded = serde_json::to_value(&preview).map_err(invalid)?;
        encoded["_automatic"] =
            serde_json::json!({"kind":"sidecar-v1","source":source_identity,"sha256":content_hash});
        let encoded = json(&encoded)?;
        let ui_path = directory.join("preview.db");
        unlinked(&ui_path)?;
        for suffix in ["-wal", "-shm", "-journal"] {
            unlinked(&directory.join(format!("preview.db{suffix}")))?;
        }
        let ui = Connection::open_with_flags(
            &ui_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(db_error)?;
        stage_schema(&ui)?;
        set_meta(&ui, "preview", &preview)?;
        set_meta(&ui, "cancel", &false)?;
        set_meta(&ui, "inputs", &Vec::<Input>::new())?;
        set_meta(&ui, "rejected", &Vec::<VendorFileResult>::new())?;
        {
            let conn = self.writer()?;
            storage::ensure_schema(&conn)?;
            let exists: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM audiolib_vendor_jobs WHERE import_id=?1)",
                    [&id],
                    |row| row.get(0),
                )
                .map_err(db_error)?;
            if exists && !storage::automatic_job(&conn, &id)? {
                return Err(invalid(
                    "Automatic source identity is occupied by a manual import",
                ));
            }
            // A stable source job survives process exit. Its events, not a stale
            // row offset, define the next refresh and the ordinary rollback.
            if !exists {
                conn.execute("INSERT INTO audiolib_vendor_jobs(import_id,idempotency_key,options_hash,state,checkpoint,written,restored,created,preview_json,options_json,operation) VALUES(?1,?2,?3,'running',0,0,0,?4,?5,?6,'automatic')",params![id,format!("auto:{id}"),content_hash,now()?,encoded,json(&VendorImportOptions::default())?]).map_err(db_error)?;
            }
        }
        if summary.unmatched > 0 || summary.ambiguous > 0 {
            self.automatic_preserve_previous(&stage, &id, &summary, callback)?;
        }
        if summary.unmatched == 0 && summary.ambiguous == 0 {
            let mut after = None::<i64>;
            loop {
                let mut count = 0;
                {
                    let conn = self.writer()?;
                    let mut old=conn.prepare("SELECT DISTINCT audio_file_id FROM audiolib_vendor_events WHERE import_id=?1 AND (?2 IS NULL OR audio_file_id>?2) ORDER BY audio_file_id LIMIT 1024").map_err(db_error)?;
                    let mut ids = old.query(params![id, after]).map_err(db_error)?;
                    stage.execute_batch("BEGIN IMMEDIATE").map_err(db_error)?;
                    while let Some(row) = ids.next().map_err(db_error)? {
                        let audio_id: i64 = row.get(0).map_err(db_error)?;
                        after = Some(audio_id);
                        count += 1;
                        stage
                            .execute("INSERT OR IGNORE INTO work(id) VALUES(?1)", [audio_id])
                            .map_err(db_error)?;
                    }
                    stage.execute_batch("COMMIT").map_err(db_error)?;
                }
                checkpoint(&summary, callback)?;
                if count < 1024 {
                    break;
                }
            }
        }
        let result = self.automatic_apply(
            &stage,
            &ui,
            &id,
            &encoded,
            &content_hash,
            root,
            session_id,
            &mut summary,
            callback,
        );
        if let Err(error) = &result {
            let conn = self.writer()?;
            let state = if matches!(error, Error::Cancelled) {
                "paused"
            } else {
                "failed"
            };
            conn.execute("UPDATE audiolib_vendor_jobs SET state=?2,last_error=?3,operation='automatic' WHERE import_id=?1",params![id,state,error.to_string()]).map_err(db_error)?;
        }
        result?;
        preview.state = Some("committed".into());
        set_meta(&ui, "preview", &preview)?;
        Ok(summary)
    }

    // Row coordinates are not stable identities after insertion/reordering.
    // Any uncertain association therefore makes this attempt additive; only a
    // completely associated table may retire its own older contributions.
    fn automatic_preserve_previous(
        &self,
        stage: &Connection,
        id: &str,
        summary: &VendorScanSummary,
        callback: &mut impl FnMut(&VendorScanSummary) -> bool,
    ) -> Result<()> {
        let mut after = None::<i64>;
        loop {
            let events = {
                let conn = self.writer()?;
                let mut query=conn.prepare("SELECT rowid,audio_file_id,field,value,source_file,sheet,row,language FROM audiolib_vendor_events WHERE import_id=?1 AND (?2 IS NULL OR rowid>?2) ORDER BY rowid LIMIT 128").map_err(db_error)?;
                query
                    .query_map(params![id, after], |row| {
                        Ok(PreviousEvent {
                            cursor: row.get(0)?,
                            audio: row.get(1)?,
                            field: row.get(2)?,
                            value: row.get(3)?,
                            source: row.get(4)?,
                            sheet: row.get(5)?,
                            row: row.get(6)?,
                            language: row.get(7)?,
                        })
                    })
                    .map_err(db_error)?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(db_error)?
            };
            if events.is_empty() {
                return Ok(());
            }
            stage.execute_batch("BEGIN IMMEDIATE").map_err(db_error)?;
            for event in &events {
                stage.execute("INSERT OR IGNORE INTO desired(audio_id,field,value,source_file,sheet,row_number,language) VALUES(?1,?2,?3,?4,?5,?6,?7)",params![event.audio,event.field,event.value,event.source,event.sheet,event.row,event.language]).map_err(db_error)?;
                stage
                    .execute("INSERT OR IGNORE INTO work(id) VALUES(?1)", [event.audio])
                    .map_err(db_error)?;
            }
            stage.execute_batch("COMMIT").map_err(db_error)?;
            after = events.last().map(|event| event.cursor);
            checkpoint(summary, callback)?;
        }
    }

    /// Folders whose WAVs one table may describe: the table directory's whole
    /// subtree and, only when that directory is itself `{stem}--Docs` or
    /// `{stem}--Sounds*`, the subtrees of siblings with the same stem. Other
    /// packs side by side and the library root are never candidates.
    fn automatic_scope(&self, parent: &Path, root: &Path) -> Result<Vec<(i64, String)>> {
        let conn = self.writer()?;
        let own_text = crate::folder_path_text(parent);
        let own_key = crate::normalize_source_path_identity(&own_text);
        let mut anchors = Vec::new();
        if let Some(id) = crate::path_identity::folder_id(&conn, &own_text)? {
            anchors.push(id);
        }
        let approved = PathBuf::from(crate::display_path(root));
        let own = PathBuf::from(crate::display_path(parent));
        let own_stem = parent
            .file_name()
            .and_then(|name| pack_stem(&name.to_string_lossy()));
        if own != approved
            && let Some(own_stem) = own_stem
            && let Some(grand) = own.parent().filter(|path| path.starts_with(&approved))
            && let Some(grand) =
                crate::path_identity::folder_id(&conn, &crate::folder_path_text(grand))?
        {
            let mut query = conn
                .prepare_cached("SELECT id,folder_path,folder_name FROM folders WHERE parent_id=?1 AND (folder_name LIKE '%--docs' OR folder_name LIKE '%--sounds%') ORDER BY id")
                .map_err(db_error)?;
            let siblings = query
                .query_map([grand], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .map_err(db_error)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(db_error)?;
            anchors.extend(
                siblings
                    .into_iter()
                    .filter(|(_, path, name)| {
                        crate::normalize_source_path_identity(path) != own_key
                            && pack_stem(name).as_deref() == Some(own_stem.as_str())
                    })
                    .map(|(id, _, _)| id),
            );
        }
        let mut scope = BTreeMap::new();
        let mut query = conn
            .prepare_cached("SELECT f.id,f.folder_path FROM folder_closure c JOIN folders f ON f.id=c.descendant_id WHERE c.ancestor_id=?1")
            .map_err(db_error)?;
        for anchor in anchors {
            let rows = query
                .query_map([anchor], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(db_error)?;
            for row in rows {
                let (id, path) = row.map_err(db_error)?;
                scope.insert(id, path);
            }
        }
        Ok(scope.into_iter().collect())
    }

    fn automatic_candidates(
        &self,
        stage: &Connection,
        scope: &[(i64, String)],
        session: i64,
        summary: &VendorScanSummary,
        callback: &mut impl FnMut(&VendorScanSummary) -> bool,
    ) -> Result<()> {
        for (folder, folder_path) in scope {
            let directory = crate::normalize_source_path_identity(folder_path);
            let mut after = None::<i64>;
            loop {
                let mut count = 0;
                {
                    let conn = self.writer()?;
                    crate::ensure_scan_seen_table(&conn)?;
                    let mut query=conn.prepare("SELECT id,file_path,coalesce(file_size,0),source_mtime_ns FROM audio_files a WHERE folder_id=?1 AND missing IS 0 AND (?3 IS NULL OR id>?3) AND (seen_scan_id=?2 OR EXISTS(SELECT 1 FROM temp.scan_seen s WHERE s.session_id=?2 AND s.audio_id=a.id)) ORDER BY id LIMIT 1024").map_err(db_error)?;
                    let mut rows = query
                        .query(params![folder, session, after])
                        .map_err(db_error)?;
                    stage.execute_batch("BEGIN IMMEDIATE").map_err(db_error)?;
                    while let Some(row) = rows.next().map_err(db_error)? {
                        after = Some(row.get::<_, i64>(0).map_err(db_error)?);
                        count += 1;
                        let path: String = row.get(1).map_err(db_error)?;
                        let path_object = Path::new(&path);
                        if !path_object
                            .extension()
                            .is_some_and(|extension| extension.eq_ignore_ascii_case("wav"))
                        {
                            continue;
                        }
                        let Some(timestamp) = row.get::<_, Option<i64>>(3).map_err(db_error)?
                        else {
                            continue;
                        };
                        let Some(name) = path_object.file_name() else {
                            continue;
                        };
                        if crate::source_parent_path_identity(&path) != directory {
                            continue;
                        }
                        stage.execute("INSERT INTO auto_candidates(id,path,name,size,mtime,dir) VALUES(?1,?2,?3,?4,?5,?6)",params![row.get::<_,i64>(0).map_err(db_error)?,path,matching::fold(&name.to_string_lossy()),row.get::<_,i64>(2).map_err(db_error)?,timestamp,directory]).map_err(db_error)?;
                    }
                    stage.execute_batch("COMMIT").map_err(db_error)?;
                }
                checkpoint(summary, callback)?;
                if count < 1024 {
                    break;
                }
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn automatic_apply(
        &self,
        stage: &Connection,
        ui: &Connection,
        id: &str,
        encoded: &str,
        content_hash: &str,
        root: &Path,
        session: i64,
        summary: &mut VendorScanSummary,
        callback: &mut impl FnMut(&VendorScanSummary) -> bool,
    ) -> Result<()> {
        let mut after = None::<i64>;
        loop {
            checkpoint(summary, callback)?;
            if optional_meta::<bool>(ui, "cancel")?.unwrap_or(false) {
                return Err(Error::Cancelled);
            }
            let ids = stage
                .prepare("SELECT id FROM work WHERE (?1 IS NULL OR id>?1) ORDER BY id LIMIT ?2")
                .map_err(db_error)?
                .query_map(params![after, BATCH], |row| row.get::<_, i64>(0))
                .map_err(db_error)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(db_error)?;
            if ids.is_empty() {
                break;
            }
            // File capabilities are acquired without the main writer. Keep them
            // through commit; Windows denies source replacement/writes meanwhile.
            let mut pinned = Vec::new();
            for audio_id in &ids {
                let candidate = stage
                    .query_row(
                        "SELECT path,size,mtime,dir FROM auto_candidates WHERE id=?1",
                        [audio_id],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, i64>(1)?,
                                row.get::<_, i64>(2)?,
                                row.get::<_, String>(3)?,
                            ))
                        },
                    )
                    .optional()
                    .map_err(db_error)?;
                let Some((path, size, timestamp, dir)) = candidate else {
                    continue;
                };
                let mut capability = rooted::open(root, Path::new(&path))?;
                let metadata = capability.file.metadata()?;
                let mut head = [0_u8; 12];
                if capability.file.read_exact(&mut head).is_err()
                    || !matches!(&head[..4], b"RIFF" | b"RF64" | b"BW64")
                    || &head[8..] != b"WAVE"
                    || i64::try_from(metadata.len()).ok() != Some(size)
                    || mtime(&metadata)? != timestamp
                    || capability.path.parent().map(|folder| {
                        crate::normalize_source_path_identity(&crate::folder_path_text(folder))
                    }) != Some(dir)
                {
                    continue;
                }
                pinned.push((*audio_id, path, size, timestamp, capability));
            }
            checkpoint(summary, callback)?;
            let mut updated = 0;
            let mut unchanged = 0;
            {
                let mut conn = self.writer()?;
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(db_error)?;
                let cache = self.begin_search_cache_batch(&tx)?;
                let mut mutated = false;
                for (audio_id, path, size, timestamp, _) in &pinned {
                    let actual:Option<String>=tx.query_row("SELECT file_path FROM audio_files a WHERE id=?1 AND missing IS 0 AND file_size=?2 AND source_mtime_ns=?3 AND (seen_scan_id=?4 OR EXISTS(SELECT 1 FROM temp.scan_seen s WHERE s.session_id=?4 AND s.audio_id=a.id))",params![audio_id,size,timestamp,session],|row|row.get(0)).optional().map_err(db_error)?;
                    if actual.as_deref() != Some(path.as_str()) {
                        continue;
                    }
                    let old = event_hash(
                        &tx,
                        "SELECT field,value,source_file,sheet,row,language FROM audiolib_vendor_events WHERE audio_file_id=?1 AND import_id=?2 ORDER BY field,value,source_file,sheet,row,language",
                        *audio_id,
                        id,
                    )?;
                    let desired = event_hash(
                        stage,
                        "SELECT field,value,source_file,sheet,row_number,language FROM desired WHERE audio_id=?1 AND ?2 IS NOT NULL ORDER BY field,value,source_file,sheet,row_number,language",
                        *audio_id,
                        id,
                    )?;
                    let before: String = tx
                        .query_row(
                            "SELECT metadata_json FROM audio_files WHERE id=?1",
                            [audio_id],
                            |row| row.get(0),
                        )
                        .map_err(db_error)?;
                    storage::save_before(&tx, id, *audio_id)?;
                    if old != desired {
                        mutated = true;
                        tx.execute("DELETE FROM audiolib_vendor_events WHERE import_id=?1 AND audio_file_id=?2",params![id,audio_id]).map_err(db_error)?;
                        let mut query=stage.prepare("SELECT field,value,source_file,sheet,row_number,language FROM desired WHERE audio_id=?1 ORDER BY field,value,source_file,sheet,row_number,language").map_err(db_error)?;
                        let mut events = query.query([audio_id]).map_err(db_error)?;
                        while let Some(event) = events.next().map_err(db_error)? {
                            storage::add_event(
                                &tx,
                                storage::EventRow {
                                    import: id,
                                    id: *audio_id,
                                    field: &event.get::<_, String>(0).map_err(db_error)?,
                                    value: &event.get::<_, String>(1).map_err(db_error)?,
                                    source_file: &event.get::<_, String>(2).map_err(db_error)?,
                                    sheet: &event.get::<_, String>(3).map_err(db_error)?,
                                    row: crate::unsigned(event, 4).map_err(db_error)?,
                                    overwrite: false,
                                    language: &event.get::<_, String>(5).map_err(db_error)?,
                                },
                            )?;
                        }
                    }
                    // Even equal raw events must follow this scan's current
                    // embedded Metadata: suppression is reversible projection.
                    storage::project(&tx, *audio_id)?;
                    let changed: bool = tx
                        .query_row(
                            "SELECT metadata_json<>?2 FROM audio_files WHERE id=?1",
                            params![audio_id, before],
                            |row| row.get(0),
                        )
                        .map_err(db_error)?;
                    if changed {
                        updated += 1;
                        mutated = true;
                    } else {
                        unchanged += 1;
                    }
                }
                if mutated {
                    tx.execute("UPDATE audiolib_vendor_jobs SET state='running',checkpoint=?2,written=?3,last_error=NULL,operation='automatic' WHERE import_id=?1 AND (state<>'running' OR checkpoint<>?2 OR written<>?3 OR last_error IS NOT NULL)",params![id,ids.last().copied().unwrap_or_default(),sql_u64(summary.updated+summary.unchanged+updated+unchanged)?]).map_err(db_error)?;
                }
                let cache = self.prepare_search_cache_batch(&tx, cache, &AtomicBool::new(false))?;
                let cache = self.commit_search_cache_batch(tx, cache)?;
                if let Some(cache) = cache
                    && self.publish_search_cache_batch(&conn, cache).is_err()
                {
                    let _ = self.reset_search_caches();
                }
            }
            drop(pinned);
            summary.updated += updated;
            summary.unchanged += unchanged;
            after = ids.last().copied();
            checkpoint(summary, callback)?;
        }
        let result = VendorImportResult {
            import_id: id.into(),
            state: Some("committed".into()),
            written: summary.updated + summary.unchanged,
            restored: 0,
        };
        let conn = self.writer()?;
        conn.execute("UPDATE audiolib_vendor_jobs SET state='committed',preview_json=?2,options_hash=?3,commit_result=?4,last_error=NULL,operation='automatic' WHERE import_id=?1 AND (state<>'committed' OR preview_json<>?2 OR options_hash<>?3 OR commit_result IS NOT ?4 OR last_error IS NOT NULL)",params![id,encoded,content_hash,json(&result)?]).map_err(db_error)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
