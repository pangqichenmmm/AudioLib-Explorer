// crates/audiolib-db/src/bin/search-diagnose.rs
//! Read-only raw-token SQL plans and timings, retained as comparable SQLite baselines.
//! Literal `MATCH 'metal'` bypasses the current DataProvider fragment compiler and
//! search pipeline. These probes are not DataProvider or GUI end-to-end measurements.
//! `DATABASE --path QUERY` instead prints EQP and VDBE plans for directory-aware
//! search statements, without executing the searches or reading file paths.
use audiolib_db::SQLITE_READER_MMAP_BYTES;
use rusqlite::{Connection, OpenFlags, params_from_iter, types::Value};
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let usage = "Usage: search-diagnose DATABASE [--path QUERY]";
    let path = args.next().ok_or(usage)?;
    let path_query = match args.next().as_deref() {
        Some("--path") => Some(args.next().ok_or(usage)?),
        Some(_) => return Err(usage.into()),
        None => None,
    };
    if args.next().is_some() {
        return Err(usage.into());
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    if let Some(query) = path_query {
        return path_plans(&conn, &query);
    }
    conn.execute_batch("PRAGMA cache_size=-65536; PRAGMA temp_store=MEMORY;")?;
    conn.pragma_update(None, "mmap_size", SQLITE_READER_MMAP_BYTES)?;
    println!("SQLite {}", rusqlite::version());
    println!(
        "Mode: raw-token SQL baseline using literal MATCH 'metal'; not current DataProvider letter-fragment search."
    );
    println!(
        "Original SQL predicates and labels are retained for comparison. These timings do not measure the DataProvider pipeline or GUI input-to-display latency."
    );
    for (label, sql) in [
        (
            "count",
            "SELECT COUNT(*) FROM audio_search WHERE audio_search MATCH 'metal'",
        ),
        (
            "rank_ids",
            "SELECT rowid,rank FROM audio_search WHERE audio_search MATCH 'metal' ORDER BY rank LIMIT 200",
        ),
        (
            "rank_join",
            "SELECT af.id,af.filename FROM audio_files af INNER JOIN audio_search ON audio_search.rowid=af.id WHERE af.missing=0 AND audio_search MATCH 'metal' ORDER BY audio_search.rank LIMIT 200",
        ),
        (
            "rank_cte",
            "WITH ranked AS MATERIALIZED (SELECT rowid,rank FROM audio_search WHERE audio_search MATCH 'metal' ORDER BY rank LIMIT 200) SELECT af.id,af.filename FROM ranked INNER JOIN audio_files af ON af.id=ranked.rowid ORDER BY ranked.rank,af.id",
        ),
        (
            "bm25_ids",
            "SELECT rowid,bm25(audio_search) FROM audio_search WHERE audio_search MATCH 'metal' ORDER BY bm25(audio_search),rowid LIMIT 200",
        ),
        (
            "rowids_without_relevance",
            "SELECT rowid FROM audio_search WHERE audio_search MATCH 'metal' LIMIT 200",
        ),
        (
            "folder_counts",
            "SELECT af.folder_id,COUNT(*) FROM audio_files af JOIN audio_search ON audio_search.rowid=af.id WHERE af.missing=0 AND audio_search MATCH 'metal' GROUP BY af.folder_id",
        ),
    ] {
        let mut plan = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
        println!("{label}:");
        for detail in plan.query_map([], |row| row.get::<_, String>(3))? {
            println!("  {}", detail?);
        }
        for pass in 0..3 {
            let start = Instant::now();
            let mut statement = conn.prepare(sql)?;
            let mut rows = statement.query([])?;
            let mut count = 0;
            while rows.next()?.is_some() {
                count += 1;
            }
            println!(
                "  pass {pass}: {} rows, {:.3} ms",
                count,
                start.elapsed().as_secs_f64() * 1000.0
            );
        }
    }
    for pass in 0..3 {
        let start = Instant::now();
        let mut statement = conn.prepare("SELECT af.folder_id FROM audio_search JOIN audio_files af ON af.id=audio_search.rowid WHERE af.missing=0 AND audio_search MATCH 'metal'")?;
        let mut rows = statement.query([])?;
        let mut counts = std::collections::HashMap::<Option<i64>, u64>::new();
        while let Some(row) = rows.next()? {
            *counts.entry(row.get(0)?).or_default() += 1;
        }
        println!(
            "folder_counts_stream pass {pass}: {} folders, {:.3} ms",
            counts.len(),
            start.elapsed().as_secs_f64() * 1000.0
        );
    }
    Ok(())
}

/// Explain the SQL shapes in `dir_search.rs` using visible-file predicates.
/// Bindings are deliberately representative: this is not the production fragment
/// compiler, a query execution, or a measure of the number of calls made by a page.
fn path_plans(conn: &Connection, query: &str) -> Result<(), Box<dyn std::error::Error>> {
    let terms: Vec<_> = query
        .split_whitespace()
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect();
    if terms.is_empty() {
        return Err("--path requires a nonempty query".into());
    }
    let main_match = terms.join(" AND ");
    let directory_match = format!("{{path_tokens}} : {}", terms[0]);
    println!("SQLite {}", rusqlite::version());
    println!("Mode: directory-aware SQL plans (dir_search.rs), FileVisibility::Present.");
    println!(
        "Only EXPLAIN QUERY PLAN and EXPLAIN are executed. No search timing, result counts, or filesystem paths are emitted."
    );
    println!(
        "MATCH bindings use quoted raw whitespace terms, not production fragment compilation; directory MATCH models one term per loop. Numeric bindings are synthetic planner inputs."
    );
    println!(
        "Before-only statements are retained for plan comparison and are no longer executed by directory-aware search."
    );
    let raw = Value::Text(main_match);
    let directory = Value::Text(directory_match);
    explain(
        conn,
        "segment_matches (once per term)",
        "SELECT rowid FROM audio_dir_search WHERE audio_dir_search MATCH ?1",
        &[directory],
    )?;
    explain(
        conn,
        "main_has_hit (term with no directory hits)",
        "SELECT rowid FROM audio_search WHERE audio_search MATCH ?1 LIMIT 1",
        &[Value::Text(terms[0].clone())],
    )?;
    explain(
        conn,
        "before-only matches_row (removed per-file main MATCH)",
        "SELECT 1 FROM audio_search WHERE audio_search MATCH ?1 AND rowid=?2",
        &[raw.clone(), Value::Integer(1)],
    )?;
    explain(
        conn,
        "before-only stream_main (removed unbounded-upper main stream)",
        "SELECT rowid FROM audio_search WHERE audio_search MATCH ?1 AND rowid>?2 ORDER BY rowid LIMIT 256",
        &[raw.clone(), Value::Integer(0)],
    )?;
    explain(
        conn,
        "MainPostings::head (mixed-tier per-term advancing batch)",
        "SELECT rowid FROM audio_search WHERE audio_search MATCH ?1
         AND rowid>?2 AND rowid<=?3 ORDER BY rowid LIMIT 256",
        &[
            Value::Text(terms[0].clone()),
            Value::Integer(0),
            Value::Integer(1000),
        ],
    )?;
    explain(
        conn,
        "MainPostings::head (mixed-tier full-match exclusion batch)",
        "SELECT rowid FROM audio_search WHERE audio_search MATCH ?1
         AND rowid>?2 AND rowid<=?3 ORDER BY rowid LIMIT 256",
        &[raw.clone(), Value::Integer(0), Value::Integer(1000)],
    )?;
    explain(
        conn,
        "MainPostings::head (scope=all tier-2 full-match exclusion batch)",
        "SELECT rowid FROM audio_search WHERE audio_search MATCH ?1
         AND rowid>?2 AND rowid<=?3 ORDER BY rowid LIMIT 256",
        &[raw.clone(), Value::Integer(0), Value::Integer(i64::MAX)],
    )?;
    explain(
        conn,
        "stream_mixed visibility (one actual posting candidate)",
        "SELECT 1 FROM audio_files WHERE id=?1 AND folder_id=?2 AND missing=0",
        &[Value::Integer(1), Value::Integer(1)],
    )?;
    explain(
        conn,
        "expand (one run batch)",
        "SELECT id FROM audio_files WHERE id>=?1 AND id<=?2 AND folder_id=?4 AND missing=0 ORDER BY id LIMIT ?3",
        &[
            Value::Integer(1),
            Value::Integer(1000),
            Value::Integer(256),
            Value::Integer(1),
        ],
    )?;
    explain(
        conn,
        "tier0 (scope=all, first page)",
        "SELECT s, id FROM (
            SELECT rowid AS id, bm25(audio_search) AS s
            FROM audio_search WHERE audio_search MATCH ?1
         ) ranked
         WHERE EXISTS (
            SELECT 1 FROM audio_files af WHERE af.id = ranked.id AND missing=0
         )
         ORDER BY s, id LIMIT ?2",
        &[raw.clone(), Value::Integer(201)],
    )?;
    explain(
        conn,
        "tier0 (scope=all, cursor page)",
        "SELECT s, id FROM (
            SELECT rowid AS id, bm25(audio_search) AS s
            FROM audio_search WHERE audio_search MATCH ?1
         ) ranked
         WHERE (s > ?2 OR (s = ?2 AND id > ?3))
           AND EXISTS (
            SELECT 1 FROM audio_files af WHERE af.id = ranked.id AND missing=0
           )
         ORDER BY s, id LIMIT ?4",
        &[
            raw,
            Value::Real(-1.0),
            Value::Integer(1),
            Value::Integer(201),
        ],
    )?;
    explain(
        conn,
        "load_index generation",
        "SELECT value FROM library_meta WHERE key=?1",
        &[Value::Text("dir_index_gen".into())],
    )?;
    explain(
        conn,
        "read_index runs (on cache miss)",
        "SELECT lo, hi, n, dir_id FROM audio_dir_runs ORDER BY lo",
        &[],
    )?;
    explain(
        conn,
        "read_index folders (on cache miss)",
        "SELECT id, parent_id, COALESCE(total_file_count, 0) FROM folders",
        &[],
    )?;
    Ok(())
}

fn explain(conn: &Connection, label: &str, sql: &str, bindings: &[Value]) -> rusqlite::Result<()> {
    println!("\n{label}:");
    println!("SQL: {sql}");
    println!("Bindings: {bindings:?}");
    let mut plan = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
    let mut rows = plan.query(params_from_iter(bindings))?;
    println!("EQP: id parent detail");
    while let Some(row) = rows.next()? {
        println!(
            "  {} {} {}",
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(3)?
        );
    }
    let mut program = conn.prepare(&format!("EXPLAIN {sql}"))?;
    let mut rows = program.query(params_from_iter(bindings))?;
    println!("VDBE: addr opcode p1 p2 p3 p4 p5 comment");
    while let Some(row) = rows.next()? {
        println!(
            "  {} {} {} {} {} {} {} {}",
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, Option<String>>(5)?.unwrap_or_default(),
            row.get::<_, i64>(6)?,
            row.get::<_, Option<String>>(7)?.unwrap_or_default()
        );
    }
    Ok(())
}
