// crates/audiolib-db/tests/smv2/mod.rs
//! Search Metadata v2 harness (doc/TEST-SEARCH-METADATA-V2.md §4 EM, §6 MG).
//!
//! Builds a synthetic schema-8 library through the production scanner, then adds
//! non-WAV rows (for s8-compatible snapshot derivation) through raw SQL that fires the
//! production FTS triggers. Golden results live in `tests/fixtures/smv2_em_golden.json`;
//! regenerate them with `AUDIOLIB_SMV2_BLESS=1` (only at the default fixture size).
#![allow(dead_code)]
pub mod driver;
pub mod http;
pub mod schema;

use crate::support;
use audiolib_core::{DataProvider, MetadataPatch, SearchRequest};
use audiolib_db::{ScanCancellation, ScanOptions, ScanService, SqliteProvider};
use rusqlite::Connection;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

pub const ENV_FILES: &str = "AUDIOLIB_SMV2_FIXTURE_FILES";
pub const ENV_BLESS: &str = "AUDIOLIB_SMV2_BLESS";
pub const DEFAULT_FILES: usize = 400;

/// Planted description tokens (EM-06 analog of the r824 baseline queries) and the
/// modulus that decides which synthetic file carries them, plus the carrying field.
pub const PLANTED: [(&str, usize, Field); 4] = [
    ("zildjian", 7, Field::Bext),
    ("electrovoice", 11, Field::Info),
    ("sortilege", 5, Field::Ixml),
    ("personage", 3, Field::Filename),
];
pub const CAT_IDS: [&str; 3] = ["DOORWood", "EXPLReal", "AMBUrbn"];
pub const USER_TAG: &str = "usertagalpha";
pub const USER_NOTE: &str = "usernoteomega";
const FOLDERS: [&str; 5] = [
    "Drums/Cymbals",
    "Mics/Dynamic",
    "音效库/爆炸",
    "Foley/Doors",
    "Ambience/Urban",
];
const WORDS: [&str; 4] = ["hit", "swell", "tick", "rumble"];

/// EM-09 samples: full-width / half-width katakana / NFD names next to their folded
/// or composed twins. Folder names repeat the variants for future path-table checks.
pub const UNICODE_SAMPLES: [(&str, &str); 8] = [
    ("ＳＦＸ_全角", "ＥＸＰＬＯＳＩＯＮ_０１.wav"),
    ("SFX_half", "EXPLOSION_01.wav"),
    ("ＳＦＸ_全角", "ｶﾞﾗｽ_ﾜﾚ.wav"),
    ("SFX_half", "ガラス_ワレ.wav"),
    ("Cafe\u{301}_nfd", "cafe\u{301}_ambience.wav"),
    ("Café_nfc", "café_ambience.wav"),
    ("Cafe\u{301}_nfd", "\u{30AB}\u{3099}_door.wav"),
    ("Café_nfc", "\u{30AC}_door.wav"),
];
/// Queries for EM-09 (each written in every variant form). A lone kana is below the
/// two-letter minimum, so the ガ forms carry a second term.
pub const UNICODE_QUERIES: [&str; 10] = [
    "ＥＸＰＬＯＳＩＯＮ",
    "EXPLOSION",
    "ｶﾞﾗｽ",
    "ガラス",
    "cafe\u{301}",
    "café",
    "\u{30AB}\u{3099} door",
    "\u{30AC} door",
    "ＳＦＸ",
    "SFX",
];

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Field {
    Bext,
    Info,
    Ixml,
    Filename,
}

pub fn fixture_size() -> usize {
    std::env::var(ENV_FILES)
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_FILES)
}

pub fn chunk(bytes: &mut Vec<u8>, name: &[u8; 4], value: &[u8]) {
    bytes.extend(name);
    bytes.extend((value.len() as u32).to_le_bytes());
    bytes.extend(value);
    if !value.len().is_multiple_of(2) {
        bytes.push(0);
    }
}

pub fn fmt_chunk() -> Vec<u8> {
    let mut fmt = Vec::new();
    fmt.extend(1_u16.to_le_bytes());
    fmt.extend(1_u16.to_le_bytes());
    fmt.extend(48000_u32.to_le_bytes());
    fmt.extend(96000_u32.to_le_bytes());
    fmt.extend(2_u16.to_le_bytes());
    fmt.extend(16_u16.to_le_bytes());
    fmt
}

/// A bext payload of the given total size whose description field holds `description`.
pub fn bext(description: &[u8], size: usize) -> Vec<u8> {
    let mut data = vec![0_u8; size.max(description.len())];
    let n = description.len().min(256).min(data.len());
    data[..n].copy_from_slice(&description[..n]);
    data
}

/// RIFF builder; `extra` chunks follow `fmt `/`data` in order.
pub fn riff(extra: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
    let mut bytes = b"RIFF\0\0\0\0WAVE".to_vec();
    chunk(&mut bytes, b"fmt ", &fmt_chunk());
    chunk(&mut bytes, b"data", &[0; 128]);
    for (name, value) in extra {
        chunk(&mut bytes, name, value);
    }
    fix_riff_size(&mut bytes);
    bytes
}

pub fn fix_riff_size(bytes: &mut [u8]) {
    let size = bytes.len() as u32 - 8;
    bytes[4..8].copy_from_slice(&size.to_le_bytes());
}

pub fn info(entries: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
    let mut list = b"INFO".to_vec();
    for (key, value) in entries {
        let mut v = value.to_vec();
        v.push(0);
        chunk(&mut list, key, &v);
    }
    list
}

fn synthetic_wav(i: usize) -> (String, Vec<u8>) {
    let word = WORDS[i % WORDS.len()];
    let mut bext_text = format!("synthetic {word} take {i}");
    let mut info_text = format!("comment {word}");
    let mut scene = format!("scene{}", i % 9);
    let mut name = format!("smv2_{i:05}_{word}");
    for (token, modulus, field) in PLANTED {
        if i.is_multiple_of(modulus) {
            match field {
                Field::Bext => bext_text.push_str(&format!(" {token}")),
                Field::Info => info_text.push_str(&format!(" {token}")),
                Field::Ixml => scene.push_str(&format!(" {token}")),
                Field::Filename => name.push_str(&format!("_{token}")),
            }
        }
    }
    let catid = i
        .is_multiple_of(4)
        .then(|| CAT_IDS[(i / 4) % CAT_IDS.len()]);
    let mut ixml = format!("<BWFXML><SCENE>{scene}</SCENE><TAKE>{}</TAKE><USER>", i % 3);
    if let Some(catid) = catid {
        ixml.push_str(&format!(
            "<CATID>{catid}</CATID><CATEGORY>Cat{catid}</CATEGORY><SUBCATEGORY>Sub{catid}</SUBCATEGORY>"
        ));
    }
    ixml.push_str("</USER></BWFXML>");
    let bytes = riff(&[
        (b"bext", bext(bext_text.as_bytes(), 602)),
        (b"LIST", info(&[(b"ICMT", info_text.as_bytes())])),
        (b"iXML", ixml.into_bytes()),
    ]);
    (format!("{}/{name}.wav", FOLDERS[i % FOLDERS.len()]), bytes)
}

pub fn expected_planted(token: &str, files: usize) -> usize {
    let (_, modulus, _) = PLANTED.iter().find(|p| p.0 == token).unwrap();
    (0..files).filter(|i| i.is_multiple_of(*modulus)).count()
}
pub fn expected_catid(catid: &str, files: usize) -> usize {
    let k = CAT_IDS.iter().position(|c| *c == catid).unwrap();
    (0..files)
        .filter(|i| i.is_multiple_of(4) && (i / 4) % CAT_IDS.len() == k)
        .count()
}
pub fn tagged(i: usize) -> bool {
    i.is_multiple_of(13)
}
pub fn noted(i: usize) -> bool {
    i.is_multiple_of(17)
}

pub struct Fixture {
    pub dir: tempfile::TempDir,
    pub root: PathBuf,
    pub db: PathBuf,
    pub files: usize,
    pub non_wav: usize,
}

impl Fixture {
    /// Build a schema-8 library with `files` synthetic WAVs, the EM-09 Unicode samples,
    /// user tags/notes, and one `.flac`/`.ogg` clone for every tenth WAV row.
    pub fn build(files: usize) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("lib");
        for i in 0..files {
            let (rel, bytes) = synthetic_wav(i);
            write(&root.join(rel), &bytes);
        }
        for (folder, name) in UNICODE_SAMPLES {
            let body = riff(&[(b"LIST", info(&[(b"ICMT", b"unicode sample")]))]);
            write(&root.join(folder).join(name), &body);
        }
        let db = dir.path().join("library.db");
        {
            let provider = Arc::new(SqliteProvider::open(&db).unwrap());
            let progress = ScanService::new(provider.clone())
                .scan(
                    &root,
                    &ScanOptions::default(),
                    &ScanCancellation::default(),
                    |_| {},
                )
                .unwrap();
            assert_eq!(progress.errors, 0, "fixture scan errors");
            for i in (0..files).filter(|i| tagged(*i) || noted(*i)) {
                let (rel, _) = synthetic_wav(i);
                let file = provider
                    .find_audio_by_path(&root.join(rel))
                    .unwrap()
                    .expect("scanned fixture row");
                provider
                    .update_metadata(
                        file.id,
                        &MetadataPatch {
                            tags: tagged(i).then(|| USER_TAG.to_string()),
                            notes: noted(i).then(|| USER_NOTE.to_string()),
                            ..Default::default()
                        },
                    )
                    .unwrap();
            }
        }
        let non_wav = add_non_wav_rows(&db);
        Fixture {
            dir,
            root,
            db,
            files,
            non_wav,
        }
    }

    pub fn default_size() -> Fixture {
        Fixture::build(fixture_size())
    }

    pub fn provider(&self) -> SqliteProvider {
        SqliteProvider::open_read_only(&self.db).unwrap()
    }

    /// Copy the DB (and any WAL) to a new temp dir for a destructive scenario.
    pub fn copy_db(&self) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("library.db");
        checkpoint(&self.db);
        fs::copy(&self.db, &target).unwrap();
        (dir, target)
    }

    pub fn rel(&self, path: &str) -> String {
        let p = Path::new(path);
        p.strip_prefix(&self.root)
            .unwrap_or(p)
            .to_string_lossy()
            .replace('\\', "/")
    }
}

fn write(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

pub fn checkpoint(db: &Path) {
    if let Ok(conn) = Connection::open(db) {
        let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    }
}

/// Clone every tenth synthetic WAV (by file index, not row id, so goldens are stable
/// regardless of scan order) as `.flac` / `.ogg` (alternating). The production
/// INSERT trigger indexes them, so s8-snapshot derivation (SC-02) has rows to filter.
fn add_non_wav_rows(db: &Path) -> usize {
    let conn = support::connection(db).unwrap();
    let columns: Vec<String> = conn
        .prepare("SELECT name FROM pragma_table_info('audio_files') WHERE name<>'id' ORDER BY cid")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let mut added = 0;
    for ext in ["flac", "ogg"] {
        let parity = if ext == "flac" { 0 } else { 1 };
        let select = columns
            .iter()
            .map(|c| {
                let q = format!("\"{c}\"");
                if c == "filename" || c.contains("path") {
                    format!("CASE WHEN typeof({q})='text' THEN replace({q},'.wav','.{ext}') ELSE {q} END")
                } else {
                    q
                }
            })
            .collect::<Vec<_>>()
            .join(",");
        let names = columns
            .iter()
            .map(|c| format!("\"{c}\""))
            .collect::<Vec<_>>()
            .join(",");
        added += conn
            .execute(
                &format!(
                    "INSERT INTO audio_files({names}) SELECT {select} FROM audio_files \
                     WHERE filename GLOB 'smv2_*.wav' \
                     AND CAST(substr(filename,6,5) AS INTEGER) % 20 = {parity} * 10 ORDER BY filename"
                ),
                [],
            )
            .unwrap();
    }
    added
}

/// All hits for `query` as (id, relative path), following offset pages.
pub fn hits(provider: &SqliteProvider, fixture: &Fixture, query: &str) -> Vec<(i64, String)> {
    let mut out = Vec::new();
    loop {
        let page = provider
            .search(&SearchRequest {
                query: query.into(),
                limit: 500,
                offset: out.len(),
                ..Default::default()
            })
            .unwrap();
        let n = page.results.len();
        out.extend(
            page.results
                .into_iter()
                .map(|f| (f.id, fixture.rel(&f.file_path))),
        );
        if n == 0 || out.len() as u64 >= page.total_count {
            break;
        }
    }
    out
}

pub fn path_set(hits: &[(i64, String)]) -> BTreeSet<String> {
    hits.iter().map(|h| h.1.clone()).collect()
}
pub fn id_set(hits: &[(i64, String)]) -> BTreeSet<i64> {
    hits.iter().map(|h| h.0).collect()
}

pub fn sha_lines<I: IntoIterator<Item = String>>(lines: I) -> String {
    let mut h = Sha256::new();
    for l in lines {
        h.update(l.as_bytes());
        h.update(b"\n");
    }
    format!("{:x}", h.finalize())
}

/// Hash of the main FTS shadow tables (MG-10: must not change across migration).
pub fn main_fts_shadow_hash(conn: &Connection) -> String {
    let mut lines = Vec::new();
    for (table, order) in [
        ("audio_search_data", "id"),
        ("audio_search_idx", "segid,term"),
        ("audio_search_docsize", "id"),
        ("audio_search_config", "k"),
    ] {
        let mut stmt = conn
            .prepare(&format!("SELECT * FROM {table} ORDER BY {order}"))
            .unwrap();
        let cols = stmt.column_count();
        let rows = stmt
            .query_map([], |r| {
                let mut s = format!("{table}|");
                for c in 0..cols {
                    let v: rusqlite::types::Value = r.get(c)?;
                    s.push_str(&format!("{v:?}|"));
                }
                Ok(s)
            })
            .unwrap();
        for row in rows {
            lines.push(row.unwrap());
        }
    }
    sha_lines(lines)
}

/// Semantic token content of an FTS5 table, independent of segment layout:
/// sorted (term, rowid, column, offset) from an fts5vocab `instance` table.
pub fn fts_instance_hash(conn: &Connection, table: &str) -> String {
    let vocab = format!("temp.smv2_vocab_{table}");
    conn.execute_batch(&format!(
        "DROP TABLE IF EXISTS {vocab}; CREATE VIRTUAL TABLE {vocab} USING fts5vocab(main, {table}, instance);"
    ))
    .unwrap();
    let lines: Vec<String> = conn
        .prepare(&format!(
            "SELECT term,doc,col,offset FROM {vocab} ORDER BY term,doc,col,offset"
        ))
        .unwrap()
        .query_map([], |r| {
            Ok(format!(
                "{}|{}|{}|{}",
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    sha_lines(lines)
}

pub fn file_sha(path: &Path) -> String {
    format!("{:x}", Sha256::digest(fs::read(path).unwrap()))
}

pub fn user_version(db: &Path) -> i64 {
    Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap()
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap()
}

pub fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE name=?1",
        [name],
        |r| r.get::<_, i64>(0),
    )
    .unwrap()
        > 0
}

// ---------------------------------------------------------------- golden file

pub fn golden_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/smv2_em_golden.json")
}

/// Compare `value` with golden `key`. At a non-default fixture size the golden is
/// not applicable and only the in-test invariants are checked. With
/// `AUDIOLIB_SMV2_BLESS=1` the key is (re)written instead.
pub fn golden(key: &str, value: Value) {
    if fixture_size() != DEFAULT_FILES {
        eprintln!("golden {key}: skipped at non-default fixture size");
        return;
    }
    let path = golden_path();
    let _guard = GOLDEN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut doc: Map<String, Value> = fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    if std::env::var(ENV_BLESS).as_deref() == Ok("1") {
        doc.insert(key.into(), value);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            serde_json::to_string_pretty(&Value::Object(doc)).unwrap() + "\n",
        )
        .unwrap();
        return;
    }
    let expected = doc
        .get(key)
        .unwrap_or_else(|| panic!("golden key {key} missing; run with {ENV_BLESS}=1"));
    assert_eq!(expected, &value, "golden mismatch for {key}");
}
static GOLDEN_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn hits_json(hits: &[(i64, String)]) -> Value {
    let paths = path_set(hits);
    json!({ "count": hits.len(), "paths_sha256": sha_lines(paths.iter().cloned()) })
}
