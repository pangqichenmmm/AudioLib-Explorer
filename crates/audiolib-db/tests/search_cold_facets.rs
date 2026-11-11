// crates/audiolib-db/tests/search_cold_facets.rs
//! Cold directory membership must preserve complete filters and the later BM25 page.
use audiolib_core::{
    DataProvider, Error, FileVisibility, MaintenanceAction, SearchFilters, SearchRequest, SortOrder,
};
use audiolib_db::SqliteProvider;
use rusqlite::{Connection, params};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
};

// Fixed native-FTS oracle for this fixture's contiguous fragment. This is not a
// call back into either the query compiler or the directory counting helpers.
const PLOS: &str = "({filename user_tags user_notes bwf_description ixml_scene ixml_take} : \"plos\" OR {filename_tokenized user_tags_tokenized user_notes_tokenized bwf_description_tokenized ixml_scene_tokenized ixml_take_tokenized} : \"g00007000006c g00006c00006f g00006f000073\")";
const BOOM: &str = "({filename user_tags user_notes bwf_description ixml_scene ixml_take} : \"boom\" OR {filename_tokenized user_tags_tokenized user_notes_tokenized bwf_description_tokenized ixml_scene_tokenized ixml_take_tokenized} : \"g00006200006f g00006f00006f g00006f00006d\")";

fn fixture(extra_candidates: bool) -> (tempfile::TempDir, PathBuf, SqliteProvider) {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("cold-facets.db");
    let legacy = directory.path().join("legacy-nullable.db");
    let conn = Connection::open(&legacy).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    // Retain legacy NULL and nonbinary visibility rather than normalizing them.
    conn.execute_batch(
        "CREATE TABLE audio_files(id INTEGER PRIMARY KEY,file_path TEXT NOT NULL UNIQUE,
         filename TEXT NOT NULL,folder_id INTEGER,missing INTEGER);",
    )
    .unwrap();
    drop(conn);
    audiolib_db::import_legacy(&legacy, &database).unwrap();
    let provider = SqliteProvider::open(&database).unwrap();
    let mut conn = Connection::open(&database).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    conn.execute_batch(
        "INSERT INTO folders(id,folder_path,folder_name,parent_id) VALUES
         (1,'root','root',NULL),(2,'root/a','a',1),(3,'root/b','b',1);
         INSERT INTO folder_closure VALUES
         (1,1,0),(2,2,0),(3,3,0),(1,2,1),(1,3,1);",
    )
    .unwrap();
    let tx = conn.transaction().unwrap();
    for (id, folder, missing, rate, duration, favorite) in [
        (i64::MIN, Some(2), Some(0), 48_000, 1.0, 1),
        (-7, Some(3), Some(1), 48_000, 2.0, 1),
        (0, Some(2), None, 48_000, 3.0, 1),
        (4, Some(3), Some(2), 48_000, 1.0, 1),
        (7, Some(3), Some(0), 48_000, 1.0, 1),
        (10, Some(2), Some(0), 96_000, 8.0, 0),
        (i64::MAX, None, Some(0), 48_000, 1.0, 1),
    ] {
        let filename = if id == 10 {
            "embedded.wav"
        } else {
            "explosion.wav"
        };
        let metadata = if id == 10 {
            r#"{"wav_metadata":{"fields":{"keywords":"explosion"}}}"#
        } else {
            "{}"
        };
        let notes = if id == i64::MIN {
            format!("boom {}", "padding ".repeat(80))
        } else if id == 7 {
            "boom boom boom".into()
        } else {
            String::new()
        };
        tx.execute(
            "INSERT INTO audio_files(id,file_path,filename,folder_id,missing,sample_rate,
             duration,favorite,user_notes,metadata_json) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                id,
                format!("fixture/{id}.wav"),
                filename,
                folder,
                missing,
                rate,
                duration,
                favorite,
                notes,
                metadata
            ],
        )
        .unwrap();
    }
    let last = if extra_candidates { 8500 } else { 130 };
    tx.execute(
        "WITH RECURSIVE ids(id) AS (
           VALUES(100) UNION ALL SELECT id+1 FROM ids WHERE id<?1
         ) INSERT INTO audio_files(id,file_path,filename,folder_id,missing,sample_rate,
             duration,favorite) SELECT id,'wood/'||id||'.wav','wood.wav',2,0,48000,1.0,1 FROM ids",
        [last],
    )
    .unwrap();
    if extra_candidates {
        // This matching row lies after the eligible nonmatching prefix. A
        // truncated candidate result would silently omit its directory count.
        tx.execute(
            "INSERT INTO audio_files(id,file_path,filename,folder_id,missing,sample_rate,duration,favorite)
             VALUES(9000,'fixture/late.wav','explosion.wav',3,0,48000,1.0,1)",
            [],
        ).unwrap();
    }
    tx.commit().unwrap();
    drop(conn);
    provider
        .maintain_library(MaintenanceAction::RebuildSearch)
        .unwrap();
    (directory, database, provider)
}

fn expression(request: &SearchRequest) -> String {
    match request.query.as_str() {
        "plos" => PLOS.into(),
        "plos boom" => format!("{PLOS} AND {BOOM}"),
        other => panic!("Unexpected fixed-oracle query {other}"),
    }
}

fn request() -> SearchRequest {
    SearchRequest {
        query: "plos".into(),
        ..Default::default()
    }
}

fn oracle(database: &Path, request: &SearchRequest, folders: bool) -> Vec<(i64, u64)> {
    let conn = Connection::open(database).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    let visibility = match request.filters.visibility {
        FileVisibility::Present => "af.missing=0",
        FileVisibility::Missing => "af.missing=1",
        FileVisibility::All => "1=1",
    };
    let projection = if folders {
        "SELECT fc.ancestor_id,COUNT(*)"
    } else {
        "SELECT af.id,1"
    };
    let join = if folders {
        "JOIN folder_closure fc ON fc.descendant_id=af.folder_id"
    } else {
        ""
    };
    let order = if folders {
        "GROUP BY fc.ancestor_id ORDER BY fc.ancestor_id"
    } else {
        "ORDER BY bm25(audio_search),af.id ASC"
    };
    conn.prepare(&format!(
        "{projection} FROM audio_search JOIN audio_files af ON af.id=audio_search.rowid
         {join} WHERE audio_search MATCH ?1 AND {visibility}
         AND (?2 IS NULL OR af.sample_rate=?2)
         AND (?3 IS NULL OR af.duration>=?3) AND (?4 IS NULL OR af.duration<=?4)
         AND (?5=0 OR af.favorite=1) {order}"
    ))
    .unwrap()
    .query_map(
        params![
            expression(request),
            request.filters.sample_rate.map(i64::from),
            request.filters.duration_min,
            request.filters.duration_max,
            request.filters.favorites_only
        ],
        |row| Ok((row.get(0)?, row.get::<_, i64>(1)?)),
    )
    .unwrap()
    .map(|row| {
        let (id, count) = row.unwrap();
        (id, u64::try_from(count).unwrap())
    })
    .collect()
}

fn counts(provider: &SqliteProvider, request: &SearchRequest) -> BTreeMap<i64, u64> {
    provider
        .search_folders(request)
        .unwrap()
        .into_iter()
        .map(|folder| (folder.id, folder.file_count))
        .collect()
}

#[test]
fn cold_fragment_facets_never_publish_unscored_ids_as_a_bm25_ranking() {
    let (_directory, database, provider) = fixture(false);
    for query in ["plos", "plos boom"] {
        for visibility in [
            FileVisibility::All,
            FileVisibility::Present,
            FileVisibility::Missing,
        ] {
            let scoped = SearchRequest {
                query: query.into(),
                folder_id: Some(3),
                include_subfolders: false,
                limit: 1,
                offset: 999,
                order_by: SortOrder::Filename,
                descending: true,
                filters: SearchFilters {
                    visibility,
                    ..Default::default()
                },
                ..Default::default()
            };
            assert_eq!(
                counts(&provider, &scoped),
                oracle(&database, &scoped, true).into_iter().collect()
            );
        }
        let page_request = SearchRequest {
            query: query.into(),
            filters: SearchFilters {
                visibility: FileVisibility::All,
                ..Default::default()
            },
            ..Default::default()
        };
        let expected: Vec<_> = oracle(&database, &page_request, false)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        let mut rowid_order = expected.clone();
        rowid_order.sort_unstable();
        assert_ne!(
            expected, rowid_order,
            "fixture must distinguish scores from rowid order"
        );
        let page = provider.search(&page_request).unwrap();
        assert!(
            !page.cache_hit,
            "cold facets must not populate the ranking cache"
        );
        assert_eq!(page.total_count, expected.len() as u64);
        assert_eq!(
            page.results.iter().map(|file| file.id).collect::<Vec<_>>(),
            expected
        );
        assert!(provider.search(&page_request).unwrap().cache_hit);
    }
}

#[test]
fn cold_cover_filters_and_general_candidate_overflow_keep_every_predicate() {
    let (_directory, database, provider) = fixture(true);
    for favorites_only in [false, true] {
        let request = SearchRequest {
            filters: SearchFilters {
                sample_rate: Some(48_000),
                duration_min: Some(0.0),
                duration_max: Some(5.0),
                favorites_only,
                ..Default::default()
            },
            ..request()
        };
        // 8401 eligible wood rows exceed 8*8+8192. Basic filters can use the
        // full narrow budget; favorites must discard partial IDs and fully fall back.
        assert_eq!(
            counts(&provider, &request),
            oracle(&database, &request, true).into_iter().collect()
        );
    }
    let request = SearchRequest {
        filters: SearchFilters {
            sample_rate: Some(48_000),
            duration_max: Some(5.0),
            ..Default::default()
        },
        ..request()
    };
    let conn = Connection::open(&database).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    for replacement in [
        "DROP INDEX idx_audio_visible_filter; CREATE INDEX idx_audio_visible_filter
         ON audio_files(sample_rate,duration,id) WHERE missing=1;",
        "DROP INDEX idx_audio_visible_filter;",
    ] {
        conn.execute_batch(replacement).unwrap();
        assert_eq!(
            counts(&provider, &request),
            oracle(&database, &request, true).into_iter().collect()
        );
    }
    assert!(!provider.search(&request).unwrap().cache_hit);
}

#[test]
fn cancelled_cold_facets_leave_the_reader_and_future_ranking_usable() {
    let (_directory, database, provider) = fixture(false);
    let request = request();
    assert!(matches!(
        provider.search_folders_cancellable(&request, Arc::new(AtomicBool::new(true))),
        Err(Error::Cancelled)
    ));
    assert_eq!(
        counts(&provider, &request),
        oracle(&database, &request, true).into_iter().collect()
    );
    assert!(!provider.search(&request).unwrap().cache_hit);
    assert!(provider.search(&request).unwrap().cache_hit);
}

#[test]
fn damaged_fts_shadow_storage_is_an_error_instead_of_partial_facets() {
    let (_directory, database, provider) = fixture(false);
    let conn = Connection::open(&database).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    // Damage only this owned fixture. Neither an empty ranking cache nor the
    // optional fast-path fallback may turn a broken MATCH into successful counts.
    conn.execute_batch("DROP TABLE audio_search_data;").unwrap();
    assert!(matches!(
        provider.search_folders(&request()),
        Err(Error::Database(_))
    ));
}
