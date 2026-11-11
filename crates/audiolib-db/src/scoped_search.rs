// crates/audiolib-db/src/scoped_search.rs
//! Filtered directory search and facets share one parameterized membership recipe.
use crate::*;
use rusqlite::types::Value;
use sha2::{Digest, Sha256};

/// Compose directory membership without expanding matching files in application memory.
pub(crate) fn predicate(
    conn: &Connection,
    tokenizer: &Jieba,
    query: &str,
    scope: SearchScope,
    values: &mut Vec<Value>,
) -> Result<String> {
    if !dir_search::ready(conn)? {
        return if scope == SearchScope::Path {
            Ok("0".into())
        } else {
            values.push(Value::Text(compile_query(tokenizer, query)?));
            Ok(format!(
                "af.id IN (SELECT rowid FROM audio_search WHERE audio_search MATCH ?{})",
                values.len()
            ))
        };
    }
    let mut conditions = Vec::new();
    for term in search_text::query_terms(tokenizer, query) {
        let path = search_text::compile_query_columns(
            tokenizer,
            &dir_search::t3_norm(&term),
            "{path_tokens}",
            "{path_tokens_tokenized}",
        )?;
        values.push(Value::Text(path));
        let parameter = values.len();
        let directories = format!(
            "af.folder_id IN (
               WITH matched(dir_id) AS MATERIALIZED (
                 SELECT DISTINCT descendant_id FROM folder_closure
                 WHERE ancestor_id IN (SELECT rowid FROM audio_dir_search WHERE audio_dir_search MATCH ?{parameter})
               )
               SELECT dir_id FROM matched WHERE
                 (SELECT COALESCE(SUM(n),0) FROM audio_dir_runs WHERE dir_id IN matched)*2
                 <= (SELECT COUNT(*) FROM audio_files)
             )"
        );
        if scope == SearchScope::Path {
            conditions.push(directories);
        } else {
            values.push(Value::Text(compile_query(tokenizer, &term)?));
            conditions.push(format!(
                "({directories} OR af.id IN (SELECT rowid FROM audio_search WHERE audio_search MATCH ?{}))",
                values.len()
            ));
        }
    }
    if conditions.is_empty() {
        return Err(Error::Validation("Search requires a word or number".into()));
    }
    Ok(format!("({})", conditions.join(" AND ")))
}

#[derive(Serialize, Deserialize)]
struct FilterCursor {
    request: String,
    revision: u64,
    id: i64,
}

#[derive(Serialize, Deserialize)]
struct NativeCursor {
    request: String,
    revision: u64,
    position: u64,
    inner: String,
}

/// Validate the opaque optimized-page cursor before exposing its inner rank key.
pub(crate) fn native_cursor(
    provider: &SqliteProvider,
    request: &SearchRequest,
    revision: u64,
) -> Result<Option<(String, u64)>> {
    let Some(raw) = request.cursor.as_deref() else {
        return Ok(None);
    };
    let bytes = raw
        .strip_prefix("v3.")
        .ok_or_else(|| Error::Validation("CURSOR_STALE: search cursor protocol changed".into()))
        .and_then(|raw| {
            dir_search::base64url_decode(raw)
                .map_err(|_| Error::Validation("Search cursor is invalid".into()))
        })?;
    let value: NativeCursor = serde_json::from_slice(&bytes)
        .map_err(|_| Error::Validation("Search cursor is invalid".into()))?;
    if value.request != fingerprint(provider, request)? || value.revision != revision {
        return Err(Error::Validation(
            "CURSOR_STALE: search or library changed".into(),
        ));
    }
    Ok(Some((value.inner, value.position)))
}

pub(crate) fn encode_native_cursor(
    provider: &SqliteProvider,
    request: &SearchRequest,
    revision: u64,
    position: u64,
    inner: String,
) -> Result<String> {
    let value = NativeCursor {
        request: fingerprint(provider, request)?,
        revision,
        position,
        inner,
    };
    let bytes = serde_json::to_vec(&value).map_err(|error| Error::Database(error.to_string()))?;
    Ok(format!("v3.{}", dir_search::base64url(&bytes)))
}

fn number_conditions(conditions: Vec<String>, parameters: usize) -> Result<Vec<String>> {
    let mut last = 0_usize;
    let mut output = Vec::with_capacity(conditions.len());
    for condition in conditions {
        let mut rest = condition.as_str();
        let mut numbered = String::with_capacity(condition.len());
        while let Some(position) = rest.find('?') {
            numbered.push_str(&rest[..position]);
            rest = &rest[position + 1..];
            let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
            let index = if digits == 0 {
                last + 1
            } else {
                rest[..digits]
                    .parse::<usize>()
                    .map_err(|_| Error::Database("Invalid internal SQL parameter".into()))?
            };
            last = last.max(index);
            numbered.push('?');
            numbered.push_str(&index.to_string());
            rest = &rest[digits..];
        }
        numbered.push_str(rest);
        output.push(numbered);
    }
    if last != parameters {
        return Err(Error::Database(
            "Search filter parameter count disagrees with its SQL".into(),
        ));
    }
    Ok(output)
}

fn fingerprint(provider: &SqliteProvider, request: &SearchRequest) -> Result<String> {
    let mut identity = request.clone();
    identity.cursor = None;
    identity.offset = 0;
    identity.page = None;
    let mut hash = Sha256::new();
    hash.update(provider.cursor_session.as_bytes());
    hash.update(provider.path.to_string_lossy().as_bytes());
    hash.update(serde_json::to_vec(&identity).map_err(|e| Error::Validation(e.to_string()))?);
    Ok(format!("{:x}", hash.finalize()))
}

pub(crate) fn page(
    provider: &SqliteProvider,
    request: &SearchRequest,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<SearchPage> {
    let start = Instant::now();
    let before = provider.observe_search_revision()?;
    let mut conn = provider.search_reader(cancel)?;
    let tx = conn.transaction().map_err(db_error)?;
    tx.query_row("SELECT COUNT(*) FROM library_meta", [], |r| {
        r.get::<_, i64>(0)
    })
    .map_err(db_error)?;
    let revision = provider.observe_search_revision()?;
    if before != revision {
        return Err(Error::Validation(
            "CURSOR_STALE: library changed while opening search".into(),
        ));
    }
    page_in_snapshot(provider, &tx, request, revision, start)
}

/// Execute the complete scoped page in the caller's pinned snapshot. This does
/// not open another reader or admit uncommitted directory/ranking cache state.
pub(crate) fn page_in_snapshot(
    provider: &SqliteProvider,
    tx: &Connection,
    request: &SearchRequest,
    revision: u64,
    start: Instant,
) -> Result<SearchPage> {
    if tx.is_autocommit() {
        return Err(Error::Database(
            "Scoped search snapshot requires an active transaction".into(),
        ));
    }
    let identity = fingerprint(provider, request)?;
    let cursor = request
        .cursor
        .as_deref()
        .map(|raw| -> Result<FilterCursor> {
            let bytes = raw
                .strip_prefix("v2.")
                .ok_or_else(|| {
                    Error::Validation("CURSOR_STALE: search configuration changed".into())
                })
                .and_then(|v| {
                    dir_search::base64url_decode(v)
                        .map_err(|_| Error::Validation("Search cursor is invalid".into()))
                })?;
            let cursor: FilterCursor = serde_json::from_slice(&bytes)
                .map_err(|_| Error::Validation("Search cursor is invalid".into()))?;
            if cursor.request != identity || cursor.revision != revision {
                return Err(Error::Validation(
                    "CURSOR_STALE: search or library changed".into(),
                ));
            }
            Ok(cursor)
        })
        .transpose()?;
    let SearchSql {
        from,
        conditions,
        mut values,
    } = search_sql(
        tx,
        &provider.tokenizer,
        request,
        true,
        provider.derived_tags.load(Ordering::Acquire),
    )?;
    let conditions = number_conditions(conditions, values.len())?;
    let relevance = request.order_by == SortOrder::Relevance;
    let (prefix, join, tier, value) = if relevance && request.scope == SearchScope::Path {
        (String::new(), String::new(), "2".into(), "0.0".into())
    } else if relevance {
        values.push(Value::Text(compile_query(
            &provider.tokenizer,
            &request.query,
        )?));
        let parameter = values.len();
        let path = predicate(
            tx,
            &provider.tokenizer,
            &request.query,
            SearchScope::Path,
            &mut values,
        )?;
        (
            format!(
                "full AS MATERIALIZED (SELECT rowid AS id,bm25(audio_search) AS score FROM audio_search WHERE audio_search MATCH ?{parameter}),"
            ),
            "LEFT JOIN full ON full.id=af.id".to_string(),
            format!("CASE WHEN full.id IS NOT NULL THEN 0 WHEN {path} THEN 2 ELSE 1 END"),
            "COALESCE(full.score,0.0)".to_string(),
        )
    } else {
        let order = match request.order_by {
            SortOrder::Filename => "af.filename",
            SortOrder::Duration => "af.duration",
            SortOrder::SampleRate => "af.sample_rate",
            SortOrder::Modified => "af.modified_at",
            SortOrder::Channels => "af.channels",
            SortOrder::Tags => {
                "COALESCE(audiolib_user_tags_key(af.user_tags),audiolib_source_tags_key(af.metadata_json))"
            }
            SortOrder::Relevance => unreachable!(),
        };
        (String::new(), String::new(), "0".into(), order.to_owned())
    };
    // Only the main-field tier uses BM25. The agreed mixed and pure-path tiers
    // retain stable ID order, independent of incidental main-field partial scores.
    let score = if relevance {
        format!("CASE WHEN {tier}=0 THEN {value} ELSE 0.0 END")
    } else {
        value
    };
    let common = format!(
        "WITH {prefix} matching AS MATERIALIZED (
          SELECT af.id,{tier} AS tier,{score} AS sort_value FROM {from} {join} WHERE {}
        )",
        conditions.join(" AND ")
    );
    let text = matches!(request.order_by, SortOrder::Filename | SortOrder::Tags);
    let key = if text {
        "r.sort_value COLLATE NOCASE"
    } else {
        "r.sort_value"
    };
    let direction = if request.descending && !relevance {
        "DESC"
    } else {
        "ASC"
    };
    let continuation = if let Some(cursor) = cursor {
        values.push(Value::Integer(cursor.id));
        let p = values.len();
        if relevance {
            format!(
                "WHERE (r.tier,r.sort_value,r.id) > (SELECT tier,sort_value,id FROM matching WHERE id=?{p})"
            )
        } else {
            let op = if request.descending { "<" } else { ">" };
            let nulls = if request.descending {
                format!(
                    "r.sort_value IS NULL AND (SELECT sort_value FROM matching WHERE id=?{p}) IS NOT NULL"
                )
            } else {
                format!(
                    "r.sort_value IS NOT NULL AND (SELECT sort_value FROM matching WHERE id=?{p}) IS NULL"
                )
            };
            format!(
                "WHERE (({nulls}) OR {key} {op} (SELECT sort_value FROM matching WHERE id=?{p}) OR ({key} IS (SELECT sort_value FROM matching WHERE id=?{p}) AND r.id>?{p}))"
            )
        }
    } else {
        String::new()
    };
    values.push(Value::Integer(request.limit.saturating_add(1) as i64));
    let limit = values.len();
    values.push(Value::Integer(if request.cursor.is_some() {
        0
    } else {
        i64::try_from(request.offset)
            .map_err(|_| Error::Validation("Offset is too large".into()))?
    }));
    let offset = values.len();
    let projection = audio_projection(tx)?;
    let page_key = if text {
        "h.page_sort_value COLLATE NOCASE"
    } else {
        "h.page_sort_value"
    };
    // Count and page share exactly one complete membership in this snapshot.
    // Hydrate only the bounded page, before the count singleton's outer join:
    // an empty/deep page must not run Metadata functions on a synthetic NULL row.
    let mut statement = tx
        .prepare(&format!(
            "{common},
         paged AS MATERIALIZED (
           SELECT r.id,r.tier,r.sort_value FROM matching r {continuation}
           ORDER BY r.tier ASC,{key} {direction},r.id ASC LIMIT ?{limit} OFFSET ?{offset}
         ),
         hydrated AS MATERIALIZED (
           SELECT {projection},r.tier AS page_tier,r.sort_value AS page_sort_value,r.id AS page_id
           FROM paged r JOIN audio_files af ON af.id=r.id
         ),
         exact_count AS MATERIALIZED (SELECT COUNT(*) AS n FROM matching)
         SELECT h.*,c.n FROM exact_count c LEFT JOIN hydrated h ON 1
         ORDER BY h.page_tier ASC,{page_key} {direction},h.page_id ASC"
        ))
        .map_err(db_error)?;
    let count_column = statement.column_count() - 1;
    let mut rows = statement
        .query(params_from_iter(&values))
        .map_err(db_error)?;
    let mut total = 0;
    let mut results = Vec::new();
    while let Some(row) = rows.next().map_err(db_error)? {
        total = unsigned(row, count_column).map_err(db_error)?;
        if row.get::<_, Option<i64>>(0).map_err(db_error)?.is_some() {
            results.push(audio_from_row(row).map_err(db_error)?);
        }
    }
    let more = results.len() > request.limit;
    results.truncate(request.limit);
    let next_cursor = if more {
        results
            .last()
            .map(|row| {
                serde_json::to_vec(&FilterCursor {
                    request: identity,
                    revision,
                    id: row.id,
                })
                .map(|bytes| format!("v2.{}", dir_search::base64url(&bytes)))
                .map_err(|e| Error::Database(e.to_string()))
            })
            .transpose()?
    } else {
        None
    };
    Ok(SearchPage {
        total_count_exact: true,
        returned_count: results.len(),
        results,
        total_count: total,
        query: request.query.clone(),
        execution_time_ms: start.elapsed().as_secs_f64() * 1000.0,
        cache_hit: false,
        next_cursor,
    })
}

/// Encode a committed first-page continuation without trusting a provisional
/// writer cursor or a native cursor's opaque representation. Both scoped plans
/// order by tier, then exact main BM25 (or zero for other tiers), then audio ID.
pub(crate) fn encode_committed_first_page_cursor(
    provider: &SqliteProvider,
    request: &SearchRequest,
    revision: u64,
    last_id: Option<i64>,
    has_more: bool,
) -> Result<Option<String>> {
    if !has_more
        || request.query.trim().is_empty()
        || !matches!(request.scope, SearchScope::All | SearchScope::Path)
    {
        return Ok(None);
    }
    let id = last_id.ok_or_else(|| {
        Error::Database("Scoped first page has a continuation without a last audio ID".into())
    })?;
    let cursor = FilterCursor {
        request: fingerprint(provider, request)?,
        revision,
        id,
    };
    serde_json::to_vec(&cursor)
        .map(|bytes| Some(format!("v2.{}", dir_search::base64url(&bytes))))
        .map_err(|error| Error::Database(error.to_string()))
}

pub(crate) fn folders(
    provider: &SqliteProvider,
    request: &SearchRequest,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<Vec<Folder>> {
    let mut reader = provider.search_reader(cancel)?;
    let tx = reader.transaction().map_err(db_error)?;
    let sql = search_sql(
        &tx,
        &provider.tokenizer,
        request,
        false,
        provider.derived_tags.load(Ordering::Acquire),
    )?;
    let mut statement = tx.prepare(&format!(
        "WITH direct AS (SELECT af.folder_id AS id,COUNT(*) AS n FROM {} WHERE {} GROUP BY af.folder_id),
         totals AS (SELECT fc.ancestor_id AS id,SUM(d.n) AS n FROM direct d JOIN folder_closure fc ON fc.descendant_id=d.id GROUP BY fc.ancestor_id)
         SELECT f.id,f.folder_path,f.folder_name,f.parent_id,t.n FROM folders f JOIN totals t ON t.id=f.id WHERE t.n>0 ORDER BY f.folder_path COLLATE NOCASE,f.id",
        sql.from, sql.conditions.join(" AND ")
    )).map_err(db_error)?;
    statement
        .query_map(params_from_iter(&sql.values), |r| {
            Ok(Folder {
                id: r.get(0)?,
                path: r.get(1)?,
                name: r.get(2)?,
                parent_id: r.get(3)?,
                file_count: unsigned(r, 4)?,
            })
        })
        .map_err(db_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(db_error)
}
