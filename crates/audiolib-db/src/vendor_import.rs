// crates/audiolib-db/src/vendor_import.rs
//! Recoverable vendor-table import, with owned previews and transactional business provenance.
//! Preview files never modify the library. Commit/rollback use the provider's administration lease.
#[cfg(feature = "scanner")]
mod automatic;
mod matching;
mod parser;
mod resources;
mod storage;

use crate::{SqliteProvider, db_error};
use audiolib_core::{Error, Result, vendor_import::*};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub(crate) use storage::{drop_legacy_snapshot_storage, ensure_schema, validate_schema};

/// Re-evaluate supplementary last-good table values after embedded WAV changes.
/// This runs in the caller's scan transaction, even if the table disappeared.
#[cfg(feature = "scanner")]
pub(crate) fn reproject_automatic_if_present(conn: &Connection, id: i64) -> Result<()> {
    if !storage::schema_exists(conn)? {
        return Ok(());
    }
    let present:bool=conn.prepare_cached("SELECT EXISTS(SELECT 1 FROM audiolib_vendor_events e JOIN audiolib_vendor_jobs j USING(import_id) WHERE e.audio_file_id=?1 AND CASE WHEN json_valid(j.preview_json) THEN json_extract(j.preview_json,'$._automatic.kind')='sidecar-v1' ELSE 0 END)").map_err(db_error)?.query_row([id],|row|row.get(0)).map_err(db_error)?;
    if present {
        storage::project(conn, id)?;
    }
    Ok(())
}
fn invalid(message: impl std::fmt::Display) -> Error {
    Error::Validation(message.to_string())
}
fn json(value: &impl Serialize) -> Result<String> {
    serde_json::to_string(value).map_err(invalid)
}
fn decode<T: serde::de::DeserializeOwned>(value: &str) -> Result<T> {
    serde_json::from_str(value).map_err(invalid)
}
fn sql_u64(value: u64) -> Result<i64> {
    i64::try_from(value)
        .map_err(|_| invalid("Vendor count exceeds SQLite signed 64-bit integer range"))
}
fn now() -> Result<i64> {
    sql_u64(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(invalid)?
            .as_secs(),
    )
}

#[derive(Clone, Serialize, Deserialize)]
struct Input {
    name: String,
    path: PathBuf,
}

/// Staging is immutable under operation.lock; only the live audio path needs
/// another check after the writer transaction has been acquired.
struct CommitRow {
    ordinal: u64,
    row: VendorRowPreview,
    expected_path: Option<String>,
}

fn unlinked(path: &Path) -> Result<()> {
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) => {
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    if metadata.file_attributes() & 0x400 != 0 {
                        return Err(invalid("Linked vendor staging paths are refused"));
                    }
                }
                if metadata.file_type().is_symlink() {
                    return Err(invalid("Linked vendor staging paths are refused"));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn owner_matches(path: &Path, owner: &str) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    unlinked(path)?;
    let mut file = File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() != owner.len() as u64 {
        return Ok(false);
    }
    let mut bytes = Vec::with_capacity(owner.len() + 1);
    (&mut file)
        .take(owner.len() as u64 + 1)
        .read_to_end(&mut bytes)?;
    Ok(bytes == owner.as_bytes())
}

fn legacy_templates(path: &Path) -> Result<bool> {
    if !path.is_file() || fs::metadata(path)?.len() > 2 * 1024 * 1024 {
        return Ok(false);
    }
    let mut bytes = Vec::new();
    File::open(path)?
        .take(2 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    Ok(bytes.len() <= 2 * 1024 * 1024
        && serde_json::from_slice::<Vec<VendorMappingTemplate>>(&bytes).is_ok())
}

fn stage_schema(conn: &Connection) -> Result<()> {
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(db_error)?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
      CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS rows(ordinal INTEGER PRIMARY KEY,row_key TEXT NOT NULL UNIQUE,source_file TEXT NOT NULL,sheet TEXT NOT NULL,row_number INTEGER NOT NULL,cells TEXT NOT NULL,result TEXT NOT NULL DEFAULT '{}',candidate_set INTEGER NOT NULL DEFAULT 0);
      CREATE TABLE IF NOT EXISTS candidate_sets(id INTEGER PRIMARY KEY,lookup_key TEXT NOT NULL UNIQUE,total INTEGER NOT NULL);
      CREATE TABLE IF NOT EXISTS candidate_set_members(set_id INTEGER NOT NULL,audio_id INTEGER NOT NULL,PRIMARY KEY(set_id,audio_id)) WITHOUT ROWID;").map_err(db_error)
}
fn set_meta(conn: &Connection, key: &str, value: &impl Serialize) -> Result<()> {
    conn.execute("INSERT INTO meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![key,json(value)?]).map_err(db_error)?;
    Ok(())
}
fn get_meta<T: serde::de::DeserializeOwned>(conn: &Connection, key: &str) -> Result<T> {
    let value: String = conn
        .query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get(0))
        .map_err(db_error)?;
    decode(&value)
}
fn optional_meta<T: serde::de::DeserializeOwned>(
    conn: &Connection,
    key: &str,
) -> Result<Option<T>> {
    conn.query_row("SELECT value FROM meta WHERE key=?1", [key], |r| {
        r.get::<_, String>(0)
    })
    .optional()
    .map_err(db_error)?
    .map(|s| decode(&s))
    .transpose()
}
fn clear_error(stage: &Connection) -> Result<()> {
    stage
        .execute(
            "DELETE FROM meta WHERE key IN ('last_error','last_operation')",
            [],
        )
        .map_err(db_error)?;
    Ok(())
}

fn validate_commit_resume(operation: Option<&str>, restored: u64) -> Result<()> {
    if operation == Some("rollback") || restored > 0 {
        return Err(invalid(
            "Cannot resume commit after rollback has started; finish this batch's rollback and create a fresh preview",
        ));
    }
    Ok(())
}

impl SqliteProvider {
    // Best effort secondary recording must never hide the original operation error.
    fn vendor_record_failure(&self, id: &str, operation: &str, key: Option<&str>, error: &Error) {
        if let Ok(stage) = self.vendor_stage(id) {
            let _ = set_meta(&stage, "last_error", &format!("{operation}: {error}"));
            let _ = set_meta(&stage, "last_operation", &operation);
        }
        if matches!(operation, "commit" | "rollback")
            && let Ok(conn) = self.writer()
            && storage::schema_exists(&conn).is_ok_and(|v| v)
        {
            let _=conn.execute("UPDATE audiolib_vendor_jobs SET state='failed',last_error=?3,operation=?4 WHERE import_id=?1 AND (?2 IS NULL OR idempotency_key=?2) AND state IN ('running','paused','failed','rolling_back')",params![id,key,format!("{operation}: {error}"),operation]);
        }
    }
    fn vendor_root_directory(&self, create: bool) -> Result<PathBuf> {
        let name = self
            .path
            .file_name()
            .ok_or_else(|| invalid("Library path has no filename"))?
            .to_string_lossy();
        let root = self.path.with_file_name(format!("{name}.vendor-import"));
        unlinked(&root)?;
        if create {
            fs::create_dir_all(&root)?;
            let marker = root.join(".audiolib-owner");
            if marker.exists() {
                if !owner_matches(&marker, &self.album_namespace())? {
                    return Err(invalid("Vendor staging root belongs to another library"));
                }
            } else {
                // Adopt only the complete, recognizable older owned layout. A
                // user's same-named audio directory must never become hidden.
                for child in fs::read_dir(&root)? {
                    let child = child?;
                    let name = child.file_name().to_string_lossy().into_owned();
                    unlinked(&child.path())?;
                    let owned_job = name.len() == 32
                        && name.bytes().all(|byte| byte.is_ascii_hexdigit())
                        && child.file_type()?.is_dir()
                        && owner_matches(&child.path().join("owner"), &self.album_namespace())?;
                    let owned_templates = (name == "templates.json"
                        && child.file_type()?.is_file()
                        && legacy_templates(&child.path())?)
                        || (name == "templates.lock"
                            && child.file_type()?.is_file()
                            && child.metadata()?.len() == 0);
                    if !owned_job && !owned_templates {
                        return Err(invalid(
                            "Refusing to adopt a nonempty foreign vendor staging directory",
                        ));
                    }
                }
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(marker)?
                    .write_all(self.album_namespace().as_bytes())?;
            }
        }
        Ok(root)
    }

    /// The exact verified preview root, excluded from source-table discovery.
    /// Foreign same-named directories remain ordinary user scan inputs.
    #[cfg(feature = "scanner")]
    pub(crate) fn vendor_import_owned_root(&self) -> Result<Option<PathBuf>> {
        let root = match self.vendor_root_directory(false) {
            Ok(root) => root,
            Err(Error::Validation(_)) => return Ok(None),
            Err(error) => return Err(error),
        };
        let marker = root.join(".audiolib-owner");
        if marker.is_file() && owner_matches(&marker, &self.album_namespace()).unwrap_or(false) {
            unlinked(&marker)?;
            return Ok(Some(root.canonicalize()?));
        }
        Ok(None)
    }

    fn vendor_job_directory(&self, id: &str) -> Result<PathBuf> {
        if id.len() != 32 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(invalid("Invalid vendor import id"));
        }
        let root = self.vendor_root_directory(false)?;
        let job = root.join(id);
        unlinked(&job)?;
        if !job.is_dir() || !owner_matches(&job.join("owner"), &self.album_namespace())? {
            return Err(invalid("Vendor preview belongs to another library"));
        }
        Ok(job)
    }

    fn vendor_stage(&self, id: &str) -> Result<Connection> {
        self.vendor_recover_stage(id)?;
        let path = self.vendor_job_directory(id)?.join("preview.db");
        unlinked(&path)?;
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
            .map_err(db_error)?;
        conn.busy_timeout(Duration::from_secs(5))
            .map_err(db_error)?;
        Ok(conn)
    }

    fn vendor_job_lock(&self, id: &str) -> Result<File> {
        self.vendor_recover_stage(id)?;
        let path = self.vendor_job_directory(id)?.join("operation.lock");
        unlinked(&path)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        file.try_lock()
            .map_err(|_| Error::Busy("Vendor import operation is already running".into()))?;
        Ok(file)
    }

    fn vendor_recover_stage(&self, id: &str) -> Result<()> {
        if id.len() != 32 || !id.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err(invalid("Invalid vendor import id"));
        }
        let directory = self.vendor_root_directory(false)?.join(id);
        unlinked(&directory)?;
        if directory.join("preview.db").exists() {
            return Ok(());
        }
        if directory.join("owner").exists()
            && fs::read_to_string(directory.join("owner"))? != self.album_namespace()
        {
            return Err(invalid(
                "Existing preview directory belongs to another library",
            ));
        }
        // A restored main-library backup still owns committed history and rollback
        // before-images. Recreate only its UI summary; never pretend lost source rows exist.
        let encoded = {
            let conn = self.reader()?;
            if !storage::schema_exists(&conn)? {
                return Err(invalid("Unknown vendor preview"));
            }
            conn.query_row(
                "SELECT preview_json FROM audiolib_vendor_jobs WHERE import_id=?1",
                [id],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(db_error)?
            .ok_or_else(|| invalid("Unknown vendor preview"))?
        };
        if self.read_only {
            return Err(invalid(
                "Recovering import history requires an owned writable data directory",
            ));
        }
        fs::create_dir_all(&directory)?;
        File::create(directory.join("owner"))?.write_all(self.album_namespace().as_bytes())?;
        let stage = Connection::open(directory.join("preview.db")).map_err(db_error)?;
        stage_schema(&stage)?;
        let mut preview: VendorImportPreview = decode(&encoded)?;
        preview.warnings.push("Source preview files are unavailable; committed batches can be rolled back, but cannot resume without their original owned preview".into());
        set_meta(&stage, "preview", &preview)?;
        set_meta(&stage, "cancel", &false)?;
        set_meta(&stage, "inputs", &Vec::<Input>::new())?;
        set_meta(&stage, "rejected", &Vec::<VendorFileResult>::new())?;
        Ok(())
    }

    /// Preview one table without changing library data.
    pub fn vendor_import_preview_impl(&self, file: &Path) -> Result<VendorImportPreview> {
        self.vendor_import_preview_with_options_impl(file, &VendorImportOptions::default())
    }

    /// Preview one table using explicit parsing/mapping options.
    pub fn vendor_import_preview_with_options_impl(
        &self,
        file: &Path,
        options: &VendorImportOptions,
    ) -> Result<VendorImportPreview> {
        self.vendor_import_preview_files_impl(&[file.to_path_buf()], options)
    }

    /// Persist recoverable owned copies and parse/match a multi-file preview.
    pub fn vendor_import_preview_files_impl(
        &self,
        files: &[PathBuf],
        options: &VendorImportOptions,
    ) -> Result<VendorImportPreview> {
        if self.read_only {
            return Err(invalid("Vendor import requires a writable library"));
        }
        if files.is_empty() || files.len() > 512 {
            return Err(invalid("Choose between 1 and 512 vendor tables"));
        }
        matching::validate_options(options)?;
        let id = uuid::Uuid::new_v4().simple().to_string();
        let directory = self.vendor_root_directory(true)?.join(&id);
        unlinked(&directory)?;
        fs::create_dir(&directory)?;
        File::create(directory.join("owner"))?.write_all(self.album_namespace().as_bytes())?;
        let mut stage = Connection::open(directory.join("preview.db")).map_err(db_error)?;
        stage_schema(&stage)?;
        set_meta(
            &stage,
            "preview",
            &VendorImportPreview {
                import_id: id.clone(),
                state: Some("preparing".into()),
                options: options.clone(),
                ..Default::default()
            },
        )?;
        let mut inputs = Vec::new();
        let mut rejected = Vec::new();
        let mut labels = BTreeSet::new();
        let mut copied_bytes = 0u64;
        for (index, file) in files.iter().enumerate() {
            let basename = file
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let mut name = basename.clone();
            let mut suffix = index + 1;
            while !labels.insert(name.clone()) {
                name = format!("{suffix}:{basename}");
                suffix += 1;
            }
            let copied = (|| -> Result<Input> {
                if is_skipped_name(&basename) {
                    return Err(invalid("System/Office lock file skipped"));
                }
                let mut input = File::open(file)?;
                if input.metadata()?.len() > VendorLimits::default().max_file_bytes {
                    return Err(invalid("File exceeds 50 MiB input limit"));
                }
                let remaining = resources::TOTAL_DECODED_BYTES.saturating_sub(copied_bytes);
                if input.metadata()?.len() > remaining {
                    return Err(invalid("Vendor batch exceeds 512 MiB input safety budget"));
                }
                let path = directory.join(format!("input-{index}"));
                let mut output = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)?;
                let bytes = std::io::copy(
                    &mut (&mut input)
                        .take(VendorLimits::default().max_file_bytes.min(remaining) + 1),
                    &mut output,
                )?;
                output.sync_all()?;
                if bytes > VendorLimits::default().max_file_bytes || bytes > remaining {
                    return Err(invalid("File grew beyond the input size limit"));
                }
                copied_bytes += bytes;
                Ok(Input {
                    name: name.clone(),
                    path,
                })
            })();
            match copied {
                Ok(input) => inputs.push(input),
                Err(error) => rejected.push(VendorFileResult {
                    name,
                    error: Some(error.to_string()),
                    ..Default::default()
                }),
            }
        }
        set_meta(&stage, "inputs", &inputs)?;
        set_meta(&stage, "rejected", &rejected)?;
        set_meta(&stage, "cancel", &false)?;
        set_meta(&stage, "created", &now()?)?;
        let result = (|| {
            let library = self.reader()?;
            matching::build_index(&mut stage, &library)?;
            self.vendor_parse_preview(&id, &mut stage, options)
        })();
        if let Err(error) = &result {
            set_meta(
                &stage,
                "preview",
                &VendorImportPreview {
                    import_id: id.clone(),
                    state: Some("failed".into()),
                    warnings: vec![error.to_string()],
                    options: options.clone(),
                    ..Default::default()
                },
            )?;
            self.vendor_record_failure(&id, "preview", None, error);
        }
        result
    }

    fn vendor_parse_preview(
        &self,
        id: &str,
        stage: &mut Connection,
        options: &VendorImportOptions,
    ) -> Result<VendorImportPreview> {
        stage
            .execute_batch("SAVEPOINT vendor_preview")
            .map_err(db_error)?;
        let result = self.vendor_parse_preview_inner(id, stage, options);
        match result {
            Ok(preview) => {
                stage
                    .execute_batch("RELEASE vendor_preview")
                    .map_err(db_error)?;
                clear_error(stage)?;
                Ok(preview)
            }
            Err(error) => {
                stage
                    .execute_batch("ROLLBACK TO vendor_preview; RELEASE vendor_preview")
                    .map_err(db_error)?;
                let _ = set_meta(stage, "last_error", &format!("preview: {error}"));
                let _ = set_meta(stage, "last_operation", &"preview");
                Err(error)
            }
        }
    }

    fn vendor_parse_preview_inner(
        &self,
        id: &str,
        stage: &mut Connection,
        options: &VendorImportOptions,
    ) -> Result<VendorImportPreview> {
        let mut options = options.clone();
        if let Some(template) = &options.template {
            let template = self
                .vendor_import_templates_impl()?
                .into_iter()
                .find(|t| &t.id == template)
                .ok_or_else(|| invalid("Unknown vendor mapping template"))?;
            if options.mapping.is_none() {
                options.mapping = Some(template.mapping);
            }
            if options.encoding.is_none() {
                options.encoding = template.encoding;
            }
            if options.delimiter.is_none() {
                options.delimiter = template.delimiter;
            }
        }
        matching::validate_options(&options)?;
        stage.execute("DELETE FROM rows", []).map_err(db_error)?;
        let inputs: Vec<Input> = get_meta(stage, "inputs")?;
        let mut files: Vec<VendorFileResult> = get_meta(stage, "rejected")?;
        let mut sheets = BTreeMap::<(String, String), VendorSheetPreview>::new();
        let mut decoded_remaining = resources::TOTAL_DECODED_BYTES;
        for (file_index, input) in inputs.iter().enumerate() {
            if !input.path.starts_with(self.vendor_job_directory(id)?) {
                return Err(invalid(
                    "Preview source copy escaped its owned job directory",
                ));
            }
            unlinked(&input.path)?;
            stage
                .execute_batch("SAVEPOINT vendor_file")
                .map_err(db_error)?;
            let mut file_sheets = BTreeMap::new();
            let parsed = parser::parse(
                &input.path,
                &input.name,
                &options,
                VendorLimits::default(),
                &mut decoded_remaining,
                |event| {
                    match event {
                        parser::Event::Sheet(sheet) => {
                            file_sheets
                                .insert((sheet.source_file.clone(), sheet.sheet.clone()), sheet);
                        }
                        parser::Event::Row { sheet, row, cells } => {
                            let key = format!("{file_index}:{sheet}:{row}");
                            stage.execute("INSERT INTO rows(row_key,source_file,sheet,row_number,cells) VALUES(?1,?2,?3,?4,?5)",params![key,input.name,sheet,sql_u64(row)?,json(&cells)?]).map_err(db_error)?;
                        }
                    }
                    Ok(())
                },
            );
            match parsed {
                Ok(result) => {
                    stage
                        .execute_batch("RELEASE vendor_file")
                        .map_err(db_error)?;
                    sheets.extend(file_sheets);
                    files.push(result);
                }
                Err(error) => {
                    stage
                        .execute_batch("ROLLBACK TO vendor_file; RELEASE vendor_file")
                        .map_err(db_error)?;
                    files.push(VendorFileResult {
                        name: input.name.clone(),
                        error: Some(error.to_string()),
                        ..Default::default()
                    });
                }
            }
        }
        let mut preview = VendorImportPreview {
            import_id: id.into(),
            state: Some("preview".into()),
            files,
            sheets: sheets.into_values().collect(),
            options,
            ..Default::default()
        };
        let mut cursor = 0;
        let mut writable_rows = 0;
        loop {
            let batch = {
                let mut query=stage.prepare("SELECT ordinal,row_key,source_file,sheet,row_number,cells FROM rows WHERE ordinal>?1 ORDER BY ordinal LIMIT 1000").map_err(db_error)?;
                query
                    .query_map([cursor], |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, String>(3)?,
                            crate::unsigned(r, 4)?,
                            r.get::<_, String>(5)?,
                        ))
                    })
                    .map_err(db_error)?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(db_error)?
            };
            if batch.is_empty() {
                break;
            }
            stage
                .execute_batch("SAVEPOINT vendor_match")
                .map_err(db_error)?;
            for (ordinal, key, source, sheet, row, cells) in batch {
                let info = preview
                    .sheets
                    .iter()
                    .find(|s| s.source_file == source && s.sheet == sheet)
                    .ok_or_else(|| invalid("Preview sheet mapping missing"))?;
                let (resolved, candidate_set) = matching::resolve(
                    stage,
                    VendorRowPreview {
                        row_id: key,
                        source_file: source,
                        sheet,
                        row,
                        ..Default::default()
                    },
                    &decode::<Vec<String>>(&cells)?,
                    &info.headers,
                    &info.mapping,
                    &preview.options,
                )?;
                preview.rows += 1;
                if resolved.matched_id.is_some() {
                    preview.matched += 1;
                    if !resolved.values.is_empty() {
                        writable_rows += 1;
                    }
                } else if resolved.suggested {
                    preview.suggested += 1;
                } else if resolved.candidate_count > 1 {
                    preview.ambiguous += 1;
                } else {
                    preview.unmatched += 1;
                }
                if preview.samples.len() < 100 && preview.samples.len() as u64 == preview.rows - 1 {
                    let size = preview
                        .samples
                        .iter()
                        .map(|row| json(row).map(|s| s.len() + 1))
                        .collect::<Result<Vec<_>>>()?
                        .into_iter()
                        .sum::<usize>();
                    if size + json(&resolved)?.len() + 2 <= 2 * 1024 * 1024 {
                        preview.samples.push(resolved.clone());
                    }
                }
                stage
                    .execute(
                        "UPDATE rows SET result=?1,candidate_set=?3 WHERE ordinal=?2",
                        params![matching::encoded(&resolved)?, ordinal, candidate_set],
                    )
                    .map_err(db_error)?;
                cursor = ordinal;
            }
            stage
                .execute_batch("RELEASE vendor_match")
                .map_err(db_error)?;
        }
        preview.can_commit = writable_rows > 0
            && preview.sheets.iter().all(|s| {
                s.rows == 0
                    || [&s.mapping.filename, &s.mapping.relative_path]
                        .into_iter()
                        .flatten()
                        .any(|column| {
                            s.headers
                                .iter()
                                .any(|header| matching::fold(header) == matching::fold(column))
                        })
            });
        if !preview.can_commit {
            preview.warnings.push(
                "Map a filename/path column and resolve at least one row before committing".into(),
            );
        }
        if preview.files.iter().any(|file| file.truncated_rows > 0) {
            preview
                .warnings
                .push("Rows exceeding 256 KiB were truncated at UTF-8 boundaries".into());
        }
        set_meta(stage, "preview", &preview)?;
        set_meta(stage, "cancel", &false)?;
        Ok(preview)
    }

    /// Update mappings/options from persisted source copies; committing jobs are immutable.
    pub fn vendor_import_mapping_impl(
        &self,
        id: &str,
        options: &VendorImportOptions,
    ) -> Result<VendorImportPreview> {
        let _lock = self.vendor_job_lock(id)?;
        {
            let conn = self.reader()?;
            if storage::schema_exists(&conn)?
                && conn
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM audiolib_vendor_jobs WHERE import_id=?1)",
                        [id],
                        |r| r.get::<_, bool>(0),
                    )
                    .map_err(db_error)?
            {
                return Err(invalid("A started import's mapping cannot be changed"));
            }
        }
        let mut stage = self.vendor_stage(id)?;
        self.vendor_parse_preview(id, &mut stage, options)
    }

    /// Page a continuous prefix of row details up to 2 MiB; candidates have a separate page API.
    pub fn vendor_import_rows_impl(
        &self,
        id: &str,
        offset: u64,
        limit: u64,
    ) -> Result<Vec<VendorRowPreview>> {
        if offset > i64::MAX as u64 {
            return Err(invalid("Preview row offset is too large"));
        }
        let stage = self.vendor_stage(id)?;
        let mut query = stage
            .prepare("SELECT result,candidate_set FROM rows ORDER BY ordinal LIMIT ?1 OFFSET ?2")
            .map_err(db_error)?;
        let encoded = query
            .query_map(
                params![sql_u64(limit.clamp(1, 1000))?, sql_u64(offset)?],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .map_err(db_error)?;
        let mut rows = Vec::new();
        let mut bytes = 2usize;
        for encoded in encoded {
            let (value, set) = encoded.map_err(db_error)?;
            let mut row: VendorRowPreview = decode(&value)?;
            let page = matching::inline_page(&stage, set)?;
            row.candidate_count = page.total;
            row.candidates_truncated = page.total > page.candidates.len() as u64;
            row.candidates = page.candidates;
            let length = json(&row)?.len() + 1;
            if bytes + length > 2 * 1024 * 1024 {
                if rows.is_empty() {
                    return Err(invalid("Preview row exceeds 2 MiB response limit"));
                }
                break;
            }
            bytes += length;
            rows.push(row);
        }
        Ok(rows)
    }

    /// Inspect every ambiguous candidate with exact totals and bounded responses.
    pub fn vendor_import_candidates_impl(
        &self,
        id: &str,
        row_id: &str,
        offset: u64,
        limit: u64,
    ) -> Result<VendorCandidatePage> {
        let stage = self.vendor_stage(id)?;
        let set = stage
            .query_row(
                "SELECT candidate_set FROM rows WHERE row_key=?1",
                [row_id],
                |r| r.get::<_, i64>(0),
            )
            .optional()
            .map_err(db_error)?
            .ok_or_else(|| invalid("Unknown vendor preview row"))?;
        matching::page(&stage, set, offset, limit)
    }

    /// Report durable main-library progress together with the original recoverable preview.
    pub fn vendor_import_status_impl(&self, id: &str) -> Result<VendorImportStatus> {
        let stage = self.vendor_stage(id)?;
        let mut preview: VendorImportPreview = get_meta(&stage, "preview")?;
        let mut result = None;
        let mut completed = 0;
        let mut idempotency_key = None;
        let mut error: Option<String> = optional_meta(&stage, "last_error")?;
        {
            let conn = self.reader()?;
            if storage::schema_exists(&conn)? {
                let job:Option<(String,u64,u64,u64,String,Option<String>)>=conn.query_row("SELECT state,checkpoint,written,restored,idempotency_key,last_error FROM audiolib_vendor_jobs WHERE import_id=?1",[id],|r|Ok((r.get(0)?,crate::unsigned(r,1)?,crate::unsigned(r,2)?,crate::unsigned(r,3)?,r.get(4)?,r.get(5)?))).optional().map_err(db_error)?;
                if let Some((mut state, checkpoint, written, restored, key, saved_error)) = job {
                    error = saved_error.or(error);
                    idempotency_key = Some(key);
                    if state == "running" {
                        let owner = OpenOptions::new()
                            .read(true)
                            .write(true)
                            .open(self.vendor_job_directory(id)?.join("operation.lock"));
                        if owner.is_ok_and(|file| file.try_lock().is_ok()) {
                            state = "paused".into();
                        }
                    }
                    preview.state = Some(state.clone());
                    completed = checkpoint;
                    result = Some(VendorImportResult {
                        import_id: id.into(),
                        state: Some(state),
                        written,
                        restored,
                    });
                }
            }
        }
        let cancellable = matches!(
            preview.state.as_deref(),
            Some("running" | "paused" | "preview")
        );
        Ok(VendorImportStatus {
            total: preview.rows,
            preview,
            result,
            completed,
            cancellable,
            error,
            idempotency_key,
        })
    }

    /// Request cancellation at the next committed batch boundary; retains sources/checkpoints.
    pub fn vendor_import_cancel_impl(&self, id: &str) -> Result<VendorImportStatus> {
        let stage = self.vendor_stage(id)?;
        set_meta(&stage, "cancel", &true)?;
        let active = {
            let conn = self.reader()?;
            storage::schema_exists(&conn)?
                && conn
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM audiolib_vendor_jobs WHERE import_id=?1)",
                        [id],
                        |r| r.get::<_, bool>(0),
                    )
                    .map_err(db_error)?
        };
        if !active {
            let mut preview: VendorImportPreview = get_meta(&stage, "preview")?;
            preview.state = Some("cancelled".into());
            preview.can_commit = false;
            set_meta(&stage, "preview", &preview)?;
        }
        self.vendor_import_status_impl(id)
    }

    /// List at most the newest 1000 owned jobs for this library.
    pub fn vendor_import_history_impl(&self) -> Result<VendorImportHistory> {
        let root = self.vendor_root_directory(false)?;
        let mut ids = if root.exists() {
            fs::read_dir(root)?
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.len() == 32 && name.bytes().all(|b| b.is_ascii_hexdigit()))
                .filter(|name| self.vendor_job_directory(name).is_ok())
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        ids.sort_by_key(|id| {
            std::cmp::Reverse(
                fs::metadata(self.vendor_job_directory(id).unwrap_or_default())
                    .and_then(|m| m.modified())
                    .ok(),
            )
        });
        {
            let conn = self.reader()?;
            if storage::schema_exists(&conn)? {
                let mut query=conn.prepare("SELECT import_id FROM audiolib_vendor_jobs ORDER BY job_order DESC LIMIT 1000").map_err(db_error)?;
                for id in query
                    .query_map([], |r| r.get::<_, String>(0))
                    .map_err(db_error)?
                {
                    let id = id.map_err(db_error)?;
                    if !ids.contains(&id) {
                        ids.push(id);
                    }
                }
            }
        }
        let imports = ids
            .iter()
            .take(1000)
            .map(|id| self.vendor_import_status_impl(id))
            .collect::<Result<Vec<_>>>()?;
        Ok(VendorImportHistory { imports })
    }

    /// List named templates persisted beside this library's owned previews.
    pub fn vendor_import_templates_impl(&self) -> Result<Vec<VendorMappingTemplate>> {
        let path = self.vendor_root_directory(false)?.join("templates.json");
        unlinked(&path)?;
        match fs::read_to_string(path) {
            Ok(value) => decode(&value),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(error.into()),
        }
    }

    /// Save a reusable mapping without touching audio Metadata or the business database.
    pub fn vendor_import_save_template_impl(
        &self,
        template: &VendorMappingTemplate,
    ) -> Result<VendorMappingTemplate> {
        if template.name.trim().is_empty() || template.name.len() > 200 {
            return Err(invalid(
                "Template name is required and must be at most 200 bytes",
            ));
        }
        matching::validate_options(&VendorImportOptions {
            mapping: Some(template.mapping.clone()),
            ..Default::default()
        })?;
        let root = self.vendor_root_directory(true)?;
        unlinked(&root.join("templates.lock"))?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("templates.lock"))?;
        lock.try_lock()
            .map_err(|_| Error::Busy("Vendor template store is busy".into()))?;
        let mut templates = self.vendor_import_templates_impl()?;
        let mut saved = template.clone();
        if saved.id.is_empty() {
            saved.id = uuid::Uuid::new_v4().simple().to_string();
        }
        templates.retain(|item| item.id != saved.id);
        templates.push(saved.clone());
        let mut temporary = tempfile::NamedTempFile::new_in(&root)?;
        temporary.write_all(json(&templates)?.as_bytes())?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(root.join("templates.json"))
            .map_err(|error| Error::Io(error.error))?;
        Ok(saved)
    }

    /// Commit/resume with a durable idempotency receipt and bounded transactions.
    pub fn vendor_import_commit_impl(&self, id: &str, key: &str) -> Result<VendorImportResult> {
        self.vendor_import_commit_cancellable_impl(id, key, &AtomicBool::new(false), &mut |_| {})
    }

    /// Cooperative cancellation preserves each completed batch and resumes from the same checkpoint.
    pub fn vendor_import_commit_cancellable_impl(
        &self,
        id: &str,
        key: &str,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(VendorImportStatus),
    ) -> Result<VendorImportResult> {
        if key.trim().is_empty() || key.len() > 200 || key.chars().any(char::is_control) {
            return Err(invalid(
                "A nonempty Idempotency-Key of at most 200 bytes is required",
            ));
        }
        let _lock = self.vendor_job_lock(id)?;
        // This is admission refusal, not a failed write. Preserve the original
        // rollback error/phase/checkpoint so repeated commit retries cannot erase
        // the reason that only rollback recovery is now safe. Completed receipts
        // retain their immutable replay contract inside the normal commit path.
        {
            let conn = self.reader()?;
            if storage::schema_exists(&conn)? {
                if storage::automatic_job(&conn, id)? {
                    return Err(invalid(
                        "Resume automatic table Metadata by rescanning its source directory",
                    ));
                }
                let phase: Option<(Option<String>, u64)> = conn
                    .query_row(
                        "SELECT operation,restored FROM audiolib_vendor_jobs WHERE import_id=?1 AND commit_result IS NULL",
                        [id],
                        |row| Ok((row.get(0)?, crate::unsigned(row, 1)?)),
                    )
                    .optional()
                    .map_err(db_error)?;
                if let Some((operation, restored)) = phase {
                    validate_commit_resume(operation.as_deref(), restored)?;
                }
            }
        }
        let result = self.vendor_import_commit_locked(id, key, cancel, progress);
        if let Err(error) = &result {
            self.vendor_record_failure(id, "commit", Some(key), error);
        }
        result
    }

    fn vendor_import_commit_locked(
        &self,
        id: &str,
        key: &str,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(VendorImportStatus),
    ) -> Result<VendorImportResult> {
        let stage = self.vendor_stage(id)?;
        let preview: VendorImportPreview = get_meta(&stage, "preview")?;
        let options_hash = format!("{:x}", Sha256::digest(json(&preview.options)?.as_bytes()));
        // A durable receipt needs no source payload. In particular a restored backup
        // may recover only preview_json while still owning the completed commit result.
        {
            let conn = self.reader()?;
            if storage::schema_exists(&conn)? {
                type Receipt = (String, String, Option<String>, Option<String>, u64);
                let saved:Option<Receipt>=conn.query_row("SELECT idempotency_key,options_hash,commit_result,operation,restored FROM audiolib_vendor_jobs WHERE import_id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,crate::unsigned(r,4)?))).optional().map_err(db_error)?;
                if let Some((saved_key, hash, receipt, operation, restored)) = saved {
                    if saved_key != key || hash != options_hash {
                        return Err(invalid(
                            "Idempotency-Key or mapping differs from the persisted import",
                        ));
                    }
                    if let Some(receipt) = receipt {
                        let result = decode(&receipt)?;
                        clear_error(&stage)?;
                        return Ok(result);
                    }
                    validate_commit_resume(operation.as_deref(), restored)?;
                }
            }
        }
        if !preview.can_commit {
            return Err(invalid(
                "Preview has no confirmed matches or lacks an identity-column mapping",
            ));
        }
        let total: u64 = stage
            .query_row("SELECT count(*) FROM rows", [], |r| crate::unsigned(r, 0))
            .map_err(db_error)?;
        if total != preview.rows {
            return Err(invalid(
                "Recoverable source preview is incomplete; restore its owned source files before resuming",
            ));
        }
        let mut checkpoint = 0u64;
        let mut written = 0u64;
        {
            let mut conn = self.writer()?;
            ensure_schema(&conn)?;
            let tx = conn.transaction().map_err(db_error)?;
            let owner: Option<String> = tx
                .query_row(
                    "SELECT import_id FROM audiolib_vendor_jobs WHERE idempotency_key=?1",
                    [key],
                    |r| r.get(0),
                )
                .optional()
                .map_err(db_error)?;
            if owner.as_deref().is_some_and(|owner| owner != id) {
                return Err(invalid("Idempotency-Key belongs to another preview"));
            }
            type Existing = (
                String,
                String,
                String,
                u64,
                u64,
                Option<String>,
                Option<String>,
                u64,
            );
            let existing:Option<Existing>=tx.query_row("SELECT idempotency_key,options_hash,state,checkpoint,written,commit_result,operation,restored FROM audiolib_vendor_jobs WHERE import_id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,crate::unsigned(r,3)?,crate::unsigned(r,4)?,r.get(5)?,r.get(6)?,crate::unsigned(r,7)?))).optional().map_err(db_error)?;
            if let Some((
                saved_key,
                hash,
                state,
                saved_checkpoint,
                saved_written,
                receipt,
                operation,
                restored,
            )) = existing
            {
                if saved_key != key || hash != options_hash {
                    return Err(invalid(
                        "Idempotency-Key or mapping differs from the persisted import",
                    ));
                }
                if let Some(receipt) = receipt {
                    return decode(&receipt);
                }
                validate_commit_resume(operation.as_deref(), restored)?;
                if matches!(state.as_str(), "rolling_back" | "rolled_back") {
                    return Err(invalid("This import is being rolled back"));
                }
                checkpoint = saved_checkpoint;
                written = saved_written;
                tx.execute(
                    "UPDATE audiolib_vendor_jobs SET state='running',last_error=NULL,operation='commit' WHERE import_id=?1",
                    [id],
                )
                .map_err(db_error)?;
            } else {
                tx.execute("INSERT INTO audiolib_vendor_jobs(import_id,idempotency_key,options_hash,state,created,preview_json,options_json) VALUES(?1,?2,?3,'running',?4,?5,?6)",params![id,key,options_hash,now()?,json(&preview)?,json(&preview.options)?]).map_err(db_error)?;
            }
            tx.commit().map_err(db_error)?;
            let _ = self.search_cache_bookkeeping_committed(&conn);
        }
        clear_error(&stage)?;
        // Calibrate on a small first batch before approaching the 5000-row ceiling.
        let mut batch_rows = 256u64;
        loop {
            if cancel.load(Ordering::Acquire) || get_meta::<bool>(&stage, "cancel")? {
                // Consume only this cancellation. Never reset an unobserved request
                // when starting a commit/resume: it may race the UI worker startup.
                set_meta(&stage, "cancel", &false)?;
                let conn = self.writer()?;
                conn.execute(
                    "UPDATE audiolib_vendor_jobs SET state='paused' WHERE import_id=?1",
                    [id],
                )
                .map_err(db_error)?;
                let _ = self.search_cache_bookkeeping_committed(&conn);
                return Ok(VendorImportResult {
                    import_id: id.into(),
                    state: Some("paused".into()),
                    written,
                    restored: 0,
                });
            }
            let batch = {
                let mut query=stage.prepare_cached("SELECT ordinal,result,candidate_set FROM rows WHERE ordinal>?1 ORDER BY ordinal LIMIT ?2").map_err(db_error)?;
                query
                    .query_map(params![sql_u64(checkpoint)?, sql_u64(batch_rows)?], |r| {
                        Ok((
                            crate::unsigned(r, 0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, i64>(2)?,
                        ))
                    })
                    .map_err(db_error)?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(db_error)?
            };
            if batch.is_empty() {
                break;
            }
            let started = Instant::now();
            // Decode and consult the owned preview before acquiring the main
            // writer. The per-job operation lock keeps mappings/candidates fixed.
            let batch = {
                let mut paths = stage.prepare_cached("SELECT c.display_path FROM candidate_set_members m JOIN candidates c ON c.id=m.audio_id WHERE m.set_id=?1 AND m.audio_id=?2").map_err(db_error)?;
                batch
                    .into_iter()
                    .map(|(ordinal, encoded, candidate_set)| {
                        let row: VendorRowPreview = decode(&encoded)?;
                        let expected_path = if let Some(audio_id) = row.matched_id
                            && !row.values.is_empty()
                        {
                            paths
                                .query_row(params![candidate_set, audio_id], |r| r.get(0))
                                .optional()
                                .map_err(db_error)?
                        } else {
                            None
                        };
                        Ok(CommitRow {
                            ordinal,
                            row,
                            expected_path,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?
            };
            {
                let mut conn = self.writer()?;
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(db_error)?;
                let search_cache = self.begin_search_cache_batch(&tx)?;
                let mut affected = BTreeSet::new();
                for prepared in &batch {
                    let row = &prepared.row;
                    if let Some(audio_id) = row.matched_id
                        && !row.values.is_empty()
                    {
                        let actual: Option<String> = tx
                            .prepare_cached("SELECT file_path FROM audio_files WHERE id=?1")
                            .map_err(db_error)?
                            .query_row([audio_id], |r| r.get(0))
                            .optional()
                            .map_err(db_error)?;
                        if prepared.expected_path.is_none() || actual != prepared.expected_path {
                            return Err(invalid(
                                "Audio identity/path changed after preview; create a fresh preview",
                            ));
                        }
                        if !affected.contains(&audio_id) {
                            storage::save_before(&tx, id, audio_id)?;
                        }
                        for (field, value) in &row.values {
                            storage::add_event(
                                &tx,
                                storage::EventRow {
                                    import: id,
                                    id: audio_id,
                                    field,
                                    value,
                                    source_file: &row.source_file,
                                    sheet: &row.sheet,
                                    row: row.row,
                                    overwrite: preview.options.overwrite,
                                    language: row.values.get("language").map_or("", String::as_str),
                                },
                            )?;
                        }
                        affected.insert(audio_id);
                        written += 1;
                    }
                    checkpoint = prepared.ordinal;
                }
                for audio_id in &affected {
                    storage::project(&tx, *audio_id)?;
                }
                tx.execute(
                    "UPDATE audiolib_vendor_jobs SET checkpoint=?2,written=?3 WHERE import_id=?1",
                    params![id, sql_u64(checkpoint)?, sql_u64(written)?],
                )
                .map_err(db_error)?;
                let search_cache = match self.prepare_search_cache_batch(&tx, search_cache, cancel)
                {
                    Ok(candidate) => candidate,
                    // Cache work is optional. Cancellation still commits the
                    // requested batch and pauses at the existing durable boundary.
                    Err(Error::Cancelled) => None,
                    Err(error) => return Err(error),
                };
                let search_cache = self.commit_search_cache_batch(tx, search_cache)?;
                if let Some(candidate) = search_cache
                    && self.publish_search_cache_batch(&conn, candidate).is_err()
                {
                    // A late cache publication failure cannot turn a durable
                    // business commit into a failed import. Fall back to rebuild.
                    let _ = self.reset_search_caches();
                    tracing::debug!("Vendor commit kept; search cache publication was discarded");
                }
            }
            if started.elapsed() > Duration::from_secs(2) {
                batch_rows = (batch_rows / 2).max(1);
            } else if started.elapsed() < Duration::from_secs(1) {
                batch_rows = (batch_rows + 500).min(5000);
            }
            progress(self.vendor_import_status_impl(id)?);
        }
        let result = VendorImportResult {
            import_id: id.into(),
            state: Some("committed".into()),
            written,
            restored: 0,
        };
        {
            let conn = self.writer()?;
            conn.execute("UPDATE audiolib_vendor_jobs SET state='committed',commit_result=?2,last_error=NULL,operation='commit' WHERE import_id=?1",params![id,json(&result)?]).map_err(db_error)?;
            let _ = self.search_cache_bookkeeping_committed(&conn);
        }
        progress(self.vendor_import_status_impl(id)?);
        Ok(result)
    }

    /// Roll back only this batch's immutable events and reproject remaining sources.
    /// Later imports and rescanned embedded Metadata survive, including overlapping fields.
    pub fn vendor_import_rollback_impl(&self, id: &str) -> Result<VendorImportResult> {
        let _lock = self.vendor_job_lock(id)?;
        let result = self.vendor_import_rollback_locked(id);
        if let Err(error) = &result {
            self.vendor_record_failure(id, "rollback", None, error);
        }
        result
    }

    fn vendor_import_rollback_locked(&self, id: &str) -> Result<VendorImportResult> {
        {
            let conn = self.writer()?;
            validate_schema(&conn)?;
            let exists: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM audiolib_vendor_jobs WHERE import_id=?1)",
                    [id],
                    |r| r.get(0),
                )
                .map_err(db_error)?;
            if !exists {
                return Err(invalid(
                    "This preview has no committed batches to roll back",
                ));
            }
            conn.execute("UPDATE audiolib_vendor_jobs SET state='rolling_back',last_error=NULL,operation='rollback' WHERE import_id=?1 AND state<>'rolled_back'",[id]).map_err(db_error)?;
            let _ = self.search_cache_bookkeeping_committed(&conn);
        }
        clear_error(&self.vendor_stage(id)?)?;
        loop {
            let ids = {
                let conn = self.reader()?;
                let mut query=conn.prepare("SELECT DISTINCT audio_file_id FROM audiolib_vendor_events WHERE import_id=?1 ORDER BY audio_file_id LIMIT 5000").map_err(db_error)?;
                query
                    .query_map([id], |r| r.get::<_, i64>(0))
                    .map_err(db_error)?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(db_error)?
            };
            if ids.is_empty() {
                break;
            }
            let mut conn = self.writer()?;
            let tx = conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(db_error)?;
            let search_cache = self.begin_search_cache_batch(&tx)?;
            for audio_id in &ids {
                // Before-images are retained and required even if another batch replaced the same value.
                storage::verify_before(&tx, id, *audio_id)?;
                tx.execute(
                    "DELETE FROM audiolib_vendor_events WHERE import_id=?1 AND audio_file_id=?2",
                    params![id, audio_id],
                )
                .map_err(db_error)?;
                storage::project(&tx, *audio_id)?;
            }
            tx.execute(
                "UPDATE audiolib_vendor_jobs SET restored=restored+?2 WHERE import_id=?1",
                params![id, i64::try_from(ids.len()).map_err(invalid)?],
            )
            .map_err(db_error)?;
            let search_cache =
                self.prepare_search_cache_batch(&tx, search_cache, &AtomicBool::new(false))?;
            let search_cache = self.commit_search_cache_batch(tx, search_cache)?;
            if let Some(candidate) = search_cache
                && self.publish_search_cache_batch(&conn, candidate).is_err()
            {
                let _ = self.reset_search_caches();
                tracing::debug!("Vendor rollback kept; search cache publication was discarded");
            }
        }
        let conn = self.writer()?;
        conn.execute(
            "UPDATE audiolib_vendor_jobs SET state='rolled_back',last_error=NULL,operation='rollback' WHERE import_id=?1",
            [id],
        )
        .map_err(db_error)?;
        let _ = self.search_cache_bookkeeping_committed(&conn);
        let (written, restored): (u64, u64) = conn
            .query_row(
                "SELECT written,restored FROM audiolib_vendor_jobs WHERE import_id=?1",
                [id],
                |r| Ok((crate::unsigned(r, 0)?, crate::unsigned(r, 1)?)),
            )
            .map_err(db_error)?;
        Ok(VendorImportResult {
            import_id: id.into(),
            state: Some("rolled_back".into()),
            written,
            restored,
        })
    }
}
