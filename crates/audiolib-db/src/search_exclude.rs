// crates/audiolib-db/src/search_exclude.rs
//! Exclude-only tag filters without testing every visible file's wide row.
//!
//! An exclude-only filter keeps a file when it has no tags, or when none of the
//! terms occurs in its tags. Present files with tags are exactly the entries of the
//! sparse idx_audio_visible_tagged_folders index, so the matches are the present
//! files minus the indexed files containing a term. Both sides read narrow
//! covering indexes and only tagged files evaluate the terms; the result is the
//! same set the shared statement selects, so totals and facets are unchanged.
//! A library-wide duration page over dense matches reads idx_duration in order and
//! stops after the page in either direction, whether or not ANALYZE statistics exist.
//! With a query, the complete ranking skips the excluded (and non-present) files
//! rather than intersecting with every passing file.
use crate::{Result, db_error, id_membership::IdMembership, schema, search_sorted, unsigned};
use audiolib_core::{Error, SearchFilters, SearchRequest, SortOrder};
use rusqlite::{Connection, params_from_iter, types::Value};

pub(super) const TAGGED_INDEX: &str = "CREATE INDEX idx_audio_visible_tagged_folders
     ON audio_files(folder_id,id,user_tags,source_tags_text)
     WHERE missing=0 AND (COALESCE(trim(user_tags),'')<>'' OR source_tags_text<>'')";

// Tagged present files containing any term (audiolib_tag_terms is 0 for them).
// The partial predicate is repeated verbatim so SQLite may read the sparse index.
const EXCLUDED: &str = "audio_files af INDEXED BY idx_audio_visible_tagged_folders
     WHERE af.missing=0 AND (COALESCE(trim(af.user_tags),'')<>'' OR af.source_tags_text<>'')
     AND audiolib_tag_terms(af.user_tags,af.source_tags_text,?1)=0";

/// A proven exclude-only filter on present files with no other filter. A request
/// with a query is planned with its query cleared, as its filter membership.
pub(super) struct Exclusion {
    terms: Value,
}

impl Exclusion {
    /// Some only when the subtraction is exact for this request in this snapshot:
    /// derived tag columns are current and the sparse index has its known
    /// definition. Queries, include terms and other filters keep the shared plan.
    pub(super) fn plan(
        tx: &Connection,
        request: &SearchRequest,
        derived_tags: bool,
    ) -> Result<Option<Self>> {
        let filters = &request.filters;
        // The default visibility is Present, so this also requires present files.
        if !derived_tags
            || !request.query.trim().is_empty()
            || filters.exclude_tags.is_empty()
            || *filters
                != (SearchFilters {
                    exclude_tags: filters.exclude_tags.clone(),
                    ..Default::default()
                })
            || !schema::definition::matches_sql(
                tx,
                "idx_audio_visible_tagged_folders",
                "index",
                "audio_files",
                TAGGED_INDEX,
            )?
        {
            return Ok(None);
        }
        // Exactly the [exclude, folded term] argument search_sql binds.
        let terms = filters
            .exclude_tags
            .iter()
            .map(|tag| (true, schema::fold_case(tag)))
            .collect::<Vec<_>>();
        let terms =
            serde_json::to_string(&terms).map_err(|error| Error::Database(error.to_string()))?;
        Ok(Some(Self {
            terms: Value::Text(terms),
        }))
    }

    /// Matching files in the requested folder scope: present files minus excluded files.
    pub(super) fn total(&self, tx: &Connection, request: &SearchRequest) -> Result<u64> {
        let (scope, mut values) = folder_scope(request);
        values.insert(0, self.terms.clone());
        tx.query_row(
            &format!(
                "SELECT (SELECT COUNT(*) FROM audio_files af WHERE af.missing=0{scope})
                 -(SELECT COUNT(*) FROM {EXCLUDED}{scope})"
            ),
            params_from_iter(&values),
            |row| unsigned(row, 0),
        )
        .map_err(db_error)
    }

    /// The FROM clause for the shared page statement. Without statistics SQLite sorts
    /// every present file for a descending duration page; the proven sort index stops
    /// after the page instead. That walk tests each file it passes, so it is used only
    /// while the `total` matches are dense enough to end within about a quarter of the
    /// index; sparse matches and deep pages keep the planner, as does a folder scope.
    pub(super) fn page_from(
        &self,
        tx: &Connection,
        request: &SearchRequest,
        total: u64,
    ) -> Result<Option<&'static str>> {
        let end = request.offset.saturating_add(request.limit) as u64;
        Ok((request.order_by == SortOrder::Duration
            && request.folder_id.is_none()
            && end.saturating_mul(4) <= total
            && search_sorted::single_column_index(tx, "idx_duration", "duration")?)
        .then_some("audio_files af INDEXED BY idx_duration"))
    }

    /// IDs a library-wide query ranking skips for this filter: files that are not
    /// present (including legacy NULL/nonbinary flags) and excluded tagged files.
    pub(super) fn dropped_ids(&self) -> (String, Vec<Value>) {
        (
            format!(
                "SELECT id FROM audio_files INDEXED BY idx_audio_files_not_present
                 WHERE missing IS NOT 0
                 UNION ALL SELECT af.id FROM {EXCLUDED}"
            ),
            vec![self.terms.clone()],
        )
    }

    /// Complete query IDs without the excluded tagged files, sorted for the folder
    /// counters. Those counters apply the present-file predicate, as for every filter.
    pub(super) fn without_excluded(
        &self,
        tx: &Connection,
        ids: &[i64],
        mut check_cancel: impl FnMut() -> Result<()>,
    ) -> Result<Vec<i64>> {
        let mut excluded = IdMembership::for_ranked_ids(ids, &mut check_cancel)?;
        let mut statement = tx
            .prepare(&format!("SELECT af.id FROM {EXCLUDED}"))
            .map_err(db_error)?;
        let mut rows = statement.query([&self.terms]).map_err(db_error)?;
        let mut visited = 0usize;
        while let Some(row) = rows.next().map_err(db_error)? {
            if visited.is_multiple_of(1024) {
                check_cancel()?;
            }
            visited += 1;
            excluded.insert(row.get(0).map_err(db_error)?);
        }
        let mut kept = Vec::with_capacity(ids.len());
        for (index, &id) in ids.iter().enumerate() {
            if index.is_multiple_of(1024) {
                check_cancel()?;
            }
            if !excluded.contains(&id) {
                kept.push(id);
            }
        }
        kept.sort_unstable();
        check_cancel()?;
        Ok(kept)
    }

    /// Direct per-folder matches for the facet rollup, independent of folder scope.
    /// Folders whose files are all excluded keep a zero row; the rollup drops them.
    pub(super) fn folder_counts(&self) -> (String, Vec<Value>) {
        (
            format!(
                "SELECT folder_id,SUM(matches) AS matches FROM (
                    SELECT af.folder_id,COUNT(*) AS matches FROM audio_files af
                    WHERE af.missing=0 GROUP BY af.folder_id
                    UNION ALL
                    SELECT af.folder_id,-COUNT(*) AS matches FROM {EXCLUDED}
                    GROUP BY af.folder_id
                ) GROUP BY folder_id"
            ),
            vec![self.terms.clone()],
        )
    }
}

fn folder_scope(request: &SearchRequest) -> (&'static str, Vec<Value>) {
    match request.folder_id {
        Some(folder) if request.include_subfolders => (
            " AND af.folder_id IN (SELECT descendant_id FROM folder_closure WHERE ancestor_id=?2)",
            vec![Value::Integer(folder)],
        ),
        Some(folder) => (" AND af.folder_id=?2", vec![Value::Integer(folder)]),
        None => ("", Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SqliteProvider, load_tokenizer, search_sql};
    use audiolib_core::FileVisibility;

    fn fixture() -> (tempfile::TempDir, Connection) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("exclude.db");
        drop(SqliteProvider::open(&path).unwrap());
        let conn = Connection::open(&path).unwrap();
        crate::register_library_functions(&conn).unwrap();
        (directory, conn)
    }

    fn excluding(terms: &[&str]) -> SearchRequest {
        SearchRequest {
            filters: SearchFilters {
                exclude_tags: terms.iter().map(|term| (*term).into()).collect(),
                ..Default::default()
            },
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

    #[test]
    fn only_present_exclude_only_requests_on_the_known_tag_index_are_planned() {
        let (_directory, conn) = fixture();
        let request = excluding(&["Door", "ÜBER"]);
        let exclusion = Exclusion::plan(&conn, &request, true).unwrap().unwrap();
        // The subtraction must test exactly the terms the shared statement binds.
        let shared = search_sql(&conn, &load_tokenizer(), &request, true, true).unwrap();
        assert_eq!(shared.values, [exclusion.terms]);
        for eligible in [
            SearchRequest {
                folder_id: Some(1),
                include_subfolders: false,
                ..request.clone()
            },
            SearchRequest {
                order_by: SortOrder::Duration,
                descending: true,
                offset: 400,
                ..request.clone()
            },
        ] {
            assert!(Exclusion::plan(&conn, &eligible, true).unwrap().is_some());
        }
        let filtered = |filters: SearchFilters| SearchRequest {
            filters: SearchFilters {
                exclude_tags: request.filters.exclude_tags.clone(),
                ..filters
            },
            ..request.clone()
        };
        for other in [
            SearchRequest {
                query: "door".into(),
                ..request.clone()
            },
            excluding(&[]),
            filtered(SearchFilters {
                tag_contains: Some("engine".into()),
                ..Default::default()
            }),
            filtered(SearchFilters {
                include_tags: vec!["engine".into()],
                ..Default::default()
            }),
            filtered(SearchFilters {
                tagged_only: true,
                ..Default::default()
            }),
            filtered(SearchFilters {
                duration_max: Some(5.0),
                ..Default::default()
            }),
            filtered(SearchFilters {
                visibility: FileVisibility::All,
                ..Default::default()
            }),
            filtered(SearchFilters {
                visibility: FileVisibility::Missing,
                ..Default::default()
            }),
        ] {
            assert!(
                Exclusion::plan(&conn, &other, true).unwrap().is_none(),
                "{other:?}"
            );
        }
        // Tag columns still being backfilled use the JSON fallback statement.
        assert!(Exclusion::plan(&conn, &request, false).unwrap().is_none());
        // An index omitting other tagged files would make the subtraction inexact.
        conn.execute_batch(
            "DROP INDEX idx_audio_visible_tagged_folders;
             CREATE INDEX idx_audio_visible_tagged_folders
             ON audio_files(folder_id,id,user_tags,source_tags_text)
             WHERE missing=0 AND COALESCE(trim(user_tags),'')<>'';",
        )
        .unwrap();
        assert!(Exclusion::plan(&conn, &request, true).unwrap().is_none());
    }

    #[test]
    fn counts_read_narrow_indexes_and_duration_pages_stop_in_index_order() {
        // A new library has no ANALYZE statistics, as most real libraries.
        let (_directory, conn) = fixture();
        let request = SearchRequest {
            order_by: SortOrder::Duration,
            descending: true,
            ..excluding(&["door"])
        };
        let exclusion = Exclusion::plan(&conn, &request, true).unwrap().unwrap();
        for scoped in [None, Some(true), Some(false)] {
            let request = SearchRequest {
                folder_id: scoped.map(|_| 7),
                include_subfolders: scoped.unwrap_or(true),
                ..request.clone()
            };
            let (scope, mut values) = folder_scope(&request);
            values.insert(0, exclusion.terms.clone());
            let plan = plan_of(
                &conn,
                &format!(
                    "SELECT (SELECT COUNT(*) FROM audio_files af WHERE af.missing=0{scope})
                     -(SELECT COUNT(*) FROM {EXCLUDED}{scope})"
                ),
                &values,
            );
            assert!(
                plan.iter()
                    .any(|line| line.contains("idx_audio_visible_tagged_folders")),
                "{plan:?}"
            );
            assert!(!plan.iter().any(|line| line == "SCAN af"), "{plan:?}");
        }
        let (direct, values) = exclusion.folder_counts();
        let plan = plan_of(&conn, &direct, &values);
        assert!(
            plan.iter()
                .any(|line| line.contains("COVERING INDEX idx_audio_visible_tagged_folders")),
            "{plan:?}"
        );
        assert!(!plan.iter().any(|line| line == "SCAN af"), "{plan:?}");

        let shared = search_sql(&conn, &load_tokenizer(), &request, true, true).unwrap();
        // A 200-file first page needs at least 800 matches to use the index walk.
        let from = exclusion.page_from(&conn, &request, 800).unwrap().unwrap();
        let plan = plan_of(
            &conn,
            &format!(
                "SELECT af.id,af.duration AS sort_value FROM {from} WHERE {}
                 ORDER BY sort_value DESC,af.id ASC LIMIT 200 OFFSET 0",
                shared.conditions.join(" AND ")
            ),
            &shared.values,
        );
        assert!(
            plan.iter().any(|line| line.contains("INDEX idx_duration")),
            "{plan:?}"
        );
        assert!(
            !plan.iter().any(|line| line.contains("FOR ORDER BY")),
            "only equal durations may be sorted: {plan:?}"
        );
        let deep = SearchRequest {
            offset: 400,
            ..request.clone()
        };
        assert!(exclusion.page_from(&conn, &deep, 2_400).unwrap().is_some());
        for (other, total) in [
            (request.clone(), 799),
            (deep, 2_399),
            (
                SearchRequest {
                    folder_id: Some(7),
                    ..request.clone()
                },
                1_000_000,
            ),
            (
                SearchRequest {
                    order_by: SortOrder::Filename,
                    ..request.clone()
                },
                1_000_000,
            ),
        ] {
            assert!(exclusion.page_from(&conn, &other, total).unwrap().is_none());
        }
        conn.execute_batch(
            "DROP INDEX idx_duration;
             CREATE INDEX idx_duration ON audio_files(duration) WHERE missing=0;",
        )
        .unwrap();
        assert!(
            exclusion
                .page_from(&conn, &request, 1_000_000)
                .unwrap()
                .is_none()
        );
    }
}
