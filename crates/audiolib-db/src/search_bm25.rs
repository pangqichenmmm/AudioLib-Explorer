// crates/audiolib-db/src/search_bm25.rs
//! Private SQLite BM25 with snapshot-owned document lengths. Query phrases, global
//! IDF, instance frequencies and floating-point evaluation follow bundled SQLite.
use crate::db_error;
use crate::search_rank_parallel::{AUXILIARY_READERS_MAX, SplitPoints};
use audiolib_core::{Error, Result};
use rusqlite::{Connection, OptionalExtension, ffi, types::ValueRef};
use std::{
    collections::HashMap,
    ffi::{c_int, c_void},
    marker::PhantomData,
    panic::{AssertUnwindSafe, catch_unwind},
    ptr,
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

const COLUMNS: usize = 12;
const MAX_ROWS: usize = 2_097_152;
const MAX_SLOTS: usize = 16 * 1024 * 1024 / std::mem::size_of::<u32>();
const MIN_SPLIT_ROWS: usize = 65_536;
const MAX_NATIVE_LENGTH_MATCHES: u64 = 65_536;
const MISSING: u32 = u32::MAX;
const LENGTH_BLOCK_SLOTS: usize = 4096;

#[derive(Clone, Debug)]
struct LengthBlocks {
    len: usize,
    blocks: Vec<Arc<Vec<u32>>>,
}

impl LengthBlocks {
    fn new(len: usize) -> Option<Self> {
        let mut blocks = Vec::new();
        blocks
            .try_reserve_exact(len.div_ceil(LENGTH_BLOCK_SLOTS))
            .ok()?;
        let mut remaining = len;
        while remaining != 0 {
            let count = remaining.min(LENGTH_BLOCK_SLOTS);
            let mut block = Vec::new();
            block.try_reserve_exact(count).ok()?;
            block.resize(count, MISSING);
            blocks.push(Arc::new(block));
            remaining -= count;
        }
        Some(Self { len, blocks })
    }

    fn get(&self, offset: usize) -> Option<&u32> {
        if offset >= self.len {
            return None;
        }
        self.blocks
            .get(offset / LENGTH_BLOCK_SLOTS)?
            .get(offset % LENGTH_BLOCK_SLOTS)
    }

    fn set(&mut self, offset: usize, value: u32) -> Option<()> {
        if offset >= self.len {
            return None;
        }
        let block = self.blocks.get_mut(offset / LENGTH_BLOCK_SLOTS)?;
        if Arc::strong_count(block) != 1 || Arc::weak_count(block) != 0 {
            let mut owned = Vec::new();
            owned.try_reserve_exact(block.len()).ok()?;
            owned.extend_from_slice(block);
            *block = Arc::new(owned);
        }
        *Arc::get_mut(block)?.get_mut(offset % LENGTH_BLOCK_SLOTS)? = value;
        Some(())
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.len
    }
}

#[derive(Clone, Debug)]
struct Domain {
    first: i64,
    values: LengthBlocks,
}

/// Up to two dense row-ID domains, containing at most 16 MiB of owned lengths.
/// A missing slot always asks SQLite for its original xColumnSize(-1) result.
#[derive(Clone, Debug)]
pub(crate) struct Lengths {
    domains: [Option<Domain>; 2],
    split_points: Option<SplitPoints>,
}

impl Lengths {
    /// Actual ordered row-ID quantiles for splitting a sufficiently large pinned
    /// snapshot. These are boundaries, not assumptions about dense or positive IDs.
    pub(crate) fn split_points(&self) -> Option<SplitPoints> {
        self.split_points
    }

    fn get(&self, rowid: i64) -> Option<c_int> {
        for domain in self.domains.iter().flatten() {
            let offset = usize::try_from(i128::from(rowid) - i128::from(domain.first)).ok();
            if let Some(length) = offset.and_then(|offset| domain.values.get(offset))
                && *length != MISSING
            {
                return Some(*length as c_int);
            }
        }
        None
    }
}

fn span(first: i64, last: i64) -> Option<usize> {
    usize::try_from(i128::from(last) - i128::from(first) + 1).ok()
}

fn domain(first: i64, last: i64) -> Option<Domain> {
    let count = span(first, last)?;
    if count > MAX_SLOTS {
        return None;
    }
    let values = LengthBlocks::new(count)?;
    Some(Domain { first, values })
}

fn cancelled(cancel: Option<&AtomicBool>) -> Result<()> {
    if cancel.is_some_and(|flag| flag.load(Ordering::Acquire)) {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

// SQLite varints have seven useful bits in the first eight bytes and eight in
// the ninth. Reject malformed/oversized values rather than interpreting damaged
// docsize data differently from the native xColumnSize error/fallback path.
fn varint(blob: &[u8], offset: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for index in 0..9 {
        let byte = *blob.get(*offset)?;
        *offset += 1;
        if index == 8 {
            return Some((value << 8) | u64::from(byte));
        }
        value = (value << 7) | u64::from(byte & 0x7f);
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

fn decode_docsize(blob: &[u8]) -> Option<u32> {
    let mut offset = 0;
    let mut total = 0u32;
    for _ in 0..COLUMNS {
        let length = u32::try_from(varint(blob, &mut offset)?).ok()?;
        // Native fts5GetVarint32 masks unusually large values. Let the native
        // path handle such records, including sums that overflow its int output.
        if length > c_int::MAX as u32 {
            return None;
        }
        total = total.checked_add(length)?;
        if total > c_int::MAX as u32 {
            return None;
        }
    }
    (offset == blob.len()).then_some(total)
}

fn row_id_quantiles(staging: &[(i64, u32)]) -> Option<SplitPoints> {
    if staging.len() < MIN_SPLIT_ROWS {
        return None;
    }
    let mut points = [0; AUXILIARY_READERS_MAX];
    for (index, point) in points.iter_mut().enumerate() {
        let position = staging.len().checked_mul(index + 1)? / (AUXILIARY_READERS_MAX + 1);
        *point = staging.get(position)?.0;
    }
    Some(points)
}

/// Read lengths only inside the caller's pinned transaction. SQL/I/O errors and
/// cancellation propagate; unsuitable layouts use native BM25 instead. At most
/// 2M staging pairs (32 MiB) coexist with 16 MiB of final dense payload; no source
/// file is changed. The returned cache must never be reused for another snapshot.
pub(crate) fn load_lengths(
    conn: &Connection,
    cancel: Option<&AtomicBool>,
) -> Result<Option<Arc<Lengths>>> {
    cancelled(cancel)?;
    if conn.is_autocommit() {
        return Ok(None);
    }
    let mut statement = conn
        .prepare("SELECT id,sz FROM audio_search_docsize ORDER BY id")
        .map_err(db_error)?;
    let mut rows = statement.query([]).map_err(db_error)?;
    let mut staging: Vec<(i64, u32)> = Vec::new();
    let mut first = None;
    let mut previous = None;
    let mut largest_gap = 0i128;
    let mut split = None;
    while let Some(row) = rows.next().map_err(db_error)? {
        if staging.len().is_multiple_of(1024) {
            cancelled(cancel)?;
        }
        if staging.len() == MAX_ROWS {
            return Ok(None);
        }
        let id: i64 = row.get(0).map_err(db_error)?;
        if let Some(before) = previous {
            if id <= before {
                return Ok(None);
            }
            let gap = i128::from(id) - i128::from(before) - 1;
            if gap > largest_gap {
                largest_gap = gap;
                split = Some((before, id));
            }
        }
        first.get_or_insert(id);
        previous = Some(id);
        let length = match row.get_ref(1).map_err(db_error)? {
            ValueRef::Blob(blob) => decode_docsize(blob).unwrap_or(MISSING),
            _ => MISSING,
        };
        if staging.len() == staging.capacity()
            && staging
                .try_reserve_exact((MAX_ROWS - staging.len()).min(65_536))
                .is_err()
        {
            return Ok(None);
        }
        staging.push((id, length));
    }
    cancelled(cancel)?;
    let split_points = row_id_quantiles(&staging);
    let mut lengths = Lengths {
        domains: [None, None],
        split_points,
    };
    if let (Some(first), Some(last)) = (first, previous) {
        if span(first, last).is_some_and(|count| count <= MAX_SLOTS) {
            let Some(value) = domain(first, last) else {
                return Ok(None);
            };
            lengths.domains[0] = Some(value);
        } else {
            let Some((left_end, right_start)) = split else {
                return Ok(None);
            };
            let slots = span(first, left_end)
                .and_then(|left| span(right_start, last).and_then(|right| left.checked_add(right)));
            if slots.is_none_or(|count| count > MAX_SLOTS) {
                return Ok(None);
            }
            let (Some(left), Some(right)) = (domain(first, left_end), domain(right_start, last))
            else {
                return Ok(None);
            };
            lengths.domains = [Some(left), Some(right)];
        }
    }
    for (index, (id, length)) in staging.into_iter().enumerate() {
        if index % 1024 == 0 {
            cancelled(cancel)?;
        }
        let mut stored = false;
        for domain in lengths.domains.iter_mut().flatten() {
            let offset = usize::try_from(i128::from(id) - i128::from(domain.first)).ok();
            if offset
                .and_then(|offset| domain.values.set(offset, length))
                .is_some()
            {
                stored = true;
                break;
            }
        }
        if !stored {
            return Ok(None);
        }
    }
    cancelled(cancel)?;
    Ok(Some(Arc::new(lengths)))
}

/// Apply a complete journal delta inside its caller's pinned transaction. Only
/// changed document IDs are read; unchanged blocks remain shared with old readers.
/// A row outside the cached domains requires the normal full-layout fallback.
/// Missing/deleted/malformed sizes use native xColumnSize rather than a guessed
/// length. Membership changes retire the previous exact quantile cuts; normal
/// length-only changes preserve them without rescanning every document ID.
pub(crate) fn apply_length_delta(
    conn: &Connection,
    previous: &Arc<Lengths>,
    changed_ids: &[i64],
    cancel: Option<&AtomicBool>,
) -> Result<Option<Arc<Lengths>>> {
    cancelled(cancel)?;
    if conn.is_autocommit() || changed_ids.len() > MAX_ROWS {
        return Ok(None);
    }
    let mut next = (**previous).clone();
    let mut statement = conn
        .prepare_cached("SELECT sz FROM audio_search_docsize WHERE id=?1")
        .map_err(db_error)?;
    for (index, id) in changed_ids.iter().enumerate() {
        if index.is_multiple_of(1024) {
            cancelled(cancel)?;
        }
        let observed = statement
            .query_row([id], |row| {
                Ok(match row.get_ref(0)? {
                    ValueRef::Blob(blob) => decode_docsize(blob).unwrap_or(MISSING),
                    _ => MISSING,
                })
            })
            .optional()
            .map_err(db_error)?;
        let present = observed.is_some();
        let length = observed.unwrap_or(MISSING);
        if !present || previous.get(*id).is_none() {
            next.split_points = None;
        }
        let mut stored = false;
        for domain in next.domains.iter_mut().flatten() {
            let offset = usize::try_from(i128::from(*id) - i128::from(domain.first)).ok();
            if offset
                .and_then(|offset| domain.values.set(offset, length))
                .is_some()
            {
                stored = true;
                break;
            }
        }
        // A deleted ID outside all previous domains needs no new storage.
        // An inserted document outside those domains changes layout/quantiles.
        if !stored && present {
            return Ok(None);
        }
    }
    cancelled(cancel)?;
    Ok(Some(Arc::new(next)))
}

#[derive(Default)]
struct Slot {
    generation: u64,
    active: bool,
    lengths: Option<Arc<Lengths>>,
    prepared: Option<Arc<Prepared>>,
    preparation: Option<PrepareMode>,
}

#[derive(Debug)]
struct Layout {
    avgdl: f64,
    phrase_count: c_int,
    column_count: c_int,
    row_count: i64,
    phrase_sizes: Vec<usize>,
}

impl Layout {
    fn agrees(&self, other: &Self) -> bool {
        self.avgdl.to_bits() == other.avgdl.to_bits()
            && self.phrase_count == other.phrase_count
            && self.column_count == other.column_count
            && self.row_count == other.row_count
            && self.phrase_sizes == other.phrase_sizes
    }
}

struct Stats {
    layout: Arc<Layout>,
    idf: Vec<f64>,
    common_phrase: bool,
    exact_phrase_hit_sum: Option<u64>,
}

/// One reader's owned global phrase statistics. Only complete, disjoint partials
/// with the exact root layout may be published as a prepared ranking's stats.
#[derive(Debug)]
pub(crate) struct PartialStats {
    layout: Arc<Layout>,
    entries: Vec<(usize, f64)>,
    common_phrase: bool,
    // Sum of exact global DFs. None if any phrase stopped at the IDF clamp or
    // addition overflowed. Overlapping phrase hits deliberately count twice.
    exact_phrase_hit_sum: Option<u64>,
}

#[derive(Clone)]
enum PrepareMode {
    Layout,
    Phrases {
        indices: Arc<[usize]>,
        output: Arc<Mutex<Option<PartialStats>>>,
    },
}

/// One exact compiled MATCH query and its immutable statistics from a pinned
/// snapshot. All participating readers must pin that same snapshot before use
/// and bind MATCH only from query(); row-ID ranges do not change the global IDF.
pub(crate) struct Prepared {
    query: Arc<str>,
    lengths: Mutex<Option<Arc<Lengths>>>,
    layout: OnceLock<Arc<Layout>>,
    stats: OnceLock<Arc<Stats>>,
}

impl Prepared {
    /// Create a request-owned plan. It must never be reused across snapshots.
    #[cfg(test)]
    pub(crate) fn new(query: String, lengths: Arc<Lengths>) -> Arc<Self> {
        Arc::new(Self {
            query: query.into(),
            lengths: Mutex::new(Some(lengths)),
            layout: OnceLock::new(),
            stats: OnceLock::new(),
        })
    }

    /// Create a query whose layout, lengths and phrase statistics are prepared
    /// separately within readers proven to share the same pinned snapshot.
    pub(crate) fn empty(query: String) -> Arc<Self> {
        Arc::new(Self {
            query: query.into(),
            lengths: Mutex::new(None),
            layout: OnceLock::new(),
            stats: OnceLock::new(),
        })
    }

    /// Publish the root's optional snapshot lengths before scoring begins. An
    /// existing cursor keeps its own Arc; no scoring row takes this mutex.
    pub(crate) fn set_lengths(&self, lengths: Option<Arc<Lengths>>) -> Result<()> {
        *self
            .lengths
            .lock()
            .map_err(|_| Error::Database("Prepared length lock was poisoned".into()))? = lengths;
        Ok(())
    }

    /// Actual row-ID quantiles, when a suitable snapshot length cache is present.
    pub(crate) fn split_points(&self) -> Option<SplitPoints> {
        self.lengths.lock().ok()?.as_ref()?.split_points()
    }

    /// Whether the root has read the lightweight, document-size-free layout.
    pub(crate) fn has_layout(&self) -> bool {
        self.layout.get().is_some()
    }

    /// Whether the root layout is large enough to consider parallel preparation.
    pub(crate) fn is_large(&self) -> bool {
        self.layout
            .get()
            .is_some_and(|layout| layout.row_count >= 262_144)
    }

    /// Only positional multi-token phrases consider deferring a cold length
    /// scan. Two-letter fragment queries remain on their existing overlap path.
    pub(crate) fn has_complex_phrase(&self) -> bool {
        self.layout
            .get()
            .is_some_and(|layout| layout.phrase_sizes.iter().any(|size| *size > 1))
    }

    /// Every matching document contains at least one query phrase. The sum of
    /// complete global phrase DFs therefore bounds matches conservatively, even
    /// for overlapping OR alternatives or conjunctions. Never infer this bound
    /// from floating-point IDF or an early-stopped phrase count.
    pub(crate) fn sparse_native_lengths(&self) -> bool {
        self.stats.get().is_some_and(|stats| {
            let Ok(rows) = u64::try_from(stats.layout.row_count) else {
                return false;
            };
            !stats.common_phrase
                && stats.exact_phrase_hit_sum.is_some_and(|hits| {
                    hits > 0 && hits <= (rows / 8).min(MAX_NATIVE_LENGTH_MATCHES)
                })
        })
    }

    /// Disjoint groups balanced greedily by each phrase's token count.
    /// Their entries remain original phrase indices for exact-order merging.
    pub(crate) fn phrase_groups(&self) -> Vec<Vec<usize>> {
        let mut groups = vec![Vec::new(); AUXILIARY_READERS_MAX];
        let Some(layout) = self.layout.get() else {
            return groups;
        };
        let mut phrases = layout
            .phrase_sizes
            .iter()
            .copied()
            .enumerate()
            .collect::<Vec<_>>();
        phrases.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
        let mut costs = [0usize; AUXILIARY_READERS_MAX];
        for (phrase, cost) in phrases {
            let target = (0..AUXILIARY_READERS_MAX)
                .min_by_key(|index| (costs[*index], *index))
                .unwrap_or(0);
            groups[target].push(phrase);
            costs[target] = costs[target].saturating_add(cost);
        }
        groups
    }

    /// Merge every global phrase exactly once; reject missing, overlapping or
    /// mismatched partials before making any statistics visible to score cursors.
    pub(crate) fn complete_preparation(&self, partials: Vec<PartialStats>) -> Result<()> {
        let layout = self
            .layout
            .get()
            .ok_or_else(|| Error::Database("Prepared layout is missing".into()))?;
        let count = usize::try_from(layout.phrase_count)
            .map_err(|_| sqlite_error(ffi::SQLITE_CORRUPT_VTAB))?;
        let mut idf = zeroes(count).map_err(sqlite_error)?;
        let mut seen = Vec::new();
        seen.try_reserve_exact(count)
            .map_err(|_| sqlite_error(ffi::SQLITE_NOMEM))?;
        seen.resize(count, false);
        let mut common_phrase = false;
        let mut exact_phrase_hit_sum = Some(0u64);
        for partial in partials {
            if !layout.agrees(&partial.layout) {
                return Err(Error::Database("Prepared phrase layouts disagree".into()));
            }
            common_phrase |= partial.common_phrase;
            exact_phrase_hit_sum =
                add_exact_hits(exact_phrase_hit_sum, partial.exact_phrase_hit_sum);
            for (index, value) in partial.entries {
                let Some(visited) = seen.get_mut(index) else {
                    return Err(Error::Database(
                        "Prepared phrase index is out of range".into(),
                    ));
                };
                if *visited || !value.is_finite() || value <= 0.0 {
                    return Err(Error::Database(
                        "Prepared phrase is duplicated or invalid".into(),
                    ));
                }
                *visited = true;
                idf[index] = value;
            }
        }
        if seen.iter().any(|visited| !visited) {
            return Err(Error::Database(
                "Prepared phrase statistics are incomplete".into(),
            ));
        }
        self.stats
            .set(Arc::new(Stats {
                layout: layout.clone(),
                idf,
                common_phrase,
                exact_phrase_hit_sum,
            }))
            .map_err(|_| Error::Database("Prepared statistics were already published".into()))
    }

    /// The complete compiled MATCH expression to bind on every participating reader.
    pub(crate) fn query(&self) -> &str {
        &self.query
    }

    /// Whether a root cursor has successfully prepared the global BM25 statistics.
    #[cfg(test)]
    pub(crate) fn is_ready(&self) -> bool {
        self.stats.get().is_some()
    }

    #[cfg(test)]
    pub(crate) fn has_cached_lengths(&self) -> bool {
        self.lengths.lock().unwrap().is_some()
    }

    /// A scoring-only scheduling hint: a large snapshot and a globally common
    /// phrase may justify partitioning. This never changes matches or scores.
    pub(crate) fn parallel_worthy(&self) -> bool {
        self.stats
            .get()
            .is_some_and(|stats| stats.layout.row_count >= 262_144 && stats.common_phrase)
    }
}

#[derive(Default)]
struct Registration {
    slot: Mutex<Slot>,
    preparation_registered: AtomicBool,
}

type Registry = HashMap<usize, Weak<Registration>>;
static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();

/// Keep this guard until all FTS statements for the pinned snapshot are dropped.
/// Its connection borrow prevents closing/reusing the SQLite handle first; Drop
/// releases the idle reader's length reference. Overlapping registrations fail.
pub(crate) struct RegistrationGuard<'conn> {
    registration: Arc<Registration>,
    generation: u64,
    _connection: PhantomData<&'conn Connection>,
}

impl Drop for RegistrationGuard<'_> {
    fn drop(&mut self) {
        let mut slot = self
            .registration
            .slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot.generation == self.generation {
            slot.active = false;
            slot.lengths = None;
            slot.prepared = None;
            slot.preparation = None;
        }
    }
}

fn sqlite_error(code: c_int) -> Error {
    db_error(rusqlite::Error::SqliteFailure(ffi::Error::new(code), None))
}

struct Statement(*mut ffi::sqlite3_stmt);

impl Drop for Statement {
    fn drop(&mut self) {
        // SAFETY: This owns the prepared statement exclusively, and its creating
        // connection remains borrowed until the surrounding registration ends.
        unsafe { ffi::sqlite3_finalize(self.0) };
    }
}

fn fts5_api(conn: &Connection) -> Result<*mut ffi::fts5_api> {
    let mut statement = ptr::null_mut();
    let mut api: *mut ffi::fts5_api = ptr::null_mut();
    // SAFETY: The connection is exclusively used by its caller, SQL is a static
    // nul-terminated string, and both output pointers refer to live stack slots.
    let code = unsafe {
        ffi::sqlite3_prepare_v2(
            conn.handle(),
            c"SELECT fts5(?1)".as_ptr(),
            -1,
            &mut statement,
            ptr::null_mut(),
        )
    };
    if code != ffi::SQLITE_OK {
        return Err(sqlite_error(code));
    }
    let statement = Statement(statement);
    // SAFETY: fts5()'s documented pointer protocol writes one fts5_api pointer to
    // api. The bound address and static type name outlive stepping/finalization;
    // SQLite is not given ownership of this stack slot (no destructor).
    let code = unsafe {
        ffi::sqlite3_bind_pointer(
            statement.0,
            1,
            (&mut api as *mut *mut ffi::fts5_api).cast(),
            c"fts5_api_ptr".as_ptr(),
            None,
        )
    };
    if code != ffi::SQLITE_OK {
        return Err(sqlite_error(code));
    }
    // SAFETY: The statement and bound stack slot are still live here.
    let code = unsafe { ffi::sqlite3_step(statement.0) };
    if code != ffi::SQLITE_ROW {
        return Err(sqlite_error(code));
    }
    if api.is_null() {
        return Err(Error::Database("FTS5 extension API is unavailable".into()));
    }
    Ok(api)
}

/// Register the private rank function once per connection and lend it lengths
/// from this exact pinned snapshot. Call before preparing any FTS statement; the
/// guard must outlive those statements. No SQLite settings or schema are changed.
#[cfg(test)]
pub(crate) fn register(conn: &Connection, lengths: Arc<Lengths>) -> Result<RegistrationGuard<'_>> {
    register_snapshot(conn, Some(lengths), None, None)
}

/// Lend one prepared query to this connection. The caller must bind its exact
/// query() and pin the same database snapshot as the root that initialized it.
/// The returned guard must outlive every FTS statement using this registration.
pub(crate) fn register_prepared(
    conn: &Connection,
    prepared: Arc<Prepared>,
) -> Result<RegistrationGuard<'_>> {
    register_snapshot(conn, None, Some(prepared), None)
}

fn register_snapshot(
    conn: &Connection,
    lengths: Option<Arc<Lengths>>,
    prepared: Option<Arc<Prepared>>,
    preparation: Option<PrepareMode>,
) -> Result<RegistrationGuard<'_>> {
    // SAFETY: The borrowed connection remains live throughout this function and
    // the returned guard. Registry entries are weak and cannot retain a closed DB.
    let key = unsafe { conn.handle() } as usize;
    let mut registry = REGISTRY
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map_err(|_| Error::Database("BM25 registration registry was poisoned".into()))?;
    registry.retain(|_, registration| registration.strong_count() != 0);
    let registration = if let Some(registration) = registry.get(&key).and_then(Weak::upgrade) {
        registration
    } else {
        let api = fts5_api(conn)?;
        // SAFETY: fts5_api() obtained this connection-owned API pointer. The first
        // version's xCreateFunction field exists for all bundled FTS5 versions.
        let create = unsafe { (*api).xCreateFunction }
            .ok_or_else(|| Error::Database("FTS5 auxiliary registration is unavailable".into()))?;
        let registration = Arc::new(Registration::default());
        let user_data = Box::into_raw(Box::new(registration.clone()));
        // SAFETY: user_data owns a boxed Arc until xDestroy at connection close.
        // SQLite does not destroy it when xCreateFunction fails; reclaim below.
        // Registering once avoids SQLite's same-name auxiliary-node accumulation.
        let code = unsafe {
            create(
                api,
                c"audiolib_bm25".as_ptr(),
                user_data.cast(),
                Some(bm25),
                Some(drop_registration),
            )
        };
        if code != ffi::SQLITE_OK {
            // SAFETY: Failed xCreateFunction retained neither this box nor its
            // destructor, so ownership remains here exactly once.
            unsafe { drop(Box::from_raw(user_data)) };
            return Err(sqlite_error(code));
        }
        registry.insert(key, Arc::downgrade(&registration));
        registration
    };
    if preparation.is_some() && !registration.preparation_registered.load(Ordering::Acquire) {
        let api = fts5_api(conn)?;
        // SAFETY: This is the same live, connection-owned API used for the rank
        // registration. A separate Box owns this function's Arc until xDestroy.
        let create = unsafe { (*api).xCreateFunction }
            .ok_or_else(|| Error::Database("FTS5 auxiliary registration is unavailable".into()))?;
        let user_data = Box::into_raw(Box::new(registration.clone()));
        // SAFETY: Registry serialization prevents duplicate successful installs.
        // Failed xCreateFunction leaves this box with us, as for the rank function.
        let code = unsafe {
            create(
                api,
                c"audiolib_prepare_bm25".as_ptr(),
                user_data.cast(),
                Some(prepare_aux),
                Some(drop_registration),
            )
        };
        if code != ffi::SQLITE_OK {
            // SAFETY: SQLite retained neither the box nor its destructor on error.
            unsafe { drop(Box::from_raw(user_data)) };
            return Err(sqlite_error(code));
        }
        registration
            .preparation_registered
            .store(true, Ordering::Release);
    }
    drop(registry);
    let generation = {
        let mut slot = registration
            .slot
            .lock()
            .map_err(|_| Error::Database("BM25 registration slot was poisoned".into()))?;
        if slot.active {
            return Err(Error::Busy(
                "This reader already has an active BM25 snapshot".into(),
            ));
        }
        slot.generation = slot
            .generation
            .checked_add(1)
            .ok_or_else(|| Error::Database("BM25 registration generation is exhausted".into()))?;
        slot.active = true;
        slot.lengths = lengths;
        slot.prepared = prepared;
        slot.preparation = preparation;
        slot.generation
    };
    Ok(RegistrationGuard {
        registration,
        generation,
        _connection: PhantomData,
    })
}

/// Read the query's first matching context without document lengths or global
/// phrase enumeration. False means the complete query has no matching document.
pub(crate) fn prepare_layout(conn: &Connection, prepared: &Arc<Prepared>) -> Result<bool> {
    let _registration = register_snapshot(
        conn,
        None,
        Some(prepared.clone()),
        Some(PrepareMode::Layout),
    )?;
    conn.query_row(
        "SELECT audiolib_prepare_bm25(audio_search) FROM audio_search
         WHERE audio_search MATCH ?1 LIMIT 1",
        [prepared.query()],
        |row| row.get::<_, i64>(0),
    )
    .optional()
    .map(|row| row.is_some())
    .map_err(db_error)
}

/// Compute only the requested global phrase frequencies in this participant's
/// pinned snapshot. Empty groups still validate the complete root query layout.
pub(crate) fn prepare_phrases(
    conn: &Connection,
    prepared: &Arc<Prepared>,
    indices: &[usize],
) -> Result<PartialStats> {
    if !prepared.has_layout() {
        return Err(Error::Database("Prepared layout is missing".into()));
    }
    let output = Arc::new(Mutex::new(None));
    let mode = PrepareMode::Phrases {
        indices: indices.to_vec().into(),
        output: output.clone(),
    };
    let _registration = register_snapshot(conn, None, Some(prepared.clone()), Some(mode))?;
    let present = conn
        .query_row(
            "SELECT audiolib_prepare_bm25(audio_search) FROM audio_search
         WHERE audio_search MATCH ?1 LIMIT 1",
            [prepared.query()],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(db_error)?;
    if present.is_none() {
        return Err(Error::Database(
            "Prepared query has no matching row in this snapshot".into(),
        ));
    }
    let result = output
        .lock()
        .map_err(|_| Error::Database("Prepared result lock was poisoned".into()))?
        .take()
        .ok_or_else(|| Error::Database("Prepared phrase result is missing".into()))?;
    Ok(result)
}

unsafe extern "C" fn drop_registration(pointer: *mut c_void) {
    if !pointer.is_null() {
        let _ = catch_unwind(AssertUnwindSafe(|| {
            // SAFETY: xCreateFunction received this exact Box<Arc<Registration>>
            // and calls its destructor once at connection close.
            unsafe { drop(Box::from_raw(pointer.cast::<Arc<Registration>>())) };
        }));
    }
}

struct Bm25Data {
    stats: Arc<Stats>,
    frequency: Vec<f64>,
    lengths: Option<Arc<Lengths>>,
}

unsafe extern "C" fn drop_query_data(pointer: *mut c_void) {
    if !pointer.is_null() {
        let _ = catch_unwind(AssertUnwindSafe(|| {
            // SAFETY: This pointer was installed by query_data as one Box, and
            // xSetAuxdata destroys it once, including on registration failure.
            unsafe { drop(Box::from_raw(pointer.cast::<Bm25Data>())) };
        }));
    }
}

struct PhraseCount {
    hits: i64,
    clamp_at: i64,
}

fn add_exact_hits(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    left?.checked_add(right?)
}

unsafe extern "C" fn count_phrase(
    _api: *const ffi::Fts5ExtensionApi,
    _fts: *mut ffi::Fts5Context,
    pointer: *mut c_void,
) -> c_int {
    // SAFETY: xQueryPhrase invokes this synchronously with the stack counter below.
    // No allocation or panicking operation crosses this C callback boundary.
    let Some(count) = (unsafe { pointer.cast::<PhraseCount>().as_mut() }) else {
        return ffi::SQLITE_MISUSE;
    };
    count.hits = count.hits.wrapping_add(1);
    // At ceil(N/2) matching documents SQLite's IDF is necessarily <=0 and
    // therefore exactly 1e-6, independent of any remaining matches. FTS5 treats
    // SQLITE_DONE as a successful early end to this phrase-only enumeration.
    if count.hits >= count.clamp_at {
        ffi::SQLITE_DONE
    } else {
        ffi::SQLITE_OK
    }
}

fn zeroes(count: usize) -> std::result::Result<Vec<f64>, c_int> {
    let mut values = Vec::new();
    values
        .try_reserve_exact(count)
        .map_err(|_| ffi::SQLITE_NOMEM)?;
    values.resize(count, 0.0);
    Ok(values)
}

unsafe fn read_layout(
    api: &ffi::Fts5ExtensionApi,
    fts: *mut ffi::Fts5Context,
) -> std::result::Result<Arc<Layout>, c_int> {
    // SAFETY: SQLite supplies this live cursor. Checked function pointers write
    // only to live stack outputs; no document-size or phrase enumeration is used.
    unsafe {
        let phrase_count = api.xPhraseCount.ok_or(ffi::SQLITE_MISUSE)?(fts);
        let count = usize::try_from(phrase_count).map_err(|_| ffi::SQLITE_CORRUPT_VTAB)?;
        let column_count = api.xColumnCount.ok_or(ffi::SQLITE_MISUSE)?(fts);
        let mut row_count = 0;
        let code = api.xRowCount.ok_or(ffi::SQLITE_MISUSE)?(fts, &mut row_count);
        if code != ffi::SQLITE_OK {
            return Err(code);
        }
        if row_count <= 0 {
            return Err(ffi::SQLITE_CORRUPT_VTAB);
        }
        let mut token_count = 0;
        let code = api.xColumnTotalSize.ok_or(ffi::SQLITE_MISUSE)?(fts, -1, &mut token_count);
        if code != ffi::SQLITE_OK {
            return Err(code);
        }
        let mut phrase_sizes = Vec::new();
        phrase_sizes
            .try_reserve_exact(count)
            .map_err(|_| ffi::SQLITE_NOMEM)?;
        let phrase_size = api.xPhraseSize.ok_or(ffi::SQLITE_MISUSE)?;
        for phrase in 0..phrase_count {
            let size =
                usize::try_from(phrase_size(fts, phrase)).map_err(|_| ffi::SQLITE_CORRUPT_VTAB)?;
            if size == 0 {
                return Err(ffi::SQLITE_CORRUPT_VTAB);
            }
            phrase_sizes.push(size);
        }
        Ok(Arc::new(Layout {
            avgdl: token_count as f64 / row_count as f64,
            phrase_count,
            column_count,
            row_count,
            phrase_sizes,
        }))
    }
}

unsafe fn compute_partial(
    api: &ffi::Fts5ExtensionApi,
    fts: *mut ffi::Fts5Context,
    layout: Arc<Layout>,
    indices: &[usize],
) -> std::result::Result<PartialStats, c_int> {
    // SAFETY: The layout belongs to this exact cursor/query snapshot. Each phrase
    // index is checked before use; xQueryPhrase synchronously visits the live
    // stack counter, with no retained pointers or mutable state shared by readers.
    unsafe {
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(indices.len())
            .map_err(|_| ffi::SQLITE_NOMEM)?;
        let mut common_phrase = false;
        let mut exact_phrase_hit_sum = Some(0u64);
        let row_count = layout.row_count;
        for (position, index) in indices.iter().copied().enumerate() {
            if index >= layout.phrase_sizes.len() || indices[..position].contains(&index) {
                return Err(ffi::SQLITE_MISUSE);
            }
            let phrase = c_int::try_from(index).map_err(|_| ffi::SQLITE_MISUSE)?;
            let mut counter = PhraseCount {
                hits: 0,
                clamp_at: row_count / 2 + row_count % 2,
            };
            let code = api.xQueryPhrase.ok_or(ffi::SQLITE_MISUSE)?(
                fts,
                phrase,
                (&mut counter as *mut PhraseCount).cast(),
                Some(count_phrase),
            );
            if code != ffi::SQLITE_OK {
                return Err(code);
            }
            let clamped = counter.hits >= counter.clamp_at;
            common_phrase |= clamped;
            exact_phrase_hit_sum = add_exact_hits(
                exact_phrase_hit_sum,
                (!clamped)
                    .then(|| u64::try_from(counter.hits).ok())
                    .flatten(),
            );
            // Preserve SQLite's integer N-nHit subtraction before conversion to
            // double, and its exact phrase iteration/clamp order.
            let hits = counter.hits;
            let mut idf = ((row_count.wrapping_sub(hits) as f64 + 0.5) / (hits as f64 + 0.5)).ln();
            if idf <= 0.0 {
                idf = 1e-6;
            }
            entries.push((index, idf));
        }
        Ok(PartialStats {
            layout,
            entries,
            common_phrase,
            exact_phrase_hit_sum,
        })
    }
}

unsafe fn compute_stats(
    api: &ffi::Fts5ExtensionApi,
    fts: *mut ffi::Fts5Context,
    phrase_count: c_int,
    column_count: c_int,
) -> std::result::Result<Arc<Stats>, c_int> {
    // SAFETY: Both helpers use this live callback cursor and retain only owned
    // values; its checked shape is required before constructing every phrase.
    unsafe {
        let layout = read_layout(api, fts)?;
        if layout.phrase_count != phrase_count || layout.column_count != column_count {
            return Err(ffi::SQLITE_CORRUPT_VTAB);
        }
        let count = layout.phrase_sizes.len();
        let indices = (0..count).collect::<Vec<_>>();
        let partial = compute_partial(api, fts, layout.clone(), &indices)?;
        let mut idf = zeroes(count)?;
        for (index, value) in partial.entries {
            idf[index] = value;
        }
        Ok(Arc::new(Stats {
            layout,
            idf,
            common_phrase: partial.common_phrase,
            exact_phrase_hit_sum: partial.exact_phrase_hit_sum,
        }))
    }
}

unsafe fn query_data(
    api: &ffi::Fts5ExtensionApi,
    fts: *mut ffi::Fts5Context,
) -> std::result::Result<*mut Bm25Data, c_int> {
    // SAFETY: SQLite supplies a live FTS cursor and API for this callback. All
    // function pointers are checked, and this module alone owns its auxdata slot.
    // Each cursor owns its mutable frequencies; shared statistics are immutable.
    unsafe {
        let get_aux = api.xGetAuxdata.ok_or(ffi::SQLITE_MISUSE)?;
        let previous = get_aux(fts, 0).cast::<Bm25Data>();
        if !previous.is_null() {
            return Ok(previous);
        }
        let phrase_count = api.xPhraseCount.ok_or(ffi::SQLITE_MISUSE)?(fts);
        let count = usize::try_from(phrase_count).map_err(|_| ffi::SQLITE_CORRUPT_VTAB)?;
        let column_count = api.xColumnCount.ok_or(ffi::SQLITE_MISUSE)?(fts);
        let (stored_lengths, prepared) = if column_count == COLUMNS as c_int {
            let pointer = api.xUserData.ok_or(ffi::SQLITE_MISUSE)?(fts);
            let Some(registration) = pointer.cast::<Arc<Registration>>().as_ref() else {
                return Err(ffi::SQLITE_MISUSE);
            };
            let slot = registration.slot.lock().map_err(|_| ffi::SQLITE_ERROR)?;
            (slot.lengths.clone(), slot.prepared.clone())
        } else {
            (None, None)
        };
        let lengths = if let Some(prepared) = prepared.as_ref() {
            prepared
                .lengths
                .lock()
                .map_err(|_| ffi::SQLITE_ERROR)?
                .clone()
        } else {
            stored_lengths
        };
        let shared = prepared
            .as_ref()
            .and_then(|prepared| prepared.stats.get())
            .filter(|stats| {
                stats.layout.phrase_count == phrase_count
                    && stats.layout.column_count == column_count
            });
        let stats = if let Some(stats) = shared {
            stats.clone()
        } else {
            let stats = compute_stats(api, fts, phrase_count, column_count)?;
            if let Some(prepared) = prepared.as_ref() {
                // A root seed publishes only a complete successful result. A
                // different layout gets cursor-local native-equivalent stats;
                // OnceLock never replaces the prepared query's existing values.
                let _ = prepared.layout.set(stats.layout.clone());
                let _ = prepared.stats.set(stats.clone());
            }
            stats
        };
        let data = Box::new(Bm25Data {
            stats,
            frequency: zeroes(count)?,
            lengths,
        });
        let set_aux = api.xSetAuxdata.ok_or(ffi::SQLITE_MISUSE)?;
        let pointer = Box::into_raw(data);
        let code = set_aux(fts, pointer.cast(), Some(drop_query_data));
        if code != ffi::SQLITE_OK {
            // SQLite has already invoked drop_query_data on this failure path.
            // Do not free or dereference pointer a second time.
            return Err(code);
        }
        Ok(pointer)
    }
}

unsafe fn score(
    api: *const ffi::Fts5ExtensionApi,
    fts: *mut ffi::Fts5Context,
    values: c_int,
) -> std::result::Result<f64, c_int> {
    // SAFETY: The auxiliary callback receives live SQLite-owned pointers. Query
    // auxdata was installed by this module, and SQLite invokes one cursor at a
    // time; its frequencies may therefore be mutably reused between rows.
    unsafe {
        let api = api.as_ref().ok_or(ffi::SQLITE_MISUSE)?;
        if fts.is_null() || values != 0 {
            return Err(ffi::SQLITE_MISUSE);
        }
        let data = query_data(api, fts)?.as_mut().ok_or(ffi::SQLITE_MISUSE)?;
        let first = api.xPhraseFirst.ok_or(ffi::SQLITE_MISUSE)?;
        let next = api.xPhraseNext.ok_or(ffi::SQLITE_MISUSE)?;
        // These APIs expose the same instances as xInstCount/xInst without
        // merging every phrase's positions into SQLite's ordered instance array.
        // Default BM25 has unit column weights, so only each phrase's count is
        // needed. Keep one floating-point increment per instance as native does.
        for (phrase, frequency) in data.frequency.iter_mut().enumerate() {
            *frequency = 0.0;
            let phrase = c_int::try_from(phrase).map_err(|_| ffi::SQLITE_CORRUPT_VTAB)?;
            let mut iterator = ffi::Fts5PhraseIter {
                a: ptr::null(),
                b: ptr::null(),
            };
            let mut column = -1;
            let mut offset = -1;
            // SAFETY: The iterator and outputs live for this complete iteration.
            // Only SQLite reads or modifies its opaque iterator pointers; they
            // never escape the current cursor row or auxiliary invocation.
            let code = first(fts, phrase, &mut iterator, &mut column, &mut offset);
            if code != ffi::SQLITE_OK {
                return Err(code);
            }
            while column >= 0 {
                *frequency += 1.0;
                next(fts, &mut iterator, &mut column, &mut offset);
            }
        }
        let rowid = api.xRowid.ok_or(ffi::SQLITE_MISUSE)?(fts);
        let tokens =
            if let Some(tokens) = data.lengths.as_ref().and_then(|lengths| lengths.get(rowid)) {
                tokens
            } else {
                let mut tokens = 0;
                let code = api.xColumnSize.ok_or(ffi::SQLITE_MISUSE)?(fts, -1, &mut tokens);
                if code != ffi::SQLITE_OK {
                    return Err(code);
                }
                tokens
            };
        let document_length = tokens as f64;
        let mut score = 0.0;
        let k1 = 1.2;
        let b = 0.75;
        for (idf, frequency) in data.stats.idf.iter().zip(&data.frequency) {
            score += idf
                * ((frequency * (k1 + 1.0))
                    / (frequency + k1 * (1.0 - b + b * document_length / data.stats.layout.avgdl)));
        }
        Ok(-score)
    }
}

unsafe fn prepare_context(
    api: *const ffi::Fts5ExtensionApi,
    fts: *mut ffi::Fts5Context,
    values: c_int,
) -> std::result::Result<(), c_int> {
    // SAFETY: SQLite owns this live callback context. The registration mutex is
    // held only while cloning owned inputs, never while SQLite enumerates phrases.
    unsafe {
        let api = api.as_ref().ok_or(ffi::SQLITE_MISUSE)?;
        if fts.is_null() || values != 0 {
            return Err(ffi::SQLITE_MISUSE);
        }
        let pointer = api.xUserData.ok_or(ffi::SQLITE_MISUSE)?(fts);
        let registration = pointer
            .cast::<Arc<Registration>>()
            .as_ref()
            .ok_or(ffi::SQLITE_MISUSE)?;
        let (prepared, mode) = {
            let slot = registration.slot.lock().map_err(|_| ffi::SQLITE_ERROR)?;
            (
                slot.prepared.clone().ok_or(ffi::SQLITE_MISUSE)?,
                slot.preparation.clone().ok_or(ffi::SQLITE_MISUSE)?,
            )
        };
        let layout = read_layout(api, fts)?;
        if let Some(expected) = prepared.layout.get()
            && !expected.agrees(&layout)
        {
            return Err(ffi::SQLITE_CORRUPT_VTAB);
        }
        match mode {
            PrepareMode::Layout => {
                if let Err(layout) = prepared.layout.set(layout)
                    && !prepared
                        .layout
                        .get()
                        .is_some_and(|expected| expected.agrees(&layout))
                {
                    return Err(ffi::SQLITE_CORRUPT_VTAB);
                }
            }
            PrepareMode::Phrases { indices, output } => {
                let expected = prepared.layout.get().ok_or(ffi::SQLITE_MISUSE)?;
                if !expected.agrees(&layout) {
                    return Err(ffi::SQLITE_CORRUPT_VTAB);
                }
                let partial = compute_partial(api, fts, layout, &indices)?;
                let mut result = output.lock().map_err(|_| ffi::SQLITE_ERROR)?;
                if result.is_some() {
                    return Err(ffi::SQLITE_MISUSE);
                }
                *result = Some(partial);
            }
        }
        Ok(())
    }
}

unsafe extern "C" fn prepare_aux(
    api: *const ffi::Fts5ExtensionApi,
    fts: *mut ffi::Fts5Context,
    context: *mut ffi::sqlite3_context,
    values: c_int,
    _arguments: *mut *mut ffi::sqlite3_value,
) {
    if context.is_null() {
        return;
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: SQLite owns callback arguments until this invocation returns.
        unsafe { prepare_context(api, fts, values) }
    }));
    // SAFETY: The result context is live. Preparation is a private side channel;
    // it returns a constant marker, never a score, and cannot unwind through C.
    unsafe {
        match result {
            Ok(Ok(())) => ffi::sqlite3_result_int(context, 0),
            Ok(Err(code)) => ffi::sqlite3_result_error_code(context, code),
            Err(_) => ffi::sqlite3_result_error_code(context, ffi::SQLITE_ERROR),
        }
    }
}

unsafe extern "C" fn bm25(
    api: *const ffi::Fts5ExtensionApi,
    fts: *mut ffi::Fts5Context,
    context: *mut ffi::sqlite3_context,
    values: c_int,
    _arguments: *mut *mut ffi::sqlite3_value,
) {
    if context.is_null() {
        return;
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: SQLite owns these callback arguments for its full invocation.
        unsafe { score(api, fts, values) }
    }));
    // SAFETY: context is SQLite's live result context. No Rust panic may unwind
    // through its C ABI; native FTS error codes otherwise pass through unchanged.
    unsafe {
        match result {
            Ok(Ok(value)) => ffi::sqlite3_result_double(context, value),
            Ok(Err(code)) => ffi::sqlite3_result_error_code(context, code),
            Err(_) => ffi::sqlite3_result_error_code(context, ffi::SQLITE_ERROR),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    // SQLite's C compiler and Rust/LLVM may round the same BM25 expression
    // differently (for example, aarch64 may contract multiply/add operations).
    // Keep this oracle tolerance at two adjacent finite values, rather than a
    // relative epsilon that would hide larger errors near zero. Internal Rust
    // layout and snapshot identity checks remain exact.
    fn native_scores_agree(actual: f64, native: f64) -> bool {
        actual.is_finite()
            && native.is_finite()
            && (actual == native
                || (actual.is_sign_negative() == native.is_sign_negative()
                    && actual.to_bits().abs_diff(native.to_bits()) <= 2))
    }

    #[track_caller]
    pub(crate) fn assert_native_score(actual: f64, native: f64, context: impl std::fmt::Display) {
        assert!(
            native_scores_agree(actual, native),
            "{context}: Rust {actual:?} ({:#018x}), SQLite {native:?} ({:#018x})",
            actual.to_bits(),
            native.to_bits()
        );
    }

    #[track_caller]
    fn assert_native_ranking(actual: &[(i64, f64)], native: &[(i64, f64)], query: &str) {
        assert_eq!(actual.len(), native.len(), "{query}: match count");
        for ((actual_row, actual_score), (native_row, native_score)) in actual.iter().zip(native) {
            assert_eq!(actual_row, native_row, "{query}: ranking order");
            assert_native_score(
                *actual_score,
                *native_score,
                format!("{query}: {actual_row}"),
            );
        }
    }

    #[test]
    fn native_score_oracle_only_accepts_two_ulps_of_finite_rounding() {
        for base in [1.0f64, -1.0, 1e-6, -1e-6, f64::MIN_POSITIVE] {
            assert!(native_scores_agree(base, base));
            for offset in [1, 2] {
                for adjacent in [base.to_bits() - offset, base.to_bits() + offset] {
                    let adjacent = f64::from_bits(adjacent);
                    assert!(native_scores_agree(base, adjacent));
                    assert!(native_scores_agree(adjacent, base));
                }
            }
            for distant in [base.to_bits() - 3, base.to_bits() + 3] {
                assert!(!native_scores_agree(base, f64::from_bits(distant)));
            }
            assert!(!native_scores_agree(base, -base));
        }
        assert!(native_scores_agree(0.0, -0.0));
        assert!(native_scores_agree(0.0, f64::from_bits(2)));
        assert!(!native_scores_agree(0.0, f64::from_bits(3)));
        for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(!native_scores_agree(invalid, invalid));
            assert!(!native_scores_agree(invalid, 1.0));
            assert!(!native_scores_agree(1.0, invalid));
        }
    }

    fn fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE VIRTUAL TABLE audio_search USING fts5(a,b,c,d,e,f,g,h,i,j,k,l);
             INSERT INTO audio_search(rowid,a,b) VALUES(1,'alpha beta beta','gamma');
             INSERT INTO audio_search(rowid,a,b) VALUES(2,'alpha','beta gamma');
             INSERT INTO audio_search(rowid,a,b) VALUES(3,'delta','epsilon');
             INSERT INTO audio_search(rowid,a,b) VALUES(5,'','');
             INSERT INTO audio_search(rowid,a,b) VALUES(1800000000000000,'alpha beta','gamma');
             BEGIN; SELECT COUNT(*) FROM audio_search_docsize;",
        )
        .unwrap();
        conn
    }

    #[test]
    fn docsize_decoder_requires_exact_columns_and_bounded_totals() {
        assert_eq!(decode_docsize(&[0; COLUMNS]), Some(0));
        let mut blob = vec![0; COLUMNS - 1];
        blob.extend([0x81, 0x00]);
        assert_eq!(decode_docsize(&blob), Some(128));
        assert_eq!(decode_docsize(&blob[..blob.len() - 1]), None);
        blob.push(0);
        assert_eq!(decode_docsize(&blob), None);
        assert_eq!(decode_docsize(&[0x80; COLUMNS]), None);
        for (encoded, expected) in [
            (vec![0x7f], 127),
            (vec![0x81, 0x00], 128),
            (vec![0xff, 0x7f], 16_383),
            (vec![0x81, 0x80, 0x00], 16_384),
            (vec![0x87, 0xff, 0xff, 0xff, 0x7f], c_int::MAX as u32),
        ] {
            let mut blob = encoded;
            blob.resize(blob.len() + COLUMNS - 1, 0);
            assert_eq!(decode_docsize(&blob), Some(expected));
        }
        let mut oversized = vec![0x88, 0x80, 0x80, 0x80, 0x00];
        oversized.resize(oversized.len() + COLUMNS - 1, 0);
        assert_eq!(decode_docsize(&oversized), None);
        let mut offset = 0;
        assert_eq!(varint(&[0xff; 9], &mut offset), Some(u64::MAX));
        assert_eq!(offset, 9);
    }

    #[test]
    fn lengths_cover_two_dense_id_domains_and_keep_holes_distinct_from_zero() {
        let conn = fixture();
        let lengths = load_lengths(&conn, None).unwrap().unwrap();
        assert_eq!(lengths.get(1), Some(4));
        assert_eq!(lengths.get(2), Some(3));
        assert_eq!(lengths.get(4), None);
        assert_eq!(lengths.get(5), Some(0));
        assert_eq!(lengths.get(1800000000000000), Some(3));
        assert_eq!(lengths.split_points(), None);
        assert!(
            lengths
                .domains
                .iter()
                .flatten()
                .map(|domain| domain.values.len())
                .sum::<usize>()
                <= MAX_SLOTS
        );
        let cancel = AtomicBool::new(true);
        assert!(matches!(
            load_lengths(&conn, Some(&cancel)),
            Err(Error::Cancelled)
        ));
    }

    #[test]
    fn journal_lengths_copy_only_changed_blocks_and_preserve_old_snapshots() {
        let conn = fixture();
        let previous = load_lengths(&conn, None).unwrap().unwrap();
        conn.execute_batch(
            "UPDATE audio_search SET a='alpha alpha alpha' WHERE rowid=2;
            DELETE FROM audio_search WHERE rowid=1;
            INSERT INTO audio_search(rowid,a) VALUES(4,'zeta');",
        )
        .unwrap();
        let next = apply_length_delta(&conn, &previous, &[1, 2, 4, 2], None)
            .unwrap()
            .unwrap();
        let full = load_lengths(&conn, None).unwrap().unwrap();
        for id in [1, 2, 3, 4, 5, 1800000000000000] {
            assert_eq!(next.get(id), full.get(id), "{id}");
        }
        assert_eq!(previous.get(1), Some(4));
        assert_eq!(previous.get(2), Some(3));
        assert_eq!(previous.get(4), None);
        assert_eq!(next.get(2), Some(5));
        assert!(Arc::ptr_eq(
            &previous.domains[1].as_ref().unwrap().values.blocks[0],
            &next.domains[1].as_ref().unwrap().values.blocks[0]
        ));
        assert!(!Arc::ptr_eq(
            &previous.domains[0].as_ref().unwrap().values.blocks[0],
            &next.domains[0].as_ref().unwrap().values.blocks[0]
        ));
        assert!(Arc::ptr_eq(
            &next.domains[0].as_ref().unwrap().values.blocks[0],
            &apply_length_delta(&conn, &next, &[], None)
                .unwrap()
                .unwrap()
                .domains[0]
                .as_ref()
                .unwrap()
                .values
                .blocks[0]
        ));
        conn.execute(
            "INSERT INTO audio_search(rowid,a) VALUES(1000000000,'new domain')",
            [],
        )
        .unwrap();
        assert!(
            apply_length_delta(&conn, &next, &[1000000000], None)
                .unwrap()
                .is_none()
        );
        let cancelled = AtomicBool::new(true);
        assert!(matches!(
            apply_length_delta(&conn, &next, &[2], Some(&cancelled)),
            Err(Error::Cancelled)
        ));
    }

    #[test]
    fn length_blocks_share_untouched_pages_and_fallback_on_unsupported_transactions() {
        let mut original = LengthBlocks::new(LENGTH_BLOCK_SLOTS * 3).unwrap();
        original.set(0, 7).unwrap();
        let mut next = original.clone();
        next.set(LENGTH_BLOCK_SLOTS + 4, 9).unwrap();
        assert_eq!(original.get(LENGTH_BLOCK_SLOTS + 4), Some(&MISSING));
        assert_eq!(next.get(LENGTH_BLOCK_SLOTS + 4), Some(&9));
        assert!(Arc::ptr_eq(&original.blocks[0], &next.blocks[0]));
        assert!(!Arc::ptr_eq(&original.blocks[1], &next.blocks[1]));
        assert!(Arc::ptr_eq(&original.blocks[2], &next.blocks[2]));
        let conn = fixture();
        let lengths = load_lengths(&conn, None).unwrap().unwrap();
        conn.execute_batch("COMMIT").unwrap();
        assert!(
            apply_length_delta(&conn, &lengths, &[1], None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn quantiles_use_actual_sorted_signed_ids_without_dense_arithmetic() {
        assert_eq!(row_id_quantiles(&[]), None);
        let staging = (0..MIN_SPLIT_ROWS)
            .map(|index| {
                let id = if index < MIN_SPLIT_ROWS / 2 {
                    i64::MIN + index as i64
                } else {
                    i64::MAX - (MIN_SPLIT_ROWS - index - 1) as i64
                };
                (id, 0)
            })
            .collect::<Vec<_>>();
        assert_eq!(row_id_quantiles(&staging[..MIN_SPLIT_ROWS - 1]), None);
        assert_eq!(
            row_id_quantiles(&staging),
            Some([
                i64::MIN + 8_192,
                i64::MIN + 16_384,
                i64::MIN + 24_576,
                i64::MAX - 32_767,
                i64::MAX - 24_575,
                i64::MAX - 16_383,
                i64::MAX - 8_191,
            ])
        );
    }

    #[test]
    fn prepared_stats_are_shared_across_readers_with_cursor_local_layout_fallback() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("prepared-stats.db");
        let root = Connection::open(&database).unwrap();
        root.execute_batch(
            "CREATE VIRTUAL TABLE audio_search USING fts5(a,b,c,d,e,f,g,h,i,j,k,l);
             INSERT INTO audio_search(rowid,a,b) VALUES
               (1,'alpha beta beta','gamma'),(2,'alpha','beta gamma'),
               (3,'delta','epsilon'),(4,'beta alpha beta alpha','gamma alpha');
             CREATE VIRTUAL TABLE incompatible_search USING fts5(a,b,c,d,e,f,g,h,i,j,k,l,m);
             INSERT INTO incompatible_search(rowid,a,m) VALUES
               (1,'alpha alpha','beta'),(2,'alpha','gamma');
             BEGIN; SELECT COUNT(*) FROM audio_search_docsize;",
        )
        .unwrap();
        let lengths = load_lengths(&root, None).unwrap().unwrap();
        let prepared = Prepared::new("alpha OR beta".into(), lengths);
        assert!(!prepared.is_ready());
        assert!(!prepared.parallel_worthy());
        let root_guard = register_prepared(&root, prepared.clone()).unwrap();
        assert!(matches!(
            register_prepared(&root, prepared.clone()),
            Err(Error::Busy(_))
        ));
        let mut root_statement = root
            .prepare(
                "SELECT rowid,audiolib_bm25(audio_search),bm25(audio_search)
                 FROM audio_search WHERE audio_search MATCH ?1 ORDER BY rowid",
            )
            .unwrap();
        let mut root_rows = root_statement.query([prepared.query()]).unwrap();
        let first = root_rows.next().unwrap().unwrap();
        assert_native_score(
            first.get::<_, f64>(1).unwrap(),
            first.get::<_, f64>(2).unwrap(),
            "first root row",
        );
        assert!(prepared.is_ready());
        assert!(!prepared.parallel_worthy(), "small fixtures do not fan out");
        let shared_stats = prepared.stats.get().unwrap().clone();
        assert_eq!(shared_stats.layout.phrase_count, 2);
        assert_eq!(shared_stats.layout.column_count, COLUMNS as c_int);
        let retained = Arc::strong_count(&shared_stats);
        let worker =
            Connection::open_with_flags(&database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
        worker
            .execute_batch("BEGIN; SELECT COUNT(*) FROM audio_search_docsize;")
            .unwrap();
        let worker_guard = register_prepared(&worker, prepared.clone()).unwrap();
        {
            let mut statement = worker
                .prepare(
                    "SELECT rowid,audiolib_bm25(audio_search),bm25(audio_search)
                     FROM audio_search WHERE audio_search MATCH ?1 AND rowid>=2 ORDER BY rowid",
                )
                .unwrap();
            let mut rows = statement.query([prepared.query()]).unwrap();
            while let Some(row) = rows.next().unwrap() {
                assert_native_score(
                    row.get::<_, f64>(1).unwrap(),
                    row.get::<_, f64>(2).unwrap(),
                    format!("worker row {}", row.get::<_, i64>(0).unwrap()),
                );
                assert_eq!(Arc::strong_count(&shared_stats), retained + 1);
            }
        }
        assert_eq!(Arc::strong_count(&shared_stats), retained);
        // These deliberate caller-contract violations exercise conservative
        // shape checks: neither mismatch may replace the original plan's stats.
        for (table, query) in [
            ("audio_search", "alpha OR beta OR gamma"),
            ("incompatible_search", prepared.query()),
        ] {
            let mut statement = worker
                .prepare(&format!(
                    "SELECT audiolib_bm25({table}),bm25({table})
                     FROM {table} WHERE {table} MATCH ?1 ORDER BY rowid"
                ))
                .unwrap();
            let mut rows = statement.query([query]).unwrap();
            while let Some(row) = rows.next().unwrap() {
                assert_native_score(
                    row.get::<_, f64>(0).unwrap(),
                    row.get::<_, f64>(1).unwrap(),
                    format!("fallback for {table}: {query}"),
                );
                assert_eq!(Arc::strong_count(&shared_stats), retained);
            }
            assert!(Arc::ptr_eq(prepared.stats.get().unwrap(), &shared_stats));
        }
        drop(worker_guard);
        // Continue the root while the worker has visited rows with different
        // phrase frequencies; only immutable global statistics were shared.
        while let Some(row) = root_rows.next().unwrap() {
            assert_native_score(
                row.get::<_, f64>(1).unwrap(),
                row.get::<_, f64>(2).unwrap(),
                format!("root row {}", row.get::<_, i64>(0).unwrap()),
            );
        }
        drop(root_rows);
        drop(root_statement);
        drop(root_guard);
        assert_eq!(
            Arc::strong_count(&shared_stats),
            2,
            "only plan and oracle retain stats"
        );
    }

    #[test]
    fn separate_phrase_preparation_merges_native_scores_from_owned_reader_results() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("parallel-preparation.db");
        let root = Connection::open(&database).unwrap();
        root.execute_batch(
            "CREATE VIRTUAL TABLE audio_search USING fts5(a,b,c,d,e,f,g,h,i,j,k,l);
             INSERT INTO audio_search(rowid,a,b) VALUES
               (1,'alpha beta alpha beta','gamma gamma'),(2,'alpha','beta gamma'),
               (3,'delta','epsilon'),(4,'beta alpha beta alpha','gamma alpha'),
               (5,'delta','gamma beta gamma'),(6,'alpha beta','');
             BEGIN; SELECT COUNT(*) FROM audio_search_docsize;",
        )
        .unwrap();
        let plan = Prepared::empty("alpha OR \"alpha beta\" OR b:gamma OR \"beta gamma\"".into());
        assert!(!plan.has_layout());
        assert!(!plan.is_large());
        assert!(prepare_layout(&root, &plan).unwrap());
        assert!(plan.has_layout());
        assert!(!plan.is_ready());
        let groups = plan.phrase_groups();
        assert_eq!(groups.len(), AUXILIARY_READERS_MAX);
        assert_eq!(groups.iter().filter(|group| group.is_empty()).count(), 3);
        let mut coverage = groups.iter().flatten().copied().collect::<Vec<_>>();
        coverage.sort_unstable();
        assert_eq!(coverage, vec![0, 1, 2, 3]);
        let mut workers = Vec::new();
        for group in groups {
            let path = database.clone();
            let plan = plan.clone();
            workers.push(std::thread::spawn(move || {
                let conn =
                    Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                        .unwrap();
                conn.execute_batch("BEGIN; SELECT COUNT(*) FROM audio_search_docsize;")
                    .unwrap();
                prepare_phrases(&conn, &plan, &group).unwrap()
            }));
        }
        plan.set_lengths(load_lengths(&root, None).unwrap())
            .unwrap();
        let mut partials = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();
        partials.reverse();
        plan.complete_preparation(partials).unwrap();
        assert!(plan.is_ready());
        let _registration = register_prepared(&root, plan.clone()).unwrap();
        let mut statement = root
            .prepare(
                "SELECT rowid,audiolib_bm25(audio_search),bm25(audio_search)
             FROM audio_search WHERE audio_search MATCH ?1 ORDER BY rowid",
            )
            .unwrap();
        let mut rows = statement.query([plan.query()]).unwrap();
        let mut matched = 0;
        while let Some(row) = rows.next().unwrap() {
            assert_native_score(
                row.get::<_, f64>(1).unwrap(),
                row.get::<_, f64>(2).unwrap(),
                format!("row {}", row.get::<_, i64>(0).unwrap()),
            );
            matched += 1;
        }
        assert!(matched > 0);
        let ranking = |function: &str| {
            root.prepare(
                "SELECT rowid,rank FROM audio_search WHERE audio_search MATCH ?1
                AND rank MATCH ?2 ORDER BY rank,rowid",
            )
            .unwrap()
            .query_map(rusqlite::params![plan.query(), function], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
        };
        assert_native_ranking(
            &ranking("audiolib_bm25()"),
            &ranking("bm25()"),
            plan.query(),
        );
    }

    #[test]
    fn incomplete_duplicate_or_mismatched_phrase_results_never_publish_stats() {
        let conn = fixture();
        let plan = Prepared::empty("alpha OR beta OR gamma".into());
        assert!(prepare_phrases(&conn, &plan, &[0]).is_err());
        assert!(prepare_layout(&conn, &plan).unwrap());
        assert!(plan.complete_preparation(Vec::new()).is_err());
        assert!(!plan.is_ready());
        assert!(prepare_phrases(&conn, &plan, &[0, 0]).is_err());
        assert!(prepare_phrases(&conn, &plan, &[usize::MAX]).is_err());
        let partial = prepare_phrases(&conn, &plan, &[0]).unwrap();
        assert!(plan.complete_preparation(vec![partial]).is_err());
        assert!(!plan.is_ready());
        let duplicate = vec![
            prepare_phrases(&conn, &plan, &[0]).unwrap(),
            prepare_phrases(&conn, &plan, &[0, 1, 2]).unwrap(),
        ];
        assert!(plan.complete_preparation(duplicate).is_err());
        assert!(!plan.is_ready());
        let other = Prepared::empty("alpha OR beta".into());
        assert!(prepare_layout(&conn, &other).unwrap());
        let wrong_layout = prepare_phrases(&conn, &other, &[0, 1]).unwrap();
        assert!(plan.complete_preparation(vec![wrong_layout]).is_err());
        assert!(!plan.is_ready());
        let complete = vec![
            prepare_phrases(&conn, &plan, &[2, 0]).unwrap(),
            prepare_phrases(&conn, &plan, &[1]).unwrap(),
            prepare_phrases(&conn, &plan, &[]).unwrap(),
        ];
        plan.complete_preparation(complete).unwrap();
        assert!(plan.is_ready());
        let published = plan.stats.get().unwrap().clone();
        assert!(plan.complete_preparation(Vec::new()).is_err());
        assert!(Arc::ptr_eq(plan.stats.get().unwrap(), &published));
        let absent = Prepared::empty("nonexistent".into());
        assert!(!prepare_layout(&conn, &absent).unwrap());
        assert!(!absent.has_layout());
        assert!(!absent.is_ready());
    }

    #[test]
    fn layout_and_phrase_preparation_do_not_read_document_lengths() {
        let conn = fixture();
        conn.execute("UPDATE audio_search_docsize SET sz=x'80'", [])
            .unwrap();
        let plan = Prepared::empty("alpha OR beta".into());
        assert!(prepare_layout(&conn, &plan).unwrap());
        let partials = plan
            .phrase_groups()
            .iter()
            .map(|group| prepare_phrases(&conn, &plan, group).unwrap())
            .collect::<Vec<_>>();
        plan.complete_preparation(partials).unwrap();
        assert!(plan.is_ready());
        assert!(plan.lengths.lock().unwrap().is_none());
        plan.set_lengths(None).unwrap();
        assert_eq!(plan.split_points(), None);
    }

    #[test]
    fn sparse_length_hint_requires_exact_bounded_global_counts() {
        let hint = |rows, hits, common_phrase| {
            let plan = Prepared::empty("\"rare token\"".into());
            let layout = Arc::new(Layout {
                avgdl: 2.0,
                phrase_count: 1,
                column_count: COLUMNS as c_int,
                row_count: rows,
                phrase_sizes: vec![2],
            });
            assert!(
                plan.stats
                    .set(Arc::new(Stats {
                        layout,
                        idf: vec![1.0],
                        common_phrase,
                        exact_phrase_hit_sum: hits,
                    }))
                    .is_ok()
            );
            plan.sparse_native_lengths()
        };
        assert!(hint(512, Some(64), false));
        assert!(!hint(512, Some(65), false));
        assert!(hint(1_000_000, Some(65_536), false));
        assert!(!hint(1_000_000, Some(65_537), false));
        assert!(!hint(512, None, false));
        assert!(!hint(512, Some(1), true));
        assert!(!hint(512, Some(0), false));
        assert!(!hint(-1, Some(1), false));
        assert_eq!(add_exact_hits(Some(u64::MAX), Some(1)), None);
        assert_eq!(add_exact_hits(Some(1), None), None);
        assert_eq!(add_exact_hits(None, Some(1)), None);

        let conn = fixture();
        let plan = Prepared::empty("alpha".into());
        assert!(prepare_layout(&conn, &plan).unwrap());
        let partials = plan
            .phrase_groups()
            .iter()
            .map(|group| prepare_phrases(&conn, &plan, group).unwrap())
            .collect();
        plan.complete_preparation(partials).unwrap();
        assert!(plan.stats.get().unwrap().common_phrase);
        assert_eq!(plan.stats.get().unwrap().exact_phrase_hit_sum, None);
        assert!(!plan.sparse_native_lengths());
    }

    #[test]
    fn prepared_native_lengths_preserve_cached_score_bits_and_sqlite_oracle() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE VIRTUAL TABLE audio_search USING fts5(a,b,c,d,e,f,g,h,i,j,k,l);",
        )
        .unwrap();
        for id in 1..=128 {
            let text = if id % 32 == 0 {
                "rare token ".repeat(id / 32)
            } else {
                "ordinary filler".into()
            };
            conn.execute(
                "INSERT INTO audio_search(rowid,a,b) VALUES(?1,?2,?3)",
                rusqlite::params![id as i64, text, "other column ".repeat(id % 7)],
            )
            .unwrap();
        }
        conn.execute_batch("BEGIN; SELECT COUNT(*) FROM audio_search_docsize;")
            .unwrap();
        let plan = Prepared::empty("\"rare token\" OR a:rare".into());
        assert!(prepare_layout(&conn, &plan).unwrap());
        assert!(plan.has_complex_phrase());
        let partials = plan
            .phrase_groups()
            .iter()
            .map(|group| prepare_phrases(&conn, &plan, group).unwrap())
            .collect();
        plan.complete_preparation(partials).unwrap();
        assert_eq!(plan.stats.get().unwrap().exact_phrase_hit_sum, Some(8));
        assert!(plan.sparse_native_lengths());
        let collect = |function: &str| {
            conn.prepare(
                "SELECT rowid,rank FROM audio_search WHERE audio_search MATCH ?1
                 AND rank MATCH ?2 ORDER BY rank,rowid",
            )
            .unwrap()
            .query_map(rusqlite::params![plan.query(), function], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
        };
        plan.set_lengths(load_lengths(&conn, None).unwrap())
            .unwrap();
        let cached = {
            let _registration = register_prepared(&conn, plan.clone()).unwrap();
            collect("audiolib_bm25()")
        };
        plan.set_lengths(None).unwrap();
        let per_match = {
            let _registration = register_prepared(&conn, plan.clone()).unwrap();
            collect("audiolib_bm25()")
        };
        assert_eq!(cached.len(), 4);
        assert_eq!(
            cached
                .iter()
                .map(|(id, score)| (*id, score.to_bits()))
                .collect::<Vec<_>>(),
            per_match
                .iter()
                .map(|(id, score)| (*id, score.to_bits()))
                .collect::<Vec<_>>()
        );
        assert_native_ranking(&per_match, &collect("bm25()"), plan.query());
    }

    #[test]
    fn private_default_bm25_matches_native_scores_and_guard_drop_restores_fallback() {
        let conn = fixture();
        let lengths = load_lengths(&conn, None).unwrap().unwrap();
        let guard = register(&conn, lengths.clone()).unwrap();
        assert!(matches!(register(&conn, lengths), Err(Error::Busy(_))));
        for query in [
            "alpha",
            "beta",
            "delta",
            "epsilon",
            "alpha OR beta",
            "alpha AND beta",
            "\"beta gamma\"",
            "\"alpha beta\" OR \"beta gamma\"",
            "a:alpha",
            "{a b} : (alpha OR gamma)",
        ] {
            let mut statement = conn
                .prepare(
                    "SELECT rowid,bm25(audio_search),audiolib_bm25(audio_search)
                 FROM audio_search WHERE audio_search MATCH ?1 ORDER BY rowid",
                )
                .unwrap();
            let scores = statement
                .query_map([query], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, f64>(1)?,
                        row.get::<_, f64>(2)?,
                    ))
                })
                .unwrap();
            for scores in scores {
                let (row, native, cached) = scores.unwrap();
                assert_native_score(cached, native, format!("{query}: {row}"));
            }
        }
        drop(guard);
        let (native, fallback): (f64, f64) = conn
            .query_row(
                "SELECT bm25(audio_search),audiolib_bm25(audio_search)
             FROM audio_search WHERE audio_search MATCH 'alpha' LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_native_score(fallback, native, "guard drop fallback");
    }

    #[test]
    fn phrase_iteration_matches_native_for_interleaved_overlapping_and_repeated_instances() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE VIRTUAL TABLE audio_search USING fts5(a,b,c,d,e,f,g,h,i,j,k,l);",
        )
        .unwrap();
        for (id, a, b, l) in [
            (
                1,
                "alpha beta gamma ".repeat(256),
                "beta alpha beta gamma ".repeat(64),
                "alpha beta gamma",
            ),
            (
                2,
                "beta gamma alpha beta".into(),
                "alpha gamma beta alpha".into(),
                "gamma alpha",
            ),
            (
                3,
                "alpha alpha alpha beta".into(),
                "beta beta alpha".into(),
                "alpha beta",
            ),
            (4, "delta epsilon".into(), "gamma".into(), ""),
            (5, "gamma".into(), "".into(), "alpha"),
        ] {
            conn.execute(
                "INSERT INTO audio_search(rowid,a,b,l) VALUES(?1,?2,?3,?4)",
                rusqlite::params![id, a, b, l],
            )
            .unwrap();
        }
        conn.execute_batch("BEGIN; SELECT COUNT(*) FROM audio_search_docsize;")
            .unwrap();
        let lengths = load_lengths(&conn, None).unwrap().unwrap();
        let _guard = register(&conn, lengths).unwrap();
        for query in [
            "alpha OR beta OR gamma",
            "\"alpha beta\" OR \"beta gamma\" OR \"gamma alpha\"",
            "alpha OR \"alpha beta\"",
            "alpha OR alpha OR \"alpha beta\"",
            "alpha AND alpha AND beta",
            "a : (alpha OR beta)",
            "{a b} : (alpha OR \"alpha beta\")",
            "b : \"beta alpha\" OR a : \"alpha beta\"",
            "l : (alpha OR \"alpha beta\")",
            "NEAR(alpha beta, 3)",
            "delta OR gamma",
        ] {
            let mut statement = conn
                .prepare(
                    // Evaluate the private function first, before native BM25
                    // has populated this cursor's combined instance array.
                    "SELECT rowid,audiolib_bm25(audio_search),bm25(audio_search)
                     FROM audio_search WHERE audio_search MATCH ?1 ORDER BY rowid",
                )
                .unwrap();
            let scores = statement
                .query_map([query], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, f64>(1)?,
                        row.get::<_, f64>(2)?,
                    ))
                })
                .unwrap();
            let mut matched = 0;
            for scores in scores {
                let (row, private, native) = scores.unwrap();
                assert_native_score(private, native, format!("{query}: {row}"));
                matched += 1;
            }
            assert!(matched > 0, "oracle query must exercise instances: {query}");
            let ranking = |function: &str| {
                let mut statement = conn
                    .prepare(
                        "SELECT rowid,rank FROM audio_search
                         WHERE audio_search MATCH ?1 AND rank MATCH ?2 ORDER BY rank,rowid",
                    )
                    .unwrap();
                statement
                    .query_map(rusqlite::params![query, function], |row| {
                        Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?))
                    })
                    .unwrap()
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .unwrap()
            };
            assert_native_ranking(&ranking("audiolib_bm25()"), &ranking("bm25()"), query);
        }
    }

    #[test]
    fn idf_clamp_boundaries_match_native_scores_for_even_and_odd_row_counts() {
        for total_rows in [1i64, 6i64, 7i64] {
            let conn = Connection::open_in_memory().unwrap();
            conn.execute_batch(
                "CREATE VIRTUAL TABLE audio_search USING fts5(a,b,c,d,e,f,g,h,i,j,k,l);",
            )
            .unwrap();
            let ceiling_half = (total_rows + 1) / 2;
            let phrases = [
                ("below pair", (total_rows - 1) / 2),
                ("edge pair", ceiling_half),
                ("above pair", (ceiling_half + 1).min(total_rows)),
            ];
            for index in 0..total_rows {
                let mut text = vec!["filler"];
                for (phrase, hits) in phrases {
                    if index < hits {
                        text.push(phrase);
                        if index % 2 == 0 {
                            text.push(phrase);
                        }
                    }
                }
                conn.execute(
                    "INSERT INTO audio_search(rowid,a) VALUES(?1,?2)",
                    rusqlite::params![index + 1, text.join(" ")],
                )
                .unwrap();
            }
            conn.execute_batch("BEGIN; SELECT COUNT(*) FROM audio_search_docsize;")
                .unwrap();
            for (phrase, expected_hits) in phrases {
                let actual_hits: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM audio_search WHERE audio_search MATCH ?1",
                        [format!("\"{phrase}\"")],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(actual_hits, expected_hits, "N={total_rows}: {phrase}");
            }
            let lengths = load_lengths(&conn, None).unwrap().unwrap();
            let _guard = register(&conn, lengths).unwrap();
            for query in [
                "\"below pair\"",
                "\"edge pair\"",
                "\"above pair\"",
                "\"below pair\" OR \"edge pair\"",
                "\"edge pair\" OR \"above pair\"",
                "\"below pair\" AND \"edge pair\"",
                "\"below pair\" OR (\"edge pair\" AND \"above pair\")",
            ] {
                let expected_matches: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM audio_search WHERE audio_search MATCH ?1",
                        [query],
                        |row| row.get(0),
                    )
                    .unwrap();
                let mut statement = conn
                    .prepare(
                        "SELECT rowid,bm25(audio_search),audiolib_bm25(audio_search)
                         FROM audio_search WHERE audio_search MATCH ?1 ORDER BY rowid",
                    )
                    .unwrap();
                let scores = statement
                    .query_map([query], |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, f64>(1)?,
                            row.get::<_, f64>(2)?,
                        ))
                    })
                    .unwrap();
                let mut matched = 0i64;
                for scores in scores {
                    let (row, native, cached) = scores.unwrap();
                    assert_native_score(
                        cached,
                        native,
                        format!("N={total_rows}, row={row}, query={query}"),
                    );
                    matched += 1;
                }
                assert_eq!(matched, expected_matches, "N={total_rows}: {query}");
                if matched == 0 {
                    assert_eq!(total_rows, 1, "only the single-row absent phrase may miss");
                }
            }
        }
    }

    #[test]
    fn repeated_busy_registration_keeps_the_active_snapshot_and_generation() {
        let conn = fixture();
        let lengths = load_lengths(&conn, None).unwrap().unwrap();
        let guard = register(&conn, lengths.clone()).unwrap();
        let generation = guard.generation;
        let registration = guard.registration.clone();
        for _ in 0..4 {
            let rejected = Arc::new(Lengths {
                domains: [None, None],
                split_points: None,
            });
            let rejected_reference = Arc::downgrade(&rejected);
            assert!(matches!(register(&conn, rejected), Err(Error::Busy(_))));
            assert!(
                rejected_reference.upgrade().is_none(),
                "failed registration releases its proposed snapshot"
            );
            let slot = registration.slot.lock().unwrap();
            assert!(slot.active);
            assert_eq!(slot.generation, generation);
            assert!(Arc::ptr_eq(slot.lengths.as_ref().unwrap(), &lengths));
        }
        let (private, native): (f64, f64) = conn
            .query_row(
                "SELECT audiolib_bm25(audio_search),bm25(audio_search)
                 FROM audio_search WHERE audio_search MATCH 'alpha' LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_native_score(private, native, "active registration after Busy");
        drop(guard);
        let replacement = register(&conn, lengths).unwrap();
        assert!(Arc::ptr_eq(&replacement.registration, &registration));
        assert_eq!(replacement.generation, generation + 1);
    }

    #[test]
    fn closed_connection_stale_handle_entries_allow_fresh_registration() {
        let conn = fixture();
        let plan = Prepared::empty("alpha OR beta".into());
        assert!(prepare_layout(&conn, &plan).unwrap());
        let guard = register_prepared(&conn, plan).unwrap();
        assert!(
            guard
                .registration
                .preparation_registered
                .load(Ordering::Acquire)
        );
        let expired_registration = Arc::downgrade(&guard.registration);
        drop(guard);
        conn.close().unwrap();
        assert!(
            expired_registration.upgrade().is_none(),
            "closing SQLite destroys both rank and preparation function payloads"
        );

        let conn = fixture();
        // A SQLite allocator need not recycle the same address on every platform.
        // Reproduce its observable registry state deterministically: a new live
        // handle address has the expired Weak entry from a closed connection.
        // SAFETY: This connection is live and remains owned by this test.
        let key = unsafe { conn.handle() } as usize;
        REGISTRY
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .insert(key, expired_registration.clone());
        let plan = Prepared::empty("alpha OR beta".into());
        assert!(prepare_layout(&conn, &plan).unwrap());
        let partials = plan
            .phrase_groups()
            .iter()
            .map(|indices| prepare_phrases(&conn, &plan, indices).unwrap())
            .collect();
        plan.complete_preparation(partials).unwrap();
        let guard = register_prepared(&conn, plan.clone()).unwrap();
        let fresh_registration = Arc::downgrade(&guard.registration);
        assert!(!Weak::ptr_eq(&fresh_registration, &expired_registration));
        let (private, native): (f64, f64) = conn
            .query_row(
                "SELECT audiolib_bm25(audio_search),bm25(audio_search)
                 FROM audio_search WHERE audio_search MATCH ?1 LIMIT 1",
                [plan.query()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_native_score(private, native, "registration after handle reuse");
        drop(guard);
        conn.close().unwrap();
        assert!(
            fresh_registration.upgrade().is_none(),
            "the replacement connection also destroys both FFI payloads"
        );
    }

    #[test]
    fn query_finalization_and_registration_drop_release_each_snapshot() {
        let conn = fixture();
        let mut same_registration = None;
        for _ in 0..3 {
            let lengths = load_lengths(&conn, None).unwrap().unwrap();
            let retained = Arc::downgrade(&lengths);
            let guard = register(&conn, lengths).unwrap();
            if let Some(previous) = &same_registration {
                assert!(Arc::ptr_eq(previous, &guard.registration));
            }
            same_registration = Some(guard.registration.clone());
            assert_eq!(retained.strong_count(), 1);
            {
                let mut statement = conn
                    .prepare(
                        "SELECT audiolib_bm25(audio_search) FROM audio_search
                     WHERE audio_search MATCH 'alpha'",
                    )
                    .unwrap();
                let mut rows = statement.query([]).unwrap();
                assert!(rows.next().unwrap().is_some());
                assert_eq!(
                    retained.strong_count(),
                    2,
                    "query auxdata owns this snapshot"
                );
            }
            assert_eq!(
                retained.strong_count(),
                1,
                "finalization releases query auxdata"
            );
            drop(guard);
            assert!(
                retained.upgrade().is_none(),
                "idle readers retain no length payload"
            );
        }
        let registration = Arc::downgrade(same_registration.as_ref().unwrap());
        drop(same_registration);
        drop(conn);
        assert!(
            registration.upgrade().is_none(),
            "connection close destroys its registration"
        );
    }

    #[test]
    fn rank_mapping_and_extreme_ids_keep_native_order_and_fallback() {
        let conn = fixture();
        conn.execute(
            "INSERT INTO audio_search(rowid,a) VALUES(?1,'alpha')",
            [i64::MIN],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO audio_search(rowid,a) VALUES(?1,'alpha')",
            [i64::MAX],
        )
        .unwrap();
        assert!(
            load_lengths(&conn, None).unwrap().is_none(),
            "three sparse domains exceed the payload budget"
        );
        let _guard = register(
            &conn,
            Arc::new(Lengths {
                domains: [None, None],
                split_points: None,
            }),
        )
        .unwrap();
        let ranks =
            |rank: &str| {
                let mut statement = conn.prepare(
                "SELECT rowid,rank FROM audio_search WHERE audio_search MATCH 'alpha OR delta'
                 AND rank MATCH ?1 ORDER BY rank,rowid",
            ).unwrap();
                statement
                    .query_map([rank], |row| {
                        Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?))
                    })
                    .unwrap()
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .unwrap()
            };
        assert_native_ranking(
            &ranks("audiolib_bm25()"),
            &ranks("bm25()"),
            "alpha OR delta",
        );
    }

    #[test]
    fn damaged_docsize_uses_native_error_instead_of_a_cached_zero() {
        let conn = fixture();
        conn.execute("UPDATE audio_search_docsize SET sz=x'80' WHERE id=1", [])
            .unwrap();
        let lengths = load_lengths(&conn, None).unwrap().unwrap();
        assert_eq!(lengths.get(1), None);
        let _guard = register(&conn, lengths).unwrap();
        let error = conn
            .query_row(
                "SELECT audiolib_bm25(audio_search) FROM audio_search
             WHERE audio_search MATCH 'alpha' ORDER BY rowid LIMIT 1",
                [],
                |row| row.get::<_, f64>(0),
            )
            .unwrap_err();
        assert!(matches!(error, rusqlite::Error::SqliteFailure(code, _)
            if code.extended_code == ffi::SQLITE_CORRUPT_VTAB));
    }
}
