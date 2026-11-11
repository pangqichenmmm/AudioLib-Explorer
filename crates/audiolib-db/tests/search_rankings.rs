// crates/audiolib-db/tests/search_rankings.rs
//! Cached ranking identities retain exact BM25 order while reading current owned DTOs.
use audiolib_core::{
    DataProvider, Error, FileVisibility, MaintenanceAction, MetadataPatch, SearchFilters,
    SearchPage, SearchRequest, SortOrder,
};
use audiolib_db::SqliteProvider;
use rusqlite::{Connection, params, params_from_iter, types::Value};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

fn fixture() -> (tempfile::TempDir, SqliteProvider) {
    let temp = tempfile::tempdir().unwrap();
    let database = temp.path().join("rankings.db");
    let provider = SqliteProvider::open(&database).unwrap();
    let conn = Connection::open(database).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    conn.execute_batch(
        "INSERT INTO folders(id,folder_path,folder_name,parent_id) VALUES
         (1,'root','root',NULL),(2,'root/child','child',1),(3,'other','other',NULL);
         INSERT INTO folder_closure VALUES(1,1,0),(2,2,0),(3,3,0),(1,2,1);",
    )
    .unwrap();
    for id in 1..=72_i64 {
        conn.execute(
            "INSERT INTO audio_files(id,file_path,path_key,filename,folder_id,sample_rate,bit_depth,channels,duration,user_rating,favorite,user_tags,user_notes,metadata_json,user_color,missing) VALUES(?1,?2,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
            params![
                id,
                format!("root/{id}.wav"),
                "metal.wav",
                1 + id % 3,
                if id % 2 == 0 { 48_000 } else { 96_000 },
                if id % 2 == 0 { 24 } else { 16 },
                if id % 2 == 0 { 2 } else { 1 },
                id as f64 / 10.0,
                if id % 5 == 0 { None } else { Some(id % 6) },
                id % 2 == 0,
                match id % 3 {
                    0 => "metal metal",
                    1 => "metal",
                    _ => "",
                },
                format!("old-notes-{id}"),
                format!(r#"{{"private":{{"identity":{id}}}}}"#),
                if id % 2 == 0 { "#AABBCC" } else { "" },
                id % 7 == 0
            ],
        )
        .unwrap();
    }
    drop(conn);
    provider
        .maintain_library(MaintenanceAction::RebuildSearch)
        .unwrap();
    (temp, provider)
}

fn expected(conn: &Connection, request: &SearchRequest) -> Vec<i64> {
    assert_eq!(
        request.query, "metal",
        "This oracle covers the metal fixtures"
    );
    // Independent SQL oracle for the public letter-fragment contract: the raw
    // complete word OR the positional m-e/e-t/t-a/a-l phrase in derived columns.
    const METAL_QUERY: &str = "({filename user_tags user_notes bwf_description ixml_scene ixml_take} : \"metal\" OR {filename_tokenized user_tags_tokenized user_notes_tokenized bwf_description_tokenized ixml_scene_tokenized ixml_take_tokenized} : \"g00006d000065 g000065000074 g000074000061 g00006100006c\")";
    let mut predicates = vec!["audio_search MATCH ?".to_string()];
    let mut values = vec![Value::Text(METAL_QUERY.into())];
    match request.filters.visibility {
        FileVisibility::Present => predicates.push("af.missing=0".into()),
        FileVisibility::Missing => predicates.push("af.missing=1".into()),
        FileVisibility::All => {}
    }
    if let Some(folder) = request.folder_id {
        predicates.push(if request.include_subfolders {
            "af.folder_id IN(SELECT descendant_id FROM folder_closure WHERE ancestor_id=?)".into()
        } else {
            "af.folder_id=?".into()
        });
        values.push(Value::Integer(folder));
    }
    for (column, value) in [
        ("sample_rate", request.filters.sample_rate.map(i64::from)),
        ("bit_depth", request.filters.bit_depth.map(i64::from)),
        ("channels", request.filters.channels.map(i64::from)),
    ] {
        if let Some(value) = value {
            predicates.push(format!("af.{column}=?"));
            values.push(Value::Integer(value));
        }
    }
    for (comparison, value) in [
        (">=", request.filters.duration_min),
        ("<=", request.filters.duration_max),
    ] {
        if let Some(value) = value {
            predicates.push(format!("af.duration{comparison}?"));
            values.push(Value::Real(value));
        }
    }
    if request.filters.favorites_only {
        predicates.push("af.favorite=1".into());
    }
    if request.filters.tagged_only {
        predicates.push("COALESCE(trim(af.user_tags),'')<>''".into());
    }
    if let Some(value) = &request.filters.color {
        predicates.push("af.user_color COLLATE NOCASE=?".into());
        values.push(Value::Text(value.clone()));
    }
    let order = match request.order_by {
        SortOrder::Relevance => "bm25(audio_search)",
        SortOrder::Filename => "af.filename COLLATE NOCASE",
        SortOrder::Duration => "af.duration",
        SortOrder::SampleRate => "af.sample_rate",
        SortOrder::Modified => "af.modified_at",
        SortOrder::Channels => "af.channels",
        // This fixture has only lowercase ASCII user tags and no source tags.
        SortOrder::Tags => {
            "COALESCE(NULLIF(trim(af.user_tags),''),af.source_tags_text) COLLATE NOCASE"
        }
    };
    let direction = if request.descending && request.order_by != SortOrder::Relevance {
        "DESC"
    } else {
        "ASC"
    };
    conn.prepare(&format!(
        "SELECT af.id FROM audio_files af JOIN audio_search ON audio_search.rowid=af.id
         WHERE {} ORDER BY {order} {direction},af.id ASC",
        predicates.join(" AND ")
    ))
    .unwrap()
    .query_map(params_from_iter(values), |row| row.get(0))
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

fn assert_exact(conn: &Connection, request: &SearchRequest, page: &SearchPage) {
    let all = expected(conn, request);
    assert_eq!(page.total_count, all.len() as u64, "{request:?}");
    let ids: Vec<_> = all
        .into_iter()
        .skip(request.offset)
        .take(request.limit)
        .collect();
    assert_eq!(
        page.results.iter().map(|row| row.id).collect::<Vec<_>>(),
        ids,
        "{request:?}"
    );
    assert_eq!(page.returned_count, page.results.len());
    for row in &page.results {
        assert_eq!(row.attributes["private"]["identity"], row.id);
    }
}

#[test]
fn alternative_sorts_with_complex_filters_keep_counts_pages_and_metadata_exact() {
    let (_temp, provider) = fixture();
    let conn = Connection::open(provider.database_path()).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    conn.execute(
        "UPDATE audio_files SET filename='metal '||printf('%03d',73-id)||'.wav'",
        [],
    )
    .unwrap();
    provider
        .maintain_library(MaintenanceAction::RebuildSearch)
        .unwrap();
    for order_by in [
        SortOrder::Relevance,
        SortOrder::Filename,
        SortOrder::Duration,
        SortOrder::SampleRate,
        SortOrder::Modified,
        SortOrder::Channels,
        SortOrder::Tags,
    ] {
        for descending in [false, true] {
            for offset in [0, 3, 100] {
                let request = SearchRequest {
                    query: "metal".into(),
                    folder_id: Some(1),
                    include_subfolders: true,
                    filters: SearchFilters {
                        sample_rate: Some(48_000),
                        duration_min: Some(0.0),
                        duration_max: Some(5.0),
                        ..Default::default()
                    },
                    order_by,
                    descending,
                    offset,
                    limit: 7,
                    ..Default::default()
                };
                assert_exact(&conn, &request, &provider.search(&request).unwrap());
            }
        }
    }
}

#[test]
fn configured_fts_rank_does_not_change_the_default_bm25_contract() {
    let (_temp, provider) = fixture();
    let conn = Connection::open(provider.database_path()).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    conn.execute(
        "INSERT INTO audio_search(audio_search,rank) VALUES('rank','bm25(0.0)')",
        [],
    )
    .unwrap();
    let request = SearchRequest {
        query: "metal".into(),
        limit: 17,
        ..Default::default()
    };
    let first = provider.search(&request).unwrap();
    assert!(!first.cache_hit);
    assert_exact(&conn, &request, &first);
    let cached = provider.search(&request).unwrap();
    assert!(cached.cache_hit);
    assert_exact(&conn, &request, &cached);
}

#[test]
fn dense_missing_rows_keep_filtered_rankings_exact() {
    let (_temp, provider) = fixture();
    let conn = Connection::open(provider.database_path()).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    for id in 1000..1129_i64 {
        conn.execute(
            "INSERT INTO audio_files(id,file_path,path_key,filename,folder_id,sample_rate,missing,metadata_json) VALUES(?1,?2,?2,'metal.wav',2,48000,1,?3)",
            params![
                id,
                format!("root/child/{id}.wav"),
                format!(r#"{{"private":{{"identity":{id}}}}}"#),
            ],
        )
        .unwrap();
    }
    provider
        .maintain_library(MaintenanceAction::RebuildSearch)
        .unwrap();
    for visibility in [
        FileVisibility::Present,
        FileVisibility::Missing,
        FileVisibility::All,
    ] {
        let request = SearchRequest {
            query: "metal".into(),
            folder_id: Some(1),
            limit: 13,
            filters: SearchFilters {
                visibility,
                sample_rate: Some(48_000),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_exact(&conn, &request, &provider.search(&request).unwrap());
        let next = SearchRequest {
            offset: 13,
            ..request
        };
        assert_exact(&conn, &next, &provider.search(&next).unwrap());
    }
}

#[test]
fn mixed_epoch_clusters_keep_filtered_bm25_pages_exact() {
    let (_temp, provider) = fixture();
    let conn = Connection::open(provider.database_path()).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    const EPOCH: i64 = 1_834_000_000_000_000;
    for offset in 0..256_i64 {
        let id = EPOCH + offset;
        conn.execute(
            "INSERT INTO audio_files(id,file_path,path_key,filename,folder_id,sample_rate,user_rating,missing,user_notes,metadata_json,bit_depth) VALUES(?1,?2,?2,'metal.wav',2,?3,?4,?5,?6,?7,?8)",
            params![
                id,
                format!("root/child/{id}.wav"),
                if offset % 2 == 0 { 48_000 } else { 96_000 },
                offset % 6,
                offset % 7 == 0,
                format!("epoch-notes-{id}"),
                format!(r#"{{"private":{{"identity":{id}}}}}"#),
                if offset % 3 == 0 { 24 } else { 16 },
            ],
        )
        .unwrap();
    }
    // SQL membership sees this far-away candidate, but it cannot occur in the
    // metal ranking and must not force a sparse representation or affect results.
    conn.execute(
        "INSERT INTO audio_files(id,file_path,path_key,filename,folder_id,sample_rate)
         VALUES(?1,'root/noise.wav','root/noise.wav','wood.wav',2,48000)",
        [EPOCH * 2],
    )
    .unwrap();
    provider
        .maintain_library(MaintenanceAction::RebuildSearch)
        .unwrap();
    for visibility in [
        FileVisibility::Present,
        FileVisibility::Missing,
        FileVisibility::All,
    ] {
        for (folder_id, sample_rate, bit_depth) in [
            (None, Some(48_000), None),
            (Some(1), None, Some(24)),
            (Some(2), Some(96_000), Some(16)),
        ] {
            let request = SearchRequest {
                query: "metal".into(),
                folder_id,
                filters: SearchFilters {
                    visibility,
                    sample_rate,
                    bit_depth,
                    ..Default::default()
                },
                limit: 13,
                ..Default::default()
            };
            assert_exact(&conn, &request, &provider.search(&request).unwrap());
            for offset in [13, 50, 1000] {
                let next = SearchRequest {
                    offset,
                    ..request.clone()
                };
                let page = provider.search(&next).unwrap();
                assert!(page.cache_hit);
                assert_exact(&conn, &next, &page);
                for file in page.results.iter().filter(|file| file.id >= EPOCH) {
                    assert_eq!(file.notes, format!("epoch-notes-{}", file.id));
                }
            }
        }
    }
}

#[test]
fn pages_folder_scopes_and_filters_reuse_exact_rankings_with_missing_rows_and_ties() {
    let (_temp, provider) = fixture();
    let conn = Connection::open(provider.database_path()).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    let base = SearchRequest {
        query: "metal".into(),
        limit: 7,
        ..Default::default()
    };
    let first = provider.search(&base).unwrap();
    assert!(!first.cache_hit);
    assert_exact(&conn, &base, &first);
    let mut requests = vec![
        SearchRequest {
            offset: 7,
            ..base.clone()
        },
        SearchRequest {
            offset: 10_000,
            ..base.clone()
        },
        SearchRequest {
            folder_id: Some(1),
            ..base.clone()
        },
        SearchRequest {
            folder_id: Some(1),
            include_subfolders: false,
            ..base.clone()
        },
        SearchRequest {
            filters: SearchFilters {
                visibility: FileVisibility::Missing,
                ..Default::default()
            },
            ..base.clone()
        },
        SearchRequest {
            filters: SearchFilters {
                visibility: FileVisibility::All,
                ..Default::default()
            },
            ..base.clone()
        },
        SearchRequest {
            filters: SearchFilters {
                duration_min: Some(0.0),
                ..Default::default()
            },
            ..base.clone()
        },
        SearchRequest {
            filters: SearchFilters {
                bit_depth: Some(24),
                favorites_only: true,
                ..Default::default()
            },
            ..base.clone()
        },
        SearchRequest {
            filters: SearchFilters {
                sample_rate: Some(48_000),
                bit_depth: Some(24),
                channels: Some(2),
                duration_min: Some(1.0),
                duration_max: Some(5.0),
                tagged_only: true,
                color: Some("#aabbcc".into()),
                ..Default::default()
            },
            ..base.clone()
        },
    ];
    requests.push(SearchRequest {
        folder_id: Some(9999),
        ..base
    });
    for request in requests {
        let page = provider.search(&request).unwrap();
        assert!(page.cache_hit, "{request:?}");
        assert_exact(&conn, &request, &page);
    }
}

#[test]
fn own_and_external_commits_removal_and_restore_invalidate_rankings_and_refresh_dtos() {
    let (temp, provider) = fixture();
    let conn = Connection::open(provider.database_path()).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    let request = SearchRequest {
        query: "metal".into(),
        limit: 100,
        ..Default::default()
    };
    assert!(!provider.search(&request).unwrap().cache_hit);
    assert!(provider.search(&request).unwrap().cache_hit);
    let backup = temp.path().join("backup.db");
    provider.backup_library(&backup).unwrap();
    provider
        .update_metadata(
            1,
            &MetadataPatch {
                notes: Some("metal new-own-note".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let page = provider.search(&request).unwrap();
    assert!(!page.cache_hit);
    assert_eq!(
        page.results.iter().find(|row| row.id == 1).unwrap().notes,
        "metal new-own-note"
    );
    assert_exact(&conn, &request, &page);
    assert!(provider.search(&request).unwrap().cache_hit);

    let editor = SqliteProvider::open_existing(provider.database_path()).unwrap();
    editor
        .update_metadata(
            2,
            &MetadataPatch {
                favorite: Some(true),
                tags: Some("metal metal metal external".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let page = provider.search(&request).unwrap();
    assert!(!page.cache_hit);
    let edited = page.results.iter().find(|row| row.id == 2).unwrap();
    assert_eq!(
        (&*edited.tags, edited.favorite),
        ("metal metal metal external", true)
    );
    assert_exact(&conn, &request, &page);
    assert!(provider.search(&request).unwrap().cache_hit);

    let page = provider.search(&request).unwrap();
    assert!(
        page.cache_hit,
        "retired playback tracking must not invalidate rankings"
    );
    let dto = serde_json::to_value(page.results.iter().find(|row| row.id == 1).unwrap()).unwrap();
    assert!(dto.get("play_count").is_none());
    assert_exact(&conn, &request, &page);
    assert!(provider.search(&request).unwrap().cache_hit);

    provider.remove_audio(&[3]).unwrap();
    let page = provider.search(&request).unwrap();
    assert!(!page.cache_hit);
    assert!(!page.results.iter().any(|row| row.id == 3));
    assert_exact(&conn, &request, &page);
    assert!(provider.search(&request).unwrap().cache_hit);

    provider.restore_library(&backup).unwrap();
    let page = provider.search(&request).unwrap();
    assert!(!page.cache_hit);
    assert_eq!(
        page.results.iter().find(|row| row.id == 1).unwrap().notes,
        "old-notes-1"
    );
    assert!(page.results.iter().any(|row| row.id == 3));
    assert_exact(&conn, &request, &page);
    assert!(provider.search(&request).unwrap().cache_hit);
}

#[test]
fn cancelling_a_cached_request_does_not_poison_later_rankings_or_readers() {
    let (_temp, provider) = fixture();
    let conn = Connection::open(provider.database_path()).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    let request = SearchRequest {
        query: "metal".into(),
        limit: 7,
        ..Default::default()
    };
    assert!(!provider.search(&request).unwrap().cache_hit);
    assert!(provider.search(&request).unwrap().cache_hit);
    let cancel = Arc::new(AtomicBool::new(true));
    assert!(matches!(
        provider.search_cancellable(&request, cancel.clone()),
        Err(Error::Cancelled)
    ));
    cancel.store(false, Ordering::Release);
    let next = SearchRequest {
        offset: 7,
        ..request
    };
    let page = provider.search_cancellable(&next, cancel).unwrap();
    assert!(page.cache_hit);
    assert_exact(&conn, &next, &page);
    assert_eq!(provider.get_audio(1).unwrap().notes, "old-notes-1");
}
