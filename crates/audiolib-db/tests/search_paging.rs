// crates/audiolib-db/tests/search_paging.rs
//! Narrow page selection must retain exact SQL ordering, visibility and ties.
use audiolib_core::{DataProvider, FileVisibility, SearchFilters, SearchRequest, SortOrder};
use audiolib_db::SqliteProvider;
use rusqlite::{Connection, params};

#[test]
fn deferred_metadata_hydration_matches_direct_sql_for_every_sort_and_visibility() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("paging.db");
    let provider = SqliteProvider::open(&path).unwrap();
    let conn = Connection::open(path).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    conn.execute_batch(
        "INSERT INTO folders(id,folder_path,folder_name) VALUES(1,'root','root');
        INSERT INTO folder_closure VALUES(1,1,0);",
    )
    .unwrap();
    for id in 1..=60 {
        conn.execute(
            "INSERT INTO audio_files(id,file_path,path_key,filename,folder_id,
            duration,sample_rate,user_rating,missing,metadata_json,modified_at)
            VALUES(?1,?2,?2,?3,1,?4,?5,?6,?7,?8,?9)",
            params![
                id,
                format!("root/{id}.wav"),
                format!(
                    "metal {} {}.wav",
                    if id % 2 == 0 { "A" } else { "a" },
                    id % 5
                ),
                id % 4,
                if id % 3 == 0 { 48000 } else { 96000 },
                if id % 6 == 0 { None } else { Some(id % 5) },
                id % 7 == 0,
                format!(r#"{{"private":"{}"}}"#, "preserved".repeat(128)),
                format!("2026-01-0{}", 1 + id % 3),
            ],
        )
        .unwrap();
    }
    for (order_by, sort_column) in [
        (SortOrder::Relevance, "bm25(audio_search)"),
        (SortOrder::Filename, "af.filename COLLATE NOCASE"),
        (SortOrder::Duration, "af.duration"),
        (SortOrder::SampleRate, "af.sample_rate"),
        (SortOrder::Modified, "af.modified_at"),
        (SortOrder::Channels, "af.channels"),
        (
            SortOrder::Tags,
            "COALESCE(NULLIF(trim(af.user_tags),''),af.source_tags_text) COLLATE NOCASE",
        ),
    ] {
        for descending in [false, true] {
            for (visibility, visible_sql) in [
                (FileVisibility::Present, "af.missing=0"),
                (FileVisibility::Missing, "af.missing=1"),
                (FileVisibility::All, "1=1"),
            ] {
                for query in ["", "metal"] {
                    let request = SearchRequest {
                        query: query.into(),
                        folder_id: Some(1),
                        include_subfolders: false,
                        filters: SearchFilters {
                            visibility,
                            ..Default::default()
                        },
                        order_by,
                        descending,
                        limit: 7,
                        offset: 2,
                        ..Default::default()
                    };
                    let (join, condition) = if query.is_empty() {
                        ("", "1=1")
                    } else {
                        (
                            "JOIN audio_search ON audio_search.rowid=af.id",
                            "audio_search MATCH 'metal'",
                        )
                    };
                    let column = if query.is_empty() && order_by == SortOrder::Relevance {
                        "af.filename COLLATE NOCASE"
                    } else {
                        sort_column
                    };
                    let direction = if descending && order_by != SortOrder::Relevance {
                        "DESC"
                    } else {
                        "ASC"
                    };
                    let sql = format!("SELECT af.id FROM audio_files af {join} WHERE {visible_sql} AND {condition}
                        AND af.folder_id=1 ORDER BY {column} {direction},af.id ASC LIMIT 7 OFFSET 2");
                    let expected = conn
                        .prepare(&sql)
                        .unwrap()
                        .query_map([], |row| row.get::<_, i64>(0))
                        .unwrap()
                        .collect::<rusqlite::Result<Vec<_>>>()
                        .unwrap();
                    let page = provider.search(&request).unwrap();
                    assert_eq!(
                        page.results.iter().map(|row| row.id).collect::<Vec<_>>(),
                        expected,
                        "{request:?}"
                    );
                    assert!(
                        page.results.iter().all(|row| row.attributes["private"]
                            .as_str()
                            .unwrap()
                            .len()
                            == 1152)
                    );
                    let empty = provider
                        .search(&SearchRequest {
                            offset: 10000,
                            ..request
                        })
                        .unwrap();
                    assert_eq!(empty.total_count, page.total_count);
                    assert!(empty.results.is_empty());
                }
            }
        }
    }
}

#[test]
fn unfiltered_fts_counts_subtract_only_matching_missing_records_for_every_sort() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("count-missing.db");
    let provider = SqliteProvider::open(&path).unwrap();
    let conn = Connection::open(&path).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    for (index, (filename, missing)) in [
        ("Metal A.wav", false),
        ("Metal a.wav", true),
        ("Metal b.wav", false),
        ("Wood.wav", true),
        ("Metal A.wav", false),
        ("Stone.wav", false),
        ("Metal wood.wav", true),
        ("Metal wood.wav", false),
        ("Metal z.wav", false),
        ("metal z.wav", true),
    ]
    .into_iter()
    .enumerate()
    {
        let id = index as i64 + 1;
        conn.execute(
            "INSERT INTO audio_files(id,file_path,filename,missing,duration,sample_rate,
             user_rating,user_notes,modified_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                id,
                format!("root/{id}.wav"),
                filename,
                missing,
                id % 3,
                if id % 2 == 0 { 48000 } else { 96000 },
                if id % 4 == 0 { None } else { Some(id % 5) },
                format!("retain-{id}"),
                format!("2026-01-0{}", 1 + id % 3),
            ],
        )
        .unwrap();
    }
    for (order_by, sort_column) in [
        (SortOrder::Relevance, "bm25(audio_search)"),
        (SortOrder::Filename, "af.filename COLLATE NOCASE"),
        (SortOrder::Duration, "af.duration"),
        (SortOrder::SampleRate, "af.sample_rate"),
        (SortOrder::Modified, "af.modified_at"),
        (SortOrder::Channels, "af.channels"),
        (
            SortOrder::Tags,
            "COALESCE(NULLIF(trim(af.user_tags),''),af.source_tags_text) COLLATE NOCASE",
        ),
    ] {
        for descending in [false, true] {
            let direction = if descending && order_by != SortOrder::Relevance {
                "DESC"
            } else {
                "ASC"
            };
            for (visibility, visible_sql) in [
                (FileVisibility::Present, "af.missing=0"),
                (FileVisibility::All, "1=1"),
            ] {
                for (query, match_query) in [
                    ("metal", "metal"),
                    ("wood", "wood"),
                    ("metal wood", "metal AND wood"),
                    ("doesnotexist", "doesnotexist"),
                ] {
                    let mut statement = conn
                        .prepare(&format!(
                            "SELECT af.id FROM audio_files af
                             JOIN audio_search ON audio_search.rowid=af.id
                             WHERE {visible_sql} AND audio_search MATCH ?1
                             ORDER BY {sort_column} {direction},af.id ASC"
                        ))
                        .unwrap();
                    let expected = statement
                        .query_map([match_query], |row| row.get::<_, i64>(0))
                        .unwrap()
                        .collect::<rusqlite::Result<Vec<_>>>()
                        .unwrap();
                    for offset in [0, 2, expected.len(), 1000] {
                        let request = SearchRequest {
                            query: query.into(),
                            filters: SearchFilters {
                                visibility,
                                ..Default::default()
                            },
                            order_by,
                            descending,
                            limit: 2,
                            offset,
                            ..Default::default()
                        };
                        let page = provider.search(&request).unwrap();
                        assert_eq!(page.total_count, expected.len() as u64, "{request:?}");
                        let ids: Vec<_> = expected.iter().copied().skip(offset).take(2).collect();
                        assert_eq!(
                            page.results.iter().map(|file| file.id).collect::<Vec<_>>(),
                            ids,
                            "{request:?}"
                        );
                        assert_eq!(page.returned_count, ids.len());
                        for file in &page.results {
                            assert_eq!(file.notes, format!("retain-{}", file.id));
                            if visibility == FileVisibility::Present {
                                assert!(!file.missing);
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn table_column_sorts_keep_annotation_priority_source_fallback_and_stable_pages() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("table-sorts.db");
    let provider = SqliteProvider::open(&path).unwrap();
    let conn = Connection::open(&path).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    for (id, channels, tags, source, missing) in [
        (1, 2, "Zulu", "aardvark", false),
        (2, 1, "Beta", "zulu", false),
        (3, 2, "\u{3000}\u{2003}\t", "alpha", false),
        (4, 6, "beta", "aardvark", false),
        (5, 1, "  ", "", false),
        (6, 2, "  Alpha\u{3000}\u{2003}", "zulu", false),
        (7, 1, "", "aardvark", true),
    ] {
        conn.execute(
            "INSERT INTO audio_files(id,file_path,filename,channels,user_tags,
             source_tags_text,metadata_json,missing) VALUES(?1,?2,'metal.wav',?3,?4,?5,?6,?7)",
            params![
                id,
                format!("root/{id}.wav"),
                channels,
                tags,
                source,
                serde_json::json!({"wav_metadata":{"fields":{"keywords":source}}}).to_string(),
                missing,
            ],
        )
        .unwrap();
    }
    for (order_by, descending, expected) in [
        (SortOrder::Channels, false, vec![2, 5, 1, 3, 6, 4]),
        (SortOrder::Channels, true, vec![4, 1, 3, 6, 2, 5]),
        (SortOrder::Tags, false, vec![5, 3, 6, 2, 4, 1]),
        (SortOrder::Tags, true, vec![1, 2, 4, 3, 6, 5]),
    ] {
        for query in ["", "metal"] {
            let mut actual = Vec::new();
            for offset in [0, 2, 4, 6] {
                let page = provider
                    .search(&SearchRequest {
                        query: query.into(),
                        order_by,
                        descending,
                        limit: 2,
                        offset,
                        ..Default::default()
                    })
                    .unwrap();
                assert_eq!(page.total_count, 6);
                actual.extend(page.results.into_iter().map(|file| file.id));
            }
            assert_eq!(
                actual, expected,
                "{order_by:?}, descending={descending}, {query}"
            );
        }
    }
    // Annotation edits immediately affect order without modifying the embedded tags.
    provider
        .update_metadata(
            1,
            &audiolib_core::MetadataPatch {
                tags: Some("".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let page = provider
        .search(&SearchRequest {
            order_by: SortOrder::Tags,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        page.results.iter().map(|file| file.id).collect::<Vec<_>>(),
        [5, 1, 3, 6, 2, 4]
    );
    assert_eq!(
        provider.get_audio(1).unwrap().attributes["wav_metadata"]["fields"]["keywords"],
        "aardvark"
    );
}

#[test]
fn sparse_and_dense_missing_boundaries_keep_exact_counts_and_filename_pages() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("count-boundaries.db");
    let provider = SqliteProvider::open(&path).unwrap();
    let conn = Connection::open(&path).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    conn.execute_batch(
        "WITH RECURSIVE ids(id) AS (SELECT 1 UNION ALL SELECT id+1 FROM ids WHERE id<512)
         INSERT INTO audio_files(id,file_path,filename,missing,user_notes)
         SELECT id,printf('root/%d.wav',id),
         CASE WHEN id%2=0 THEN printf('metal %d.wav',id%7) ELSE printf('wood %d.wav',id%7) END,
         0,printf('keep-%d',id) FROM ids;",
    )
    .unwrap();
    for missing in [0, 128, 129, 512] {
        conn.execute("UPDATE audio_files SET missing=(id<=?1)", [missing])
            .unwrap();
        for (visibility, predicate) in [
            (FileVisibility::Present, "af.missing=0"),
            (FileVisibility::All, "1=1"),
        ] {
            for query in ["metal", "wood", "notpresent"] {
                for descending in [false, true] {
                    let direction = if descending { "DESC" } else { "ASC" };
                    let expected = conn
                        .prepare(&format!(
                            "SELECT af.id FROM audio_files af
                             JOIN audio_search ON audio_search.rowid=af.id
                             WHERE {predicate} AND audio_search MATCH ?1
                             ORDER BY af.filename COLLATE NOCASE {direction},af.id ASC"
                        ))
                        .unwrap()
                        .query_map([query], |row| row.get::<_, i64>(0))
                        .unwrap()
                        .collect::<rusqlite::Result<Vec<_>>>()
                        .unwrap();
                    for offset in [0, 5, expected.len(), 1000] {
                        let page = provider
                            .search(&SearchRequest {
                                query: query.into(),
                                filters: SearchFilters {
                                    visibility,
                                    ..Default::default()
                                },
                                order_by: SortOrder::Filename,
                                descending,
                                limit: 7,
                                offset,
                                ..Default::default()
                            })
                            .unwrap();
                        assert_eq!(page.total_count, expected.len() as u64);
                        let ids: Vec<_> = expected.iter().copied().skip(offset).take(7).collect();
                        assert_eq!(
                            page.results.iter().map(|file| file.id).collect::<Vec<_>>(),
                            ids,
                            "{missing} missing, {visibility:?}, {query}, offset {offset}"
                        );
                        assert_eq!(page.returned_count, ids.len());
                        for file in &page.results {
                            assert_eq!(file.notes, format!("keep-{}", file.id));
                        }
                    }
                }
            }
        }
    }
}
