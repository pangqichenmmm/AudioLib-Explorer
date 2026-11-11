// crates/audiolib-db/src/search_page_control/cow_isolation_tests.rs
//! Cached owned DTOs must detach before callers mutate fields or nested Metadata.
use crate::SqliteProvider;
use audiolib_core::{DataProvider, SearchRequest, SearchScope, SortOrder};
use rusqlite::params;
use serde_json::json;

#[test]
fn returned_page_and_audio_mutations_preserve_cached_rows_and_database_values() {
    let directory = tempfile::tempdir().unwrap();
    let provider = SqliteProvider::open(&directory.path().join("cow-isolation.db")).unwrap();
    let metadata = json!({
        "wav_metadata":{"fields":{"library":"Owned fixture pack"}},
        "unknown_extension":{
            "nested":{"label":"保留 🦉","object":{"keep":true}},
            "array":["first",{"label":"original element"},[1,2]]
        }
    });
    {
        let writer = provider.writer().unwrap();
        writer
            .execute_batch(
                "INSERT INTO folders(id,folder_path,folder_name) VALUES(1,'/owned','Owned');
                 INSERT INTO folder_closure VALUES(1,1,0);",
            )
            .unwrap();
        for (id, filename) in [(1, "vehicle-a.wav"), (2, "vehicle-b.wav")] {
            writer
                .execute(
                    "INSERT INTO audio_files(id,file_path,path_key,filename,folder_id,user_notes,metadata_json)
                     VALUES(?1,?2,?2,?3,1,'original note',?4)",
                    params![id, format!("/owned/{filename}"), filename, metadata.to_string()],
                )
                .unwrap();
        }
    }
    let request = SearchRequest {
        query: "vehicle".into(),
        scope: SearchScope::Main,
        order_by: SortOrder::Filename,
        limit: 2,
        ..Default::default()
    };
    let first = provider.search(&request).unwrap();
    assert_eq!(first.total_count, 2);
    let cached = provider.search(&request).unwrap();
    assert!(cached.cache_hit, "exercise the actual first-page cache");
    assert_eq!(
        cached
            .results
            .iter()
            .map(|file| file.id)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    // Serialize only the results: elapsed time, hit flags and cursor bookkeeping
    // are not part of the owned row-isolation contract.
    let original_results = serde_json::to_value(&cached.results).unwrap();
    let original_file = serde_json::to_value(&cached.results[0]).unwrap();
    let untouched_page = cached.clone();
    let untouched_file = cached.results[0].clone();
    let mut changed_page = cached.clone();
    let mut changed_file = cached.results[0].clone();

    // Top-level mutation must detach every field of the returned row together.
    changed_page.results[0].notes = "page-only note".into();
    changed_page.results[0].file_path = "/owned/page-only.wav".into();
    changed_page.results[0].attributes["unknown_extension"]["nested"]["label"] =
        json!("page-only nested value");
    changed_page.results[0].attributes["unknown_extension"]["array"]
        .as_array_mut()
        .unwrap()
        .push(json!({"page_only":true}));
    changed_page.results[0].album_entity.as_mut().unwrap().name = "page-only album".into();

    // Mutate nested JSON first on this separate clone: no earlier String write
    // may accidentally hide a missing detach on the Metadata/array path.
    changed_file.attributes["unknown_extension"]["array"][1]["label"] = json!("file-only element");
    changed_file.attributes["unknown_extension"]["array"][2]
        .as_array_mut()
        .unwrap()
        .push(json!(3));
    changed_file.attributes["unknown_extension"]["nested"]["object"]["keep"] = json!(false);
    changed_file.notes = "file-only note".into();
    changed_file.file_path = "/owned/file-only.wav".into();
    changed_file.album_entity.as_mut().unwrap().name = "file-only album".into();

    assert_eq!(changed_page.results[0].notes, "page-only note");
    assert_eq!(changed_page.results[0].file_path, "/owned/page-only.wav");
    assert_eq!(
        changed_page.results[0].attributes["unknown_extension"]["array"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    assert_eq!(
        changed_page.results[0].attributes["unknown_extension"]["array"][1]["label"],
        "original element"
    );
    assert_eq!(changed_file.notes, "file-only note");
    assert_eq!(changed_file.file_path, "/owned/file-only.wav");
    assert_eq!(
        changed_file.attributes["unknown_extension"]["array"][1]["label"],
        "file-only element"
    );
    assert_eq!(
        changed_file.attributes["unknown_extension"]["array"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert_eq!(
        changed_file.attributes["unknown_extension"]["array"][2],
        json!([1, 2, 3])
    );
    assert_eq!(
        changed_page.results[0].album_entity.as_ref().unwrap().name,
        "page-only album"
    );
    assert_eq!(
        changed_file.album_entity.as_ref().unwrap().name,
        "file-only album"
    );
    assert_ne!(
        serde_json::to_value(&changed_page.results).unwrap(),
        original_results
    );
    assert_ne!(serde_json::to_value(&changed_file).unwrap(), original_file);
    assert_eq!(
        serde_json::to_value(&changed_page.results[1]).unwrap(),
        original_results[1]
    );

    assert_eq!(
        serde_json::to_value(&cached.results).unwrap(),
        original_results
    );
    assert_eq!(
        serde_json::to_value(&untouched_page.results).unwrap(),
        original_results
    );
    assert_eq!(
        serde_json::to_value(&untouched_file).unwrap(),
        original_file
    );
    let next_hit = provider.search(&request).unwrap();
    assert!(next_hit.cache_hit);
    assert_eq!(
        serde_json::to_value(&next_hit.results).unwrap(),
        original_results
    );
    // Returning/mutating an owned DTO never requests a writeback to the library.
    assert_eq!(
        serde_json::to_value(provider.get_audio(1).unwrap()).unwrap(),
        original_file
    );
    assert_eq!(
        serde_json::to_value(provider.get_audio(2).unwrap()).unwrap(),
        original_results[1]
    );
}
