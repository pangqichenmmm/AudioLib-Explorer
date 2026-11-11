// crates/audiolib-db/src/search_page_control/implicit_pin_tests.rs
//! Actual warm-page implicit reads, cancellation, connection reuse and commit fences.
use super::*;
use audiolib_core::DataProvider;
use rusqlite::{
    functions::FunctionFlags,
    hooks::{AuthAction, AuthContext, Authorization},
};

fn fixture() -> (tempfile::TempDir, SqliteProvider, SearchRequest) {
    let directory = tempfile::tempdir().unwrap();
    let provider = SqliteProvider::open(&directory.path().join("implicit-pin.db")).unwrap();
    {
        let writer = provider.writer().unwrap();
        writer.execute_batch(
            "INSERT INTO folders(id,folder_path,folder_name,parent_id) VALUES
             (1,'/owned','owned',NULL),(2,'/owned/Wind','Wind',1),(3,'/owned/Other','Other',1);
             INSERT INTO folder_closure VALUES(1,1,0),(2,2,0),(3,3,0),(1,2,1),(1,3,1);
             INSERT INTO audio_files(id,file_path,path_key,filename,folder_id,metadata_json) VALUES
             (1,'/owned/Wind/vehicle-a.wav','/owned/Wind/vehicle-a.wav','vehicle-a.wav',2,'{\"wav_metadata\":{\"fields\":{\"library\":\"Shared\"}}}'),
             (2,'/owned/Wind/vehicle-b.wav','/owned/Wind/vehicle-b.wav','vehicle-b.wav',2,'{\"wav_metadata\":{\"fields\":{\"library\":\"Shared\"}}}'),
             (3,'/owned/Wind/vehicle-c.wav','/owned/Wind/vehicle-c.wav','vehicle-c.wav',2,'{\"wav_metadata\":{\"fields\":{\"library\":\"Shared\"}}}'),
             (4,'/owned/Other/neutral-a.wav','/owned/Other/neutral-a.wav','neutral-a.wav',3,'{}'),
             (5,'/owned/Other/neutral-b.wav','/owned/Other/neutral-b.wav','neutral-b.wav',3,'{}'),
             (6,'/owned/Other/neutral-c.wav','/owned/Other/neutral-c.wav','neutral-c.wav',3,'{}');
             UPDATE albums SET name='Original album' WHERE metadata_key='Shared';"
        ).unwrap();
    }
    provider.rebuild_directory_search().unwrap();
    {
        let writer = provider.writer().unwrap();
        crate::search_tracking::install(&writer).unwrap();
    }
    let request = SearchRequest {
        scope: SearchScope::All,
        query: "vehicle".into(),
        limit: 1,
        ..Default::default()
    };
    provider.search(&request).unwrap();
    assert!(provider.search(&request).unwrap().cache_hit);
    (directory, provider, request)
}

#[test]
fn warm_implicit_pin_observes_cancellation_without_opening_an_explicit_transaction() {
    let (_directory, provider, request) = fixture();
    let cancel = Arc::new(AtomicBool::new(false));
    let saw_pin = Arc::new(AtomicBool::new(false));
    let saw_transaction = Arc::new(AtomicBool::new(false));
    let reader = provider.reader().unwrap();
    let signal = cancel.clone();
    let pin = saw_pin.clone();
    let transaction = saw_transaction.clone();
    reader
        .authorizer(Some(move |context: AuthContext<'_>| {
            match context.action {
                AuthAction::Read {
                    table_name: "audio_files",
                    column_name: "id",
                    ..
                } => {
                    pin.store(true, Ordering::Release);
                    signal.store(true, Ordering::Release);
                }
                AuthAction::Transaction { .. } => {
                    transaction.store(true, Ordering::Release);
                    return Authorization::Deny;
                }
                _ => {}
            }
            Authorization::Allow
        }))
        .unwrap();
    drop(reader);
    assert!(matches!(
        provider.search_cancellable(&request, cancel),
        Err(Error::Cancelled)
    ));
    assert!(saw_pin.load(Ordering::Acquire));
    assert!(!saw_transaction.load(Ordering::Acquire));
    let reader = provider.reader().unwrap();
    assert!(reader.is_autocommit());
    reader
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .unwrap();
    let sum: i64 = reader.query_row(
        "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<10000) SELECT sum(x) FROM n",
        [], |row| row.get(0),
    ).unwrap();
    assert_eq!(sum, 50_005_000);
    drop(reader);
    assert!(provider.search(&request).unwrap().cache_hit);
}

#[test]
fn warm_empty_library_page_needs_no_explicit_transaction_or_pin_row() {
    let directory = tempfile::tempdir().unwrap();
    let provider = SqliteProvider::open(&directory.path().join("empty-pin.db")).unwrap();
    let request = SearchRequest {
        scope: SearchScope::Filename,
        query: "车辆".into(),
        ..Default::default()
    };
    provider.search(&request).unwrap();
    assert!(provider.search(&request).unwrap().cache_hit);
    let attempted_transaction = Arc::new(AtomicBool::new(false));
    let signal = attempted_transaction.clone();
    let reader = provider.reader().unwrap();
    reader
        .authorizer(Some(move |context: AuthContext<'_>| {
            if matches!(context.action, AuthAction::Transaction { .. }) {
                signal.store(true, Ordering::Release);
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }))
        .unwrap();
    drop(reader);
    let page = provider.search(&request).unwrap();
    assert!(page.cache_hit);
    assert_eq!(page.total_count, 0);
    assert!(page.results.is_empty());
    assert!(!attempted_transaction.load(Ordering::Acquire));
    let reader = provider.reader().unwrap();
    assert!(reader.is_autocommit());
    reader
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .unwrap();
}

fn mark_dirty_reader(conn: &Connection) {
    conn.create_scalar_function("owned_reader_marker", 0, FunctionFlags::SQLITE_UTF8, |_| {
        Ok(42_i64)
    })
    .unwrap();
    conn.execute_batch("BEGIN").unwrap();
    conn.query_row("SELECT id FROM audio_files LIMIT 1", [], |row| {
        row.get::<_, i64>(0)
    })
    .unwrap();
    assert!(!conn.is_autocommit());
}

fn assert_fresh_reader(provider: &SqliteProvider, request: &SearchRequest) {
    let reader = provider.reader().unwrap();
    assert!(reader.is_autocommit());
    assert!(
        reader
            .query_row("SELECT owned_reader_marker()", [], |row| row
                .get::<_, i64>(0))
            .is_err()
    );
    drop(reader);
    assert!(provider.search(request).unwrap().cache_hit);
}

#[test]
fn dropping_an_unfinished_reader_discards_the_connection_instead_of_pooling_it() {
    let (_directory, provider, request) = fixture();
    let reader = provider.reader().unwrap();
    assert!(provider.read_pool.lock().unwrap().is_empty());
    mark_dirty_reader(&reader);
    drop(reader);
    assert!(provider.read_pool.lock().unwrap().is_empty());
    assert_fresh_reader(&provider, &request);
}

#[test]
fn checkout_discards_a_dirty_connection_even_if_it_was_injected_into_the_pool() {
    let (_directory, provider, request) = fixture();
    let mut lease = provider.reader().unwrap();
    let dirty = lease.conn.take().unwrap();
    drop(lease);
    mark_dirty_reader(&dirty);
    provider.read_pool.lock().unwrap().push(dirty);
    assert_fresh_reader(&provider, &request);
}

// Trace callbacks run synchronously at real SQLite row/profile boundaries. The
// existing test-hooks feature enables rusqlite trace; production gains no hook.
#[cfg(feature = "test-hooks")]
mod commit_interleavings {
    use super::*;
    use rusqlite::trace::{TraceEvent, TraceEventCodes};
    use std::cell::RefCell;

    enum Moment {
        PinRow,
        AfterObserver,
    }

    struct Scenario {
        foreign: Connection,
        moment: Moment,
        pin_seen: bool,
        committed: bool,
        error: Option<String>,
    }

    thread_local! {
        static SCENARIO: RefCell<Option<Scenario>> = const { RefCell::new(None) };
    }

    fn interleave(event: TraceEvent<'_>) {
        SCENARIO.with(|slot| {
            let mut slot = slot.borrow_mut();
            let Some(scenario) = slot.as_mut() else {
                return;
            };
            let fire = match event {
                TraceEvent::Row(statement)
                    if statement.sql() == "SELECT id FROM audio_files LIMIT 1" =>
                {
                    scenario.pin_seen = true;
                    matches!(scenario.moment, Moment::PinRow)
                }
                TraceEvent::Profile(statement, _) if statement.sql() == "PRAGMA data_version" => {
                    scenario.pin_seen && matches!(scenario.moment, Moment::AfterObserver)
                }
                _ => false,
            };
            if fire && !scenario.committed && scenario.error.is_none() {
                match scenario.foreign.execute(
                    "UPDATE albums SET name='Changed presentation' WHERE metadata_key='Shared'",
                    [],
                ) {
                    Ok(1) => scenario.committed = true,
                    Ok(rows) => {
                        scenario.error = Some(format!("Expected one changed album, got {rows}"))
                    }
                    Err(error) => scenario.error = Some(error.to_string()),
                }
            }
        });
    }

    fn arm(provider: &SqliteProvider, moment: Moment) {
        let reader = provider.reader().unwrap();
        reader.trace_v2(TraceEventCodes::SQLITE_TRACE_ROW, Some(interleave));
        drop(reader);
        provider
            .revision_reader
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .trace_v2(TraceEventCodes::SQLITE_TRACE_PROFILE, Some(interleave));
        let foreign = Connection::open(provider.database_path()).unwrap();
        crate::register_library_functions(&foreign).unwrap();
        SCENARIO.with(|slot| {
            assert!(slot.borrow().is_none());
            *slot.borrow_mut() = Some(Scenario {
                foreign,
                moment,
                pin_seen: false,
                committed: false,
                error: None,
            });
        });
    }

    fn disarm(provider: &SqliteProvider) {
        let scenario = SCENARIO.with(|slot| slot.borrow_mut().take().unwrap());
        let reader = provider.reader().unwrap();
        reader.trace_v2(TraceEventCodes::empty(), None);
        drop(reader);
        provider
            .revision_reader
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .trace_v2(TraceEventCodes::empty(), None);
        assert!(scenario.pin_seen);
        assert!(
            scenario.committed,
            "trace commit failed: {:?}",
            scenario.error
        );
        assert!(scenario.error.is_none());
    }

    #[test]
    fn album_commit_during_implicit_pin_rejects_old_dtos_without_an_audio_epoch_change() {
        let (_directory, provider, request) = fixture();
        let epoch = crate::search_tracking::epoch(&provider.writer().unwrap()).unwrap();
        assert!(epoch.is_some());
        let revision = provider.observe_search_revision().unwrap();
        arm(&provider, Moment::PinRow);
        let page = provider.search(&request).unwrap();
        disarm(&provider);
        assert_ne!(provider.observe_search_revision().unwrap(), revision);
        assert_eq!(
            crate::search_tracking::epoch(&provider.writer().unwrap()).unwrap(),
            epoch
        );
        assert_eq!(
            page.results[0].album_entity.as_ref().unwrap().name,
            "Changed presentation"
        );
        assert!(provider.search(&request).unwrap().cache_hit);
    }

    #[test]
    fn commit_after_the_second_observation_keeps_old_dtos_consistent_and_next_cursor_current() {
        let (_directory, provider, request) = fixture();
        let original = provider.search(&request).unwrap();
        let old_cursor = original.next_cursor.clone().unwrap();
        assert!(old_cursor.starts_with("v2."));
        let revision = provider.observe_search_revision().unwrap();
        arm(&provider, Moment::AfterObserver);
        let old = provider.search(&request).unwrap();
        disarm(&provider);
        assert!(old.cache_hit);
        assert_eq!(
            serde_json::to_value(&old.results).unwrap(),
            serde_json::to_value(&original.results).unwrap()
        );
        assert_eq!(old.next_cursor, original.next_cursor);
        assert_ne!(provider.observe_search_revision().unwrap(), revision);
        let current = provider.search(&request).unwrap();
        assert_eq!(
            current.results[0].album_entity.as_ref().unwrap().name,
            "Changed presentation"
        );
        // The uncached path keeps its native continuation contract. Only a
        // proven full-page hit encodes the committed-page v2 continuation.
        let native_cursor = current.next_cursor.clone().unwrap();
        let warm = provider.search(&request).unwrap();
        assert!(warm.cache_hit);
        assert_eq!(
            serde_json::to_value(&warm.results).unwrap(),
            serde_json::to_value(&current.results).unwrap()
        );
        let cursor = warm.next_cursor.unwrap();
        assert!(cursor.starts_with("v2."));
        assert_ne!(cursor, old_cursor);
        assert!(matches!(
            provider.search(&SearchRequest {
                cursor: Some(old_cursor),
                ..request.clone()
            }),
            Err(Error::Validation(_))
        ));
        let native_next = provider
            .search(&SearchRequest {
                cursor: Some(native_cursor),
                ..request.clone()
            })
            .unwrap();
        assert_eq!(native_next.results[0].id, 2);
        let next = provider
            .search(&SearchRequest {
                cursor: Some(cursor),
                ..request
            })
            .unwrap();
        assert_eq!(next.results[0].id, 2);
    }
}
