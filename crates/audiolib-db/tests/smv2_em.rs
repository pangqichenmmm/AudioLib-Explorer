// crates/audiolib-db/tests/smv2_em.rs
//! EM-01..EM-09 (doc/TEST-SEARCH-METADATA-V2.md §4): existing-Metadata regression.
//!
//! `em_*` tests run against the current code and pin the schema-8 behaviour in
//! `tests/fixtures/smv2_em_golden.json`. The same tests must stay green once the
//! schema-9 code lands in this crate. `em_*_s9_*` tests compare an s8 library with
//! its migrated copy and need the migration entrypoint (see `smv2/driver.rs`).
mod smv2;
mod support;

use audiolib_core::DataProvider;
use audiolib_db::{ScanCancellation, ScanOptions, ScanService, SqliteProvider};
use serde_json::{Value, json};
use smv2::{driver::Driver, *};
use std::{
    collections::BTreeSet,
    fs,
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

fn fixture() -> &'static Fixture {
    static F: OnceLock<Fixture> = OnceLock::new();
    F.get_or_init(Fixture::default_size)
}

/// Probe crafted bytes and scan them into a throwaway library; neither may panic.
fn probe_and_scan(name: &str, bytes: &[u8]) -> Value {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("in");
    fs::create_dir(&root).unwrap();
    let path = root.join(format!("{name}.wav"));
    fs::write(&path, bytes).unwrap();
    probe_and_scan_path(&root, &path)
}

fn probe_and_scan_path(root: &Path, path: &Path) -> Value {
    let started = Instant::now();
    let probed = catch_unwind(AssertUnwindSafe(|| audiolib_audio::probe(path)))
        .unwrap_or_else(|_| panic!("probe panicked on {}", path.display()));
    let probe_ms = started.elapsed();
    let probe = match &probed {
        Ok(m) => json!({
            "ok": true,
            "bext_description": m.bext_description,
            "attributes_sha256": sha_lines([m.attributes.to_string()]),
        }),
        Err(_) => json!({ "ok": false }),
    };
    let db = root.parent().unwrap().join("probe.db");
    let provider = Arc::new(SqliteProvider::open(&db).unwrap());
    let progress = catch_unwind(AssertUnwindSafe(|| {
        ScanService::new(provider.clone())
            .scan(
                root,
                &ScanOptions::default(),
                &ScanCancellation::default(),
                |_| {},
            )
            .unwrap()
    }))
    .unwrap_or_else(|_| panic!("scan panicked on {}", path.display()));
    assert!(
        probe_ms < Duration::from_secs(20),
        "probe too slow: {probe_ms:?}"
    );
    json!({ "probe": probe, "scan_added": progress.added, "scan_errors": progress.errors })
}

fn attributes(bytes: &[u8]) -> String {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.wav");
    fs::write(&path, bytes).unwrap();
    audiolib_audio::probe(&path).unwrap().attributes.to_string()
}

fn wav_with_raw_chunk(
    name: &[u8; 4],
    declared: u32,
    payload: &[u8],
    trailer: &[(&[u8; 4], Vec<u8>)],
) -> Vec<u8> {
    let mut bytes = riff(trailer);
    bytes.extend(name);
    bytes.extend(declared.to_le_bytes());
    bytes.extend(payload);
    fix_riff_size(&mut bytes);
    bytes
}

// ------------------------------------------------------------------ EM-01..05

#[test]
fn em_01_corrupt_bext_matches_s8_and_never_panics() {
    let survives = info(&[(b"ICMT", b"infosurvives")]);
    let cases: Vec<(&str, Vec<u8>)> = vec![
        // Declared length runs past EOF.
        (
            "bext_len_past_eof",
            wav_with_raw_chunk(
                b"bext",
                0xFFFF_FF00,
                &bext(b"overrun", 602),
                &[(b"LIST", survives.clone())],
            ),
        ),
        // Shorter than the 602-byte fixed header.
        (
            "bext_short",
            riff(&[(b"bext", vec![b'x'; 100]), (b"LIST", survives.clone())]),
        ),
        // Fixed header with invalid UTF-8 and no NUL terminator.
        (
            "bext_invalid_utf8",
            riff(&[
                (b"bext", bext(&[0xFF; 256], 602)),
                (b"LIST", survives.clone()),
            ]),
        ),
        // Odd declared size with missing pad byte, then a valid chunk.
        ("bext_odd_unpadded", {
            let mut b = riff(&[]);
            b.extend(b"bext");
            b.extend(603_u32.to_le_bytes());
            b.extend(bext(b"odd", 603));
            chunk(&mut b, b"LIST", &survives);
            fix_riff_size(&mut b);
            b
        }),
    ];
    let mut out = serde_json::Map::new();
    for (name, bytes) in cases {
        out.insert(name.into(), probe_and_scan(name, &bytes));
    }
    golden("EM-01", Value::Object(out));
}

#[test]
fn em_02_malformed_ixml_is_skipped_and_other_chunks_extract() {
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("ixml_unterminated", b"<BWFXML><SCENE>broken".to_vec()),
        ("ixml_bad_encoding", b"<?xml version=\"1.0\" encoding=\"Shift_JIS\"?><BWFXML><SCENE>\x82\xFF\xFF</SCENE></BWFXML>".to_vec()),
        ("ixml_binary", vec![0xFE; 300]),
        ("ixml_utf16_odd", vec![b'<', 0, b'B', 0, b'W']),
    ];
    let mut out = serde_json::Map::new();
    for (name, ixml) in cases {
        let bytes = riff(&[
            (b"bext", bext(b"bextsurvives", 602)),
            (b"iXML", ixml),
            (b"LIST", info(&[(b"ICMT", b"infosurvives")])),
        ]);
        let attrs = attributes(&bytes);
        assert!(attrs.contains("infosurvives"), "{name}: INFO lost: {attrs}");
        assert!(attrs.contains("bextsurvives"), "{name}: bext lost: {attrs}");
        out.insert(name.into(), probe_and_scan(name, &bytes));
    }
    golden("EM-02", Value::Object(out));
}

fn id3_tag(frames: &[u8]) -> Vec<u8> {
    let mut tag = b"ID3\x03\x00\x00".to_vec();
    let n = frames.len() as u32;
    tag.extend([
        (n >> 21) as u8 & 0x7F,
        (n >> 14) as u8 & 0x7F,
        (n >> 7) as u8 & 0x7F,
        n as u8 & 0x7F,
    ]);
    tag.extend(frames);
    tag
}
fn id3_frame(id: &[u8; 4], declared: u32, body: &[u8]) -> Vec<u8> {
    let mut f = id.to_vec();
    f.extend(declared.to_be_bytes());
    f.extend([0, 0]);
    f.extend(body);
    f
}

#[test]
fn em_03_corrupt_id3_comm_txxx_never_panics() {
    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "comm_size_overflow",
            id3_tag(&id3_frame(b"COMM", 0x7FFF_FFFF, b"\x00eng\x00short")),
        ),
        (
            "comm_truncated_lang",
            id3_tag(&id3_frame(b"COMM", 3, b"\x00en")),
        ),
        ("txxx_zero", id3_tag(&id3_frame(b"TXXX", 0, b""))),
        (
            "txxx_no_separator",
            id3_tag(&id3_frame(b"TXXX", 6, b"\x00descr")),
        ),
        (
            "txxx_bad_utf16",
            id3_tag(&id3_frame(b"TXXX", 5, b"\x01\xFF\xFE\x00")),
        ),
        ("tag_size_lies", {
            let mut t = id3_tag(&id3_frame(b"COMM", 10, b"\x00engx\x00ok"));
            t[6..10].copy_from_slice(&[0x7F; 4]);
            t
        }),
    ];
    let mut out = serde_json::Map::new();
    for (name, tag) in cases {
        let bytes = riff(&[
            (b"id3 ", tag),
            (b"LIST", info(&[(b"ICMT", b"infosurvives")])),
        ]);
        let attrs = attributes(&bytes);
        assert!(attrs.contains("infosurvives"), "{name}: INFO lost: {attrs}");
        out.insert(name.into(), probe_and_scan(name, &bytes));
    }
    golden("EM-03", Value::Object(out));
}

#[test]
fn em_04_legacy_encodings_decode_like_s8() {
    // GBK 爆炸, Shift-JIS 効果音, Latin-1 café.
    let samples: [(&str, &[u8]); 3] = [
        ("gbk", &[0xB1, 0xAC, 0xD5, 0xA8]),
        ("shift_jis", &[0x8C, 0xF8, 0x89, 0xCA, 0x89, 0xB9]),
        ("latin1", &[0x63, 0x61, 0x66, 0xE9]),
    ];
    let mut out = serde_json::Map::new();
    for (name, raw) in samples {
        let bytes = riff(&[
            (b"bext", bext(raw, 602)),
            (b"LIST", info(&[(b"ICMT", raw)])),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("e.wav");
        fs::write(&path, &bytes).unwrap();
        let m = audiolib_audio::probe(&path).unwrap();
        out.insert(
            name.into(),
            json!({ "bext_description": m.bext_description, "attributes": m.attributes }),
        );
    }
    golden("EM-04", Value::Object(out));
}

#[test]
fn em_05_oversized_chunks_are_bounded() {
    let mut out = serde_json::Map::new();
    // Declared ≥100 MB, physically tiny.
    let lie = wav_with_raw_chunk(b"LIST", 200 * 1024 * 1024, &info(&[(b"ICMT", b"lie")]), &[]);
    out.insert("list_declared_200mb".into(), probe_and_scan("lie", &lie));
    let lie = wav_with_raw_chunk(b"iXML", 0xFFFF_FFF0, b"<BWFXML/>", &[]);
    out.insert("ixml_declared_4gb".into(), probe_and_scan("lie4g", &lie));
    // Physically 101 MB chunks: an unknown chunk and an over-budget iXML, each
    // followed by INFO. Records whether extraction continues past them.
    for (name, id) in [("junk_101mb", b"junk"), ("ixml_101mb", b"iXML")] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("in");
        fs::create_dir(&root).unwrap();
        let path = root.join(format!("{name}.wav"));
        let mut bytes = riff(&[]);
        let size = 101 * 1024 * 1024_u32;
        bytes.extend(id);
        bytes.extend(size.to_le_bytes());
        let header = bytes.len();
        bytes.resize(header + size as usize, b' ');
        chunk(&mut bytes, b"LIST", &info(&[(b"ICMT", b"afterbig")]));
        fix_riff_size(&mut bytes);
        fs::write(&path, &bytes).unwrap();
        drop(bytes);
        let mut v = probe_and_scan_path(&root, &path);
        v["after_chunk_extracted"] =
            json!(attributes(&fs::read(&path).unwrap()).contains("afterbig"));
        out.insert(name.into(), v);
    }
    golden("EM-05", Value::Object(out));
}

// ------------------------------------------------------------------ EM-06..09

fn wav_only(hits: &[(i64, String)]) -> Vec<(i64, String)> {
    hits.iter()
        .filter(|h| h.1.ends_with(".wav"))
        .cloned()
        .collect()
}

#[test]
fn em_06_baseline_hit_counts_and_id_sets() {
    let f = fixture();
    let p = f.provider();
    let mut out = serde_json::Map::new();
    for (token, _, _) in PLANTED {
        let all = hits(&p, f, token);
        assert_eq!(
            wav_only(&all).len(),
            expected_planted(token, f.files),
            "{token}"
        );
        assert!(
            all.len() > wav_only(&all).len() || f.files < 20,
            "{token}: non-WAV clones not indexed"
        );
        out.insert(token.into(), hits_json(&all));
    }
    golden("EM-06", Value::Object(out));
}

/// Optional: exact r824 counts on a read-only snapshot copy.
#[test]
#[ignore = "needs AUDIOLIB_SMV2_BASELINE_DB (copy of the r824 snapshot)"]
fn em_06_r824_snapshot_counts() {
    let db = std::env::var("AUDIOLIB_SMV2_BASELINE_DB").expect("AUDIOLIB_SMV2_BASELINE_DB");
    let p = SqliteProvider::open_read_only(Path::new(&db)).unwrap();
    for (q, n) in [
        ("zildjian", 323),
        ("electrovoice", 228),
        ("sortilege", 2776),
        ("personage", 5943),
    ] {
        let total = p
            .search(&audiolib_core::SearchRequest {
                query: q.into(),
                ..Default::default()
            })
            .unwrap()
            .total_count;
        assert_eq!(total, n, "{q}");
    }
}

#[test]
fn em_07_ucs_catid_tokens_and_counts() {
    let f = fixture();
    let p = f.provider();
    let conn = support::connection(&f.db).unwrap();
    conn.execute_batch("CREATE VIRTUAL TABLE temp.em07 USING fts5vocab(main, audio_search, col);")
        .unwrap();
    let mut out = serde_json::Map::new();
    for catid in CAT_IDS {
        let all = hits(&p, f, catid);
        assert_eq!(
            wav_only(&all).len(),
            expected_catid(catid, f.files),
            "{catid}"
        );
        let lower = catid.to_lowercase();
        let tokens: Vec<Value> = conn
            .prepare("SELECT term,col,doc,cnt FROM temp.em07 WHERE term LIKE ?1 || '%' OR term LIKE '%' || ?2 || '%' ORDER BY term,col")
            .unwrap()
            .query_map([lower.clone(), format!("cat{lower}")], |r| {
                Ok(json!([r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?]))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        out.insert(
            catid.into(),
            json!({ "hits": hits_json(&all), "tokens": tokens }),
        );
    }
    golden("EM-07", Value::Object(out));
}

#[test]
fn em_08_user_tags_and_notes_search() {
    let f = fixture();
    let p = f.provider();
    let tags = hits(&p, f, USER_TAG);
    let notes = hits(&p, f, USER_NOTE);
    assert_eq!(
        wav_only(&tags).len(),
        (0..f.files).filter(|i| tagged(*i)).count()
    );
    assert_eq!(
        wav_only(&notes).len(),
        (0..f.files).filter(|i| noted(*i)).count()
    );
    golden(
        "EM-08",
        json!({ "tags": hits_json(&tags), "notes": hits_json(&notes) }),
    );
}

#[test]
fn em_09_fullwidth_nfd_main_table_s8_golden() {
    let f = fixture();
    let p = f.provider();
    let mut out = serde_json::Map::new();
    for q in UNICODE_QUERIES {
        let hs = hits(&p, f, q);
        let names: BTreeSet<String> = hs
            .iter()
            .map(|h| h.1.rsplit('/').next().unwrap().to_string())
            .collect();
        out.insert(
            q.escape_unicode().to_string(),
            json!({ "query": q, "count": hs.len(), "filenames": names }),
        );
    }
    golden("EM-09", Value::Object(out));
}

// --------------------------------------------- s8 vs migrated s9 comparisons

/// Every query the EM cases pin, for the s8→s9 comparison.
fn em_queries() -> Vec<String> {
    let mut q: Vec<String> = PLANTED.iter().map(|p| p.0.to_string()).collect();
    q.extend(CAT_IDS.iter().map(|c| c.to_string()));
    q.extend([USER_TAG.to_string(), USER_NOTE.to_string()]);
    q.extend(UNICODE_QUERIES.iter().map(|c| c.to_string()));
    q
}

/// (s8 hits, s9 hits) per query on a migrated copy of the fixture.
type Hits = Vec<(i64, String)>;

fn s8_s9_pairs() -> Vec<(String, Hits, Hits)> {
    let driver = Driver::require();
    let f = fixture();
    let (_dir, db) = f.copy_db();
    let s8 = SqliteProvider::open_read_only(&db).unwrap();
    let before: Vec<_> = em_queries()
        .into_iter()
        .map(|q| {
            let h = hits(&s8, f, &q);
            (q, h)
        })
        .collect();
    drop(s8);
    driver.migrate_to_done(&db);
    let s9 = SqliteProvider::open_read_only(&db).unwrap();
    before
        .into_iter()
        .map(|(q, h8)| {
            let h9 = hits(&s9, f, &q);
            (q, h8, h9)
        })
        .collect()
}

#[test]
#[ignore = "pending server migration branch (audiolib-migrate, feature test-hooks)"]
fn em_06_s9_hits_are_superset_of_s8() {
    for (q, h8, h9) in s8_s9_pairs().into_iter().take(PLANTED.len()) {
        let (a, b) = (id_set(&h8), id_set(&h9));
        assert!(
            a.is_subset(&b),
            "{q}: s8 hits missing in s9: {:?}",
            a.difference(&b).collect::<Vec<_>>()
        );
        for extra in b.difference(&a) {
            eprintln!("EM-06 {q}: new s9 hit id {extra}");
        }
    }
}

#[test]
#[ignore = "pending server migration branch (audiolib-migrate, feature test-hooks)"]
fn em_07_08_s9_main_scope_identical() {
    for (q, h8, h9) in s8_s9_pairs()
        .into_iter()
        .skip(PLANTED.len())
        .take(CAT_IDS.len() + 2)
    {
        assert_eq!(id_set(&h8), id_set(&h9), "{q}");
    }
}

#[test]
#[ignore = "pending server migration branch (audiolib-migrate, feature test-hooks)"]
fn em_09_s9_main_table_identical_to_s8() {
    // Default/omit scope is main (= s8). Compare through the entrypoint's main-only query
    // (HTTP MAIN_SEARCH omits scope; equivalent to scope=main).
    let driver = Driver::require();
    let f = fixture();
    let (_dir, db) = f.copy_db();
    let s8 = SqliteProvider::open_read_only(&db).unwrap();
    let before: Vec<_> = UNICODE_QUERIES
        .iter()
        .map(|q| id_set(&hits(&s8, f, q)))
        .collect();
    drop(s8);
    driver.migrate_to_done(&db);
    for (q, ids8) in UNICODE_QUERIES.iter().zip(before) {
        let ids9 = driver.search("main_search", &db, q);
        assert_eq!(ids8, ids9, "{q}");
    }
}

#[test]
#[ignore = "pending server migration branch (audiolib-migrate, feature test-hooks)"]
fn em_09_path_scope_matches_every_variant() {
    let driver = Driver::require();
    let f = fixture();
    let (_dir, db) = f.copy_db();
    driver.migrate_to_done(&db);
    let ids_for = |q: &str| -> BTreeSet<i64> { driver.search("path_search", &db, q) };
    // Folder twins: full-width/half-width and NFD/NFC spellings must coincide.
    for (a, b) in [("ＳＦＸ", "SFX"), ("Cafe\u{301}", "Café")] {
        let (x, y) = (ids_for(a), ids_for(b));
        assert!(!x.is_empty(), "{a}: no path hits");
        assert_eq!(x, y, "{a} vs {b}");
    }
}
