// crates/audiolib-db/src/bin/search-b-bench.rs
//! Search benchmark: s8 (main FTS only) vs s9 / option B (main FTS + directory-segment path FTS).
//!
//! Usage: `search-b-bench <mode> <db>`; `cold1` = one OS-cold sample (caller runs `sudo purge` first), env Q=query ONLY=s8|s9.
//! Env: T3=1 (t3-normalized path terms), PSCALE, ROUNDS, WARM, COLD, QOS, MEMSTATUS0.
//!
//! `BENCH_PREPARE_OUTSIDE_TIMER=1` (cold1, applies to s8 and s9 alike): on every freshly opened connection, prepare the
//! main (`audio_search`) and directory (`audio_dir_search`) FTS statements before the timer starts. This models the
//! server's pooled readers, which connect each FTS virtual table once per connection, not per query. Page cache and
//! OS cache stay cold (purge still runs before each sample); only statement preparation moves out of the timed section.
//!
//! `provider <db>` measures production server-provider search on an owned temporary
//! copy, including result hydration. Supply an immutable, complete SQLite snapshot;
//! a nonempty source WAL is refused. Source copying and provider startup are outside
//! the timer; SQLite only opens the owned copy. Defaults: the three timeout queries, all scopes,
//! five rounds, three warmups per round, `WARM=60` measured samples per round.
//! `Q` selects one query and `ROUNDS` changes the round count. JSONL output contains
//! timings/counts, never record paths. Main on this same fixture is a surrogate s8-main
//! baseline, not a measurement of an actual schema-8 fixture or a performance gate.
use audiolib_core::{DataProvider, SearchRequest, SearchScope};
use audiolib_db::SqliteProvider;
use rusqlite::{Connection, InterruptHandle, OpenFlags};
use std::sync::Arc;
use std::time::{Duration, Instant};

const RAW: &str = "filename user_tags user_notes bwf_description ixml_scene ixml_take";
const DER: &str = "filename_tokenized user_tags_tokenized user_notes_tokenized bwf_description_tokenized ixml_scene_tokenized ixml_take_tokenized";
const WMAIN: &str = "10,10,6,6,3,3,3,3,3,3,6,6";
const WPATH: &str = "1.5,1.5";
const QS: &[&str] = &[
    "slam Foley",
    "Doors slam",
    "door Construction",
    "explosion 1_Boom",
    "whoosh Designed",
    "hdd_A_Lib explosion",
    "爆炸",
    "脚步",
    "explosion",
    "whoosh",
    "expl",
    "door slam",
    "Sonomar",
    "hdd_A_Lib",
    "1_Boom",
    "Construction Kit",
    "Crowds Concert",
    "CREAHmn",
    "ROCKMvmt",
    "EXPLMisc",
];
const PAGE: usize = 200;

fn is_han(c: char) -> bool {
    ('\u{4e00}'..='\u{9fff}').contains(&c) || ('\u{3400}'..='\u{4dbf}').contains(&c)
}
fn term(word: &str, raw: &str, der: &str) -> String {
    if word.chars().any(is_han) || !word.chars().any(|c| c.is_alphabetic()) {
        return format!("\"{word}\"");
    }
    let n: Vec<char> = word
        .to_lowercase()
        .chars()
        .map(|c| if c == '_' { '.' } else { c })
        .collect();
    let grams: Vec<String> = n
        .windows(2)
        .map(|p| format!("g{:06x}{:06x}", p[0] as u32, p[1] as u32))
        .collect();
    format!(
        "({{{raw}}} : \"{word}\" OR {{{der}}} : \"{}\")",
        grams.join(" ")
    )
}
fn compile(q: &str, raw: &str, der: &str) -> String {
    q.split(|c: char| c.is_whitespace() || (!c.is_alphanumeric() && c != '_' && c != '.'))
        .filter(|w| !w.is_empty())
        .map(|w| term(w, raw, der))
        .collect::<Vec<_>>()
        .join(" AND ")
}
/// t3: NFC + fullwidth->halfwidth, path table + its query terms ONLY
fn t3(s: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    s.nfc()
        .map(|c| match c as u32 {
            0x3000 => ' ',
            u @ 0xFF01..=0xFF5E => char::from_u32(u - 0xFEE0).unwrap(),
            _ => c,
        })
        .collect()
}
fn reg_t3(c: &Connection) {
    c.create_scalar_function(
        "t3_norm",
        1,
        rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC
            | rusqlite::functions::FunctionFlags::SQLITE_UTF8,
        |ctx| Ok(ctx.get::<Option<String>>(0)?.map(|s| t3(&s))),
    )
    .unwrap();
}
fn use_t3() -> bool {
    std::env::var("T3").is_ok_and(|v| v == "1")
}
fn open_rw(db: &str) -> Connection {
    let c = Connection::open(db).unwrap();
    audiolib_db::register_library_functions(&c).unwrap();
    reg_t3(&c);
    c.execute_batch("PRAGMA cache_size=-262144;").unwrap();
    c
}
fn open(db: &str) -> Connection {
    let c = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .unwrap();
    audiolib_db::register_library_functions(&c).unwrap();
    c.execute_batch("PRAGMA cache_size=-262144;").unwrap();
    c
}

/// keyset cursor = (score, rowid); bm25 lower = better; ORDER BY score,rowid
type Key = (f64, i64);
struct Side {
    table: &'static str,
    w: &'static str,
    scale: f64,
    m: String,
}
impl Side {
    fn page(&self, c: &Connection, after: Option<Key>, lim: usize) -> rusqlite::Result<Vec<Key>> {
        let sql = self.page_sql();
        let (s0, i0) = after.unwrap_or((f64::NEG_INFINITY, i64::MIN));
        let mut st = c.prepare_cached(&sql)?;
        st.query_map(rusqlite::params![self.m, s0, i0, lim as i64], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .collect()
    }
    /// SQL used by `page`; `prepare_fts` prepares exactly this string so the statement cache hits.
    fn page_sql(&self) -> String {
        let (t, w, k) = (self.table, self.w, self.scale);
        format!(
            "SELECT s,id FROM (SELECT rowid id, bm25({t},{w})*{k} s FROM {t} WHERE {t} MATCH ?1) WHERE s>?2 OR (s=?2 AND id>?3) ORDER BY s,id LIMIT ?4"
        )
    }
}
fn sides(q: &str, pscale: f64) -> (Side, Side) {
    (
        Side {
            table: "audio_search",
            w: WMAIN,
            scale: 1.0,
            m: compile(q, RAW, DER),
        },
        if use_t3() {
            Side {
                table: "audio_path_search_t3",
                w: WPATH,
                scale: pscale,
                m: compile(&t3(q), "path_tokens", "path_tokens_tokenized"),
            }
        } else {
            Side {
                table: "audio_path_search",
                w: WPATH,
                scale: pscale,
                m: compile(q, "path_tokens", "path_tokens_tokenized"),
            }
        },
    )
}

type M = std::sync::Mutex<Connection>;
fn words(q: &str) -> Vec<String> {
    q.split(|c: char| c.is_whitespace() || (!c.is_alphanumeric() && c != '_' && c != '.'))
        .filter(|w| !w.is_empty())
        .map(String::from)
        .collect()
}
fn ids(c: &Connection, table: &str, m: &str, cap: usize) -> rusqlite::Result<Vec<i64>> {
    let mut st = c.prepare_cached(&format!(
        "SELECT rowid FROM {table} WHERE {table} MATCH ?1 LIMIT ?2"
    ))?;
    st.query_map(rusqlite::params![m, cap as i64], |r| r.get(0))?
        .collect()
}
type Set = std::collections::HashSet<i64>;
// ---- directory-table design: path terms are matched on audio_dir_search (rowid = folders.id = audio_files.folder_id, "dir_id"),
// files of a dir set are resolved through audio_dir_runs(dir_id, lo, hi, n): maximal runs of consecutive file ids with the same folder.
const DIRT: &str = "audio_dir_search";
fn dir_m(w: &str) -> String {
    compile(&t3(w), "path_tokens", "path_tokens_tokenized")
}
#[derive(Clone, Copy)]
struct Run {
    lo: i64,
    hi: i64,
    dense: bool,
    dir: i64,
}
/// process-level dir index (server: loaded at startup, reloaded when search_path_rev changes): file-id runs + dir tree.
struct RunIdx {
    all: Vec<Run>,
    by_dir: HashMap<i64, Vec<usize>>,
    parent: HashMap<i64, i64>,
    children: HashMap<i64, Vec<i64>>,
    tfc: HashMap<i64, i64>,
}
use std::collections::HashMap;
static RIDX: std::sync::OnceLock<RunIdx> = std::sync::OnceLock::new();
fn ridx(c: &Connection) -> &'static RunIdx {
    RIDX.get_or_init(|| {
        let mut st = c
            .prepare("SELECT lo, hi, n, dir_id FROM audio_dir_runs ORDER BY lo")
            .unwrap();
        let all: Vec<Run> = st
            .query_map([], |r| {
                let (lo, hi, n): (i64, i64, i64) = (r.get(0)?, r.get(1)?, r.get(2)?);
                Ok(Run {
                    lo,
                    hi,
                    dense: n == hi - lo + 1,
                    dir: r.get(3)?,
                })
            })
            .unwrap()
            .map(|x| x.unwrap())
            .collect();
        let mut by_dir: HashMap<i64, Vec<usize>> = Default::default();
        for (i, r) in all.iter().enumerate() {
            by_dir.entry(r.dir).or_default().push(i);
        }
        let (mut parent, mut children, mut tfc) = (
            HashMap::new(),
            HashMap::<i64, Vec<i64>>::new(),
            HashMap::new(),
        );
        let mut st = c
            .prepare("SELECT id, parent_id, total_file_count FROM folders")
            .unwrap();
        for x in st
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })
            .unwrap()
        {
            let (id, p, t) = x.unwrap();
            tfc.insert(id, t);
            if let Some(p) = p {
                parent.insert(id, p);
                children.entry(p).or_default().push(id);
            }
        }
        RunIdx {
            all,
            by_dir,
            parent,
            children,
            tfc,
        }
    })
}
static PROF: std::sync::Mutex<Vec<(&'static str, f64)>> = std::sync::Mutex::new(Vec::new());
fn pt(k: &'static str, t: Instant) {
    PROF.lock()
        .unwrap()
        .push((k, t.elapsed().as_secs_f64() * 1e3));
}
type DirSet = HashMap<i64, f64>; // matched SEGMENT dirs -> bm25
/// audio_dir_search indexes each dir's own segment (root: its relative path; others: folder_name). A dir path-matches a term
/// iff some ancestor-or-self segment matches -> rename touches one row, moves touch none.
fn seg_match(c: &Connection, m: &str, scored: bool) -> rusqlite::Result<DirSet> {
    let mut st = c.prepare_cached(&seg_sql(scored))?;
    st.query_map([m], |r| Ok((r.get(0)?, r.get(1)?)))?.collect()
}
fn seg_sql(scored: bool) -> String {
    if scored {
        format!(
            "SELECT rowid, bm25({DIRT},{}) FROM {DIRT} WHERE {DIRT} MATCH ?1",
            WPATH
        )
    } else {
        format!("SELECT rowid, 0.0 FROM {DIRT} WHERE {DIRT} MATCH ?1")
    }
}
/// BENCH_PREPARE_OUTSIDE_TIMER=1: connect both FTS virtual tables on `c` by preparing the statements cold1 uses.
fn prepare_outside_timer() -> bool {
    std::env::var("BENCH_PREPARE_OUTSIDE_TIMER").is_ok_and(|v| v == "1")
}
fn prepare_fts(c: &Connection, q: &str, pscale: f64) {
    c.prepare_cached(&sides(q, pscale).0.page_sql()).unwrap();
    c.prepare_cached(&seg_sql(false)).unwrap();
}
fn ri() -> &'static RunIdx {
    RIDX.get().unwrap()
}
/// best (min) score among matched ancestors-or-self of dir d
fn anc_score(set: &DirSet, mut d: i64) -> Option<f64> {
    let mut best: Option<f64> = None;
    loop {
        if let Some(&s) = set.get(&d) {
            best = Some(best.map_or(s, |b: f64| b.min(s)));
        }
        match ri().parent.get(&d) {
            Some(&p) => d = p,
            None => return best,
        }
    }
}
fn dir_of(id: i64) -> Option<i64> {
    let a = &ri().all;
    let i = a.partition_point(|r| r.hi < id);
    (i < a.len() && a[i].lo <= id).then(|| a[i].dir)
}
fn top(set: &DirSet) -> Vec<i64> {
    set.keys()
        .copied()
        .filter(|&d| {
            let mut x = d;
            while let Some(&p) = ri().parent.get(&x) {
                if set.contains_key(&p) {
                    return false;
                }
                x = p;
            }
            true
        })
        .collect()
}
fn est(set: &DirSet) -> i64 {
    top(set)
        .iter()
        .map(|d| ri().tfc.get(d).copied().unwrap_or(0))
        .sum()
}
fn subtree_dirs(set: &DirSet) -> Vec<i64> {
    let mut out = vec![];
    let mut st = top(set);
    while let Some(d) = st.pop() {
        out.push(d);
        if let Some(ch) = ri().children.get(&d) {
            st.extend(ch)
        }
    }
    out
}
fn expand(
    c: &Connection,
    r: &Run,
    from: i64,
    k: usize,
    out: &mut Vec<i64>,
) -> rusqlite::Result<()> {
    let lo = r.lo.max(from);
    if r.dense {
        let mut x = lo;
        while x <= r.hi && out.len() < k {
            out.push(x);
            x += 1;
        }
        return Ok(());
    }
    let mut st = c.prepare_cached(
        "SELECT id FROM audio_files WHERE id>=?1 AND id<=?2 ORDER BY id LIMIT ?3",
    )?;
    for id in st.query_map(rusqlite::params![lo, r.hi, (k - out.len()) as i64], |r| {
        r.get(0)
    })? {
        out.push(id?)
    }
    Ok(())
}
fn files_of(c: &Connection, set: &DirSet) -> rusqlite::Result<Vec<(i64, i64)>> {
    let mut v = vec![];
    for d in subtree_dirs(set) {
        if let Some(ix) = ri().by_dir.get(&d) {
            for &i in ix {
                let mut t = vec![];
                expand(c, &ri().all[i], i64::MIN, usize::MAX >> 2, &mut t)?;
                v.extend(t.into_iter().map(|id| (id, d)));
            }
        }
    }
    Ok(v)
}
const CAP_SMALL: i64 = 20_000; // term path file-set small enough to enumerate as mixed candidates
// Ordering contract (scope=all/path): tier0 = main-field full match, by main bm25 (identical to s8 order);
// tier1 = mixed (every term hits main OR path, some only in path), tier2 = path-only full match; tiers 1/2 by file rowid asc.
// Key = (score, id): tier0 score = bm25 (<0), tier1 = 1.0, tier2 = 2.0 -> one lexicographic keyset cursor (tier, score, id).
const T1: f64 = 1.0;
const T2: f64 = 2.0;
struct PathRes {
    terms: Vec<DirSet>,
    k: usize,
}
struct Parts {
    main: Vec<Key>,
    mainfull: Set,
    path: PathRes,
    nw: usize,
    nopath_main: Option<Vec<i64>>,
}
fn path_full(sets: &[DirSet], d: i64) -> bool {
    sets.iter().all(|t| anc_score(t, d).is_some())
}
fn gather(
    c1: &M,
    c2: &M,
    c3: &M,
    q: &str,
    pscale: f64,
    after: Option<Key>,
    lim: usize,
) -> rusqlite::Result<Parts> {
    let (main, _) = sides(q, pscale);
    let ws = words(q);
    let nw = ws.len().max(1);
    // 1) directory match alone (cheap); decide whether any term is path-empty.
    let t = Instant::now();
    let path = {
        let c = c2.lock().unwrap();
        let total = ri().all.iter().map(|r| r.hi - r.lo + 1).sum::<i64>();
        let mut sets: Vec<DirSet> = if ws.len() <= 1 {
            vec![seg_match(&c, &dir_m(q), false)?]
        } else {
            ws.iter()
                .map(|w| seg_match(&c, &dir_m(w), false))
                .collect::<rusqlite::Result<_>>()?
        };
        let ests: Vec<i64> = sets.iter().map(est).collect();
        for (j, e) in ests.iter().enumerate() {
            if *e * 2 > total {
                sets[j].clear();
            }
        }
        let ests: Vec<i64> = sets.iter().map(est).collect();
        let k = (0..sets.len()).min_by_key(|&j| ests[j]).unwrap();
        PathRes { terms: sets, k }
    };
    pt("dir_match", t);
    let need_nopath = nw > 1
        && path.terms.iter().any(|t| t.is_empty())
        && path.terms.iter().any(|t| !t.is_empty());
    // 2) empty-path term with 0 main hits => every tier is empty (term can never be satisfied). Prove with LIMIT 1
    //    *before* running the full main AND, and without overlapping another heavy FTS scan.
    if need_nopath {
        let t = Instant::now();
        let c = c3.lock().unwrap();
        for (j, w) in ws.iter().enumerate() {
            if !path.terms[j].is_empty() {
                continue;
            }
            let m = compile(w, RAW, DER);
            if ids(&c, "audio_search", &m, 1)?.is_empty() {
                pt("nopath_main", t);
                return Ok(Parts {
                    main: vec![],
                    mainfull: Set::new(),
                    path,
                    nw,
                    nopath_main: Some(vec![]),
                });
            }
        }
        drop(c);
        pt("nopath_main", t);
    }
    // 3) main bm25 (s8) in parallel with full nopath fetch when still needed for tier1.
    let (r1, nopath_main) = std::thread::scope(|s| {
        let h_np = need_nopath.then(|| s.spawn(|| -> rusqlite::Result<Vec<i64>> {
            qos(); let t = Instant::now(); let c = c3.lock().unwrap(); let mut acc: Option<Set> = None;
            let ne: DirSet = path.terms.iter().flat_map(|x| x.iter().map(|(&d, &v)| (d, v))).collect();
            let runs: Option<Vec<Run>> = (est(&ne) <= CAP_SMALL).then(|| { let mut v: Vec<Run> = subtree_dirs(&ne).iter().filter_map(|d| ri().by_dir.get(d)).flatten().map(|&i| ri().all[i]).collect(); v.sort_unstable_by_key(|r| r.lo); v });
            let mut rq = c.prepare_cached("SELECT rowid FROM audio_search WHERE audio_search MATCH ?1 AND rowid>=?2 AND rowid<=?3")?;
            for (j, w) in ws.iter().enumerate() { if !path.terms[j].is_empty() { continue }
                let m = compile(w, RAW, DER);
                let v: Set = match &runs { Some(rs) => { let mut v = Set::new(); if let (Some(a), Some(b)) = (rs.first(), rs.iter().map(|r| r.hi).max()) { for x in rq.query_map(rusqlite::params![m, a.lo, b], |r| r.get(0))? { v.insert(x?); } } v }
                    None => ids(&c, "audio_search", &m, usize::MAX >> 2)?.into_iter().collect() };
                acc = Some(match acc { None => v, Some(a) => a.intersection(&v).copied().collect() }); if acc.as_ref().unwrap().is_empty() { break } }
            let mut v: Vec<i64> = acc.unwrap().into_iter().collect(); v.sort_unstable(); pt("nopath_main", t); Ok(v)
        }));
        let t = Instant::now();
        let c = c1.lock().unwrap();
        let r1 = (|| -> rusqlite::Result<(Vec<Key>, Set)> {
            let in0 = after.is_none_or(|a| a.0 < 0.0);
            let v = if in0 {
                main.page(&c, after, lim)?
            } else {
                vec![]
            };
            let full: Set = if v.len() < lim && after.is_none() {
                v.iter().map(|k| k.1).collect()
            } else if v.len() < lim {
                ids(&c, "audio_search", &main.m, usize::MAX >> 2)?
                    .into_iter()
                    .collect()
            } else {
                Set::new()
            };
            Ok((v, full))
        })();
        pt("main_bm25", t);
        drop(c);
        let np = h_np.map(|h| h.join().unwrap().unwrap());
        (r1, np)
    });
    let (main, mainfull) = r1?;
    Ok(Parts {
        main,
        mainfull,
        path,
        nw,
        nopath_main,
    })
}
/// tier 1 or 2, ids > cur ascending. Sources: tier2 = files under path(term k) (in-memory runs);
/// tier1 = main(term k) rowid stream UNION files under path(term k): a mixed doc either path-hits k or main-hits k.
fn tier_stream(
    c2: &M,
    c3: &M,
    q: &str,
    p: &Parts,
    want: f64,
    cur: i64,
    need: usize,
) -> rusqlite::Result<Vec<i64>> {
    let mainfull = &p.mainfull;
    let ws = words(q);
    let sets = &p.path.terms;
    let k = p.path.k;
    if want == T1 && (p.nw < 2 || sets.iter().all(|s| s.is_empty())) {
        return Ok(vec![]);
    }
    if want == T2 && sets.iter().any(|s| s.is_empty()) {
        return Ok(vec![]);
    }
    if want == T1
        && let Some(np) = &p.nopath_main
    {
        // every tier1 doc is in np (main hits of all path-less terms); check the rest in memory / by probe
        let c3g = c3.lock().unwrap();
        let mut probe = c3g.prepare_cached(
            "SELECT 1 FROM audio_search WHERE audio_search MATCH ?1 AND rowid=?2",
        )?;
        let mut out = vec![];
        for &id in &np[np.partition_point(|&x| x <= cur)..] {
            if out.len() >= need {
                break;
            }
            if mainfull.contains(&id) {
                continue;
            }
            let Some(d) = dir_of(id) else { continue };
            let hit: Vec<bool> = (0..sets.len())
                .map(|j| !sets[j].is_empty() && anc_score(&sets[j], d).is_some())
                .collect();
            if !hit.iter().any(|&h| h) {
                continue;
            }
            if (0..sets.len()).all(|j| {
                hit[j]
                    || sets[j].is_empty()
                    || probe
                        .exists(rusqlite::params![compile(&ws[j], RAW, DER), id])
                        .unwrap_or(false)
            }) {
                out.push(id)
            }
        }
        return Ok(out);
    }
    let c3g = c3.lock().unwrap();
    let c2g = c2.lock().unwrap();
    let mut mst = c3g.prepare_cached("SELECT rowid FROM audio_search WHERE audio_search MATCH ?1 AND rowid>?2 ORDER BY rowid LIMIT 512")?;
    let mut probe =
        c3g.prepare_cached("SELECT 1 FROM audio_search WHERE audio_search MATCH ?1 AND rowid=?2")?;
    let mterms: Vec<String> = ws.iter().map(|w| compile(w, RAW, DER)).collect();
    // source plan. tier2: files under path(k). tier1: (A) files under ANY non-empty term path set if their total is small
    // (a mixed doc path-hits >=1 term) -> no main stream; else (B) main(k) stream + files under path(k).
    let nonempty: Vec<usize> = (0..sets.len()).filter(|&j| !sets[j].is_empty()).collect();
    let sum_est: i64 = nonempty.iter().map(|&j| est(&sets[j])).sum();
    let plan_a = want == T1 && sum_est <= CAP_SMALL;
    let src: Vec<usize> = if want == T2 || !plan_a {
        vec![k]
    } else {
        nonempty.clone()
    };
    let src_set: DirSet = src
        .iter()
        .flat_map(|&j| sets[j].iter().map(|(&d, &v)| (d, v)))
        .collect();
    let all_runs = &ri().all;
    let small = est(&src_set) <= 50_000;
    let cand_runs: Vec<Run> = if small {
        let mut v: Vec<Run> = subtree_dirs(&src_set)
            .iter()
            .filter_map(|d| ri().by_dir.get(d))
            .flatten()
            .map(|&i| all_runs[i])
            .collect();
        v.sort_unstable_by_key(|r| r.lo);
        v
    } else {
        vec![]
    };
    let all: &[Run] = if small { &cand_runs } else { all_runs };
    let (mut mbuf, mut mcur, mut mdone) = (
        std::collections::VecDeque::<i64>::new(),
        cur,
        want == T2 || plan_a,
    );
    let mut rix = all.partition_point(|r| r.hi <= cur);
    let mut pbuf = std::collections::VecDeque::<i64>::new();
    let mut out = vec![];
    let mut last = cur;
    while out.len() < need {
        if mbuf.is_empty() && !mdone {
            let v: Vec<i64> = mst
                .query_map(rusqlite::params![mterms[k], mcur], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            mdone = v.len() < 512;
            if let Some(&l) = v.last() {
                mcur = l
            }
            mbuf.extend(v);
        }
        if !src_set.is_empty() {
            while pbuf.is_empty() && rix < all.len() {
                let r = &all[rix];
                rix += 1;
                let ok = if want == T2 {
                    path_full(sets, r.dir)
                } else {
                    anc_score(&src_set, r.dir).is_some()
                };
                if ok {
                    let mut t = vec![];
                    expand(&c2g, r, last + 1, usize::MAX >> 2, &mut t)?;
                    pbuf.extend(t);
                }
            }
        }
        let (a, b) = (mbuf.front().copied(), pbuf.front().copied());
        let (id, from_main) = match (a, b) {
            (None, None) => break,
            (Some(x), None) => {
                mbuf.pop_front();
                (x, true)
            }
            (None, Some(y)) => {
                pbuf.pop_front();
                (y, false)
            }
            (Some(x), Some(y)) => {
                if x < y {
                    mbuf.pop_front();
                    (x, true)
                } else if y < x {
                    pbuf.pop_front();
                    (y, false)
                } else {
                    mbuf.pop_front();
                    pbuf.pop_front();
                    (x, true)
                }
            }
        };
        if id <= last || mainfull.contains(&id) {
            continue;
        }
        let Some(d) = dir_of(id) else { continue };
        let ok = if want == T2 {
            true
        } else {
            let miss: Vec<usize> = (0..sets.len())
                .filter(|&j| anc_score(&sets[j], d).is_none())
                .collect();
            !miss.is_empty()
                && miss.len() < sets.len()
                && miss.iter().all(|&j| {
                    (j == k && from_main)
                        || probe
                            .exists(rusqlite::params![mterms[j], id])
                            .unwrap_or(false)
                })
        };
        if ok {
            out.push(id);
            last = id;
        }
    }
    Ok(out)
}
fn b_page(
    c1: &M,
    c2: &M,
    c3: &M,
    q: &str,
    pscale: f64,
    after: Option<Key>,
    lim: usize,
) -> rusqlite::Result<Vec<Key>> {
    let tg = Instant::now();
    let p = gather(c1, c2, c3, q, pscale, after, lim)?;
    pt("gather_wall", tg);
    let tm = Instant::now();
    let mut v: Vec<Key> = p.main.clone();
    pt("merge_sort", tm);
    if v.len() < lim {
        let t = Instant::now();
        for tier in [T1, T2] {
            if v.len() >= lim {
                break;
            }
            let cur = match after {
                Some((s, id)) if s == tier => id,
                Some((s, _)) if s > tier => continue,
                _ => i64::MIN,
            };
            let got = tier_stream(c2, c3, q, &p, tier, cur, lim - v.len())?;
            v.extend(got.into_iter().map(|id| (tier, id)));
        }
        pt("dir_files_page", t);
    }
    Ok(v)
}
fn b_full(c1: &M, c2: &M, c3: &M, q: &str, pscale: f64) -> Vec<Key> {
    b_page(c1, c2, c3, q, pscale, None, usize::MAX >> 8).unwrap()
}
/// oracle using the old per-file path table (audio_path_search_t3): expected id SET
fn oracle(c: &Connection, q: &str) -> Set {
    // independent: per-FILE path table (audio_path_search_t3, full dir path incl. mount name); a path term hitting > half the files is ignored
    let all =
        |t: &str, m: &str| -> Set { ids(c, t, m, usize::MAX >> 2).unwrap().into_iter().collect() };
    let total: i64 = c
        .query_row("SELECT count(*) FROM audio_files", [], |r| r.get(0))
        .unwrap();
    let ws = words(q);
    let ws = if ws.len() <= 1 {
        vec![q.to_string()]
    } else {
        ws
    };
    let per: Vec<Set> = ws
        .iter()
        .map(|w| {
            let mut x = all("audio_search", &compile(w, RAW, DER));
            let p = all("audio_path_search_t3", &dir_m(w));
            if (p.len() as i64) * 2 <= total {
                x.extend(p);
            }
            x
        })
        .collect();
    let mut s = all("audio_search", &compile(q, RAW, DER));
    for id in &per[0] {
        if per[1..].iter().all(|p| p.contains(id)) {
            s.insert(*id);
        }
    }
    s
}
fn base_page(c: &Connection, q: &str) -> Vec<Key> {
    sides(q, 1.0).0.page(c, None, PAGE).unwrap()
}

fn pct(v: &mut [f64]) -> (f64, f64, f64) {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let at = |p: f64| {
        v[((v.len() as f64 * p).ceil() as usize)
            .saturating_sub(1)
            .min(v.len() - 1)]
    };
    (at(0.50), at(0.95), at(0.99))
}
fn time<F: FnMut() -> usize>(n: usize, mut f: F) -> (Vec<f64>, usize) {
    let mut hits = 0;
    let v = (0..n)
        .map(|_| {
            let t = Instant::now();
            hits = f();
            t.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    (v, hits)
}

#[derive(PartialEq, Eq)]
struct ProviderPageSignature {
    ids: Vec<i64>,
    total_count: u64,
    has_next_cursor: bool,
}

struct ProviderTiming {
    scope: SearchScope,
    median_p95_ms: f64,
    page: ProviderPageSignature,
}

/// Run the shipped provider rather than the historical SQL prototypes in this binary.
fn provider_bench(db: &str, warm: usize) -> Result<(), String> {
    let rounds = match std::env::var("ROUNDS") {
        Ok(value) => value
            .parse::<usize>()
            .map_err(|_| "ROUNDS must be a positive integer".to_string())?,
        Err(std::env::VarError::NotPresent) => 5,
        Err(_) => return Err("ROUNDS must be a positive integer".into()),
    };
    if rounds == 0 || warm == 0 {
        return Err("ROUNDS and WARM must be positive".into());
    }
    let queries = match std::env::var("Q") {
        Ok(query) => vec![query],
        Err(std::env::VarError::NotPresent) => {
            ["Borg Sound", "Rain Indoor Ambiences", "Car Collection"]
                .into_iter()
                .map(String::from)
                .collect()
        }
        Err(_) => return Err("Q must contain Unicode search text".into()),
    };
    for query in &queries {
        if query.trim().is_empty() {
            return Err("Q must contain a nonempty search query".into());
        }
        SearchRequest {
            query: query.clone(),
            ..Default::default()
        }
        .validate()
        .map_err(|_| "Q does not meet the search input contract".to_string())?;
    }
    let source = std::path::Path::new(db);
    let mut wal_path = source.as_os_str().to_os_string();
    wal_path.push("-wal");
    let require_complete_snapshot = || match std::fs::metadata(&wal_path) {
        Ok(metadata) if metadata.len() != 0 => Err(
            "Provider mode requires an immutable complete snapshot without a nonempty WAL"
                .to_string(),
        ),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err("Cannot verify the source snapshot WAL state".to_string()),
    };
    require_complete_snapshot()?;
    let before =
        std::fs::metadata(source).map_err(|_| "Cannot inspect the source snapshot".to_string())?;
    let before_modified = before
        .modified()
        .map_err(|_| "Cannot verify the source snapshot modification time".to_string())?;
    let owned = tempfile::Builder::new()
        .prefix("audiolib-provider-bench-")
        .tempdir()
        .map_err(|_| "Cannot create an owned benchmark directory".to_string())?;
    let copied = owned.path().join("library.db");
    {
        let mut input = std::fs::File::open(source)
            .map_err(|_| "Cannot read the source snapshot".to_string())?;
        let mut output = std::fs::File::create(&copied)
            .map_err(|_| "Cannot create the owned benchmark copy".to_string())?;
        std::io::copy(&mut input, &mut output)
            .map_err(|_| "Cannot copy the source snapshot into the owned directory".to_string())?;
    }
    require_complete_snapshot()?;
    let after =
        std::fs::metadata(source).map_err(|_| "Cannot recheck the source snapshot".to_string())?;
    if before.len() != after.len() || after.modified().ok() != Some(before_modified) {
        return Err(
            "Source snapshot changed while copying; use an immutable complete snapshot".into(),
        );
    }
    let probe = Connection::open_with_flags(&copied, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|_| "Cannot open the benchmark fixture read-only".to_string())?;
    let schema_version: i64 = probe
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|_| "Cannot read benchmark fixture schema".to_string())?;
    let format: String = probe
        .query_row(
            "SELECT value FROM library_meta WHERE key='snapshot_format'",
            [],
            |row| row.get(0),
        )
        .map_err(|_| "Cannot read benchmark fixture format".to_string())?;
    if schema_version != 9 || format != audiolib_core::compatibility::PLAN_B_V9_FORMAT {
        return Err("Provider mode requires a published schema-9 sd1 fixture".into());
    }
    drop(probe);
    let provider = SqliteProvider::open_server_existing(
        &copied,
        &std::sync::atomic::AtomicBool::new(false),
        |_| {},
    )
    .map_err(|_| {
        "Cannot initialize the production server provider on the owned copy".to_string()
    })?;
    if !provider
        .path_search_ready()
        .map_err(|_| "Cannot verify the published directory index".to_string())?
    {
        return Err(
            "Provider mode requires a ready directory index; fallback is not measured".into(),
        );
    }
    println!(
        "{}",
        serde_json::json!({
            "mode": "provider", "phase": "conditions", "sqlite": rusqlite::version(),
            "schema_version": schema_version, "format": format, "limit": PAGE,
            "rounds": rounds, "warmups_per_round": 3, "samples_per_round": warm,
            "clock": "wall_time_including_hydration", "cache": "warm_provider_and_os",
            "provider": "server_provider", "owned_fixture_copy": true,
            "source_unmodified": true, "read_only_search_operations": true,
            "copy_timed": false, "provider_startup_timed": false, "http_timed": false,
            "baseline": "surrogate_s8_main_on_same_s9_fixture", "actual_s8_fixture": false,
            "acceptance_gate": "not_evaluated"
        })
    );
    for query in queries {
        let mut timings = Vec::new();
        for scope in [SearchScope::Main, SearchScope::All, SearchScope::Path] {
            let request = SearchRequest {
                query: query.clone(),
                scope,
                limit: PAGE,
                ..Default::default()
            };
            let mut expected = None;
            let mut round_p95 = Vec::with_capacity(rounds);
            for round in 1..=rounds {
                let mut samples = Vec::with_capacity(warm);
                let mut cache_hits = 0;
                for sample in 0..warm.saturating_add(3) {
                    let start = Instant::now();
                    let page = provider.search(&request).map_err(|_| {
                        format!(
                            "Production provider search failed in scope {}",
                            scope.as_str()
                        )
                    })?;
                    let elapsed = start.elapsed().as_secs_f64() * 1000.0;
                    let signature = ProviderPageSignature {
                        ids: page.results.iter().map(|file| file.id).collect(),
                        total_count: page.total_count,
                        has_next_cursor: page.next_cursor.is_some(),
                    };
                    if expected.as_ref().is_some_and(|value| *value != signature) {
                        return Err(
                            "Search results changed during measurement; use a stable owned fixture"
                                .into(),
                        );
                    }
                    expected.get_or_insert(signature);
                    if sample >= 3 {
                        samples.push(elapsed);
                        cache_hits += usize::from(page.cache_hit);
                    }
                }
                let (p50, p95, p99) = pct(&mut samples);
                round_p95.push(p95);
                let page = expected.as_ref().expect("positive sample count");
                println!(
                    "{}",
                    serde_json::json!({
                        "mode": "provider", "phase": "round", "query": query,
                        "scope": scope.as_str(), "round": round, "p50_ms": p50,
                        "p95_ms": p95, "p99_ms": p99, "returned_count": page.ids.len(),
                        "total_count": page.total_count, "has_next_cursor": page.has_next_cursor,
                        "ranking_cache_hits": cache_hits,
                        "total_count_semantics": if scope == SearchScope::Main {
                            "exact"
                        } else {
                            "limit_plus_one_observed_prefix"
                        }
                    })
                );
            }
            let mut sorted = round_p95.clone();
            sorted.sort_by(f64::total_cmp);
            let middle = sorted.len() / 2;
            let median_p95_ms = if sorted.len().is_multiple_of(2) {
                (sorted[middle - 1] + sorted[middle]) / 2.0
            } else {
                sorted[middle]
            };
            let page = expected.expect("positive round and sample counts");
            println!(
                "{}",
                serde_json::json!({
                    "mode": "provider", "phase": "summary", "query": query,
                    "scope": scope.as_str(), "round_p95_ms": round_p95,
                    "median_p95_ms": median_p95_ms, "returned_count": page.ids.len(),
                    "total_count": page.total_count, "has_next_cursor": page.has_next_cursor
                })
            );
            timings.push(ProviderTiming {
                scope,
                median_p95_ms,
                page,
            });
        }
        let main = &timings[0];
        for measured in &timings[1..] {
            let comparable = measured.scope == SearchScope::All
                && !main.page.ids.is_empty()
                && measured.page.ids == main.page.ids
                && main.median_p95_ms > 0.0;
            println!(
                "{}",
                serde_json::json!({
                    "mode": "provider", "phase": "comparison", "query": query,
                    "scope": measured.scope.as_str(),
                    "baseline": "surrogate_s8_main_on_same_s9_fixture",
                    "actual_s8_fixture": false, "same_nonempty_ordered_page": comparable,
                    "ratio_to_surrogate_main": comparable.then(|| measured.median_p95_ms / main.median_p95_ms),
                    "acceptance_gate": "not_evaluated",
                    "reason": if comparable {
                        "same_page_timing_only_not_actual_schema8_acceptance"
                    } else if measured.scope == SearchScope::Path {
                        "path_scope_has_different_result_semantics"
                    } else {
                        "different_or_empty_page_or_zero_baseline"
                    }
                })
            );
        }
    }
    Ok(())
}

/// full (re)build of the directory tables; incremental maintenance = same statements restricted to affected dir_ids
fn build_dir(c: &Connection) {
    c.execute_batch("BEGIN; DROP TABLE IF EXISTS audio_dir_search; DROP TABLE IF EXISTS audio_dir_runs;
      CREATE VIRTUAL TABLE audio_dir_search USING fts5(path_tokens,path_tokens_tokenized,content='',contentless_delete=1,detail=full,columnsize=1,tokenize='unicode61 remove_diacritics 2');
      INSERT INTO audio_dir_search(rowid,path_tokens,path_tokens_tokenized) SELECT id, t3_norm(folder_name), audiolib_tokenize(t3_norm(folder_name)) FROM folders;
      CREATE TABLE audio_dir_runs(dir_id INTEGER NOT NULL, lo INTEGER NOT NULL, hi INTEGER NOT NULL, n INTEGER NOT NULL, PRIMARY KEY(dir_id, lo)) WITHOUT ROWID;
      INSERT INTO audio_dir_runs SELECT folder_id, min(id), max(id), count(*) FROM (SELECT id, folder_id, sum(chg) OVER (ORDER BY id) g FROM (SELECT id, folder_id, CASE WHEN folder_id IS lag(folder_id) OVER (ORDER BY id) THEN 0 ELSE 1 END chg FROM audio_files)) GROUP BY g;
      INSERT INTO audio_dir_search(audio_dir_search) VALUES('optimize'); COMMIT;").unwrap();
}
/// incremental maintenance for affected dir_ids (scan diff): deleted dirs -> rows removed; renamed/moved dirs (+subtree) -> reindexed;
/// runs recomputed for every dir whose file set changed, using global id adjacency so runs never span another dir's file.
fn maint_dirs(c: &Connection, dirs_changed: &[i64], dirs_files_changed: &[i64]) {
    let ins = "INSERT INTO audio_dir_search(rowid,path_tokens,path_tokens_tokenized) SELECT id, t3_norm(folder_name), audiolib_tokenize(t3_norm(folder_name)) FROM folders WHERE id=?1";
    for &d in dirs_changed {
        // includes deleted ids and every descendant of a renamed/moved dir
        c.execute("DELETE FROM audio_dir_search WHERE rowid=?1", [d])
            .unwrap();
        c.execute(ins, [d]).unwrap(); // no-op if folder deleted
    }
    // orphan sweep (PS-21): any dir row whose folder no longer exists
    c.execute_batch("DELETE FROM audio_dir_runs WHERE dir_id NOT IN (SELECT id FROM folders);")
        .unwrap();
    for &d in dirs_files_changed {
        c.execute("DELETE FROM audio_dir_runs WHERE dir_id=?1", [d])
            .unwrap();
    }
    for &d in dirs_files_changed {
        c.execute("INSERT INTO audio_dir_runs SELECT folder_id, min(id), max(id), count(*) FROM (SELECT id, folder_id, sum(chg) OVER (ORDER BY id) g FROM (SELECT id, folder_id, CASE WHEN folder_id IS lag(folder_id) OVER (ORDER BY id) THEN 0 ELSE 1 END chg FROM audio_files WHERE id BETWEEN (SELECT coalesce(min(id),0) FROM audio_files WHERE folder_id=?1) AND (SELECT coalesce(max(id),-1) FROM audio_files WHERE folder_id=?1))) WHERE folder_id=?1 GROUP BY g", [d]).unwrap();
    }
}
/// PS-21/PS-22 invariants
fn dir_invariants(c: &Connection) -> String {
    let q = |s: &str| -> i64 { c.query_row(s, [], |r| r.get(0)).unwrap() };
    let orphan_runs =
        q("SELECT count(*) FROM audio_dir_runs WHERE dir_id NOT IN (SELECT id FROM folders)");
    let fts_rows = q("SELECT count(*) FROM audio_dir_search");
    let folders = q("SELECT count(*) FROM folders");
    let covered = q(
        "SELECT count(*) FROM audio_files af WHERE EXISTS (SELECT 1 FROM audio_dir_runs r WHERE r.dir_id=af.folder_id AND af.id BETWEEN r.lo AND r.hi)",
    );
    let files = q("SELECT count(*) FROM audio_files");
    let sumn = q("SELECT coalesce(sum(n),0) FROM audio_dir_runs");
    let foreign = q(
        "SELECT count(*) FROM audio_dir_runs r WHERE EXISTS (SELECT 1 FROM audio_files af WHERE af.id BETWEEN r.lo AND r.hi AND af.folder_id IS NOT r.dir_id)",
    );
    format!(
        "folders={folders} dir_fts_rows={fts_rows} orphan_runs={orphan_runs} files={files} covered={covered} sum_n={sumn} runs_with_foreign_file={foreign} => {}",
        if orphan_runs == 0
            && fts_rows == folders
            && covered == files
            && sumn == files
            && foreign == 0
        {
            "OK"
        } else {
            "FAIL"
        }
    )
}
#[cfg(target_os = "macos")]
const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn pthread_set_qos_class_self_np(qos: u32, rel: i32) -> i32;
}
#[cfg(target_os = "macos")]
fn qos() {
    if std::env::var("QOS").is_ok() {
        // SAFETY: macOS accepts this QoS class and relative priority 0 for the
        // calling thread; the function takes no pointers or borrowed data.
        unsafe {
            pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0);
        }
    }
}
#[cfg(not(target_os = "macos"))]
fn qos() {}

fn main() {
    qos();
    if std::env::var("MEMSTATUS0").is_ok() {
        // SAFETY: this process has not opened a SQLite connection or started
        // worker threads; SQLITE_CONFIG_MEMSTATUS takes one C int argument.
        unsafe {
            rusqlite::ffi::sqlite3_config(rusqlite::ffi::SQLITE_CONFIG_MEMSTATUS, 0i32);
        }
    }
    let a: Vec<String> = std::env::args().collect();
    let db = a[2].as_str();
    let pscale: f64 = std::env::var("PSCALE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1.0);
    let warm: usize = std::env::var("WARM")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    let coldn: usize = std::env::var("COLD")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);
    if matches!(a[1].as_str(), "warm" | "cold" | "page" | "cancel" | "warmq") {
        ridx(&open(db));
    }
    match a[1].as_str() {
        "provider" => {
            if let Err(message) = provider_bench(db, warm) {
                eprintln!("{message}");
                std::process::exit(1);
            }
        }
        "warm" => {
            let (c1, c2, c3) = (M::new(open(db)), M::new(open(db)), M::new(open(db)));
            let rounds: usize = std::env::var("ROUNDS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(1);
            if rounds > 1 {
                for q in QS {
                    let (mut b95, mut o95, mut b50, mut o50, mut b99, mut o99) =
                        (vec![], vec![], vec![], vec![], vec![], vec![]);
                    for _ in 0..rounds {
                        for _ in 0..3 {
                            base_page(&c1.lock().unwrap(), q);
                            b_page(&c1, &c2, &c3, q, pscale, None, PAGE).unwrap();
                        }
                        let (mut vb, _) = time(warm, || base_page(&c1.lock().unwrap(), q).len());
                        let (mut vo, _) = time(warm, || {
                            b_page(&c1, &c2, &c3, q, pscale, None, PAGE).unwrap().len()
                        });
                        let (b, o) = (pct(&mut vb), pct(&mut vo));
                        b50.push(b.0);
                        b95.push(b.1);
                        b99.push(b.2);
                        o50.push(o.0);
                        o95.push(o.1);
                        o99.push(o.2);
                    }
                    let med = |v: &mut Vec<f64>| {
                        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
                        v[v.len() / 2]
                    };
                    let f = |v: &Vec<f64>| {
                        v.iter()
                            .map(|x| format!("{x:.2}"))
                            .collect::<Vec<_>>()
                            .join(",")
                    };
                    let (rb, ro) = (f(&b95), f(&o95));
                    let (mb, mo) = (med(&mut b95), med(&mut o95));
                    println!("WARM5R\t{q}\ts8_round_p95=[{rb}]\ts9_round_p95=[{ro}]");
                    println!(
                        "WARM5\t{q}\ts8_p50={:.2}\tp95={mb:.2}\tp99={:.2}\ts9_p50={:.2}\tp95={mo:.2}\tp99={:.2}\tgate={}\tratio={:.3}",
                        med(&mut b50),
                        med(&mut b99),
                        med(&mut o50),
                        med(&mut o99),
                        if mo <= mb * 1.2 { "PASS" } else { "FAIL" },
                        mo / mb
                    );
                }
                return;
            }
            for q in QS {
                for _ in 0..3 {
                    base_page(&c1.lock().unwrap(), q);
                    b_page(&c1, &c2, &c3, q, pscale, None, PAGE).unwrap();
                }
                let (mut vb, hb) = time(warm, || base_page(&c1.lock().unwrap(), q).len());
                let (mut vo, ho) = time(warm, || {
                    b_page(&c1, &c2, &c3, q, pscale, None, PAGE).unwrap().len()
                });
                let (b, o) = (pct(&mut vb), pct(&mut vo));
                println!(
                    "WARM\t{q}\tbase_rows={hb}\tB_rows={ho}\tbase_p50={:.2}\tp95={:.2}\tp99={:.2}\tB_p50={:.2}\tp95={:.2}\tp99={:.2}\tgate={}",
                    b.0,
                    b.1,
                    b.2,
                    o.0,
                    o.1,
                    o.2,
                    if o.1 <= b.1 { "PASS" } else { "FAIL" }
                );
            }
        }
        "cold1" => {
            // one OS-cold sample (caller runs `sudo purge` first): Q=query ONLY=s8|s9
            let q = std::env::var("Q").unwrap();
            let only = std::env::var("ONLY").unwrap();
            let (c1, c2, c3) = (M::new(open(db)), M::new(open(db)), M::new(open(db)));
            if only != "s8" {
                ridx(&c2.lock().unwrap());
            } // process-level index, loaded at server start
            if prepare_outside_timer() {
                for c in [&c1, &c2, &c3] {
                    prepare_fts(&c.lock().unwrap(), &q, pscale);
                }
            }
            let t = Instant::now();
            let n = if only == "s8" {
                base_page(&c1.lock().unwrap(), &q).len()
            } else {
                b_page(&c1, &c2, &c3, &q, pscale, None, PAGE).unwrap().len()
            };
            if only != "s8" {
                let p = PROF.lock().unwrap();
                println!(
                    "PROF\t{q}\t{}",
                    p.iter()
                        .map(|(k, v)| format!("{k}={v:.2}"))
                        .collect::<Vec<_>>()
                        .join("\t")
                );
            }
            println!(
                "COLD1\t{only}\t{q}\trows={n}\tms={:.2}",
                t.elapsed().as_secs_f64() * 1e3
            );
        }
        "nopath-probe" => {
            let q = std::env::var("Q").unwrap();
            let (c1, c2, c3) = (M::new(open(db)), M::new(open(db)), M::new(open(db)));
            let _ = ridx(&c1.lock().unwrap());
            let ws = words(&q);
            let c = c2.lock().unwrap();
            let total = ri().all.iter().map(|r| r.hi - r.lo + 1).sum::<i64>();
            let mut sets: Vec<DirSet> = ws
                .iter()
                .map(|w| seg_match(&c, &dir_m(w), false))
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            let ests0: Vec<i64> = sets.iter().map(est).collect();
            for (j, e) in ests0.iter().enumerate() {
                if *e * 2 > total {
                    sets[j].clear();
                }
            }
            let ests: Vec<i64> = sets.iter().map(est).collect();
            println!("terms={ws:?} ests_after_half={ests:?} ests_before={ests0:?}");
            for w in &ws {
                let m = compile(w, RAW, DER);
                let t = Instant::now();
                let v = ids(&c1.lock().unwrap(), "audio_search", &m, usize::MAX >> 2).unwrap();
                println!(
                    "main_full\t{w}\tn={}\tms={:.2}",
                    v.len(),
                    t.elapsed().as_secs_f64() * 1e3
                );
                let t = Instant::now();
                let v1 = ids(&c1.lock().unwrap(), "audio_search", &m, 1).unwrap();
                println!(
                    "main_lim1\t{w}\tn={}\tms={:.2}",
                    v1.len(),
                    t.elapsed().as_secs_f64() * 1e3
                );
            }
            let ne: DirSet = sets
                .iter()
                .flat_map(|x| x.iter().map(|(&d, &v)| (d, v)))
                .collect();
            println!("ne_est={} tops={}", est(&ne), top(&ne).len());
            let t = Instant::now();
            let files = files_of(&c, &ne).unwrap();
            println!(
                "expand_ne\tn={}\tms={:.2}",
                files.len(),
                t.elapsed().as_secs_f64() * 1e3
            );
            if let Some(j) = sets.iter().position(|t| t.is_empty()) {
                let m = compile(&ws[j], RAW, DER);
                let cg = c3.lock().unwrap();
                let t = Instant::now();
                let mut hit = 0usize;
                let mut scanned = 0usize;
                let mut st = cg
                    .prepare_cached(
                        "SELECT 1 FROM audio_search WHERE audio_search MATCH ?1 AND rowid=?2",
                    )
                    .unwrap();
                for &(id, _) in &files {
                    scanned += 1;
                    if st.exists(rusqlite::params![&m, id]).unwrap() {
                        hit += 1;
                        if hit >= 3 {
                            break;
                        }
                    }
                    if scanned >= 500 && hit == 0 {
                        break;
                    }
                }
                println!(
                    "probe_each\thit={hit}\tscanned={scanned}\tms={:.2}",
                    t.elapsed().as_secs_f64() * 1e3
                );
                cg.execute_batch(
                    "CREATE TEMP TABLE IF NOT EXISTS tpf(id INTEGER PRIMARY KEY); DELETE FROM tpf;",
                )
                .unwrap();
                let t = Instant::now();
                {
                    let mut ins = cg.prepare("INSERT OR IGNORE INTO tpf VALUES(?1)").unwrap();
                    for &(id, _) in &files {
                        ins.execute([id]).unwrap();
                    }
                }
                let ins_ms = t.elapsed().as_secs_f64() * 1e3;
                let t = Instant::now();
                let n:i64 = cg.query_row("SELECT count(*) FROM audio_search a WHERE a.audio_search MATCH ?1 AND EXISTS(SELECT 1 FROM tpf WHERE id=a.rowid)", [&m], |r| r.get(0)).unwrap();
                println!(
                    "intersect_exists\tn={n}\tinsert_ms={ins_ms:.2}\tquery_ms={:.2}",
                    t.elapsed().as_secs_f64() * 1e3
                );
                let t = Instant::now();
                let n2:i64 = cg.query_row("SELECT count(*) FROM tpf WHERE id IN (SELECT rowid FROM audio_search WHERE audio_search MATCH ?1)", [&m], |r| r.get(0)).unwrap();
                println!(
                    "intersect_in\tn={n2}\tms={:.2}",
                    t.elapsed().as_secs_f64() * 1e3
                );
            }
        }

        "warmq" => {
            let q = std::env::var("Q").unwrap();
            let (c1, c2, c3) = (M::new(open(db)), M::new(open(db)), M::new(open(db)));
            for _ in 0..3 {
                base_page(&c1.lock().unwrap(), &q);
                b_page(&c1, &c2, &c3, &q, pscale, None, PAGE).unwrap();
            }
            let (mut vb, _) = time(warm, || base_page(&c1.lock().unwrap(), &q).len());
            PROF.lock().unwrap().clear();
            let (mut vo, _) = time(warm, || {
                b_page(&c1, &c2, &c3, &q, pscale, None, PAGE).unwrap().len()
            });
            let (b, o) = (pct(&mut vb), pct(&mut vo));
            let mut agg: Vec<(&str, Vec<f64>)> = vec![];
            for (k, v) in PROF.lock().unwrap().iter() {
                match agg.iter_mut().find(|x| x.0 == *k) {
                    Some(x) => x.1.push(*v),
                    None => agg.push((k, vec![*v])),
                }
            }
            println!(
                "WARMQ\t{q}\ts8_p95={:.2}\ts9_p95={:.2}\tratio={:.3}\tphase_p50: {}",
                b.1,
                o.1,
                o.1 / b.1,
                agg.iter_mut()
                    .map(|(k, v)| format!("{k}={:.2}", pct(v).0))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
        "qs" => {
            for q in QS {
                println!("{q}")
            }
        }
        "cold" => {
            // SQLite-cold: fresh connections (empty page cache) per run; OS cache NOT cleared
            let rounds: usize = std::env::var("ROUNDS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(1);
            if rounds > 1 {
                for q in QS {
                    let (mut b95, mut o95, mut b50, mut o50) = (vec![], vec![], vec![], vec![]);
                    for _ in 0..rounds {
                        let (mut vb, _) = {
                            let mut v = vec![];
                            for _ in 0..coldn {
                                let c = open(db);
                                let t = Instant::now();
                                base_page(&c, q);
                                v.push(t.elapsed().as_secs_f64() * 1e3);
                            }
                            (v, 0)
                        };
                        let (mut vo, _) = {
                            let mut v = vec![];
                            for _ in 0..coldn {
                                let (c1, c2, c3) =
                                    (M::new(open(db)), M::new(open(db)), M::new(open(db)));
                                let t = Instant::now();
                                b_page(&c1, &c2, &c3, q, pscale, None, PAGE).unwrap();
                                v.push(t.elapsed().as_secs_f64() * 1e3);
                            }
                            (v, 0usize)
                        };
                        let (b, o) = (pct(&mut vb), pct(&mut vo));
                        b50.push(b.0);
                        b95.push(b.1);
                        o50.push(o.0);
                        o95.push(o.1);
                    }
                    let med = |v: &mut Vec<f64>| {
                        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
                        v[v.len() / 2]
                    };
                    let f = |v: &Vec<f64>| {
                        v.iter()
                            .map(|x| format!("{x:.2}"))
                            .collect::<Vec<_>>()
                            .join(",")
                    };
                    let (rb, ro) = (f(&b95), f(&o95));
                    let (mb, mo) = (med(&mut b95), med(&mut o95));
                    println!("WARM5R\t{q}\ts8_round_p95=[{rb}]\ts9_round_p95=[{ro}]");
                    println!(
                        "COLD5\t{q}\ts8_p50={:.2}\tp95={mb:.2}\ts9_p50={:.2}\tp95={mo:.2}\tgate={}",
                        med(&mut b50),
                        med(&mut o50),
                        if mo <= mb { "PASS" } else { "FAIL" }
                    );
                }
                return;
            }
            for q in QS {
                let (mut vb, _) = {
                    let mut v = vec![];
                    for _ in 0..coldn {
                        let c = open(db);
                        let t = Instant::now();
                        base_page(&c, q);
                        v.push(t.elapsed().as_secs_f64() * 1e3);
                    }
                    (v, 0)
                };
                let (mut vo, _) = {
                    let mut v = vec![];
                    for _ in 0..coldn {
                        let (c1, c2, c3) = (M::new(open(db)), M::new(open(db)), M::new(open(db)));
                        let t = Instant::now();
                        b_page(&c1, &c2, &c3, q, pscale, None, PAGE).unwrap();
                        v.push(t.elapsed().as_secs_f64() * 1e3);
                    }
                    (v, 0usize)
                };
                let (b, o) = (pct(&mut vb), pct(&mut vo));
                println!(
                    "COLD\t{q}\tbase_p50={:.2}\tp95={:.2}\tB_p50={:.2}\tp95={:.2}\tgate={}",
                    b.0,
                    b.1,
                    o.0,
                    o.1,
                    if o.1 <= b.1 { "PASS" } else { "FAIL" }
                );
            }
        }
        "build-dir" => {
            let c = open_rw(db);
            let t = Instant::now();
            build_dir(&c);
            println!("BUILD_DIR s={:.2}", t.elapsed().as_secs_f64());
        }
        "maint-test" => {
            // everything inside one transaction, rolled back: bench.db left unchanged
            let c = open_rw(db);
            c.execute_batch("BEGIN").unwrap();
            println!("INV before: {}", dir_invariants(&c));
            let q1 = |s: &str| -> i64 { c.query_row(s, [], |r| r.get(0)).unwrap() };
            // 1) delete a leaf dir with files
            let dl = q1(
                "SELECT f.id FROM folders f WHERE f.file_count>0 AND NOT EXISTS (SELECT 1 FROM folders x WHERE x.parent_id=f.id) ORDER BY f.id LIMIT 1 OFFSET 500",
            );
            c.execute("DELETE FROM audio_files WHERE folder_id=?1", [dl])
                .unwrap();
            c.execute("DELETE FROM folders WHERE id=?1", [dl]).unwrap();
            // 2) move a file to another dir
            let (fid, from): (i64, i64) = c
                .query_row(
                    "SELECT id, folder_id FROM audio_files ORDER BY id LIMIT 1 OFFSET 400000",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            let to = q1("SELECT id FROM folders WHERE file_count>0 ORDER BY id DESC LIMIT 1");
            c.execute("UPDATE audio_files SET folder_id=?1 WHERE id=?2", [to, fid])
                .unwrap();
            // 3) add files (new max ids) to an existing dir
            c.execute("INSERT INTO audio_files(file_path,filename,folder_id,path_key,missing) SELECT file_path||'.new', filename||'.new', folder_id, path_key||'.new', 0 FROM audio_files WHERE folder_id=?1 LIMIT 3", [from]).ok();
            // 4) rename a dir with subtree (rewrite folder_path prefix of it and its descendants)
            let rd = q1(
                "SELECT f.id FROM folders f WHERE EXISTS (SELECT 1 FROM folders x WHERE x.parent_id=f.id) ORDER BY f.id LIMIT 1 OFFSET 50",
            );
            let old: String = c
                .query_row("SELECT folder_path FROM folders WHERE id=?1", [rd], |r| {
                    r.get(0)
                })
                .unwrap();
            let sub: Vec<i64> = {
                let mut st = c
                    .prepare("SELECT descendant_id FROM folder_closure WHERE ancestor_id=?1")
                    .unwrap();
                st.query_map([rd], |r| r.get(0))
                    .unwrap()
                    .map(|x| x.unwrap())
                    .collect()
            };
            c.execute("UPDATE folders SET folder_path=?2||substr(folder_path, length(?1)+1) WHERE id IN (SELECT descendant_id FROM folder_closure WHERE ancestor_id=?3)", rusqlite::params![old, format!("{old}_RenamedZZQ"), rd]).unwrap();
            c.execute(
                "UPDATE folders SET folder_name=folder_name||'_RenamedZZQ' WHERE id=?1",
                [rd],
            )
            .unwrap();
            println!("INV after-change-before-maint: {}", dir_invariants(&c));
            let t = Instant::now();
            let changed = vec![rd, dl]; // segment index: only the renamed dir's own row changes
            maint_dirs(&c, &changed, &[dl, from, to]);
            println!(
                "MAINT ms={:.1} deleted_dir={dl} moved_file={fid} {from}->{to} renamed_dir={rd} subtree={}",
                t.elapsed().as_secs_f64() * 1e3,
                sub.len()
            );
            println!("INV after-maint: {}", dir_invariants(&c));
            let hit = q1(&format!(
                "SELECT count(*) FROM audio_dir_search WHERE audio_dir_search MATCH '{}'",
                compile("RenamedZZQ", "path_tokens", "path_tokens_tokenized").replace('\'', "''")
            ));
            let gone = q1(&format!(
                "SELECT count(*) FROM audio_dir_search WHERE rowid={dl}"
            ));
            let ridx_like = {
                let mut st = c.prepare("SELECT descendant_id FROM folder_closure WHERE ancestor_id IN (SELECT rowid FROM audio_dir_search WHERE audio_dir_search MATCH ?1)").unwrap();
                st.query_map(
                    [compile(
                        "RenamedZZQ",
                        "path_tokens",
                        "path_tokens_tokenized",
                    )],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap()
                .count()
            };
            println!(
                "RENAME segment_rows={hit} (expect 1) path_matching_dirs={ridx_like} (expect {}) deleted_dir_rows={gone} (expect 0)",
                sub.len()
            );
            c.execute_batch("ROLLBACK").unwrap();
            println!("ROLLED BACK; INV now: {}", dir_invariants(&c));
        }
        "page" => {
            // correctness: keyset pages vs full merged
            let pages: usize = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(5);
            let (c1, c2, c3) = (M::new(open(db)), M::new(open(db)), M::new(open(db)));
            for q in QS {
                let full = b_full(&c1, &c2, &c3, q, pscale);
                let mut got: Vec<Key> = vec![];
                let mut cur = None;
                for _ in 0..pages {
                    let pg = b_page(&c1, &c2, &c3, q, pscale, cur, PAGE).unwrap();
                    if pg.is_empty() {
                        break;
                    }
                    cur = pg.last().copied();
                    got.extend(pg);
                }
                let n = got.len();
                let mut ids: Vec<i64> = got.iter().map(|k| k.1).collect();
                ids.sort();
                ids.dedup();
                let dup = n - ids.len();
                let exp = &full[..n.min(full.len())];
                let mism = exp.iter().zip(&got).filter(|(x, y)| x.1 != y.1).count();
                let complete = n == full.len().min(pages * PAGE);
                let base = base_page(&c1.lock().unwrap(), q);
                let t0: Vec<i64> = full
                    .iter()
                    .filter(|k| k.0 < 0.0)
                    .take(base.len())
                    .map(|k| k.1)
                    .collect();
                let bi: Vec<i64> = base.iter().map(|k| k.1).collect();
                let tiers = [
                    full.iter().filter(|k| k.0 < 0.0).count(),
                    full.iter().filter(|k| k.0 == T1).count(),
                    full.iter().filter(|k| k.0 == T2).count(),
                ];
                println!(
                    "S8ORDER\t{q}\ttier0_prefix_equals_s8={}\ttiers(main,mixed,path)={:?}",
                    t0 == bi,
                    tiers
                );
                let or = oracle(&c1.lock().unwrap(), q);
                let fs: Set = full.iter().map(|k| k.1).collect();
                println!(
                    "ORACLE\t{q}\tfull={}\toracle={}\tmissing={}\textra={}",
                    fs.len(),
                    or.len(),
                    or.difference(&fs).count(),
                    fs.difference(&or).count()
                );
                println!(
                    "PAGE\t{q}\tfull={}\tpaged={n}\tdup={dup}\torder_mismatch={mism}\tcomplete={complete}\t{}",
                    full.len(),
                    if dup == 0 && mism == 0 && complete {
                        "OK"
                    } else {
                        "FAIL"
                    }
                );
            }
        }
        "cancel" => {
            let q = a.get(3).map(|s| s.as_str()).unwrap_or("hdd_A_Lib");
            let delay: u64 = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(30);
            let (c1, c2, c3) = (M::new(open(db)), M::new(open(db)), M::new(open(db)));
            let (h1, h2): (Arc<InterruptHandle>, Arc<InterruptHandle>) = (
                Arc::new(c1.lock().unwrap().get_interrupt_handle()),
                Arc::new(c2.lock().unwrap().get_interrupt_handle()),
            );
            let h3: Arc<InterruptHandle> = Arc::new(c3.lock().unwrap().get_interrupt_handle());
            let t = Instant::now();
            let full = b_page(&c1, &c2, &c3, q, pscale, None, 1_000_000)
                .unwrap()
                .len();
            let fullms = t.elapsed().as_secs_f64() * 1e3;
            let mut lat = vec![];
            let mut errs = 0;
            for _ in 0..20 {
                let (a1, a2, a3) = (h1.clone(), h2.clone(), h3.clone());
                let killer = std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(delay));
                    let t = Instant::now();
                    a1.interrupt();
                    a2.interrupt();
                    a3.interrupt();
                    t
                });
                let r = b_page(&c1, &c2, &c3, q, pscale, None, 1_000_000);
                let end = Instant::now();
                let ti = killer.join().unwrap();
                if r.is_err() {
                    errs += 1;
                    lat.push(end.duration_since(ti).as_secs_f64() * 1e3)
                }
                // both conns usable afterwards?
                c1.lock()
                    .unwrap()
                    .query_row("SELECT 1", [], |_| Ok(()))
                    .unwrap();
                c2.lock()
                    .unwrap()
                    .query_row("SELECT 1", [], |_| Ok(()))
                    .unwrap();
            }
            if lat.is_empty() {
                println!(
                    "CANCEL\t{q}\tuncancelled_ms={fullms:.1}\tdelay_ms={delay}\tinterrupted=0/20 (query finished before interrupt)"
                );
                return;
            }
            let (p50, p95, p99) = pct(&mut lat);
            println!(
                "CANCEL\t{q}\tuncancelled_ms={fullms:.1} rows={full}\tdelay_ms={delay}\tinterrupted={errs}/20\ttime_to_cancel_p50={p50:.2}\tp95={p95:.2}\tmax={p99:.2}"
            );
        }
        "migrate-t3" => {
            // path-table-only migration: main audio_search untouched. batched 20000 rows/txn like the real migration.
            // MOUNT_PREFIX: absolute mount root stripped from file paths (e.g. "/path/to/Mounts/").
            let pfx = std::env::var("MOUNT_PREFIX")
                .unwrap_or_default()
                .replace('\'', "''");
            let dirs =
                format!("replace(rtrim(af.file_path,replace(af.file_path,'/','')),'{pfx}','')");
            let c = open_rw(db);
            let t = Instant::now();
            c.execute_batch("DROP TABLE IF EXISTS audio_path_search_t3; CREATE VIRTUAL TABLE audio_path_search_t3 USING fts5(path_tokens,path_tokens_tokenized,content='',contentless_delete=1,detail=full,columnsize=1,tokenize='unicode61 remove_diacritics 2');").unwrap();
            let mut last = i64::MIN;
            let mut total = 0usize;
            let mut bt = vec![];
            loop {
                let b0 = Instant::now();
                let tx = c.unchecked_transaction().unwrap();
                let hi: Option<i64> = tx.query_row("SELECT MAX(id) FROM (SELECT id FROM audio_files WHERE id>?1 ORDER BY id LIMIT 20000)", [last], |r| r.get(0)).unwrap();
                let Some(hi) = hi else { break };
                total += tx.execute(&format!("INSERT INTO audio_path_search_t3(rowid,path_tokens,path_tokens_tokenized) SELECT af.id,t3_norm({dirs}),audiolib_tokenize(t3_norm({dirs})) FROM audio_files af WHERE af.id>?1 AND af.id<=?2"), [last, hi]).unwrap();
                tx.commit().unwrap();
                last = hi;
                bt.push(b0.elapsed().as_secs_f64());
            }
            let ins = t.elapsed().as_secs_f64();
            let t2 = Instant::now();
            c.execute(
                "INSERT INTO audio_path_search_t3(audio_path_search_t3) VALUES('optimize')",
                [],
            )
            .unwrap();
            let opt = t2.elapsed().as_secs_f64();
            let changed: i64 = c
                .query_row(
                    &format!(
                        "SELECT count(*) FROM audio_files af WHERE t3_norm({dirs}) IS NOT {dirs}"
                    ),
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            let (p50, p95, mx) = {
                let mut v = bt.clone();
                let (a, b, _) = pct(&mut v);
                (a, b, v[v.len() - 1])
            };
            println!(
                "MIGRATE_T3 rows={total} batches={} insert_s={ins:.1} batch_p50_s={p50:.2} p95_s={p95:.2} max_s={mx:.2} optimize_s={opt:.1} total_s={:.1} rows_changed_by_t3={changed}",
                bt.len(),
                t.elapsed().as_secs_f64()
            );
        }
        "migrate9" => migrate9(db),
        _ => panic!(
            "usage: search-b-bench <warm|cold|cold1|page|cancel|warmq|qs|...> <db>; cold1 env: Q, ONLY=s8|s9, T3=1, BENCH_PREPARE_OUTSIDE_TIMER=1 (prepare FTS statements before the timer)"
        ),
    }
}

/// PROTOTYPE 8->9 (t3, directory-segment index): phases backup / path_build (audio_dir_search + audio_dir_runs) / optimize / switch.
/// Env: MAX_BATCHES (pause), FORCE_FREE_BYTES (preflight sim), FORCE_DISK_FULL_AT (sim SQLITE_FULL at batch n), BACKUP_MODE api|vacuum,
/// AUDIOLIB_MIGRATION_HOLD_SWITCH_MS. Errors: read_only -> preflight failed, DB untouched (exit 4); disk_full -> paused, checkpoint kept (exit 5).
fn migrate9(db: &str) {
    const FMT: &str = "ale-s9-cld1-t3-m2-p1-a1-sd1";
    let t0 = Instant::now();
    let report = |status: &str, code: Option<&str>, ph: &[(&str, f64)], extra: String| {
        let phs: Vec<String> = ph.iter().map(|(k, v)| format!("\"{k}\":{v:.3}")).collect();
        println!(
            "MIG9_REPORT {{\"status\":\"{status}\",\"error_code\":{},\"phases\":{{{}}}{extra},\"total_s\":{:.3}}}",
            code.map_or("null".into(), |c| format!("\"{c}\"")),
            phs.join(","),
            t0.elapsed().as_secs_f64()
        );
    };
    // preflight read_only: nothing is opened for writing, nothing is created
    let dir = std::path::Path::new(db)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    let ro = std::fs::metadata(db)
        .map(|m| m.permissions().readonly())
        .unwrap_or(true)
        || std::fs::metadata(dir)
            .map(|m| m.permissions().readonly())
            .unwrap_or(true)
        || std::env::var("FORCE_READ_ONLY").is_ok();
    if ro {
        report(
            "failed",
            Some("read_only"),
            &[],
            ",\"stage\":\"preflight\"".into(),
        );
        std::process::exit(4);
    }
    let c = open_rw(db);
    let uv: i64 = c
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    let has_ck: bool = c
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name='v9_checkpoint'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
        > 0;
    if uv == 9 {
        println!("MIG9 already s9");
        return;
    }
    let mut backup_s = 0.0;
    if !has_ck {
        let size = std::fs::metadata(db).unwrap().len();
        let free: u64 = match std::env::var("FORCE_FREE_BYTES") {
            Ok(v) => v.parse().unwrap(),
            Err(_) => {
                let o = std::process::Command::new("df")
                    .args(["-k", db])
                    .output()
                    .unwrap();
                let s = String::from_utf8_lossy(&o.stdout);
                s.lines()
                    .nth(1)
                    .unwrap()
                    .split_whitespace()
                    .nth(3)
                    .unwrap()
                    .parse::<u64>()
                    .unwrap()
                    * 1024
            }
        };
        let need = size * 13 / 10;
        if free < need {
            report(
                "failed",
                Some("disk_full"),
                &[],
                format!(",\"stage\":\"preflight\",\"free\":{free},\"need\":{need}"),
            );
            std::process::exit(3);
        }
        let tb = Instant::now();
        let bk = format!("{}.audiolib-rollback-mig9.db", db);
        let mode = std::env::var("BACKUP_MODE").unwrap_or_else(|_| "api".into());
        if !std::path::Path::new(&bk).exists() {
            if mode == "vacuum" {
                c.execute("VACUUM INTO ?1", [&bk]).unwrap();
            } else {
                let mut dst = Connection::open(&bk).unwrap();
                let b = rusqlite::backup::Backup::new(&c, &mut dst).unwrap();
                b.run_to_completion(65536, Duration::from_millis(0), None)
                    .unwrap();
            }
        }
        let ok: String = Connection::open(&bk)
            .unwrap()
            .query_row("PRAGMA quick_check", [], |r| r.get(0))
            .unwrap();
        backup_s = tb.elapsed().as_secs_f64();
        println!("MIG9 backup_mode={mode} backup_s={backup_s:.1} quick_check={ok}");
        c.execute_batch("BEGIN; DROP TABLE IF EXISTS audio_dir_search; DROP TABLE IF EXISTS audio_dir_runs;
            CREATE VIRTUAL TABLE audio_dir_search USING fts5(path_tokens,path_tokens_tokenized,content='',contentless_delete=1,detail=full,columnsize=1,tokenize='unicode61 remove_diacritics 2');
            CREATE TABLE audio_dir_runs(dir_id INTEGER NOT NULL, lo INTEGER NOT NULL, hi INTEGER NOT NULL, n INTEGER NOT NULL, PRIMARY KEY(dir_id, lo)) WITHOUT ROWID;
            CREATE TABLE v9_checkpoint(k INTEGER PRIMARY KEY CHECK(k=1), last_id INTEGER NOT NULL, done INTEGER NOT NULL, phase TEXT NOT NULL);
            INSERT INTO v9_checkpoint VALUES(1,-9223372036854775808,0,'dirs'); COMMIT;").unwrap();
    }
    let maxb: usize = std::env::var("MAX_BATCHES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(usize::MAX);
    let full_at: usize = std::env::var("FORCE_DISK_FULL_AT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(usize::MAX);
    let total: i64 = c
        .query_row("SELECT count(*) FROM folders", [], |r| r.get(0))
        .unwrap();
    let (mut last, mut done, mut phase): (i64, i64, String) = c
        .query_row("SELECT last_id,done,phase FROM v9_checkpoint", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap();
    println!("MIG9 start_from phase={phase} last_id={last} done={done}/{total}");
    let tf = Instant::now();
    let mut nb = 0;
    while phase == "dirs" {
        if nb >= maxb {
            report(
                "paused",
                None,
                &[
                    ("backup", backup_s),
                    ("path_build", tf.elapsed().as_secs_f64()),
                ],
                format!(",\"progress\":{{\"done\":{done},\"total\":{total}}}"),
            );
            return;
        }
        let tx = c.unchecked_transaction().unwrap();
        let hi: Option<i64> = tx
            .query_row(
                "SELECT MAX(id) FROM (SELECT id FROM folders WHERE id>?1 ORDER BY id LIMIT 5000)",
                [last],
                |r| r.get(0),
            )
            .unwrap();
        let Some(hi) = hi else {
            tx.execute("UPDATE v9_checkpoint SET phase='runs'", [])
                .unwrap();
            tx.commit().unwrap();
            phase = "runs".into();
            break;
        };
        let r = (|| -> rusqlite::Result<i64> {
            if nb + 1 == full_at {
                return Err(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_FULL),
                    Some("simulated".into()),
                ));
            }
            tx.execute(
                "DELETE FROM audio_dir_search WHERE rowid>?1 AND rowid<=?2",
                [last, hi],
            )?;
            let n = tx.execute("INSERT INTO audio_dir_search(rowid,path_tokens,path_tokens_tokenized) SELECT id,t3_norm(folder_name),audiolib_tokenize(t3_norm(folder_name)) FROM folders WHERE id>?1 AND id<=?2", [last, hi])? as i64;
            tx.execute("UPDATE v9_checkpoint SET last_id=?1, done=done+?2", [hi, n])?;
            Ok(n)
        })();
        match r {
            Ok(n) => {
                tx.commit().unwrap();
                last = hi;
                done += n;
                nb += 1;
            }
            Err(e) if e.sqlite_error_code() == Some(rusqlite::ErrorCode::DiskFull) => {
                drop(tx);
                report(
                    "paused",
                    Some("disk_full"),
                    &[
                        ("backup", backup_s),
                        ("path_build", tf.elapsed().as_secs_f64()),
                    ],
                    format!(
                        ",\"progress\":{{\"done\":{done},\"total\":{total}}},\"checkpoint_last_id\":{last}"
                    ),
                );
                std::process::exit(5);
            }
            Err(e) => panic!("{e}"),
        }
    }
    if phase == "runs" {
        // one statement, idempotent (rebuilt whole)
        c.execute_batch("BEGIN; DELETE FROM audio_dir_runs;
          INSERT INTO audio_dir_runs SELECT folder_id, min(id), max(id), count(*) FROM (SELECT id, folder_id, sum(chg) OVER (ORDER BY id) g FROM (SELECT id, folder_id, CASE WHEN folder_id IS lag(folder_id) OVER (ORDER BY id) THEN 0 ELSE 1 END chg FROM audio_files)) GROUP BY g;
          UPDATE v9_checkpoint SET phase='optimize'; COMMIT;").unwrap();
    }
    let pb = tf.elapsed().as_secs_f64();
    let to = Instant::now();
    c.execute(
        "INSERT INTO audio_dir_search(audio_dir_search) VALUES('optimize')",
        [],
    )
    .unwrap();
    let opt = to.elapsed().as_secs_f64();
    let ts = Instant::now();
    let hold: u64 = std::env::var("AUDIOLIB_MIGRATION_HOLD_SWITCH_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if hold > 0 {
        c.execute_batch("BEGIN IMMEDIATE").unwrap();
        println!("MIG9 SWITCHING hold_ms={hold}");
        std::thread::sleep(Duration::from_millis(hold));
        c.execute_batch("COMMIT").unwrap();
    }
    c.execute_batch(&format!("BEGIN IMMEDIATE; INSERT INTO library_meta(key,value) VALUES('snapshot_format','{FMT}'),('search_path_rev','1') ON CONFLICT(key) DO UPDATE SET value=excluded.value; DROP TABLE v9_checkpoint; PRAGMA user_version=9; COMMIT;")).unwrap();
    let sw = ts.elapsed().as_secs_f64();
    let inv = dir_invariants(&c);
    report(
        "done",
        None,
        &[
            ("backup", backup_s),
            ("path_build", pb),
            ("optimize", opt),
            ("switch", sw),
        ],
        format!(",\"progress\":{{\"done\":{done},\"total\":{total}}},\"invariants\":\"{inv}\""),
    );
}
