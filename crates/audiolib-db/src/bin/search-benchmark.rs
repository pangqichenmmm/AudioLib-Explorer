// crates/audiolib-db/src/bin/search-benchmark.rs
//! Standalone 1M+ record benchmark through the real Rust DataProvider search pipeline.
use audiolib_core::{
    DataProvider, MaintenanceAction, MetadataPatch, SearchFilters, SearchRequest, SortOrder,
};
use audiolib_db::{
    ParallelRankingDiagnostics, SQLITE_AUXILIARY_READER_CACHE_KB, SQLITE_AUXILIARY_SEARCH_READERS,
    SQLITE_READER_MMAP_BYTES, SqliteProvider, auxiliary_search_reader_count,
    parallel_ranking_diagnostics, sqlite_reader_mmap_bytes,
};
use rusqlite::{Connection, params};
use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

fn diagnostics_delta(before: ParallelRankingDiagnostics) -> serde_json::Value {
    let after = parallel_ranking_diagnostics();
    let reasons = after
        .reasons
        .iter()
        .enumerate()
        .map(|(index, reason)| {
            (
                (*reason).to_string(),
                serde_json::json!(after.fallbacks[index].saturating_sub(before.fallbacks[index])),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    serde_json::json!({
        "attempts":after.attempts.saturating_sub(before.attempts),
        "sparse_native_length_selections":after.sparse_native_length_selections.saturating_sub(before.sparse_native_length_selections),
        "fallbacks":reasons,
        "scope":"process-wide counter delta during the measured search interval"
    })
}

struct ConcurrentWriter {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<Result<u64, String>>>,
}

impl ConcurrentWriter {
    fn start(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let provider = SqliteProvider::open_existing(path)?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let (ready, waiting) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let mut commits = 0_u64;
            while !stopping.load(Ordering::Acquire) {
                provider
                    .update_metadata(
                        1,
                        &MetadataPatch {
                            favorite: Some(commits.is_multiple_of(2)),
                            ..Default::default()
                        },
                    )
                    .map_err(|error| error.to_string())?;
                commits += 1;
                if commits == 1 {
                    let _ = ready.send(());
                }
                thread::sleep(Duration::from_millis(2));
            }
            Ok(commits)
        });
        let writer = Self {
            stop,
            worker: Some(worker),
        };
        if waiting.recv_timeout(Duration::from_secs(30)).is_err() {
            writer.finish()?;
            return Err("Concurrent writer did not produce its first commit".into());
        }
        Ok(writer)
    }

    fn finish(mut self) -> Result<u64, Box<dyn std::error::Error>> {
        self.stop.store(true, Ordering::Release);
        self.worker
            .take()
            .expect("active benchmark writer")
            .join()
            .map_err(|_| "Concurrent benchmark writer panicked")?
            .map_err(Into::into)
    }
}

impl Drop for ConcurrentWriter {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn concurrent_writer_report(
    path: &Path,
    cases: &[SearchRequest],
    queries: usize,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let measure = || -> Result<Vec<f64>, Box<dyn std::error::Error>> {
        let mut samples = Vec::with_capacity(queries);
        for index in 0..queries {
            // A fresh provider avoids hiding writer contention behind cached ranks.
            // Provider open time is outside the DataProvider search timer.
            let provider = SqliteProvider::open_read_only(path)?;
            samples.push(
                provider
                    .search(&cases[index % cases.len()])?
                    .execution_time_ms,
            );
        }
        Ok(samples)
    };
    let baseline_before = parallel_ranking_diagnostics();
    let baseline = measure()?;
    let baseline_diagnostics = diagnostics_delta(baseline_before);
    let writer = ConcurrentWriter::start(path)?;
    let concurrent_before = parallel_ranking_diagnostics();
    let concurrent = measure()?;
    let concurrent_diagnostics = diagnostics_delta(concurrent_before);
    let commits = writer.finish()?;
    let p95 = |values: &[f64]| {
        let mut sorted = values.to_vec();
        sorted.sort_by(f64::total_cmp);
        sorted[((sorted.len() - 1) as f64 * 0.95).ceil() as usize]
    };
    let baseline_p95 = p95(&baseline);
    let concurrent_p95 = p95(&concurrent);
    Ok(serde_json::json!({
        "queries":queries,"writer_commits":commits,"writer_pause_ms":2,
        "baseline_samples_ms":baseline,"samples_ms":concurrent,
        "baseline_p95_ms":baseline_p95,"p95_ms":concurrent_p95,
        "p95_ratio":(baseline_p95>0.0).then_some(concurrent_p95/baseline_p95),
        "baseline_parallel_ranking":baseline_diagnostics,
        "parallel_ranking":concurrent_diagnostics,
        "condition":"Owned synthetic fixture only. Fresh read-only provider per search; baseline precedes concurrent Metadata favorite toggles committed through DataProvider. OS pages are not flushed; provider opening and folder facets are excluded. This measures commit contention and snapshot-race fallback, not a complete scanning workload. Zero fallback counts are reported unchanged."
    }))
}

fn request_file_cases(path: &Path) -> Result<Vec<SearchRequest>, Box<dyn std::error::Error>> {
    const MAX_BYTES: u64 = 1024 * 1024;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err("--request-file exceeds 1 MiB".into());
    }
    let requests: Vec<SearchRequest> = serde_json::from_slice(&bytes)?;
    if requests.is_empty() {
        return Err("--request-file must contain at least one SearchRequest".into());
    }
    for request in &requests {
        request.validate()?;
    }
    Ok(requests)
}

fn first_only_report(
    path: &Path,
    case_indices: &[usize],
    cases: &[SearchRequest],
    request_file: Option<&Path>,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let source = std::fs::canonicalize(path)?;
    // Refuse implicit backup/index rebuilding before a supposed first query.
    // This reads schema/derived-state markers, never library totals or Metadata.
    if !SqliteProvider::has_current_search_state(&source)? {
        return Err("--first-only requires current derived search state; upgrade an owned snapshot separately before benchmarking".into());
    }
    let mut samples = Vec::with_capacity(cases.len());
    let mut opened = Vec::with_capacity(cases.len());
    let mut searches = Vec::with_capacity(cases.len());
    let mut facets = Vec::with_capacity(cases.len());
    let mut totals = Vec::with_capacity(cases.len());
    for (&case_index, request) in case_indices.iter().zip(cases) {
        let open_started = Instant::now();
        let provider = SqliteProvider::open_read_only(&source)?;
        let actual = std::fs::canonicalize(provider.database_path())?;
        if actual != source {
            return Err("--first-only refuses an implicitly upgraded owned copy instead of the requested source".into());
        }
        let open_ms = open_started.elapsed().as_secs_f64() * 1000.0;
        // No main provider, stats, COUNT or earlier business search precedes this.
        let started = Instant::now();
        let page = provider.search(request)?;
        let search_ms = started.elapsed().as_secs_f64() * 1000.0;
        let facets_started = Instant::now();
        let folders = provider.search_folders(request)?;
        let facet_ms = facets_started.elapsed().as_secs_f64() * 1000.0;
        let total_ms = started.elapsed().as_secs_f64() * 1000.0;
        let result_ids = page
            .results
            .iter()
            .map(|audio| audio.id)
            .collect::<Vec<_>>();
        let mut folder_counts = folders
            .iter()
            .map(|folder| (folder.id, folder.file_count))
            .collect::<Vec<_>>();
        folder_counts.sort_unstable_by_key(|&(id, _)| id);
        opened.push(open_ms);
        searches.push(search_ms);
        facets.push(facet_ms);
        totals.push(total_ms);
        samples.push(serde_json::json!({
            "case_index":case_index,"request":request,
            "provider_open_ms":open_ms,"search_ms":search_ms,"facet_ms":facet_ms,
            "total_ms":total_ms,"search_execution_ms":page.execution_time_ms,
            "search_ranking_cache_hit":page.cache_hit,"total_count":page.total_count,
            "returned_count":page.returned_count,"result_ids":result_ids,
            "matching_folders":folders.len(),"folder_counts":folder_counts,
            "actual_database":actual,"source_same":true,"owned_copy":false
        }));
    }
    // Only after every timed search and facets call: count all stored rows,
    // including legacy NULL/nonbinary visibility, from the narrow covering index.
    // Do not use stats(), which also sums wide-row duration/file-size fields.
    let probe = Connection::open_with_flags(
        &source,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    probe.pragma_update(None, "mmap_size", sqlite_reader_mmap_bytes(&source))?;
    let (records, missing): (i64, i64) = probe.query_row(
        "SELECT COUNT(*),COALESCE(SUM(CASE WHEN missing=1 THEN 1 ELSE 0 END),0)
         FROM audio_files INDEXED BY idx_audio_search_facets",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let sqlite_build: (bool, bool) = probe.query_row(
        "SELECT sqlite_compileoption_used('DEFAULT_MEMSTATUS=0'),
                sqlite_compileoption_used('THREADSAFE=1')",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let mmap_limit: i64 = probe.pragma_query_value(None, "mmap_size", |row| row.get(0))?;
    let percentile = |values: &mut [f64], percent: f64| {
        values.sort_by(f64::total_cmp);
        values[((values.len() - 1) as f64 * percent).ceil() as usize]
    };
    let open_p95 = percentile(&mut opened, 0.95);
    let search_p95 = percentile(&mut searches, 0.95);
    let facet_p95 = percentile(&mut facets, 0.95);
    let total_p95 = percentile(&mut totals, 0.95);
    let mut report = serde_json::json!({
        "records":usize::try_from(records)?,"missing_records":usize::try_from(missing)?,
        "queries":cases.len(),"selected_case_count":cases.len(),
        "database":source,"database_requested":path,"actual_database":source,
        "source_same":true,"owned_copy":false,"database_bytes":std::fs::metadata(&source)?.len(),
        "existing_database":true,"generation_ms":0.0,"request_file":request_file,
        "first_only":true,"pre_query_stats":false,"statistics_collected_after_timing":true,
        "shared_provider_measured":false,"cases":[],"ranking_cache_hits":0,
        "first_provider":true,"folder_facets":true,"first_provider_samples_per_case":1,
        "first_provider_order":"search_then_facets","first_provider_cases":samples,
        "first_provider_condition":"One fresh read-only provider per selected request; no main provider, pre-query stats/COUNT, shared initial pass or warm searches. One request per process when one case is selected. OS page cache is not cleared; total_ms excludes provider opening; facets may reuse the first search's state. Counts and configuration probes run only after all timed calls.",
        "first_provider_open_p95_ms":open_p95,"first_provider_search_p95_ms":search_p95,
        "first_provider_facet_p95_ms":facet_p95,"first_provider_total_p95_ms":total_p95,
        "first_provider_search_meets_target":search_p95<500.0,
        "first_provider_facet_meets_target":facet_p95<500.0,
        "first_provider_sequential_meets_target":total_p95<500.0,
        "target_p95_ms":500,"cache_enabled":true,
        "cache_kind":"bounded ranked IDs and exact-snapshot first pages; cache-hit fields cover either layer",
        "reader_pool_size":4,"sqlite_cache_kb_per_reader":65536
    });
    report["parallel_ranking_auxiliary_readers_max"] = SQLITE_AUXILIARY_SEARCH_READERS.into();
    report["parallel_ranking_auxiliary_readers_active"] = auxiliary_search_reader_count().into();
    report["sqlite_cache_kb_per_auxiliary_reader"] = SQLITE_AUXILIARY_READER_CACHE_KB.into();
    report["sqlite_default_memstatus_disabled"] = sqlite_build.0.into();
    report["sqlite_threadsafe"] = sqlite_build.1.into();
    report["mmap_bytes"] = mmap_limit.into();
    report["mmap_requested_bytes"] = sqlite_reader_mmap_bytes(&source).into();
    report["mmap_fixed_drive_max_bytes"] = SQLITE_READER_MMAP_BYTES.into();
    report["mmap_reporting"] = "Configured upper limit read back from a same-file, same-build read-only probe after timing; not actual mapped length, resident memory, or proof that every timed connection mapped each page.".into();
    Ok(report)
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut records = 1_000_000usize;
    let mut queries = 100usize;
    let mut database = None;
    let mut existing_database = None;
    let mut folder_facets = false;
    let mut first_provider = false;
    let mut edge_cases = false;
    let mut fragment_cases = false;
    let mut tag_exclusion_cases = false;
    let mut missing_records = 0usize;
    let mut case_index_filter = None;
    let mut query_filter = None;
    let mut request_file = None;
    let mut first_only = false;
    let mut concurrent_writer = false;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--folder-facets" => folder_facets = true,
            "--first-provider" => first_provider = true,
            "--first-only" => first_only = true,
            "--concurrent-writer" => concurrent_writer = true,
            "--request-file" => {
                request_file = Some(PathBuf::from(
                    arguments.next().ok_or("--request-file requires a path")?,
                ));
            }
            "--edge-cases" => edge_cases = true,
            "--fragment-cases" => fragment_cases = true,
            "--tag-exclusion-cases" => tag_exclusion_cases = true,
            "--case-index" => {
                case_index_filter = Some(
                    arguments
                        .next()
                        .ok_or("--case-index requires a zero-based index")?
                        .parse::<usize>()?,
                );
            }
            "--query" => {
                query_filter = Some(arguments.next().ok_or("--query requires query text")?);
            }
            "--missing-records" => {
                missing_records = arguments
                    .next()
                    .ok_or("--missing-records requires a number")?
                    .parse()?
            }
            "--records" => {
                records = arguments
                    .next()
                    .ok_or("--records requires a number")?
                    .parse()?
            }
            "--queries" => {
                queries = arguments
                    .next()
                    .ok_or("--queries requires a number")?
                    .parse()?
            }
            "--db" => {
                database = Some(PathBuf::from(
                    arguments.next().ok_or("--db requires a path")?,
                ))
            }
            "--existing-db" => {
                existing_database = Some(PathBuf::from(
                    arguments.next().ok_or("--existing-db requires a path")?,
                ));
            }
            "--help" | "-h" => {
                println!(
                    "search-benchmark [--records 1000000] [--queries 100] [--folder-facets] [--first-provider | --first-only] [--concurrent-writer] [--request-file REQUESTS_JSON] [--edge-cases] [--fragment-cases] [--tag-exclusion-cases] [--missing-records N] [--case-index N | --query TEXT] [--db NEW_DATABASE | --existing-db MIGRATED_DATABASE]"
                );
                println!(
                    "--first-provider measures one fresh read-only provider per case: first search, then first folder facets; provider opening is reported separately and the OS page cache is not cleared."
                );
                println!(
                    "--case-index selects one zero-based case from the enabled default/fragment/edge cases; --query selects every enabled case with exactly that query text. Neither option changes the default workload when omitted."
                );
                println!(
                    "--tag-exclusion-cases adds exclude-only tag filters sorted by duration in both directions, by the default order, and with an include term, plus a sparse include term (rareword) sorted by descending duration; every synthetic row has library tags, the worst case for exclusion. Avoid door: it is a substring of every row's outdoor tag."
                );
                println!(
                    "--request-file replaces built-in cases with a JSON array of validated SearchRequest values; --case-index may select its zero-based entry. It cannot combine with --query, --edge-cases, --fragment-cases or --tag-exclusion-cases."
                );
                println!(
                    "--first-only requires a current --existing-db: one first search then facets per selected case, without pre-query stats, initial/warm passes or --queries repetitions. Select one request and launch a new process for each independent first-query repetition."
                );
                println!(
                    "--concurrent-writer adds fresh-provider baseline and concurrent Metadata commit samples with parallel fallback counters. It requires a newly generated synthetic fixture and cannot modify --existing-db or combine with --first-only."
                );
                return Ok(());
            }
            _ => return Err(format!("Unknown argument {argument}").into()),
        }
    }
    if concurrent_writer && (existing_database.is_some() || first_only) {
        return Err("--concurrent-writer requires a new synthetic fixture and cannot combine with --existing-db or --first-only".into());
    }
    let mut cases = vec![
        SearchRequest {
            query: "explosion".into(),
            ..Default::default()
        },
        SearchRequest {
            query: "metal".into(),
            ..Default::default()
        },
        SearchRequest {
            query: "爆炸".into(),
            ..Default::default()
        },
        SearchRequest {
            query: "发动机".into(),
            ..Default::default()
        },
        SearchRequest {
            query: "door outdoor".into(),
            folder_id: Some(1),
            ..Default::default()
        },
        SearchRequest {
            query: "ambient".into(),
            filters: SearchFilters {
                sample_rate: Some(48000),
                duration_min: Some(0.0),
                duration_max: Some(5.0),
                ..Default::default()
            },
            ..Default::default()
        },
        SearchRequest {
            filters: SearchFilters {
                favorites_only: true,
                ..Default::default()
            },
            ..Default::default()
        },
        SearchRequest {
            query: "notpresent".into(),
            ..Default::default()
        },
        SearchRequest {
            query: "vehicle".into(),
            offset: 1000,
            ..Default::default()
        },
        SearchRequest {
            folder_id: Some(1),
            ..Default::default()
        },
        // Extracted-tag filters (user tags OR source tags).
        SearchRequest {
            filters: SearchFilters {
                tagged_only: true,
                ..Default::default()
            },
            ..Default::default()
        },
        SearchRequest {
            filters: SearchFilters {
                tag_contains: Some("rareword".into()),
                ..Default::default()
            },
            ..Default::default()
        },
        SearchRequest {
            filters: SearchFilters {
                tag_contains: Some("outdoor".into()),
                ..Default::default()
            },
            ..Default::default()
        },
        SearchRequest {
            query: "rareword".into(),
            ..Default::default()
        },
    ];
    if fragment_cases {
        cases.extend(
            ["ex", "pl", "plos", "sion", "et", "plos met", "爆炸 sion"].map(|query| {
                SearchRequest {
                    query: query.into(),
                    ..Default::default()
                }
            }),
        );
    }
    if tag_exclusion_cases {
        let excluding = |exclude: &[&str], order_by: SortOrder, descending: bool| SearchRequest {
            filters: SearchFilters {
                exclude_tags: exclude.iter().map(|tag| (*tag).into()).collect(),
                ..Default::default()
            },
            order_by,
            descending,
            ..Default::default()
        };
        cases.extend([
            excluding(&["explosion"], SortOrder::Duration, false),
            excluding(&["explosion"], SortOrder::Duration, true),
            excluding(&["explosion", "footstep"], SortOrder::Duration, false),
            excluding(&["explosion", "footstep"], SortOrder::Duration, true),
            excluding(&["explosion"], SortOrder::Relevance, false),
            SearchRequest {
                filters: SearchFilters {
                    tag_contains: Some("outdoor".into()),
                    exclude_tags: vec!["explosion".into(), "footstep".into()],
                    ..Default::default()
                },
                order_by: SortOrder::Duration,
                ..Default::default()
            },
            // A sparse include term: 1 in 1000 files, all of the shortest duration, so a
            // descending duration walk passes every other file first.
            SearchRequest {
                filters: SearchFilters {
                    tag_contains: Some("rareword".into()),
                    ..Default::default()
                },
                order_by: SortOrder::Duration,
                descending: true,
                ..Default::default()
            },
            SearchRequest {
                filters: SearchFilters {
                    include_tags: vec!["rareword".into()],
                    exclude_tags: vec!["footstep".into()],
                    ..Default::default()
                },
                order_by: SortOrder::Duration,
                descending: true,
                ..Default::default()
            },
        ]);
    }
    if edge_cases {
        cases.extend([
            SearchRequest {
                query: "metal".into(),
                folder_id: Some(1),
                ..Default::default()
            },
            SearchRequest {
                query: "metal".into(),
                filters: SearchFilters {
                    sample_rate: Some(48000),
                    ..Default::default()
                },
                ..Default::default()
            },
            SearchRequest {
                query: "metal".into(),
                offset: 200_000,
                ..Default::default()
            },
            SearchRequest {
                query: "metal".into(),
                filters: SearchFilters {
                    favorites_only: true,
                    ..Default::default()
                },
                ..Default::default()
            },
            SearchRequest {
                query: "metal".into(),
                order_by: SortOrder::Filename,
                ..Default::default()
            },
        ]);
    }
    if let Some(path) = &request_file {
        if query_filter.is_some() || edge_cases || fragment_cases || tag_exclusion_cases {
            return Err(
                "--request-file cannot combine with --query, --edge-cases, --fragment-cases or --tag-exclusion-cases"
                    .into(),
            );
        }
        cases = request_file_cases(path)?;
    }
    if case_index_filter.is_some() && query_filter.is_some() {
        return Err("--case-index and --query are mutually exclusive".into());
    }
    // Preserve the enabled workload's order and original case identities. Filtering
    // is opt-in and happens before any provider opening or timed service calls.
    let (case_indices, cases): (Vec<_>, Vec<_>) = cases
        .into_iter()
        .enumerate()
        .filter(|(index, request)| {
            case_index_filter.is_none_or(|selected| selected == *index)
                && query_filter
                    .as_ref()
                    .is_none_or(|query| query == &request.query)
        })
        .unzip();
    if cases.is_empty() {
        return Err(
            "No enabled benchmark case matches --case-index or --query; check the case flags"
                .into(),
        );
    }
    if first_only {
        if database.is_some() || missing_records != 0 {
            return Err("--first-only cannot generate or modify synthetic fixtures".into());
        }
        let path = existing_database
            .as_deref()
            .ok_or("--first-only requires --existing-db")?;
        for request in &cases {
            request.validate()?;
        }
        let report = first_only_report(path, &case_indices, &cases, request_file.as_deref())?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    let minimum_queries = cases.len();
    if records == 0 || queries < minimum_queries {
        return Err(format!(
            "records must be positive and queries must be at least {minimum_queries}"
        )
        .into());
    }
    let temporary = tempfile::tempdir()?;
    if existing_database.is_some() && database.is_some() {
        return Err("--db and --existing-db are mutually exclusive".into());
    }
    let existing = existing_database.is_some();
    if existing && missing_records != 0 {
        return Err("--missing-records only applies to a newly generated fixture".into());
    }
    if missing_records > records {
        return Err("missing-records must not exceed records".into());
    }
    let path = existing_database
        .or(database)
        .unwrap_or_else(|| temporary.path().join("benchmark.db"));
    if !existing && path.exists() {
        return Err("Benchmark refuses to overwrite an existing database".into());
    }
    let provider = if existing {
        SqliteProvider::open_read_only(&path)?
    } else {
        SqliteProvider::open(&path)?
    };
    let start = Instant::now();
    if existing {
        let stats = provider.stats()?;
        records = usize::try_from(
            stats
                .audio_files
                .checked_add(stats.missing_files)
                .ok_or("Total library record count overflow")?,
        )?;
        missing_records = usize::try_from(stats.missing_files)?;
    } else {
        let mut conn = Connection::open(&path)?;
        audiolib_db::register_library_functions(&conn)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
        INSERT INTO folders(id,folder_path,folder_name,depth) VALUES(1,'synthetic','synthetic',0);
        INSERT INTO folder_closure VALUES(1,1,0);",
        )?;
        let types = [
            "explosion",
            "footstep",
            "door",
            "ambient",
            "weapon",
            "vehicle",
            "nature",
            "impact",
            "swoosh",
            "ui",
        ];
        for begin in (0..records).step_by(10_000) {
            let tx = conn.transaction()?;
            {
                let mut insert=tx.prepare("INSERT INTO audio_files(file_path,path_key,filename,user_tags,
                user_notes,bwf_description,folder_id,file_size,sample_rate,bit_depth,channels,duration,user_rating,favorite,missing,metadata_json,source_tags_text)
                VALUES(?1,?1,?2,?3,?4,?5,1,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)")?;
                for index in begin..(begin + 10_000).min(records) {
                    let kind = types[index % types.len()];
                    let filename = format!("{kind}_metal_{index:07}.wav");
                    insert.execute(params![
                        format!("synthetic/{filename}"),
                        filename,
                        format!("{kind} metal outdoor"),
                        if index % 5 == 0 {
                            "车辆发动机环境声"
                        } else {
                            ""
                        },
                        if index % 10 == 0 {
                            "大型金属爆炸"
                        } else {
                            ""
                        },
                        100_000 + (index % 5_000_000) as i64,
                        if index % 3 == 0 { 96000 } else { 48000 },
                        if index % 2 == 0 { 24 } else { 16 },
                        if index % 2 == 0 { 2 } else { 1 },
                        0.5 + (index % 200) as f64 / 10.0,
                        (index % 6) as i64,
                        index % 17 == 0,
                        index < missing_records,
                        match (index % 4 == 0, index % 1000 == 0) {
                            (true, true) => format!(
                                "{{\"wav_metadata\":{{\"fields\":{{\"keywords\":\"{kind}, outdoor, rareword\",\"category\":\"SFX\"}}}}}}"
                            ),
                            (true, false) => format!(
                                "{{\"wav_metadata\":{{\"fields\":{{\"keywords\":\"{kind}, outdoor\",\"category\":\"SFX\"}}}}}}"
                            ),
                            _ => "{}".to_string(),
                        },
                        match (index % 4 == 0, index % 1000 == 0) {
                            (true, true) => format!("{kind}, outdoor, rareword, sfx"),
                            (true, false) => format!("{kind}, outdoor, sfx"),
                            _ => String::new(),
                        }
                    ])?;
                }
            }
            tx.commit()?;
        }
        conn.execute_batch("ANALYZE; PRAGMA wal_checkpoint(TRUNCATE);")?;
        drop(conn);
        // Bulk SQL fixtures must use the same derived text as real scans and
        // Metadata edits, including positional letter-fragment postings.
        provider.maintain_library(MaintenanceAction::RebuildSearch)?;
    }
    let generation_ms = if existing {
        0.0
    } else {
        start.elapsed().as_secs_f64() * 1000.0
    };
    let sqlite_build = {
        let conn = Connection::open_in_memory()?;
        conn.query_row(
            "SELECT sqlite_compileoption_used('DEFAULT_MEMSTATUS=0'),
                    sqlite_compileoption_used('THREADSAFE=1')",
            [],
            |row| Ok((row.get::<_, bool>(0)?, row.get::<_, bool>(1)?)),
        )?
    };
    // Every timed query is a full service call: validation, tokenization, SQL COUNT, page DTOs.
    // Report the bounded ranking cache explicitly; Metadata remains a snapshot read.
    // No search has run on any of these providers. Run this phase before the
    // shared-provider initial/warm samples; OS pages remain shared and are not flushed.
    let mut first_provider_cases = Vec::new();
    let mut first_open_samples = Vec::new();
    let mut first_search_samples = Vec::new();
    let mut first_facet_samples = Vec::new();
    let mut first_total_samples = Vec::new();
    if first_provider {
        for (case_index, request) in case_indices.iter().zip(&cases) {
            let opened = Instant::now();
            let fresh = SqliteProvider::open_read_only(&path)?;
            let provider_open_ms = opened.elapsed().as_secs_f64() * 1000.0;
            // Measure search then facets strictly sequentially. The first facets
            // call may use state naturally produced by this provider's first
            // search, with no preceding query or explicit cache prewarm.
            let started = Instant::now();
            let page = fresh.search(request)?;
            let search_ms = started.elapsed().as_secs_f64() * 1000.0;
            let facets_started = Instant::now();
            let folders = fresh.search_folders(request)?;
            let facet_ms = facets_started.elapsed().as_secs_f64() * 1000.0;
            let total_ms = started.elapsed().as_secs_f64() * 1000.0;
            first_open_samples.push(provider_open_ms);
            first_search_samples.push(search_ms);
            first_facet_samples.push(facet_ms);
            first_total_samples.push(total_ms);
            first_provider_cases.push(serde_json::json!({
                "case_index": case_index,
                "request": request,
                "provider_open_ms": provider_open_ms,
                "search_ms": search_ms,
                "facet_ms": facet_ms,
                "total_ms": total_ms,
                "search_execution_ms": page.execution_time_ms,
                "search_ranking_cache_hit": page.cache_hit,
                "total_count": page.total_count,
                "matching_folders": folders.len()
            }));
        }
    }
    let measure = |request: &SearchRequest| -> audiolib_core::Result<(f64, f64, bool)> {
        let started = Instant::now();
        let page = provider.search(request)?;
        if folder_facets {
            provider.search_folders(request)?;
            Ok((
                started.elapsed().as_secs_f64() * 1000.0,
                page.execution_time_ms,
                page.cache_hit,
            ))
        } else {
            Ok((
                page.execution_time_ms,
                page.execution_time_ms,
                page.cache_hit,
            ))
        }
    };
    let mut cold = Vec::new();
    let mut initial_page_samples = Vec::new();
    let mut initial_cache_hits = Vec::new();
    for request in &cases {
        let (latency, page_latency, cache_hit) = measure(request)?;
        cold.push(latency);
        initial_page_samples.push(page_latency);
        initial_cache_hits.push(cache_hit);
    }
    let initial_samples = cold.clone();
    let mut samples = Vec::with_capacity(queries);
    let mut page_samples = Vec::with_capacity(queries);
    let mut per_case = vec![Vec::<f64>::new(); cases.len()];
    let mut per_case_page = vec![Vec::<f64>::new(); cases.len()];
    let mut ranking_cache_hits = 0usize;
    for index in 0..queries {
        let (latency, page_latency, cache_hit) = measure(&cases[index % cases.len()])?;
        ranking_cache_hits += usize::from(cache_hit);
        samples.push(latency);
        page_samples.push(page_latency);
        per_case[index % cases.len()].push(latency);
        per_case_page[index % cases.len()].push(page_latency);
    }
    samples.sort_by(f64::total_cmp);
    page_samples.sort_by(f64::total_cmp);
    cold.sort_by(f64::total_cmp);
    let mut initial_pages = initial_page_samples.clone();
    initial_pages.sort_by(f64::total_cmp);
    first_open_samples.sort_by(f64::total_cmp);
    first_search_samples.sort_by(f64::total_cmp);
    first_facet_samples.sort_by(f64::total_cmp);
    first_total_samples.sort_by(f64::total_cmp);
    let percentile = |values: &[f64], percent: f64| {
        values[((values.len() - 1) as f64 * percent).ceil() as usize]
    };
    let cases_report: Vec<_> = cases
        .iter()
        .zip(per_case)
        .zip(per_case_page)
        .enumerate()
        .map(|(index, ((request, mut values), mut page_values))| {
            values.sort_by(f64::total_cmp);
            page_values.sort_by(f64::total_cmp);
            serde_json::json!({
                "case_index":case_indices[index],
                "request":request,
                "p95_ms":percentile(&values,0.95),
                "samples_ms":values,
                "initial_pass_ms":initial_samples[index],
                "initial_ranking_cache_hit":initial_cache_hits[index],
                "sample_count":page_values.len(),
                "result_page_p50_ms":percentile(&page_values,0.5),
                "result_page_p95_ms":percentile(&page_values,0.95),
                "result_page_p99_ms":percentile(&page_values,0.99),
                "result_page_samples_ms":page_values,
                "initial_result_page_ms":initial_page_samples[index]
            })
        })
        .collect();
    // Inspect the build/VFS limit only after all measured calls, so this probe
    // cannot warm database pages before any first-provider sample. This is a
    // configured upper limit, not mapped length or resident physical memory.
    let mmap_probe = Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let writer_report = concurrent_writer
        .then(|| concurrent_writer_report(&path, &cases, queries))
        .transpose()?;
    mmap_probe.pragma_update(None, "mmap_size", sqlite_reader_mmap_bytes(&path))?;
    let mmap_limit: i64 = mmap_probe.pragma_query_value(None, "mmap_size", |row| row.get(0))?;
    let mut report = serde_json::json!({"records":records,"queries":queries,"cache_enabled":true,"cache_kind":"bounded ranked IDs and exact-snapshot first pages; cache-hit fields cover either layer","ranking_cache_hits":ranking_cache_hits,"edge_cases":edge_cases,"missing_records":missing_records,"folder_facets":folder_facets,
        "database":path,"database_bytes":std::fs::metadata(&path)?.len(),"existing_database":existing,
        "reader_pool_size":4,"sqlite_cache_kb_per_reader":65536,"mmap_bytes":mmap_limit,
        "generation_ms":generation_ms,"p50_ms":percentile(&samples,0.5),
        "p95_ms":percentile(&samples,0.95),"p99_ms":percentile(&samples,0.99),
        "result_page_p50_ms":percentile(&page_samples,0.5),"result_page_p95_ms":percentile(&page_samples,0.95),
        "result_page_timing":"DataProvider-reported execution_time_ms; per-case samples use the same timer as the existing aggregate result-page percentiles.",
        "initial_result_page_p95_ms":percentile(&initial_pages,0.95),
        "case_index_filter":case_index_filter,"query_filter":query_filter,
        "initial_pass_p95_ms":percentile(&cold,0.95),"target_p95_ms":500,
        "meets_target":percentile(&samples,0.95)<500.0,"latencies_ms":samples,"cases":cases_report,
        "first_provider":first_provider,"first_provider_order":"search_then_facets",
        "first_provider_condition":"One new read-only DataProvider per case, before shared-provider initial/warm samples; no earlier search on that provider; OS page cache is not cleared; total_ms excludes provider opening; facets may use state naturally produced by the first search.",
        "first_provider_samples_per_case":usize::from(first_provider),"first_provider_cases":first_provider_cases,
        "first_provider_open_p95_ms":first_provider.then(||percentile(&first_open_samples,0.95)),
        "first_provider_search_p95_ms":first_provider.then(||percentile(&first_search_samples,0.95)),
        "first_provider_facet_p95_ms":first_provider.then(||percentile(&first_facet_samples,0.95)),
        "first_provider_total_p95_ms":first_provider.then(||percentile(&first_total_samples,0.95)),
        "first_provider_search_meets_target":first_provider.then(||percentile(&first_search_samples,0.95)<500.0),
        "first_provider_facet_meets_target":first_provider.then(||percentile(&first_facet_samples,0.95)<500.0),
        "first_provider_sequential_meets_target":first_provider.then(||percentile(&first_total_samples,0.95)<500.0)});
    report["parallel_ranking_auxiliary_readers_max"] = SQLITE_AUXILIARY_SEARCH_READERS.into();
    report["parallel_ranking_auxiliary_readers_active"] = auxiliary_search_reader_count().into();
    report["sqlite_cache_kb_per_auxiliary_reader"] = SQLITE_AUXILIARY_READER_CACHE_KB.into();
    report["concurrent_writer"] = concurrent_writer.into();
    report["tag_exclusion_cases"] = tag_exclusion_cases.into();
    report["concurrent_writer_report"] = serde_json::to_value(writer_report)?;
    report["sqlite_default_memstatus_disabled"] = sqlite_build.0.into();
    report["sqlite_threadsafe"] = sqlite_build.1.into();
    report["mmap_requested_bytes"] = sqlite_reader_mmap_bytes(&path).into();
    report["mmap_fixed_drive_max_bytes"] = SQLITE_READER_MMAP_BYTES.into();
    report["mmap_reporting"] = "Configured upper limit read back from a same-file, same-build read-only probe after timing; not actual mapped length, resident memory, or proof that every timed connection mapped each page.".into();
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
