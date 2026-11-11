// crates/audiolib-db/src/search_include.rs
//! Include tag filters without testing every visible file's wide row.
//!
//! An include term only matches a file that has tags, so the matches of a present,
//! query-free request are a subset of the sparse idx_audio_visible_tagged_folders
//! index. That index holds both tag columns, so the terms are tested on it and only
//! matching files read their row for the sort key and any other filter.
//! Otherwise a sort index walk (idx_duration, idx_audio_visible_modified, ...)
//! reads the wide row of every file it passes, which for sparse matches is most of
//! the library and its Metadata. Dense matches end a library-wide duration walk
//! after a few pages' worth of files, so that case keeps reading idx_duration in order.
use crate::{Result, db_error, schema, search_exclude, search_sorted};
use audiolib_core::{FileVisibility, SearchRequest, SortOrder};
use rusqlite::Connection;

/// Present files with tags. search_sql binds the index's partial predicate
/// verbatim for every include filter, so SQLite may read it.
const TAGGED_FROM: &str = "audio_files af INDEXED BY idx_audio_visible_tagged_folders";

/// A present, query-free request with an include term on the known tag index.
pub(super) struct Inclusion;

impl Inclusion {
    /// Some only when every match is an entry of the sparse tag index: derived tag
    /// columns are current, the index has its known definition and the request
    /// selects present files. Queries keep their FTS5 plans.
    pub(super) fn plan(
        tx: &Connection,
        request: &SearchRequest,
        derived_tags: bool,
    ) -> Result<Option<Self>> {
        let filters = &request.filters;
        if !derived_tags
            || !request.query.trim().is_empty()
            || filters.visibility != FileVisibility::Present
            || (filters.tag_contains.is_none() && filters.include_tags.is_empty())
            || !schema::definition::matches_sql(
                tx,
                "idx_audio_visible_tagged_folders",
                "index",
                "audio_files",
                search_exclude::TAGGED_INDEX,
            )?
        {
            return Ok(None);
        }
        Ok(Some(Self))
    }

    /// The FROM clause for the shared count statement.
    pub(super) fn count_from(&self) -> &'static str {
        TAGGED_FROM
    }

    /// The FROM clause for the shared page statement; None keeps the planner.
    ///
    /// Filename order (and relevance without a query) already reads
    /// idx_audio_visible_tagged_any in order and stops after the page. A library-wide
    /// duration page over dense matches (at least 1 in 16 files, page within the
    /// first quarter of the matches) walks idx_duration; evenly spread matches end it
    /// within 16 pages' worth of files.
    /// Other dense library-wide orders keep their sort indexes. Everything else reads
    /// the tag index and sorts only the matches: a sparse walk would read most rows.
    pub(super) fn page_from(
        &self,
        tx: &Connection,
        request: &SearchRequest,
        total: u64,
    ) -> Result<Option<&'static str>> {
        if matches!(request.order_by, SortOrder::Filename | SortOrder::Relevance) {
            return Ok(None);
        }
        let rows: i64 = tx
            .query_row("SELECT COALESCE(max(id),0) FROM audio_files", [], |row| {
                row.get(0)
            })
            .map_err(db_error)?;
        let dense = request.folder_id.is_none()
            && total.saturating_mul(16) >= u64::try_from(rows).unwrap_or(u64::MAX);
        if !dense {
            return Ok(Some(TAGGED_FROM));
        }
        if request.order_by != SortOrder::Duration {
            return Ok(None);
        }
        let end = request.offset.saturating_add(request.limit) as u64;
        Ok(
            if end.saturating_mul(4) <= total
                && search_sorted::single_column_index(tx, "idx_duration", "duration")?
            {
                Some("audio_files af INDEXED BY idx_duration")
            } else {
                Some(TAGGED_FROM)
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SqliteProvider, load_tokenizer, search_sql};
    use audiolib_core::SearchFilters;
    use rusqlite::{params_from_iter, types::Value};

    fn fixture() -> (tempfile::TempDir, Connection) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("include.db");
        drop(SqliteProvider::open(&path).unwrap());
        let conn = Connection::open(&path).unwrap();
        crate::register_library_functions(&conn).unwrap();
        (directory, conn)
    }

    fn including(include: &[&str], exclude: &[&str]) -> SearchRequest {
        SearchRequest {
            filters: SearchFilters {
                include_tags: include.iter().map(|term| (*term).into()).collect(),
                exclude_tags: exclude.iter().map(|term| (*term).into()).collect(),
                ..Default::default()
            },
            order_by: SortOrder::Duration,
            descending: true,
            ..Default::default()
        }
    }

    fn plan_of(conn: &Connection, sql: &str, values: &[Value]) -> Vec<String> {
        conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .query_map(params_from_iter(values), |row| row.get(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    fn set_rows(conn: &Connection, rows: i64) {
        conn.execute(
            "INSERT INTO audio_files(id,file_path,path_key,filename) VALUES(?1,'a.wav','a.wav','a.wav')",
            [rows],
        )
        .unwrap();
    }

    #[test]
    fn only_present_query_free_include_requests_on_the_known_tag_index_are_planned() {
        let (_directory, conn) = fixture();
        for eligible in [
            including(&["engine"], &[]),
            including(&["engine"], &["rain"]),
            SearchRequest {
                filters: SearchFilters {
                    tag_contains: Some("engine".into()),
                    sample_rate: Some(48_000),
                    ..Default::default()
                },
                folder_id: Some(3),
                ..Default::default()
            },
        ] {
            assert!(
                Inclusion::plan(&conn, &eligible, true).unwrap().is_some(),
                "{eligible:?}"
            );
        }
        let request = including(&["engine"], &[]);
        for other in [
            including(&[], &["rain"]),
            SearchRequest {
                query: "engine".into(),
                ..request.clone()
            },
            SearchRequest {
                filters: SearchFilters {
                    visibility: FileVisibility::All,
                    ..request.filters.clone()
                },
                ..request.clone()
            },
            SearchRequest {
                filters: SearchFilters {
                    visibility: FileVisibility::Missing,
                    ..request.filters.clone()
                },
                ..request.clone()
            },
        ] {
            assert!(
                Inclusion::plan(&conn, &other, true).unwrap().is_none(),
                "{other:?}"
            );
        }
        // Tag columns still being backfilled use the JSON fallback statement.
        assert!(Inclusion::plan(&conn, &request, false).unwrap().is_none());
        // An index omitting source-tagged files would drop matches.
        conn.execute_batch(
            "DROP INDEX idx_audio_visible_tagged_folders;
             CREATE INDEX idx_audio_visible_tagged_folders
             ON audio_files(folder_id,id,user_tags,source_tags_text)
             WHERE missing=0 AND COALESCE(trim(user_tags),'')<>'';",
        )
        .unwrap();
        assert!(Inclusion::plan(&conn, &request, true).unwrap().is_none());
    }

    #[test]
    fn sparse_matches_read_the_tag_index_and_dense_duration_pages_walk_in_order() {
        // A new library has no ANALYZE statistics, as most real libraries.
        let (_directory, conn) = fixture();
        set_rows(&conn, 16_000);
        let request = including(&["engine"], &["rain"]);
        let inclusion = Inclusion::plan(&conn, &request, true).unwrap().unwrap();
        let shared = search_sql(&conn, &load_tokenizer(), &request, true, true).unwrap();
        let where_sql = shared.conditions.join(" AND ");
        let count = plan_of(
            &conn,
            &format!(
                "SELECT COUNT(*) FROM {} WHERE {where_sql}",
                inclusion.count_from()
            ),
            &shared.values,
        );
        assert!(
            count
                .iter()
                .any(|line| line.contains("idx_audio_visible_tagged_folders")),
            "{count:?}"
        );
        let page = |from: &str, order: &str| {
            plan_of(
                &conn,
                &format!(
                    "SELECT af.id,{order} AS sort_value FROM {from} WHERE {where_sql}
                     ORDER BY sort_value DESC,af.id ASC LIMIT 200 OFFSET 0"
                ),
                &shared.values,
            )
        };
        // 999 matches in 16 000 files are sparse for every order and scope.
        for (order_by, order) in [
            (SortOrder::Duration, "af.duration"),
            (SortOrder::Modified, "af.modified_at"),
            (SortOrder::SampleRate, "af.sample_rate"),
        ] {
            for folder_id in [None, Some(7)] {
                let request = SearchRequest {
                    order_by,
                    folder_id,
                    ..request.clone()
                };
                let from = inclusion.page_from(&conn, &request, 999).unwrap().unwrap();
                let plan = page(from, order);
                assert!(
                    plan.iter()
                        .any(|line| line.contains("idx_audio_visible_tagged_folders")),
                    "{plan:?}"
                );
            }
        }
        // 1 000 matches are dense: the first page walks the duration index.
        let from = inclusion
            .page_from(&conn, &request, 1_000)
            .unwrap()
            .unwrap();
        let plan = page(from, "af.duration");
        assert!(
            plan.iter().any(|line| line.contains("INDEX idx_duration")),
            "{plan:?}"
        );
        assert!(
            !plan.iter().any(|line| line.contains("FOR ORDER BY")),
            "only equal durations may be sorted: {plan:?}"
        );
        // A deep page or a folder scope would pass too many files first.
        for other in [
            SearchRequest {
                offset: 200,
                ..request.clone()
            },
            SearchRequest {
                folder_id: Some(7),
                ..request.clone()
            },
        ] {
            assert_eq!(
                inclusion.page_from(&conn, &other, 1_000).unwrap(),
                Some(TAGGED_FROM),
                "{other:?}"
            );
        }
        // Dense matches keep the planner's sort indexes for other orders.
        for order_by in [SortOrder::Modified, SortOrder::SampleRate] {
            let request = SearchRequest {
                order_by,
                ..request.clone()
            };
            assert!(
                inclusion
                    .page_from(&conn, &request, 1_000)
                    .unwrap()
                    .is_none()
            );
        }
        // Filename order already reads idx_audio_visible_tagged_any in order.
        for order_by in [SortOrder::Filename, SortOrder::Relevance] {
            let request = SearchRequest {
                order_by,
                ..request.clone()
            };
            assert!(inclusion.page_from(&conn, &request, 999).unwrap().is_none());
        }
        conn.execute_batch(
            "DROP INDEX idx_duration;
             CREATE INDEX idx_duration ON audio_files(duration) WHERE missing=0;",
        )
        .unwrap();
        assert_eq!(
            inclusion.page_from(&conn, &request, 1_000).unwrap(),
            Some(TAGGED_FROM)
        );
    }
}
