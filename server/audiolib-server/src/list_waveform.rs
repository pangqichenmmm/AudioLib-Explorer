// server/audiolib-server/src/list_waveform.rs
//! List thumbnail waveforms (doc/api/README.md §9.1–§9.2): `POST /waveforms`, the
//! in-memory on-demand queue, optional pre-generation of the whole library and the
//! sharded on-disk cache under `<waveform_cache_dir>/list/<library hash>/`.
//!
//! Each record keeps at most one 128-column mono min/max summary (the union of every
//! channel's envelope, int16) keyed by the same hash as
//! `GET /audio/{id}/waveform?buckets=128&colors=0`. Requests never decode audio: a miss
//! (or a stale entry) is answered `pending` and queued for the background workers, which
//! take their decode slots from the shared `max_waveforms` pool and yield to scans and
//! audio streams (§9.2).
use crate::{
    ApiError, AppState,
    auth::Source,
    state::StatFailure,
    waveform::{WaveformKey, WaveformParams},
    wf_job::{
        self, CurrentItem, JobMode, JobRecord, JobState, JobView, LastError, RateWindow, StateFile,
    },
};
use audiolib_audio::{WAVEFORM_CACHE_VERSION, WaveformPeak};
use audiolib_core::{Error, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

/// Columns of a list waveform (the only accepted `buckets`).
pub const LIST_BUCKETS: u32 = 128;
/// Most ids in one `POST /waveforms`.
pub const MAX_LIST_IDS: usize = 200;
/// List encoding version (`-l1` in the entity tag, `list_waveforms.version`).
pub const LIST_VERSION: u32 = 1;
/// `list_waveforms.encoding` of `/capabilities`.
pub const LIST_ENCODING: &str = "i16le-minmax-base64";
/// Every `encoding` of `POST /waveforms` (§9.4).
pub const LIST_ENCODINGS: [&str; 2] = [LIST_ENCODING, crate::wfdb::I8_ENCODING];

/// `encoding` of a `POST /waveforms` request (§9.4).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ListEncoding {
    /// `i16le-minmax-base64` (default, `-l1`).
    #[default]
    I16,
    /// `i8-minmax-base64` (`-l1i8`).
    I8,
}

impl ListEncoding {
    /// Parse a wire name; unknown names are 400 `VALIDATION` / `UNSUPPORTED_ENCODING`.
    pub fn parse(name: &str) -> std::result::Result<Self, ApiError> {
        match name {
            LIST_ENCODING => Ok(Self::I16),
            crate::wfdb::I8_ENCODING => Ok(Self::I8),
            _ => Err(ApiError::unsupported_encoding(format!(
                "unsupported list waveform encoding {name:?}"
            ))),
        }
    }
    /// Wire name.
    pub fn name(self) -> &'static str {
        match self {
            Self::I16 => LIST_ENCODING,
            Self::I8 => crate::wfdb::I8_ENCODING,
        }
    }
    /// Entity tag of a list entry in this encoding.
    pub fn etag(self, hash: u64) -> String {
        match self {
            Self::I16 => list_etag(hash),
            Self::I8 => list_etag_i8(hash),
        }
    }
    /// `peaks` value of interleaved int16 min/max values.
    pub fn encode(self, values: &[i16]) -> String {
        match self {
            Self::I16 => encode_peaks(values),
            Self::I8 => base64(&crate::wfdb::to_i8_bytes(values)),
        }
    }
}
/// Concurrent `POST /waveforms` requests; more get 503 `BUSY` (internal constant).
pub const MAX_BATCH_REQUESTS: usize = 4;
/// Records per cache shard (`id / 1024`).
const SHARD_IDS: i64 = 1024;
/// A shard with this many unsaved updates is written at once.
const FLUSH_ENTRIES: usize = 64;
/// Unsaved updates are written after at most this long.
const FLUSH_AFTER: Duration = Duration::from_secs(2);
/// Upper bound of shards kept in memory (bytes of peaks and record headers).
const SHARD_MEMORY_BYTES: u64 = 64 * 1024 * 1024;
/// Longest on-demand queue; the oldest entries are dropped beyond it.
const ON_DEMAND_CAPACITY: usize = 10_000;
/// Parallel metadata reads (`stat`) of one request.
const STAT_PARALLEL: usize = 8;
/// Metadata budget of one request; unfinished items answer `pending`.
const STAT_BUDGET: Duration = Duration::from_secs(2);
/// The on-demand queue takes no new file this long after a stream/preview starts.
const PREBUFFER_YIELD: Duration = Duration::from_secs(3);
/// Pre-generation resumes this long after the last yield condition ended.
const RESUME_DELAY: Duration = Duration::from_secs(5);
/// Worker idle poll interval.
const IDLE_TICK: Duration = Duration::from_millis(100);
/// Wait between attempts to take a `max_waveforms` slot.
const SLOT_POLL: Duration = Duration::from_millis(20);
/// Longest suggested `retry_after` of `POST /waveforms` (seconds); also the value when
/// the metadata budget ran out.
const RETRY_AFTER_MAX: u32 = 5;
/// On-demand files finished per second and worker assumed before any was measured.
const DEFAULT_ON_DEMAND_RATE: f64 = 8.0;
/// Weight of the newest sample in the on-demand throughput average.
const THROUGHPUT_ALPHA: f64 = 0.2;
/// Most `POST /waveforms` metadata reads outstanding at once, including the ones that
/// continue in the background after a request's budget ran out; ids beyond it are left
/// to the on-demand workers.
const MAX_OUTSTANDING_STATS: usize = 2 * MAX_LIST_IDS;
/// A metadata read made for a `pending` item is reused (instead of a new `stat`) by the
/// workers and later requests this long.
const STAT_HINT_TTL: Duration = Duration::from_secs(30);
/// Records read from the library per pre-generation batch.
const PASS_BATCH: i64 = 256;
/// Shard file magic and format version.
const SHARD_MAGIC: &[u8; 4] = b"ALWL";
const SHARD_FORMAT: u32 = 1;
const SHARD_HEADER: usize = 16;
/// Per-record header in a shard: id, key hash, n, reserved.
const RECORD_HEADER: usize = 24;
/// Unfinished metadata reads allowed per root before items answer `pending` at once
/// (same bound as unfinished stream opens, per `max_streams + max_previews` slot).
const UNFINISHED_STATS_PER_SLOT: usize = 4;

/// The `GET /audio/{id}/waveform` parameters whose cache key list entries share.
pub fn list_params() -> WaveformParams {
    WaveformParams {
        buckets: LIST_BUCKETS,
        colors: false,
        start_frame: None,
        end_frame: None,
    }
}

/// Entity tag of a list entry: the §9 key hash plus the list encoding version.
pub fn list_etag(hash: u64) -> String {
    format!(
        "W/\"wf{WAVEFORM_CACHE_VERSION}-k{}-{hash:016x}-l{LIST_VERSION}\"",
        crate::waveform::SERVER_CACHE_VERSION
    )
}

/// Entity tag of an 8-bit list entry (`-l1i8`, §9.4).
pub fn list_etag_i8(hash: u64) -> String {
    let etag = list_etag(hash);
    format!("{}i8\"", etag.trim_end_matches('"'))
}

/// Stable directory name of a library under `list/`.
pub fn library_dir_name(library_id: &str) -> String {
    format!("{:016x}", crate::waveform::fnv1a64(library_id.as_bytes()))
}

/// Quantize one peak value: clamp to -1..1, then `round(v * 32767)`.
pub fn quantize(value: f32) -> i16 {
    if value.is_nan() {
        return 0;
    }
    (value.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

/// Union of every channel's envelope: column `i` is `[min_c(min), max_c(max)]`,
/// quantized; at most [`LIST_BUCKETS`] columns (`n` of them, `2 n` values).
pub fn merge_channels(peaks: &[Vec<WaveformPeak>]) -> Vec<i16> {
    let columns = peaks
        .iter()
        .map(Vec::len)
        .max()
        .unwrap_or(0)
        .min(LIST_BUCKETS as usize);
    let mut values = Vec::with_capacity(columns * 2);
    for column in 0..columns {
        let mut low = f32::INFINITY;
        let mut high = f32::NEG_INFINITY;
        for channel in peaks {
            if let Some(peak) = channel.get(column) {
                low = low.min(peak.min);
                high = high.max(peak.max);
            }
        }
        if !low.is_finite() {
            low = 0.0;
        }
        if !high.is_finite() {
            high = 0.0;
        }
        values.push(quantize(low));
        values.push(quantize(high));
    }
    values
}

/// Read buffer for list waveform computation: whole-file sequential passes, so large
/// reads keep network shares streaming instead of issuing one small request per block.
pub const LIST_WAVEFORM_READ_BUFFER: usize = 2 * 1024 * 1024;

/// Blocking. Compute the list summary of a whole file (§9.1: `colors=0`,
/// `samples_per_pixel = ceil(frames / 128)`); a 0-frame file has no columns.
pub fn compute(path: &Path, cancel: &AtomicBool) -> Result<Vec<i16>> {
    // One open and header parse; the trailing metadata chunks are never fetched.
    let waveform = audiolib_audio::summary_waveform_with_read_buffer(
        path,
        LIST_BUCKETS,
        cancel,
        LIST_WAVEFORM_READ_BUFFER,
    )?;
    if waveform.frames == 0 {
        return Ok(Vec::new());
    }
    Ok(merge_channels(&waveform.peaks))
}

/// The previous two-pass implementation (`probe`, then a full waveform); kept for the
/// equivalence test.
#[cfg(test)]
fn compute_probed(path: &Path, cancel: &AtomicBool) -> Result<Vec<i16>> {
    let metadata = audiolib_audio::probe(path)?;
    if metadata.frames == 0 {
        return Ok(Vec::new());
    }
    let samples_per_pixel = u32::try_from(metadata.frames.div_ceil(u64::from(LIST_BUCKETS)))
        .unwrap_or(u32::MAX)
        .max(1);
    let waveform = audiolib_audio::waveform_with_read_buffer(
        path,
        audiolib_audio::WaveformRequest {
            samples_per_pixel,
            start_frame: 0,
            end_frame: None,
            include_colors: false,
        },
        cancel,
        LIST_WAVEFORM_READ_BUFFER,
    )?;
    Ok(merge_channels(&waveform.peaks))
}

/// `peaks` wire value: interleaved int16 little endian, standard base64 with padding.
pub fn encode_peaks(values: &[i16]) -> String {
    let bytes: Vec<u8> = values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    base64(&bytes)
}

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// RFC 4648 base64 with `=` padding.
pub fn base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let triple = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (index, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if index <= chunk.len() {
                out.push(BASE64_ALPHABET[((triple >> shift) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------------------
// Sharded cache

#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    hash: u64,
    /// `2 n` values: min₀, max₀, min₁, max₁, …
    values: Box<[i16]>,
}

impl Entry {
    fn bytes(&self) -> u64 {
        (RECORD_HEADER + self.values.len() * 2) as u64
    }
}

#[derive(Default)]
struct Shard {
    entries: BTreeMap<i64, Entry>,
    dirty: usize,
    dirty_since: Option<Instant>,
    last_used: u64,
}

impl Shard {
    fn memory(&self) -> u64 {
        SHARD_HEADER as u64 + self.entries.values().map(Entry::bytes).sum::<u64>()
    }
}

#[derive(Clone, Copy, Debug)]
struct DiskShard {
    bytes: u64,
    entries: u64,
    last_access: u64,
}

#[derive(Default)]
struct CacheState {
    loaded: HashMap<i64, Shard>,
    disk: HashMap<i64, DiskShard>,
    clock: u64,
    over_budget: bool,
}

impl CacheState {
    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }
}

/// The on-disk list waveform cache of one library: one file per 1024 ids, replaced
/// atomically, limited to `list_waveform_cache_mb` by evicting whole shards.
pub struct ListCache {
    dir: PathBuf,
    limit_bytes: u64,
    state: Mutex<CacheState>,
}

fn shard_of(id: i64) -> i64 {
    id.div_euclid(SHARD_IDS)
}

fn shard_file_name(shard: i64) -> String {
    format!("shard-{shard}.bin")
}

fn parse_shard_name(name: &str) -> Option<i64> {
    name.strip_prefix("shard-")?
        .strip_suffix(".bin")?
        .parse()
        .ok()
}

fn encode_shard(entries: &BTreeMap<i64, Entry>) -> Vec<u8> {
    let size = SHARD_HEADER + entries.values().map(|e| e.bytes() as usize).sum::<usize>();
    let mut bytes = Vec::with_capacity(size);
    bytes.extend_from_slice(SHARD_MAGIC);
    bytes.extend_from_slice(&SHARD_FORMAT.to_le_bytes());
    bytes.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    for (id, entry) in entries {
        bytes.extend_from_slice(&id.to_le_bytes());
        bytes.extend_from_slice(&entry.hash.to_le_bytes());
        bytes.extend_from_slice(&((entry.values.len() / 2) as u32).to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        for value in entry.values.iter() {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
    }
    bytes
}

fn decode_shard(bytes: &[u8]) -> Option<BTreeMap<i64, Entry>> {
    let u32_at = |at: usize| -> Option<u32> {
        Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
    };
    if bytes.get(..4)? != SHARD_MAGIC || u32_at(4)? != SHARD_FORMAT {
        return None;
    }
    let count = u32_at(8)? as usize;
    let mut entries = BTreeMap::new();
    let mut at = SHARD_HEADER;
    for _ in 0..count {
        let id = i64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?);
        let hash = u64::from_le_bytes(bytes.get(at + 8..at + 16)?.try_into().ok()?);
        let columns = u32_at(at + 16)? as usize;
        if columns > LIST_BUCKETS as usize {
            return None;
        }
        at += RECORD_HEADER;
        let data = bytes.get(at..at + columns * 4)?;
        let values = data
            .chunks_exact(2)
            .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        at += columns * 4;
        entries.insert(id, Entry { hash, values });
    }
    (at == bytes.len()).then_some(entries)
}

impl ListCache {
    /// Cache in `dir` (created on the first write) limited to `limit_mb` MiB. Existing
    /// shards are indexed (only their headers are read).
    pub fn open(dir: PathBuf, limit_mb: u64) -> Self {
        let cache = Self {
            dir,
            limit_bytes: limit_mb.saturating_mul(1024 * 1024),
            state: Mutex::new(CacheState::default()),
        };
        cache.index();
        cache
    }

    fn lock(&self) -> MutexGuard<'_, CacheState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn index(&self) {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        let mut state = self.lock();
        let mut found: Vec<(SystemTime, i64, DiskShard)> = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(".list-") {
                // A temporary file left by a crash; never a valid shard.
                let _ = fs::remove_file(entry.path());
                continue;
            }
            let Some(shard) = parse_shard_name(&name) else {
                continue;
            };
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let count = fs::File::open(entry.path()).ok().and_then(|mut file| {
                let mut header = [0_u8; SHARD_HEADER];
                std::io::Read::read_exact(&mut file, &mut header).ok()?;
                (&header[..4] == SHARD_MAGIC).then(|| {
                    u64::from(u32::from_le_bytes(
                        header[8..12].try_into().unwrap_or([0; 4]),
                    ))
                })
            });
            let Some(count) = count else {
                let _ = fs::remove_file(entry.path());
                continue;
            };
            found.push((
                metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                shard,
                DiskShard {
                    bytes: metadata.len(),
                    entries: count,
                    last_access: 0,
                },
            ));
        }
        // Older files count as less recently used.
        found.sort_by_key(|(modified, shard, _)| (*modified, *shard));
        for (_, shard, mut disk) in found {
            disk.last_access = state.tick();
            state.disk.insert(shard, disk);
        }
    }

    /// Cache directory (`…/list/<library hash>`).
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Bytes of all shard files.
    pub fn bytes(&self) -> u64 {
        self.lock().disk.values().map(|disk| disk.bytes).sum()
    }

    /// Size limit in bytes.
    pub fn limit_bytes(&self) -> u64 {
        self.limit_bytes
    }

    /// Whether shards had to be evicted to stay within the limit.
    pub fn over_budget(&self) -> bool {
        self.lock().over_budget
    }

    /// Entries in the cache (saved shards plus unsaved updates).
    pub fn entries(&self) -> u64 {
        let state = self.lock();
        let mut total = 0;
        for (shard, disk) in &state.disk {
            if !state.loaded.contains_key(shard) {
                total += disk.entries;
            }
        }
        total
            + state
                .loaded
                .values()
                .map(|s| s.entries.len() as u64)
                .sum::<u64>()
    }

    /// Make shard `shard` resident (reading its file), marking it recently used.
    fn load<'a>(&self, state: &'a mut CacheState, shard: i64) -> &'a mut Shard {
        let now = state.tick();
        if let Some(disk) = state.disk.get_mut(&shard) {
            disk.last_access = now;
        }
        if !state.loaded.contains_key(&shard) {
            let entries = if state.disk.contains_key(&shard) {
                let path = self.dir.join(shard_file_name(shard));
                match fs::read(&path).ok().as_deref().and_then(decode_shard) {
                    Some(entries) => entries,
                    None => {
                        tracing::warn!(shard, "Discarding an unreadable list waveform shard");
                        let _ = fs::remove_file(&path);
                        state.disk.remove(&shard);
                        BTreeMap::new()
                    }
                }
            } else {
                BTreeMap::new()
            };
            state.loaded.insert(
                shard,
                Shard {
                    entries,
                    ..Shard::default()
                },
            );
            self.trim_memory(state, shard);
        }
        let shard = state
            .loaded
            .get_mut(&shard)
            .expect("the shard was just loaded");
        shard.last_used = now;
        shard
    }

    /// Drop clean resident shards (least recently used first) beyond the memory bound.
    fn trim_memory(&self, state: &mut CacheState, keep: i64) {
        let mut memory: u64 = state.loaded.values().map(Shard::memory).sum();
        while memory > SHARD_MEMORY_BYTES {
            let victim = state
                .loaded
                .iter()
                .filter(|(shard, loaded)| **shard != keep && loaded.dirty == 0)
                .min_by_key(|(_, loaded)| loaded.last_used)
                .map(|(shard, _)| *shard);
            let Some(victim) = victim else {
                break;
            };
            if let Some(removed) = state.loaded.remove(&victim) {
                memory = memory.saturating_sub(removed.memory());
            }
        }
    }

    /// Blocking. The cached entry of `id`: its key hash and `2 n` values.
    pub fn get(&self, id: i64) -> Option<(u64, Vec<i16>)> {
        let mut state = self.lock();
        let shard = shard_of(id);
        if !state.disk.contains_key(&shard) && !state.loaded.contains_key(&shard) {
            return None;
        }
        self.load(&mut state, shard)
            .entries
            .get(&id)
            .map(|entry| (entry.hash, entry.values.to_vec()))
    }

    /// Blocking. Key hash of the cached entry of `id`, if any.
    pub fn hash_of(&self, id: i64) -> Option<u64> {
        self.get(id).map(|(hash, _)| hash)
    }

    /// Blocking. Store the entry of `id` (replacing an older version). Updates are
    /// merged in memory and written once 64 are pending or after 2 s ([`Self::flush_due`]).
    pub fn put(&self, id: i64, hash: u64, values: Vec<i16>) {
        let mut state = self.lock();
        let shard_id = shard_of(id);
        let shard = self.load(&mut state, shard_id);
        shard.entries.insert(
            id,
            Entry {
                hash,
                values: values.into_boxed_slice(),
            },
        );
        shard.dirty += 1;
        shard.dirty_since.get_or_insert_with(Instant::now);
        if shard.dirty >= FLUSH_ENTRIES {
            self.write_shard(&mut state, shard_id);
        }
    }

    /// Blocking. Write shards whose oldest unsaved update is older than 2 s.
    pub fn flush_due(&self) {
        let mut state = self.lock();
        let due: Vec<i64> = state
            .loaded
            .iter()
            .filter(|(_, shard)| {
                shard
                    .dirty_since
                    .is_some_and(|since| since.elapsed() >= FLUSH_AFTER)
            })
            .map(|(id, _)| *id)
            .collect();
        for shard in due {
            self.write_shard(&mut state, shard);
        }
    }

    /// Blocking. Write every shard with unsaved updates.
    pub fn flush_all(&self) {
        let mut state = self.lock();
        let dirty: Vec<i64> = state
            .loaded
            .iter()
            .filter(|(_, shard)| shard.dirty > 0)
            .map(|(id, _)| *id)
            .collect();
        for shard in dirty {
            self.write_shard(&mut state, shard);
        }
    }

    /// Write one resident shard to a temporary file in the same directory and rename it
    /// over the old file (an empty shard removes the file), then enforce the limit.
    fn write_shard(&self, state: &mut CacheState, shard_id: i64) {
        let Some(shard) = state.loaded.get_mut(&shard_id) else {
            return;
        };
        let path = self.dir.join(shard_file_name(shard_id));
        if shard.entries.is_empty() {
            shard.dirty = 0;
            shard.dirty_since = None;
            let _ = fs::remove_file(&path);
            state.disk.remove(&shard_id);
            return;
        }
        let bytes = encode_shard(&shard.entries);
        let count = shard.entries.len() as u64;
        let written = (|| -> std::io::Result<()> {
            fs::create_dir_all(&self.dir)?;
            let mut stage = tempfile::Builder::new()
                .prefix(".list-")
                .tempfile_in(&self.dir)?;
            stage.write_all(&bytes)?;
            stage.persist(&path).map_err(|error| error.error)?;
            Ok(())
        })();
        match written {
            Ok(()) => {
                shard.dirty = 0;
                shard.dirty_since = None;
                let now = state.tick();
                state.disk.insert(
                    shard_id,
                    DiskShard {
                        bytes: bytes.len() as u64,
                        entries: count,
                        last_access: now,
                    },
                );
                self.enforce_limit(state, shard_id);
            }
            Err(error) => {
                // Keep the updates in memory; the next flush retries (a crash only
                // costs recomputation).
                shard.dirty_since = Some(Instant::now());
                tracing::warn!(%error, shard = shard_id, "Cannot write a list waveform shard");
            }
        }
    }

    /// Evict whole shards (least recently accessed first, never `keep`) beyond the limit.
    fn enforce_limit(&self, state: &mut CacheState, keep: i64) {
        let mut total: u64 = state.disk.values().map(|disk| disk.bytes).sum();
        if total <= self.limit_bytes {
            return;
        }
        let mut evicted = 0_usize;
        while total > self.limit_bytes {
            let victim = state
                .disk
                .iter()
                .filter(|(shard, _)| **shard != keep)
                .min_by_key(|(_, disk)| disk.last_access)
                .map(|(shard, _)| *shard);
            let Some(victim) = victim else {
                break;
            };
            let _ = fs::remove_file(self.dir.join(shard_file_name(victim)));
            if let Some(disk) = state.disk.remove(&victim) {
                total = total.saturating_sub(disk.bytes);
            }
            state.loaded.remove(&victim);
            evicted += 1;
        }
        if evicted > 0 {
            state.over_budget = true;
            tracing::warn!(
                evicted,
                bytes = total,
                limit = self.limit_bytes,
                "The list waveform cache exceeds list_waveform_cache_mb; evicted shards"
            );
        }
    }

    /// Blocking. Remove every entry (all shard files and unsaved updates) for a
    /// `rebuild` job; the directory itself stays (§9.5.1).
    pub fn clear(&self) {
        let mut state = self.lock();
        let shards: Vec<i64> = state
            .disk
            .keys()
            .chain(state.loaded.keys())
            .copied()
            .collect();
        for shard in shards {
            let _ = fs::remove_file(self.dir.join(shard_file_name(shard)));
        }
        state.loaded.clear();
        state.disk.clear();
    }

    /// Every shard that exists on disk or in memory.
    fn shards(&self) -> Vec<i64> {
        let state = self.lock();
        let mut shards: Vec<i64> = state
            .disk
            .keys()
            .chain(state.loaded.keys())
            .copied()
            .collect();
        shards.sort_unstable();
        shards.dedup();
        shards
    }

    /// Blocking. Remove every entry of `shard` whose id `keep` rejects; rewrites the
    /// shard (atomically) only when something was removed. Returns the removed ids.
    fn retain_shard(&self, shard_id: i64, keep: impl Fn(i64) -> bool) -> Vec<i64> {
        let mut state = self.lock();
        let shard = self.load(&mut state, shard_id);
        let removed: Vec<i64> = shard
            .entries
            .keys()
            .copied()
            .filter(|id| !keep(*id))
            .collect();
        if removed.is_empty() {
            return removed;
        }
        for id in &removed {
            shard.entries.remove(id);
        }
        shard.dirty += 1;
        self.write_shard(&mut state, shard_id);
        removed
    }

    /// Ids of `shard` currently cached.
    fn shard_ids(&self, shard_id: i64) -> Vec<i64> {
        let mut state = self.lock();
        self.load(&mut state, shard_id)
            .entries
            .keys()
            .copied()
            .collect()
    }
}

/// Blocking. Remove `list/<other library>` directories next to `current` (the library
/// was replaced; §9.1). Only directories named like a library hash are touched.
pub fn remove_other_libraries(list_dir: &Path, current: &str) {
    let Ok(entries) = fs::read_dir(list_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_hash = name.len() == 16 && name.bytes().all(|b| b.is_ascii_hexdigit());
        if is_hash && name != current && entry.path().is_dir() {
            match fs::remove_dir_all(entry.path()) {
                Ok(()) => {
                    tracing::info!(dir = %name, "Removed the list waveforms of a replaced library")
                }
                Err(error) => {
                    tracing::warn!(%error, dir = %name, "Cannot remove old list waveforms")
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// Scheduler: on-demand queue, pre-generation pass and their workers

/// Why pre-generation is not taking new files.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum YieldReason {
    /// A scan of this server runs or is paused.
    Scan,
    /// An `/audio/{id}/stream` or `/audio/{id}/preview` response is in progress.
    Audio,
}

#[derive(Clone, Debug)]
struct Failure {
    hash: u64,
    code: &'static str,
    message: String,
}

/// Ids by recency: the most recently requested is taken first; the oldest is dropped
/// beyond [`ON_DEMAND_CAPACITY`].
#[derive(Default)]
struct OnDemand {
    by_seq: BTreeMap<u64, i64>,
    seq_of: HashMap<i64, u64>,
    next: u64,
}

impl OnDemand {
    fn push_newest(&mut self, id: i64) {
        if let Some(old) = self.seq_of.remove(&id) {
            self.by_seq.remove(&old);
        }
        self.next += 1;
        self.by_seq.insert(self.next, id);
        self.seq_of.insert(id, self.next);
        while self.by_seq.len() > ON_DEMAND_CAPACITY {
            if let Some((_, oldest)) = self.by_seq.pop_first() {
                self.seq_of.remove(&oldest);
            }
        }
    }

    fn pop_newest(&mut self) -> Option<i64> {
        let (_, id) = self.by_seq.pop_last()?;
        self.seq_of.remove(&id);
        Some(id)
    }

    fn len(&self) -> usize {
        self.by_seq.len()
    }

    /// Queue entries up to and including the one of `ids` that will be taken last
    /// (0 when none of them is queued).
    fn depth_of(&self, ids: &[i64]) -> usize {
        ids.iter()
            .filter_map(|id| self.seq_of.get(id))
            .min()
            .map_or(0, |oldest| self.by_seq.range(*oldest..).count())
    }
}

/// Exponentially weighted average of the interval between finished on-demand jobs
/// (all workers together), i.e. the measured on-demand throughput.
#[derive(Default)]
struct Throughput {
    /// Smoothed seconds per finished job; `None` before the first one.
    interval: Option<f64>,
    last_done: Option<Instant>,
}

impl Throughput {
    /// A job taken from the queue at `taken` finished at `done`. Time without any
    /// queued work (since the previous completion) does not count.
    fn record(&mut self, taken: Instant, done: Instant) {
        let from = self.last_done.map_or(taken, |last| last.max(taken));
        let sample = done.saturating_duration_since(from).as_secs_f64().max(1e-4);
        self.interval = Some(match self.interval {
            Some(old) => old + THROUGHPUT_ALPHA * (sample - old),
            None => sample,
        });
        self.last_done = Some(done);
    }

    /// Finished jobs per second, or the default for `workers` without history.
    fn per_second(&self, workers: usize) -> f64 {
        self.interval
            .map_or(DEFAULT_ON_DEMAND_RATE * workers.max(1) as f64, |interval| {
                1.0 / interval
            })
    }
}

/// Suggested `retry_after` (1..=5 s): the time until the job `position` places down the
/// on-demand queue is done at `per_second`, and at least `stat_wait` seconds, the time
/// the metadata reads still running in the background need (budget ran out).
fn estimate_retry_after(position: usize, per_second: f64, stat_wait: Option<f64>) -> u32 {
    let queue = position.max(1) as f64 / per_second.max(0.01);
    let seconds = queue.max(stat_wait.unwrap_or(0.0)).ceil();
    if seconds.is_nan() {
        return RETRY_AFTER_MAX;
    }
    (seconds.min(f64::from(RETRY_AFTER_MAX)) as u32).clamp(1, RETRY_AFTER_MAX)
}

/// Seconds the `unfinished` background metadata reads of a request need at the rate the
/// request measured (`finished` in `elapsed`); unbounded without any progress.
fn stat_wait(unfinished: usize, finished: usize, elapsed: Duration) -> f64 {
    if unfinished == 0 {
        return 0.0;
    }
    if finished == 0 || elapsed.is_zero() {
        return f64::INFINITY;
    }
    unfinished as f64 * elapsed.as_secs_f64() / finished as f64
}

/// A metadata read made for a `pending` item (reused by workers and later requests).
#[derive(Clone)]
struct StatHint {
    canonical: PathBuf,
    file_etag: String,
    at: Instant,
}

/// Ids with an outstanding `POST /waveforms` metadata read (dedupe and bound).
#[derive(Default)]
struct StatTracker {
    in_flight: Mutex<HashSet<i64>>,
}

impl StatTracker {
    /// Claim `id`; `false` when it is already being read or too many reads are
    /// outstanding.
    fn try_begin(&self, id: i64) -> bool {
        let mut in_flight = self
            .in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        in_flight.len() < MAX_OUTSTANDING_STATS && in_flight.insert(id)
    }

    fn end(&self, id: i64) {
        self.in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
    }

    fn contains(&self, id: i64) -> bool {
        self.in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&id)
    }

    fn len(&self) -> usize {
        self.in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

/// Releases a [`StatTracker`] claim when the read (or its abandoned task) ends.
struct StatClaim {
    list: Arc<ListWaveforms>,
    id: i64,
}

impl Drop for StatClaim {
    fn drop(&mut self) {
        self.list.stats.end(self.id);
    }
}

/// One pre-generation pass over the library (in memory; restarts from the beginning
/// after a restart or when the setting is switched on again).
struct Pass {
    generation: u64,
    cursor: i64,
    buffer: VecDeque<i64>,
    exhausted: bool,
    /// New or changed records reported by scans, before the remaining pass.
    extra: VecDeque<i64>,
    extra_set: HashSet<i64>,
    /// Records skipped because their root was offline, per root name.
    deferred: HashMap<String, Vec<i64>>,
    started_at: i64,
    finished_at: Option<i64>,
    in_flight: usize,
}

impl Pass {
    fn has_work(&self) -> bool {
        !self.extra.is_empty() || !self.buffer.is_empty() || !self.exhausted || self.in_flight > 0
    }

    fn push_extra(&mut self, id: i64) {
        if self.extra_set.insert(id) {
            self.extra.push_back(id);
            self.finished_at = None;
        }
    }
}

/// Result of processing one record in the background.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Outcome {
    /// Decoded and written.
    Generated,
    /// The cache already matches the file's current version.
    Skipped,
    /// Decoding failed now.
    Failed { code: &'static str, message: String },
    /// Decoding failed before for this file version (not retried).
    KnownFailure { code: &'static str, message: String },
    /// The record's root is offline (named).
    Offline(String),
    /// The record or its file is gone (or marked missing).
    Gone,
    /// The record or its metadata could not be read.
    Unreadable,
    /// The server is stopping; the record was not processed.
    Cancelled,
}

impl Outcome {
    /// The record was actually checked (counts for `current_rate`).
    fn counts(&self) -> bool {
        !matches!(self, Self::Cancelled | Self::Offline(_))
    }
}

/// A list waveform cache job being run (or the most recent one, kept for the status).
struct ActiveJob {
    /// Distinguishes this job from earlier ones in in-flight bookkeeping.
    generation: u64,
    record: JobRecord,
    resumed: bool,
    /// `failed` mode: the remaining ids to retry (the walk modes use the cursor).
    list: Option<VecDeque<i64>>,
    buffer: VecDeque<i64>,
    exhausted: bool,
    /// Largest id taken from the walk so far.
    last_taken: i64,
    /// Ids being processed, with their start time (Unix seconds).
    in_flight: BTreeMap<i64, i64>,
    /// Records skipped for an offline root, per root name (retried once it is online).
    deferred: HashMap<String, Vec<i64>>,
    requeued: VecDeque<i64>,
    /// `total` / `max_id` must be computed before taking records (resumed fallback).
    recount: bool,
    /// A worker is clearing the cache (`rebuild`).
    clearing: bool,
    /// A worker is reading the library for this job (recount or the next ids) outside
    /// the scheduler lock.
    fetching: bool,
    since_save: u64,
    saved_at: Instant,
}

impl ActiveJob {
    fn new(generation: u64, record: JobRecord, resumed: bool, list: Option<VecDeque<i64>>) -> Self {
        Self {
            generation,
            last_taken: record.cursor,
            record,
            resumed,
            list,
            buffer: VecDeque::new(),
            exhausted: false,
            in_flight: BTreeMap::new(),
            deferred: HashMap::new(),
            requeued: VecDeque::new(),
            recount: false,
            clearing: false,
            fetching: false,
            since_save: 0,
            saved_at: Instant::now(),
        }
    }

    fn active(&self) -> bool {
        self.record.state.active()
    }

    /// Largest id such that every walked id up to it was processed.
    fn safe_cursor(&self) -> i64 {
        if self.list.is_some() {
            return self.record.cursor;
        }
        match self.in_flight.keys().next() {
            Some(first) => (first - 1).min(self.last_taken),
            None => self.last_taken,
        }
    }

    fn touch(&mut self) {
        self.record.updated_at = crate::auth::unix_now();
        self.record.cursor = self
            .safe_cursor()
            .max(self.record.cursor.min(self.last_taken));
    }

    /// Account for one processed record.
    fn finish_item(&mut self, id: i64, outcome: &Outcome) {
        if *outcome == Outcome::Cancelled {
            // The server is stopping: keep the id in flight so the saved cursor stays
            // before it and the record is processed again after the restart.
            return;
        }
        self.in_flight.remove(&id);
        let record = &mut self.record;
        match outcome {
            Outcome::Generated => record.generated += 1,
            Outcome::Failed { .. } | Outcome::KnownFailure { .. } => record.failed += 1,
            Outcome::Offline(root) => {
                record.skipped_offline += 1;
                self.deferred.entry(root.clone()).or_default().push(id);
            }
            Outcome::Skipped | Outcome::Gone | Outcome::Unreadable => record.skipped += 1,
            Outcome::Cancelled => {}
        }
        self.record.done += 1;
        self.since_save += 1;
        self.touch();
    }

    /// Move records deferred for an offline root back into the job once `online(root)`
    /// holds; their counts are taken back and counted again when processed (§9.5.2).
    fn requeue_online(&mut self, online: impl Fn(&str) -> bool) {
        let roots: Vec<String> = self
            .deferred
            .keys()
            .filter(|name| online(name))
            .cloned()
            .collect();
        for name in roots {
            if let Some(ids) = self.deferred.remove(&name) {
                let count = ids.len() as u64;
                self.record.skipped_offline = self.record.skipped_offline.saturating_sub(count);
                self.record.done = self.record.done.saturating_sub(count);
                self.requeued.extend(ids);
            }
        }
    }

    fn save_due(&self) -> bool {
        self.since_save >= wf_job::SAVE_RECORDS
            || (self.since_save > 0 && self.saved_at.elapsed() >= wf_job::SAVE_EVERY)
    }

    /// The record to persist now (and reset the save counters).
    fn mark_saved(&mut self) -> JobRecord {
        self.since_save = 0;
        self.saved_at = Instant::now();
        self.record.cursor = self.safe_cursor();
        self.record.clone()
    }

    fn set_state(&mut self, state: JobState) {
        self.record.state = state;
        self.record.paused = state == JobState::Paused;
        let now = crate::auth::unix_now();
        self.record.updated_at = now;
        if !state.active() {
            self.record.finished_at = Some(now);
        }
    }

    fn view(&self, yielding: bool) -> JobView {
        let record = &self.record;
        let state = match record.state {
            JobState::Running if yielding => "yielding",
            JobState::Running => "running",
            JobState::Paused => "paused",
            JobState::Completed => "completed",
            JobState::Cancelled => "cancelled",
            JobState::Error => "error",
        };
        JobView {
            id: record.id.clone(),
            mode: record.mode,
            mode_effective: record.mode_effective,
            state: state.into(),
            started_at: record.started_at,
            updated_at: record.updated_at,
            finished_at: record.finished_at,
            total: record.total,
            done: record.done,
            generated: record.generated,
            skipped: record.skipped,
            failed: record.failed,
            skipped_offline: record.skipped_offline,
            resumed: self.resumed,
            current: self
                .in_flight
                .iter()
                .next()
                .map(|(id, started_at)| CurrentItem {
                    id: *id,
                    started_at: *started_at,
                }),
        }
    }
}

/// Why a job request was refused.
#[derive(Debug)]
pub enum JobRefusal {
    /// 409 `BUSY`: this job is running or paused.
    Busy(String),
    /// 409 `JOB_STATE`: no job in progress.
    NoJob,
    /// 503 `UNAVAILABLE`: the list cache (`waveforms.db`) cannot be used.
    Unavailable,
    /// The library could not be read.
    Failed(Error),
}

#[derive(Default)]
struct Sched {
    on_demand: OnDemand,
    on_demand_in_flight: usize,
    throughput: Throughput,
    failures: HashMap<i64, Failure>,
    pass: Option<Pass>,
    generation: u64,
    enabled: bool,
    paused: bool,
    last_blocked: Option<(Instant, YieldReason)>,
    next_decode_at: Option<Instant>,
    prune: bool,
    /// Current or most recent cache job (§9.5).
    job: Option<ActiveJob>,
    job_generation: u64,
    /// Records processed by pre-generation and jobs in the last 60 s.
    window: RateWindow,
    last_error: Option<LastError>,
    /// Job state to persist once the scheduler lock is released, with its save sequence
    /// number.
    pending_save: Option<(u64, JobRecord, Option<LastError>)>,
    /// Sequence number of the latest `job-state.json` snapshot taken (monotonic).
    save_seq: u64,
}

/// A `job-state.json` snapshot: the job and the last error.
type Snapshot = (Option<JobRecord>, Option<LastError>);

impl Sched {
    /// Number the snapshot taken now (under the scheduler lock): later snapshots get
    /// larger numbers, and a write never replaces a newer snapshot by an older one.
    fn next_save_seq(&mut self) -> u64 {
        self.save_seq += 1;
        self.save_seq
    }

    /// Persist `saved` once the scheduler lock is released ([`ListWaveforms::flush_pending_save`]).
    fn queue_save(&mut self, saved: (JobRecord, Option<LastError>)) {
        let seq = self.next_save_seq();
        self.pending_save = Some((seq, saved.0, saved.1));
    }
}

/// Result of one look at the queues ([`ListWaveforms::next_job_once`]).
enum Next {
    Run(Job),
    Nothing,
    /// A library read for the job was done without the scheduler lock: look again.
    Again,
}

/// What a worker does next for the cache job.
enum JobStep {
    /// Process this record.
    Take(Job),
    /// Nothing to take now.
    Idle,
    /// Compute `total` / `max_id` of the job of this generation (outside the lock).
    Recount(u64),
    /// Read the next ids after `after` for the job of this generation (outside the lock).
    Fetch { generation: u64, after: i64 },
}

enum Job {
    /// An on-demand id and when it was taken from the queue.
    OnDemand(i64, Instant),
    Pregenerate(i64, u64),
    /// A record of the cache job of this generation.
    Cache(i64, u64),
    /// Clear the cache for the `rebuild` job of this generation.
    ClearForRebuild(u64),
}

/// List waveform service of one opened library (owned by [`AppState`]).
pub struct ListWaveforms {
    cache: ListCache,
    sched: Mutex<Sched>,
    wake: Condvar,
    requests: Arc<tokio::sync::Semaphore>,
    /// Parallel `POST /waveforms` metadata reads, shared by all requests and their
    /// background continuations.
    stat_slots: Arc<tokio::sync::Semaphore>,
    stats: StatTracker,
    hints: Mutex<HashMap<i64, StatHint>>,
    workers: usize,
    pause_stats_on_playback: bool,
    last_playback_start: Mutex<Option<Instant>>,
    stop: AtomicBool,
    worker_handles: Mutex<Vec<std::thread::JoinHandle<()>>>,
    db_path: PathBuf,
    db: Mutex<Option<Connection>>,
    /// `waveforms.db` of the 8-bit encoding (§9.4); `None` when it cannot be opened.
    wfdb: Option<crate::wfdb::WaveformsDb>,
    library_id: String,
    /// Serialises writes of `job-state.json`; holds the sequence number of the newest
    /// snapshot written (older ones are skipped). Taken before the scheduler lock, never
    /// while holding it.
    state_write: Mutex<u64>,
}

impl Drop for ListWaveforms {
    fn drop(&mut self) {
        self.cache.flush_all();
        if let Some(wfdb) = &self.wfdb {
            wfdb.flush();
        }
    }
}

/// Snapshot of [`ListWaveforms::activity`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ListActivity {
    /// On-demand jobs being processed.
    pub on_demand_in_flight: usize,
    /// Pre-generation jobs being processed.
    pub pregen_in_flight: usize,
    /// `POST /waveforms` metadata reads outstanding (including background ones).
    pub outstanding_stats: usize,
}

/// `GET /waveforms/status` (and the pause / resume replies).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ListWaveformStatus {
    /// Effective `pregenerate_list_waveforms`.
    pub enabled: bool,
    /// `off`, `idle`, `running`, `yielding` or `paused`.
    pub state: String,
    /// Why pre-generation yields (`yielding` only).
    pub yield_reason: Option<YieldReason>,
    /// Records that are not missing.
    pub total: u64,
    /// Entries in the list cache.
    pub cached: u64,
    /// Records the current pass has not checked yet (`null` when disabled).
    pub remaining: Option<u64>,
    /// Length of the on-demand queue.
    pub queued_on_demand: u64,
    /// Records whose current file version failed to decode.
    pub failed: u64,
    /// Records of the current pass skipped because their root is offline.
    pub skipped_offline: u64,
    /// Effective `list_waveform_rate`.
    pub rate: u32,
    /// Bytes of the list cache.
    pub cache_bytes: u64,
    /// `list_waveform_cache_mb` in bytes.
    pub cache_limit_bytes: u64,
    /// Shards had to be evicted to stay within the limit.
    pub over_budget: bool,
    /// Start of the current pass (Unix seconds).
    pub pass_started_at: Option<i64>,
    /// End of the current pass (Unix seconds), `null` while it runs.
    pub pass_finished_at: Option<i64>,
    /// Records still to generate: the job's `total - done`, else `remaining` (§9.5.2).
    #[serde(default)]
    pub pending: Option<u64>,
    /// `idle`, `running`, `paused` or `error`.
    #[serde(default)]
    pub run_state: String,
    /// Records per second over the last 60 s (one decimal; 0 when idle or paused).
    #[serde(default)]
    pub current_rate: f64,
    /// `pending / current_rate` in seconds; `null` while it cannot be estimated.
    #[serde(default)]
    pub eta_secs: Option<u64>,
    /// Most recent background error.
    #[serde(default)]
    pub last_error: Option<LastError>,
    /// Current or most recent cache job.
    #[serde(default)]
    pub job: Option<JobView>,
}

/// List-waveform part of `GET /admin/api/runtime`: scheduler state and cache counters
/// only (no database queries, unlike [`ListWaveformStatus`]).
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct ListWaveformRuntime {
    /// Effective `pregenerate_list_waveforms`.
    pub enabled: bool,
    /// The pre-generation pass is paused (`POST /waveforms/pause`).
    pub paused: bool,
    /// Effective `list_waveform_rate` (files per second).
    pub rate: u32,
    /// Records per second over the last 60 s (one decimal).
    pub current_rate: f64,
    /// Current or most recent job (`done` / `total` / `failed`, ...).
    pub job: Option<JobView>,
    /// Records of the active job still to check (`total - done`), else `null`.
    pub pending: Option<u64>,
    /// Records whose current file version failed to decode.
    pub failed: u64,
    /// Length of the on-demand queue.
    pub queued_on_demand: u64,
    /// Entries in the list cache.
    pub cache_entries: u64,
    /// Bytes of the list cache.
    pub cache_bytes: u64,
    /// `list_waveform_cache_mb` in bytes.
    pub cache_limit_bytes: u64,
    /// Shards had to be evicted to stay within the limit.
    pub over_budget: bool,
    /// Most recent background error.
    pub last_error: Option<LastError>,
}

/// `list_waveforms` of `GET /capabilities`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ListWaveformsCapability {
    /// List encoding version (`-l<version>` of the entity tags).
    pub version: u32,
    /// Accepted `buckets`.
    pub buckets: Vec<u32>,
    /// Most ids per request.
    pub max_ids: usize,
    /// Default `peaks` encoding.
    pub encoding: String,
    /// Every accepted `encoding` (§9.4).
    #[serde(default)]
    pub encodings: Vec<String>,
    /// `GET /waveforms/bundle` is available (§9.4.2).
    #[serde(default)]
    pub bundle: bool,
}

impl Default for ListWaveformsCapability {
    fn default() -> Self {
        Self {
            version: LIST_VERSION,
            buckets: vec![LIST_BUCKETS],
            max_ids: MAX_LIST_IDS,
            encoding: LIST_ENCODING.into(),
            encodings: LIST_ENCODINGS.iter().map(|e| (*e).to_string()).collect(),
            bundle: true,
        }
    }
}

/// `GET /settings` / `PATCH /settings` reply.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SettingsResponse {
    /// Effective `pregenerate_list_waveforms`.
    pub pregenerate_list_waveforms: bool,
    /// `default`, `config` or `admin`.
    pub pregenerate_list_waveforms_source: Source,
    /// Effective `list_waveform_rate` (files per second, §9.5.4).
    pub list_waveform_rate: u32,
    /// `default`, `config` or `admin`.
    pub list_waveform_rate_source: Source,
    /// Whether this client may change the settings (`manage_mode`; a hint).
    pub can_change: bool,
}

/// Configuration of [`ListWaveforms::new`].
pub struct ListOptions {
    /// Waveform cache directory (`waveform_cache_dir`).
    pub cache_root: PathBuf,
    /// `list_waveform_cache_mb`.
    pub cache_mb: u64,
    /// Effective worker count (`list_waveform_workers`, at most `max(1, max_waveforms - 1)`).
    pub workers: usize,
    /// `list_waveform_rate` of `config.toml` (informational: the effective rate is read
    /// from [`crate::Security::list_waveform_rate`] for every file, §9.5.4).
    pub rate: u32,
    /// `list_waveform_pause_stats_on_playback`.
    pub pause_stats_on_playback: bool,
}

impl ListWaveforms {
    /// Service of library `library_id` stored in `db_path`; workers are started by
    /// [`spawn_workers`].
    pub fn new(options: ListOptions, library_id: &str, db_path: &Path) -> Self {
        let list_dir = options.cache_root.join("list");
        let name = library_dir_name(library_id);
        remove_other_libraries(&list_dir, &name);
        let wfdb = match crate::wfdb::WaveformsDb::open(&list_dir.join(&name)) {
            Ok(wfdb) => Some(wfdb),
            Err(error) => {
                tracing::warn!(%error, "Cannot open waveforms.db; 8-bit list waveforms are not stored");
                None
            }
        };
        let list = Self {
            library_id: library_id.to_string(),
            state_write: Mutex::new(0),
            wfdb,
            cache: ListCache::open(list_dir.join(name), options.cache_mb),
            sched: Mutex::new(Sched::default()),
            wake: Condvar::new(),
            requests: Arc::new(tokio::sync::Semaphore::new(MAX_BATCH_REQUESTS)),
            stat_slots: Arc::new(tokio::sync::Semaphore::new(STAT_PARALLEL)),
            stats: StatTracker::default(),
            hints: Mutex::new(HashMap::new()),
            workers: options.workers.max(1),
            pause_stats_on_playback: options.pause_stats_on_playback,
            last_playback_start: Mutex::new(None),
            stop: AtomicBool::new(false),
            worker_handles: Mutex::new(Vec::new()),
            db_path: db_path.to_path_buf(),
            db: Mutex::new(None),
        };
        list.restore_job();
        list
    }

    /// The on-disk cache.
    pub fn cache(&self) -> &ListCache {
        &self.cache
    }

    /// `waveforms.db` (§9.4).
    pub fn wfdb(&self) -> Option<&crate::wfdb::WaveformsDb> {
        self.wfdb.as_ref()
    }

    /// Queue the 8-bit row of `id` unless `waveforms.db` already has `hash`'s version.
    fn store_i8(&self, id: i64, hash: u64, canonical: &Path, values: &[i16]) {
        let Some(wfdb) = &self.wfdb else { return };
        let etag = list_etag_i8(hash);
        if wfdb.etag(id).as_deref() == Some(etag.as_str()) {
            return;
        }
        let (src_mtime, src_size) = std::fs::metadata(canonical).map_or((0, 0), |meta| {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs() as i64);
            (mtime, meta.len() as i64)
        });
        wfdb.queue(crate::wfdb::Row {
            file_id: id,
            etag,
            src_mtime,
            src_size,
            n: values.len() / 2,
            blob: crate::wfdb::to_blob(values),
        });
    }

    fn sched(&self) -> MutexGuard<'_, Sched> {
        self.sched.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Stop the workers after their current file and save pending cache updates.
    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.wake.notify_all();
        // Persist the job's progress so it resumes from here (§9.5.5).
        if self.sched().job.as_ref().is_some_and(ActiveJob::active) {
            self.save_current();
        }
        self.cache.flush_all();
        if let Some(wfdb) = &self.wfdb {
            wfdb.flush();
        }
    }

    /// After shutdown, join every cache writer before an in-process program update.
    /// A sleeping source file may hold a worker; that cancels deployment on timeout.
    pub fn drain_for_update(&self, timeout: Duration) -> Result<()> {
        let started = Instant::now();
        let mut handles = self
            .worker_handles
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        while handles.iter().any(|worker| !worker.is_finished()) {
            if started.elapsed() >= timeout {
                return Err(Error::Busy(
                    "A list waveform worker is still finishing".into(),
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        for worker in handles.drain(..) {
            worker.join().map_err(|_| {
                Error::Database("A list waveform worker panicked during shutdown".into())
            })?;
        }
        // A worker may have stored its final row after shutdown's first flush.
        self.cache.flush_all();
        if let Some(wfdb) = &self.wfdb {
            wfdb.flush();
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn retain_shutdown_test_worker(&self, worker: std::thread::JoinHandle<()>) {
        self.worker_handles.lock().unwrap().push(worker);
    }

    /// A stream or preview response started (the on-demand queue yields for 3 s).
    pub fn note_playback_start(&self) {
        *self
            .last_playback_start
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Instant::now());
    }

    /// With `list_waveform_pause_stats_on_playback`: the rest of the prebuffer window of
    /// the latest stream/preview start, during which `POST /waveforms` reads no metadata.
    pub fn stat_pause_remaining(&self) -> Option<Duration> {
        if !self.pause_stats_on_playback {
            return None;
        }
        let started = (*self
            .last_playback_start
            .lock()
            .unwrap_or_else(PoisonError::into_inner))?;
        PREBUFFER_YIELD
            .checked_sub(started.elapsed())
            .filter(|left| !left.is_zero())
    }

    fn playback_started_within(&self, window: Duration) -> bool {
        self.last_playback_start
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some_and(|at| at.elapsed() < window)
    }

    /// Blocking. Run `query` on this service's read-only library connection.
    fn query<T>(&self, query: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Result<T> {
        let mut slot = self.db.lock().unwrap_or_else(PoisonError::into_inner);
        if slot.is_none() {
            let conn = Connection::open_with_flags(
                &self.db_path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .and_then(|conn| {
                conn.busy_timeout(Duration::from_secs(5))?;
                conn.execute_batch("PRAGMA query_only=ON;")?;
                Ok(conn)
            })
            .map_err(|error| Error::Database(error.to_string()))?;
            *slot = Some(conn);
        }
        let conn = slot.as_ref().expect("the connection was just opened");
        query(conn).map_err(|error| Error::Database(error.to_string()))
    }

    /// Blocking. Path and missing flag of each existing id.
    fn records(&self, ids: &[i64]) -> Result<HashMap<i64, (String, bool)>> {
        let mut found = HashMap::with_capacity(ids.len());
        for chunk in ids.chunks(MAX_LIST_IDS) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            self.query(|conn| {
                let mut statement = conn.prepare(&format!(
                    "SELECT id,file_path,COALESCE(missing,0) FROM audio_files WHERE id IN ({placeholders})"
                ))?;
                let rows = statement.query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)? != 0,
                    ))
                })?;
                for row in rows {
                    let (id, path, missing) = row?;
                    found.insert(id, (path, missing));
                }
                Ok(())
            })?;
        }
        Ok(found)
    }

    fn record(&self, id: i64) -> Result<Option<(String, bool)>> {
        self.query(|conn| {
            conn.query_row(
                "SELECT file_path,COALESCE(missing,0) FROM audio_files WHERE id=?1",
                [id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? != 0)),
            )
            .optional()
        })
    }

    fn next_ids(&self, cursor: i64) -> Result<Vec<i64>> {
        self.query(|conn| {
            let mut statement = conn.prepare_cached(
                "SELECT id FROM audio_files WHERE id>?1 AND missing=0 ORDER BY id LIMIT ?2",
            )?;
            let rows = statement.query_map([cursor, PASS_BATCH], |row| row.get(0))?;
            rows.collect()
        })
    }

    fn count_present(&self, after: i64) -> Result<u64> {
        self.query(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM audio_files WHERE id>?1 AND missing=0",
                [after],
                |row| row.get::<_, i64>(0),
            )
        })
        .map(|count| count.max(0) as u64)
    }

    fn existing(&self, low: i64, high: i64) -> Result<HashSet<i64>> {
        self.query(|conn| {
            let mut statement =
                conn.prepare_cached("SELECT id FROM audio_files WHERE id BETWEEN ?1 AND ?2")?;
            let rows = statement.query_map([low, high], |row| row.get(0))?;
            rows.collect()
        })
    }

    /// Blocking. Current database time in the scanner's `modified_at` format.
    pub fn database_now(&self) -> Option<String> {
        self.query(|conn| {
            conn.query_row("SELECT strftime('%Y-%m-%d %H:%M:%f','now')", [], |row| {
                row.get(0)
            })
        })
        .ok()
    }

    /// Queue `ids` for the workers; the first id is processed first.
    pub fn enqueue_on_demand(&self, ids: &[i64]) {
        if ids.is_empty() {
            return;
        }
        let mut sched = self.sched();
        for id in ids.iter().rev() {
            sched.on_demand.push_newest(*id);
        }
        drop(sched);
        self.wake.notify_all();
    }

    /// Length of the on-demand queue.
    pub fn queued_on_demand(&self) -> usize {
        self.sched().on_demand.len()
    }

    /// Suggested `retry_after` for a batch whose `pending` ids were just queued: the
    /// time until the last of them is done at the measured on-demand throughput, at
    /// least `stat_wait` seconds (metadata reads still running), 1..=5 s.
    fn retry_after_for(&self, pending: &[i64], stat_wait: Option<f64>) -> u32 {
        let sched = self.sched();
        let position = sched.on_demand.depth_of(pending) + sched.on_demand_in_flight;
        estimate_retry_after(
            position,
            sched.throughput.per_second(self.workers),
            stat_wait,
        )
    }

    /// A fresh metadata read of `id` made for a `pending` item.
    fn hint(&self, id: i64) -> Option<StatHint> {
        let hints = self.hints.lock().unwrap_or_else(PoisonError::into_inner);
        hints
            .get(&id)
            .filter(|hint| hint.at.elapsed() < STAT_HINT_TTL)
            .cloned()
    }

    fn take_hint(&self, id: i64) -> Option<StatHint> {
        self.hints
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id)
            .filter(|hint| hint.at.elapsed() < STAT_HINT_TTL)
    }

    fn store_hint(&self, id: i64, canonical: PathBuf, file_etag: String) {
        let mut hints = self.hints.lock().unwrap_or_else(PoisonError::into_inner);
        if hints.len() >= ON_DEMAND_CAPACITY {
            hints.retain(|_, hint| hint.at.elapsed() < STAT_HINT_TTL);
        }
        if hints.len() < ON_DEMAND_CAPACITY {
            hints.insert(
                id,
                StatHint {
                    canonical,
                    file_etag,
                    at: Instant::now(),
                },
            );
        }
    }

    /// Metadata reads of `POST /waveforms` outstanding now (including background ones).
    pub fn outstanding_stats(&self) -> usize {
        self.stats.len()
    }

    /// What the list waveform machinery is doing now (playback timing diagnostics).
    pub fn activity(&self) -> ListActivity {
        let sched = self.sched();
        ListActivity {
            on_demand_in_flight: sched.on_demand_in_flight,
            pregen_in_flight: sched.pass.as_ref().map_or(0, |pass| pass.in_flight),
            outstanding_stats: self.stats.len(),
        }
    }

    /// Measured on-demand throughput (finished files per second), `None` before any.
    pub fn on_demand_rate(&self) -> Option<f64> {
        let sched = self.sched();
        sched
            .throughput
            .interval
            .map(|_| sched.throughput.per_second(self.workers))
    }

    /// Prune deleted records from the cache in the background (after removals and scans).
    pub fn request_prune(&self) {
        self.sched().prune = true;
        self.wake.notify_all();
    }

    /// Blocking. A scan finished: queue its new and changed records (`seen_scan_id` =
    /// `session_id`, modified at or after `since`) for pre-generation when it is on,
    /// then prune removed records.
    pub fn scan_finished(&self, enabled: bool, session_id: i64, since: Option<&str>) {
        self.request_prune();
        if !enabled || session_id <= 0 {
            return;
        }
        let Some(since) = since else {
            return;
        };
        let ids: Result<Vec<i64>> = self.query(|conn| {
            let mut statement = conn.prepare(
                "SELECT id FROM audio_files WHERE seen_scan_id=?1 AND modified_at>=?2 AND missing=0 ORDER BY id",
            )?;
            let rows = statement.query_map(rusqlite::params![session_id, since], |row| row.get(0))?;
            rows.collect()
        });
        let ids = match ids {
            Ok(ids) => ids,
            Err(error) => {
                tracing::warn!(%error, "Cannot list the records changed by a scan");
                return;
            }
        };
        let mut sched = self.sched();
        if let Some(pass) = sched.pass.as_mut() {
            for id in &ids {
                pass.push_extra(*id);
            }
        }
        drop(sched);
        tracing::debug!(
            count = ids.len(),
            "Queued scanned records for list waveforms"
        );
        self.wake.notify_all();
    }

    /// Manually pause / resume pre-generation (this process only).
    ///
    /// While a cache job is running or paused this acts on the job instead (alias of
    /// `POST /waveforms/jobs/current/pause|resume`, §9.5.3).
    pub fn set_paused(&self, paused: bool) {
        if self.pause_job(paused).is_ok() {
            return;
        }
        self.sched().paused = paused;
        self.wake.notify_all();
    }

    fn failure(&self, id: i64, hash: u64) -> Option<Failure> {
        self.sched()
            .failures
            .get(&id)
            .filter(|failure| failure.hash == hash)
            .cloned()
    }

    /// Start or end the pass when the effective setting changed.
    fn sync_enabled(&self, sched: &mut Sched, enabled: bool) {
        if enabled && !sched.enabled {
            sched.generation += 1;
            sched.pass = Some(Pass {
                generation: sched.generation,
                cursor: 0,
                buffer: VecDeque::new(),
                exhausted: false,
                extra: VecDeque::new(),
                extra_set: HashSet::new(),
                deferred: HashMap::new(),
                started_at: crate::auth::unix_now(),
                finished_at: None,
                in_flight: 0,
            });
            tracing::info!("List waveform pre-generation started");
        } else if !enabled && sched.enabled {
            sched.pass = None;
            tracing::info!("List waveform pre-generation switched off");
        }
        sched.enabled = enabled;
    }

    /// Current yield condition of pre-generation (`None` when it may run now).
    fn yield_reason(&self, state: &AppState, sched: &mut Sched) -> Option<YieldReason> {
        let now = if state.scans().current().is_some() {
            Some(YieldReason::Scan)
        } else if state.playback_active() || self.playback_started_within(PREBUFFER_YIELD) {
            Some(YieldReason::Audio)
        } else {
            None
        };
        if let Some(reason) = now {
            sched.last_blocked = Some((Instant::now(), reason));
            return Some(reason);
        }
        sched
            .last_blocked
            .filter(|(at, _)| at.elapsed() < RESUME_DELAY)
            .map(|(_, reason)| reason)
    }

    /// Blocking. The next file for this worker, honouring the yield rules (§9.2).
    fn next_job(&self, state: &AppState) -> Option<Job> {
        loop {
            match self.next_job_once(state) {
                Next::Run(job) => return Some(job),
                Next::Nothing => return None,
                Next::Again => {}
            }
        }
    }

    /// One look at the queues.
    fn next_job_once(&self, state: &AppState) -> Next {
        let enabled = state.security().pregenerate_list_waveforms();
        let playback = state.playback_active();
        let prebuffer = self.playback_started_within(PREBUFFER_YIELD);
        let mut sched = self.sched();
        self.sync_enabled(&mut sched, enabled);
        // On-demand: yields only during the prebuffer window after a stream/preview
        // starts, and uses at most one decode slot while any is active.
        if sched.on_demand.len() > 0 && !prebuffer && (!playback || sched.on_demand_in_flight == 0)
        {
            let Some(id) = sched.on_demand.pop_newest() else {
                return Next::Nothing;
            };
            sched.on_demand_in_flight += 1;
            return Next::Run(Job::OnDemand(id, Instant::now()));
        }
        let reason = self.yield_reason(state, &mut sched);
        if reason.is_some() || sched.on_demand.len() > 0 {
            return Next::Nothing;
        }
        // A cache job (running or paused) replaces pre-generation (§9.5.1).
        if sched.job.as_ref().is_some_and(ActiveJob::active) {
            let step = self.next_job_item(&mut sched);
            drop(sched);
            return match step {
                JobStep::Take(job) => Next::Run(job),
                JobStep::Idle => Next::Nothing,
                // Library reads run without the scheduler lock, then the worker looks
                // again.
                JobStep::Recount(generation) => {
                    self.recount_job(generation);
                    Next::Again
                }
                JobStep::Fetch { generation, after } => {
                    self.fetch_job_ids(generation, after);
                    Next::Again
                }
            };
        }
        if !enabled || sched.paused {
            return Next::Nothing;
        }
        let Some(pass) = sched.pass.as_mut() else {
            return Next::Nothing;
        };
        let id = self.take_pass_id(pass);
        match id {
            Some(id) => {
                pass.in_flight += 1;
                Next::Run(Job::Pregenerate(id, pass.generation))
            }
            None => {
                if pass.exhausted && pass.in_flight == 0 && pass.finished_at.is_none() {
                    pass.finished_at = Some(crate::auth::unix_now());
                    tracing::info!("List waveform pre-generation pass finished");
                }
                Next::Nothing
            }
        }
    }

    /// Next record of the pass: scanned records first, then the library in id order.
    fn take_pass_id(&self, pass: &mut Pass) -> Option<i64> {
        if let Some(id) = pass.extra.pop_front() {
            pass.extra_set.remove(&id);
            return Some(id);
        }
        if pass.buffer.is_empty() && !pass.exhausted {
            match self.next_ids(pass.cursor) {
                Ok(ids) if ids.is_empty() => pass.exhausted = true,
                Ok(ids) => {
                    pass.cursor = *ids.last().expect("not empty");
                    pass.buffer.extend(ids);
                }
                Err(error) => {
                    tracing::warn!(%error, "Cannot read records for list waveforms");
                    return None;
                }
            }
        }
        pass.buffer.pop_front()
    }

    /// Move records deferred for an offline root back into the pass once it is online.
    fn requeue_deferred(&self, state: &AppState) {
        let mut sched = self.sched();
        if let Some(job) = sched
            .job
            .as_mut()
            .filter(|job| job.record.state == JobState::Running && !job.deferred.is_empty())
        {
            let monitor = state.root_monitor();
            job.requeue_online(|name| {
                monitor
                    .roots()
                    .iter()
                    .position(|root| root.name == name)
                    .is_none_or(|index| monitor.status(index).state == crate::RootState::Online)
            });
        }
        let Some(pass) = sched.pass.as_mut() else {
            return;
        };
        if pass.deferred.is_empty() {
            return;
        }
        let monitor = state.root_monitor();
        let online: Vec<String> = pass
            .deferred
            .keys()
            .filter(|name| {
                monitor
                    .roots()
                    .iter()
                    .position(|root| &root.name == *name)
                    .is_none_or(|index| monitor.status(index).state == crate::RootState::Online)
            })
            .cloned()
            .collect();
        for name in online {
            if let Some(ids) = pass.deferred.remove(&name) {
                for id in ids {
                    pass.push_extra(id);
                }
            }
        }
    }

    /// Blocking. Process one job: check the file version, decode on a miss, store.
    fn run(&self, state: &AppState, job: Job) {
        let (id, limited) = match job {
            Job::OnDemand(id, _) => (id, false),
            Job::Pregenerate(id, _) | Job::Cache(id, _) => (id, true),
            Job::ClearForRebuild(generation) => {
                self.clear_for_rebuild(generation);
                return;
            }
        };
        let outcome = self.process(state, id, limited);
        let now = Instant::now();
        if let Outcome::Failed { code, message } = &outcome {
            self.note_error(code, message.clone(), Some(id));
        }
        let save_due = {
            let mut sched = self.sched();
            match job {
                Job::OnDemand(_, taken) => {
                    sched.on_demand_in_flight = sched.on_demand_in_flight.saturating_sub(1);
                    sched.throughput.record(taken, now);
                    false
                }
                Job::Pregenerate(_, generation) => {
                    if let Outcome::Offline(root) = &outcome
                        && let Some(pass) =
                            sched.pass.as_mut().filter(|p| p.generation == generation)
                    {
                        pass.deferred.entry(root.clone()).or_default().push(id);
                    }
                    if let Some(pass) = sched.pass.as_mut().filter(|p| p.generation == generation) {
                        pass.in_flight = pass.in_flight.saturating_sub(1);
                    }
                    if outcome.counts() {
                        sched.window.record(now);
                    }
                    false
                }
                Job::ClearForRebuild(_) => false,
                Job::Cache(_, generation) => {
                    let counted = outcome.counts();
                    let mut due = false;
                    if let Some(job) = sched
                        .job
                        .as_mut()
                        .filter(|job| job.generation == generation)
                    {
                        job.finish_item(id, &outcome);
                        due = job.save_due();
                    }
                    if counted {
                        sched.window.record(now);
                    }
                    due
                }
            }
        };
        if let (true, Job::Cache(_, generation)) = (save_due, job) {
            // The snapshot is taken under the write lock, so a pause / stop decided after
            // it is written after it too.
            self.persist(|sched| {
                let last_error = sched.last_error.clone();
                let record = sched
                    .job
                    .as_mut()
                    .filter(|job| job.generation == generation && job.save_due())
                    .map(ActiveJob::mark_saved);
                ((), record.map(|record| (Some(record), last_error)))
            });
        }
    }

    /// Blocking. Check `id`'s file version and decode it on a miss (`limited`: wait for
    /// the `list_waveform_rate` limiter first).
    fn process(&self, state: &AppState, id: i64, limited: bool) -> Outcome {
        let (path, missing) = match self.record(id) {
            Ok(Some(record)) => record,
            Ok(None) => return Outcome::Gone,
            Err(error) => {
                tracing::warn!(%error, id, "Cannot read a record for its list waveform");
                return Outcome::Unreadable;
            }
        };
        if missing {
            return Outcome::Gone;
        }
        let monitor = state.root_monitor();
        let index = monitor.root_index_for(Path::new(&path));
        if let Some(index) = index
            && monitor.status(index).state == crate::RootState::Offline
        {
            return Outcome::Offline(monitor.roots()[index].name.clone());
        }
        let hinted = self
            .take_hint(id)
            .map(|hint| (hint.canonical, hint.file_etag));
        let stat = match hinted {
            Some(found) => Ok(found),
            None => state.stat_audio(id, &path, index),
        };
        let (canonical, file_etag) = match stat {
            Ok(found) => found,
            Err(StatFailure::RootOffline { root }) => return Outcome::Offline(root),
            Err(StatFailure::Missing) => return Outcome::Gone,
            Err(StatFailure::Other(error)) => {
                tracing::debug!(%error, id, "Cannot read file metadata for a list waveform");
                return Outcome::Unreadable;
            }
        };
        let hash = WaveformKey::new(
            state.library_id(),
            id,
            &canonical,
            &file_etag,
            &list_params(),
        )
        .hash();
        if self.cache.hash_of(id) == Some(hash) {
            return Outcome::Skipped;
        }
        if let Some(failure) = self.failure(id, hash) {
            return Outcome::KnownFailure {
                code: failure.code,
                message: failure.message,
            };
        }
        if limited {
            let wait = {
                let mut sched = self.sched();
                let now = Instant::now();
                let at = sched.next_decode_at.map_or(now, |at| at.max(now));
                let rate = state.security().list_waveform_rate().max(1);
                sched.next_decode_at = Some(at + Duration::from_secs(1) / rate);
                at.saturating_duration_since(now)
            };
            if !wait.is_zero() {
                std::thread::sleep(wait);
            }
        }
        let permit = loop {
            if self.stop.load(Ordering::SeqCst) {
                return Outcome::Cancelled;
            }
            match state.waveform_slots().try_acquire_owned() {
                Ok(permit) => break permit,
                Err(_) => std::thread::sleep(SLOT_POLL),
            }
        };
        let started = Instant::now();
        let result = compute(&canonical, &self.stop);
        drop(permit);
        match result {
            Ok(values) => {
                tracing::debug!(
                    id,
                    columns = values.len() / 2,
                    ms = started.elapsed().as_millis() as u64,
                    "Computed list waveform"
                );
                self.store_i8(id, hash, &canonical, &values);
                self.cache.put(id, hash, values);
                self.sched().failures.remove(&id);
                Outcome::Generated
            }
            Err(Error::Cancelled) => Outcome::Cancelled,
            Err(Error::Io(io)) if crate::roots::is_transient_network_error(&io) => match index {
                Some(index) => Outcome::Offline(monitor.roots()[index].name.clone()),
                None => Outcome::Unreadable,
            },
            Err(error) => {
                let code = match &error {
                    Error::Io(_) => "IO",
                    _ => "AUDIO",
                };
                let message = path_free(&error.to_string(), &[Path::new(&path), &canonical]);
                tracing::info!(id, code, %message, "Cannot compute a list waveform");
                self.sched().failures.insert(
                    id,
                    Failure {
                        hash,
                        code,
                        message: message.clone(),
                    },
                );
                Outcome::Failed { code, message }
            }
        }
    }

    /// Blocking. Remove cache entries and failures of records that no longer exist.
    fn prune(&self) {
        let mut removed_total = 0_usize;
        for shard in self.cache.shards() {
            let ids = self.cache.shard_ids(shard);
            let (Some(low), Some(high)) = (ids.first().copied(), ids.last().copied()) else {
                continue;
            };
            let existing = match self.existing(low, high) {
                Ok(existing) => existing,
                Err(error) => {
                    tracing::warn!(%error, "Cannot prune list waveforms");
                    return;
                }
            };
            let removed = self.cache.retain_shard(shard, |id| existing.contains(&id));
            removed_total += removed.len();
            if !removed.is_empty() {
                let mut sched = self.sched();
                for id in removed {
                    sched.failures.remove(&id);
                }
            }
        }
        if removed_total > 0
            && let Some(wfdb) = &self.wfdb
        {
            wfdb.retain(|id| self.cache.hash_of(id).is_some());
        }
        let failed: Vec<i64> = self.sched().failures.keys().copied().collect();
        if let Ok(found) = self.records(&failed) {
            self.sched().failures.retain(|id, _| found.contains_key(id));
        }
        if removed_total > 0 {
            tracing::info!(
                removed = removed_total,
                "Pruned list waveforms of removed records"
            );
        }
    }

    /// Cheap runtime view for the admin page: only the scheduler lock and cache counters.
    pub fn runtime(&self, state: &AppState) -> ListWaveformRuntime {
        let enabled = state.security().pregenerate_list_waveforms();
        let mut sched = self.sched();
        let job = sched.job.as_ref().map(|job| job.view(false));
        let pending = sched
            .job
            .as_ref()
            .filter(|job| job.active())
            .map(|job| job.record.total.saturating_sub(job.record.done));
        let paused = sched.paused
            || sched
                .job
                .as_ref()
                .is_some_and(|job| job.active() && job.record.state == JobState::Paused);
        let current_rate = if paused {
            0.0
        } else {
            sched
                .window
                .rate(Instant::now())
                .map_or(0.0, wf_job::one_decimal)
        };
        ListWaveformRuntime {
            enabled,
            paused,
            rate: state.security().list_waveform_rate(),
            current_rate,
            job,
            pending,
            failed: sched.failures.len() as u64,
            queued_on_demand: sched.on_demand.len() as u64,
            cache_entries: self.cache.entries(),
            cache_bytes: self.cache.bytes(),
            cache_limit_bytes: self.cache.limit_bytes(),
            over_budget: self.cache.over_budget(),
            last_error: sched.last_error.clone(),
        }
    }

    pub fn status(&self, state: &AppState) -> Result<ListWaveformStatus> {
        let enabled = state.security().pregenerate_list_waveforms();
        let total = self.count_present(i64::MIN)?;
        let (cursor, queued) = {
            let mut sched = self.sched();
            self.sync_enabled(&mut sched, enabled);
            (
                sched.pass.as_ref().map(|pass| pass.cursor),
                sched
                    .pass
                    .as_ref()
                    .map(|pass| (pass.buffer.len() + pass.extra.len() + pass.in_flight) as u64),
            )
        };
        let after_cursor = match cursor {
            Some(cursor) => Some(self.count_present(cursor)?),
            None => None,
        };
        let mut sched = self.sched();
        let reason = self.yield_reason(state, &mut sched);
        let busy_on_demand = sched.on_demand.len() > 0 || sched.on_demand_in_flight > 0;
        let job_state = sched
            .job
            .as_ref()
            .filter(|job| job.active())
            .map(|job| job.record.state);
        let pass = sched.pass.as_ref();
        let pass_busy = pass.is_some_and(Pass::has_work);
        let (state_name, yield_reason) = match job_state {
            Some(JobState::Paused) => ("paused", None),
            Some(_) if reason.is_some() => ("yielding", reason),
            Some(_) => ("running", None),
            None if !enabled => (if busy_on_demand { "running" } else { "off" }, None),
            None if sched.paused => ("paused", None),
            None if pass_busy && reason.is_some() => ("yielding", reason),
            None if pass_busy || busy_on_demand => ("running", None),
            None => ("idle", None),
        };
        let remaining = match (enabled, after_cursor, queued, pass) {
            (true, Some(after), Some(queued), Some(pass)) => Some(if pass.exhausted {
                queued
            } else {
                after + queued
            }),
            _ => None,
        };
        let job_error = sched
            .job
            .as_ref()
            .is_some_and(|job| job.record.state == JobState::Error);
        let run_state = match state_name {
            "paused" => "paused",
            "running" | "yielding" => "running",
            _ if job_error => "error",
            _ => "idle",
        };
        let active_job = sched.job.as_ref().filter(|job| job.active());
        let pending = match active_job {
            Some(job) => Some(job.record.total.saturating_sub(job.record.done)),
            None => remaining,
        };
        let pass_started_at = match active_job {
            Some(job) => Some(job.record.started_at),
            None => pass.map(|pass| pass.started_at),
        };
        let pass_finished_at = match active_job {
            Some(job) => job.record.finished_at,
            None => pass.and_then(|pass| pass.finished_at),
        };
        let skipped_offline = pass.map_or(0, |pass| {
            pass.deferred.values().map(|ids| ids.len() as u64).sum()
        });
        let job = sched.job.as_ref().map(|job| job.view(reason.is_some()));
        let last_error = sched.last_error.clone();
        let failed = sched.failures.len() as u64;
        let queued_on_demand = sched.on_demand.len() as u64;
        let measured = if run_state == "paused" {
            None
        } else {
            sched.window.rate(Instant::now())
        };
        let current_rate = if run_state == "running" {
            measured.map_or(0.0, wf_job::one_decimal)
        } else {
            0.0
        };
        let eta_secs = if run_state == "running" {
            wf_job::eta_secs(pending, measured)
        } else {
            None
        };
        Ok(ListWaveformStatus {
            enabled,
            state: state_name.into(),
            yield_reason,
            total,
            cached: self.cache.entries(),
            remaining,
            queued_on_demand,
            failed,
            skipped_offline,
            rate: state.security().list_waveform_rate(),
            cache_bytes: self.cache.bytes(),
            cache_limit_bytes: self.cache.limit_bytes(),
            over_budget: self.cache.over_budget(),
            pass_started_at,
            pass_finished_at,
            pending,
            run_state: run_state.into(),
            current_rate,
            eta_secs,
            last_error,
            job,
        })
    }

    // -----------------------------------------------------------------------------------
    // Cache jobs (§9.5)

    /// Directory of this library's list cache (holds `job-state.json`).
    fn state_dir(&self) -> &Path {
        self.cache.dir()
    }

    /// Blocking. Decide a job state change under the scheduler lock and persist the
    /// snapshot `decide` returns, in that order: the write lock is taken first, so
    /// snapshots are written in the order they were taken.
    fn persist<R>(&self, decide: impl FnOnce(&mut Sched) -> (R, Option<Snapshot>)) -> R {
        let mut written = self
            .state_write
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let (result, snapshot) = {
            let mut sched = self.sched();
            let (result, snapshot) = decide(&mut sched);
            (
                result,
                snapshot.map(|snapshot| (sched.next_save_seq(), snapshot)),
            )
        };
        if let Some((seq, (job, last_error))) = snapshot {
            self.write_state(&mut written, seq, job, last_error);
        }
        result
    }

    /// Blocking. Persist snapshot `seq` (taken earlier under the scheduler lock).
    fn save_state(&self, seq: u64, job: Option<JobRecord>, last_error: Option<LastError>) {
        let mut written = self
            .state_write
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.write_state(&mut written, seq, job, last_error);
    }

    /// Write snapshot `seq` unless a newer one was written already (logged on failure;
    /// the job keeps running). `written`: the guarded newest sequence number.
    fn write_state(
        &self,
        written: &mut u64,
        seq: u64,
        job: Option<JobRecord>,
        last_error: Option<LastError>,
    ) {
        if seq <= *written {
            tracing::debug!(
                seq,
                written = *written,
                "Skipping a stale list waveform job state"
            );
            return;
        }
        // Even when the write fails, an older snapshot must not replace this state.
        *written = seq;
        let file = StateFile {
            version: wf_job::STATE_VERSION,
            library_id: self.library_id.clone(),
            job,
            last_error,
        };
        if let Err(error) = wf_job::save(self.state_dir(), &file) {
            tracing::warn!(%error, "Cannot save the list waveform job state");
        }
    }

    /// Blocking. Persist the current job (if any) as it is now.
    fn save_current(&self) {
        self.persist(|sched| {
            let last_error = sched.last_error.clone();
            let record = sched.job.as_mut().map(ActiveJob::mark_saved);
            let snapshot =
                (record.is_some() || last_error.is_some()).then_some((record, last_error));
            ((), snapshot)
        });
    }

    /// Restore the job of `job-state.json` (called once when the library opens).
    fn restore_job(&self) {
        let file = match wf_job::load(self.state_dir(), &self.library_id) {
            wf_job::Loaded::State(file) => *file,
            wf_job::Loaded::Absent | wf_job::Loaded::Rejected => return,
        };
        let mut sched = self.sched();
        sched.last_error = file.last_error;
        let Some(mut record) = file.job else { return };
        let resumed = record.state.active();
        let mut recount = false;
        if resumed && record.mode_effective == JobMode::Failed {
            // The failed ids are not persisted: retry them by filling every missing entry
            // (failed records have no cache entry), from the start (§9.5.5).
            record.mode_effective = JobMode::Missing;
            record.cursor = 0;
            record.total = 0;
            record.done = 0;
            record.generated = 0;
            record.skipped = 0;
            record.failed = 0;
            record.skipped_offline = 0;
            recount = true;
        }
        sched.job_generation += 1;
        let generation = sched.job_generation;
        let mut job = ActiveJob::new(generation, record, resumed, None);
        job.recount = recount;
        if resumed {
            tracing::info!(
                id = %job.record.id,
                state = ?job.record.state,
                cursor = job.record.cursor,
                "Resuming the list waveform job after a restart"
            );
            if job.record.state == JobState::Running {
                sched.window.restart(Instant::now());
            }
        }
        sched.job = Some(job);
    }

    /// Record the most recent background error.
    fn note_error(&self, code: &str, message: String, id: Option<i64>) {
        self.sched().last_error = Some(LastError {
            at: crate::auth::unix_now(),
            error_code: code.to_string(),
            message,
            id,
        });
    }

    /// Blocking. `POST /waveforms/jobs`: start a job unless one is running or paused.
    pub fn start_job(&self, mode: JobMode) -> std::result::Result<(), JobRefusal> {
        if !self.jobs_available() {
            return Err(JobRefusal::Unavailable);
        }
        if let Some(job) = self.sched().job.as_ref().filter(|job| job.active()) {
            return Err(JobRefusal::Busy(job.record.id.clone()));
        }
        let max_id = self
            .query(|conn| {
                conn.query_row("SELECT COALESCE(MAX(id),0) FROM audio_files", [], |row| {
                    row.get::<_, i64>(0)
                })
            })
            .map_err(JobRefusal::Failed)?;
        let present = self.count_present(i64::MIN).map_err(JobRefusal::Failed)?;
        let now = crate::auth::unix_now();
        let record = self.persist(|sched| {
            if let Some(job) = sched.job.as_ref().filter(|job| job.active()) {
                return (Err(JobRefusal::Busy(job.record.id.clone())), None);
            }
            let (effective, list) = match mode {
                JobMode::Failed if !sched.failures.is_empty() => {
                    let mut ids: Vec<i64> = sched.failures.keys().copied().collect();
                    ids.sort_unstable();
                    // Retry: forget the failures so the records are decoded again.
                    sched.failures.clear();
                    (
                        JobMode::Failed,
                        Some(ids.into_iter().collect::<VecDeque<_>>()),
                    )
                }
                // Nothing known to have failed in this process (e.g. after a restart):
                // fill every missing entry instead (§9.5.5).
                JobMode::Failed => (JobMode::Missing, None),
                other => (other, None),
            };
            let total = list.as_ref().map_or(present, |ids| ids.len() as u64);
            let record = JobRecord {
                id: wf_job::new_job_id(now),
                mode,
                mode_effective: effective,
                state: JobState::Running,
                paused: false,
                started_at: now,
                updated_at: now,
                finished_at: None,
                cursor: 0,
                max_id,
                total,
                done: 0,
                generated: 0,
                skipped: 0,
                failed: 0,
                skipped_offline: 0,
                rebuild_cleared: false,
            };
            sched.job_generation += 1;
            let generation = sched.job_generation;
            sched.job = Some(ActiveJob::new(generation, record.clone(), false, list));
            sched.window.restart(Instant::now());
            let last_error = sched.last_error.clone();
            (Ok(record.clone()), Some((Some(record), last_error)))
        })?;
        tracing::info!(id = %record.id, ?mode, effective = ?record.mode_effective, total = record.total, "List waveform job started");
        self.wake.notify_all();
        Ok(())
    }

    /// Blocking. Pause (`true`) or resume the current job; idempotent.
    pub fn pause_job(&self, paused: bool) -> std::result::Result<(), JobRefusal> {
        let changed = self.persist(|sched| {
            let Some(job) = sched.job.as_mut().filter(|job| job.active()) else {
                return (Err(JobRefusal::NoJob), None);
            };
            let target = if paused {
                JobState::Paused
            } else {
                JobState::Running
            };
            if job.record.state == target {
                return (Ok(false), None);
            }
            job.set_state(target);
            let record = job.mark_saved();
            if paused {
                sched.window.stop();
            } else {
                sched.window.restart(Instant::now());
            }
            let last_error = sched.last_error.clone();
            (Ok(true), Some((Some(record), last_error)))
        })?;
        if changed {
            tracing::info!(paused, "List waveform job paused/resumed");
        }
        self.wake.notify_all();
        Ok(())
    }

    /// Blocking. Stop the current job (the record being processed completes).
    pub fn stop_job(&self) -> std::result::Result<(), JobRefusal> {
        self.persist(|sched| {
            let Some(job) = sched.job.as_mut().filter(|job| job.active()) else {
                return (Err(JobRefusal::NoJob), None);
            };
            job.set_state(JobState::Cancelled);
            job.buffer.clear();
            job.requeued.clear();
            let record = job.mark_saved();
            sched.window.stop();
            let last_error = sched.last_error.clone();
            (Ok(()), Some((Some(record), last_error)))
        })?;
        tracing::info!("List waveform job stopped");
        self.wake.notify_all();
        Ok(())
    }

    /// Whether cache jobs can run: `waveforms.db` (§9.4) can be opened. Blocking (opens
    /// the database once).
    pub fn jobs_available(&self) -> bool {
        self.wfdb
            .as_ref()
            .is_some_and(crate::wfdb::WaveformsDb::usable)
    }

    /// Whether a job is running or paused (pre-generation waits for it).
    pub fn job_active(&self) -> bool {
        self.sched().job.as_ref().is_some_and(ActiveJob::active)
    }

    /// End the current job in `state` (completed / error) and persist it.
    fn end_job(
        &self,
        sched: &mut Sched,
        state: JobState,
    ) -> Option<(JobRecord, Option<LastError>)> {
        let job = sched.job.as_mut()?;
        job.set_state(state);
        let record = job.mark_saved();
        sched.window.stop();
        tracing::info!(id = %record.id, ?state, done = record.done, generated = record.generated, failed = record.failed, "List waveform job ended");
        Some((record, sched.last_error.clone()))
    }

    /// Next record of the running job. Never reads the library: a recount or the next
    /// ids are asked for ([`JobStep::Recount`] / [`JobStep::Fetch`]) and read by the
    /// worker after it released the scheduler lock. Ends the job when everything was
    /// checked.
    fn next_job_item(&self, sched: &mut Sched) -> JobStep {
        let Some(job) = sched
            .job
            .as_mut()
            .filter(|job| job.record.state == JobState::Running)
        else {
            return JobStep::Idle;
        };
        let generation = job.generation;
        if job.clearing {
            return JobStep::Idle;
        }
        if job.record.mode_effective == JobMode::Rebuild && !job.record.rebuild_cleared {
            job.clearing = true;
            return JobStep::Take(Job::ClearForRebuild(generation));
        }
        if job.recount {
            if job.fetching {
                return JobStep::Idle;
            }
            job.fetching = true;
            return JobStep::Recount(generation);
        }
        let id = if let Some(id) = job.requeued.pop_front() {
            Some(id)
        } else if let Some(list) = job.list.as_mut() {
            list.pop_front()
        } else {
            if job.buffer.is_empty() && !job.exhausted {
                if job.fetching {
                    return JobStep::Idle;
                }
                job.fetching = true;
                return JobStep::Fetch {
                    generation,
                    after: job.last_taken,
                };
            }
            let id = job.buffer.pop_front();
            if let Some(id) = id {
                job.last_taken = job.last_taken.max(id);
            }
            id
        };
        match id {
            Some(id) => {
                job.in_flight.insert(id, crate::auth::unix_now());
                JobStep::Take(Job::Cache(id, generation))
            }
            None => {
                let drained = job.list.as_ref().is_none_or(VecDeque::is_empty)
                    && (job.list.is_some() || job.exhausted)
                    && job.requeued.is_empty()
                    && job.in_flight.is_empty()
                    && !job.fetching;
                if drained {
                    if job.list.is_none() {
                        job.last_taken = job.last_taken.max(job.record.max_id);
                    }
                    if let Some(saved) = self.end_job(sched, JobState::Completed) {
                        sched.queue_save(saved);
                    }
                }
                JobStep::Idle
            }
        }
    }

    /// Blocking, without the scheduler lock. Compute `total` / `max_id` of the job of
    /// `generation` ([`JobStep::Recount`]).
    fn recount_job(&self, generation: u64) {
        let counted = self.count_present(i64::MIN).and_then(|total| {
            self.query(|conn| {
                conn.query_row("SELECT COALESCE(MAX(id),0) FROM audio_files", [], |row| {
                    row.get::<_, i64>(0)
                })
            })
            .map(|max| (total, max))
        });
        {
            let mut sched = self.sched();
            let Some(job) = sched
                .job
                .as_mut()
                .filter(|job| job.generation == generation)
            else {
                return;
            };
            job.fetching = false;
            if !job.active() {
                return;
            }
            match counted {
                Ok((total, max_id)) => {
                    job.record.total = total;
                    job.record.max_id = max_id;
                    job.recount = false;
                }
                Err(error) => {
                    if let Some(saved) = self.fail_job(&mut sched, "DATABASE", error.to_string()) {
                        sched.queue_save(saved);
                    }
                }
            }
        }
        self.wake.notify_all();
    }

    /// Blocking, without the scheduler lock. Read the ids after `after` for the job of
    /// `generation` ([`JobStep::Fetch`]).
    fn fetch_job_ids(&self, generation: u64, after: i64) {
        let ids = self.next_ids(after);
        {
            let mut sched = self.sched();
            let Some(job) = sched
                .job
                .as_mut()
                .filter(|job| job.generation == generation)
            else {
                return;
            };
            job.fetching = false;
            if !job.active() {
                return;
            }
            match ids {
                // Still the batch this job needs (else the worker reads again).
                Ok(ids) if job.last_taken == after && job.buffer.is_empty() && !job.exhausted => {
                    let max_id = job.record.max_id;
                    let ids: Vec<i64> = ids.into_iter().filter(|id| *id <= max_id).collect();
                    if ids.is_empty() {
                        job.exhausted = true;
                    }
                    job.buffer.extend(ids);
                }
                Ok(_) => {}
                Err(error) => {
                    if let Some(saved) = self.fail_job(&mut sched, "DATABASE", error.to_string()) {
                        sched.queue_save(saved);
                    }
                }
            }
        }
        self.wake.notify_all();
    }

    /// End the running job with `error` (`last_error` = `code` / `message`).
    fn fail_job(
        &self,
        sched: &mut Sched,
        code: &str,
        message: String,
    ) -> Option<(JobRecord, Option<LastError>)> {
        tracing::warn!(code, %message, "List waveform job failed");
        sched.last_error = Some(LastError {
            at: crate::auth::unix_now(),
            error_code: code.to_string(),
            message,
            id: None,
        });
        self.end_job(sched, JobState::Error)
    }

    /// Blocking. Clear this library's list cache for a `rebuild` job.
    fn clear_for_rebuild(&self, generation: u64) {
        // Only for the current, running job: a stop or a newer job must not have its
        // cache wiped by a stale request.
        {
            let mut sched = self.sched();
            match sched
                .job
                .as_mut()
                .filter(|job| job.generation == generation)
            {
                Some(job) if job.record.state == JobState::Running => {}
                Some(job) => {
                    // Paused (or ended) before the clear: clear once it runs again.
                    job.clearing = false;
                    return;
                }
                None => return,
            }
        }
        self.cache.clear();
        let cleared = match &self.wfdb {
            Some(wfdb) => wfdb.clear(),
            None => Ok(()),
        };
        tracing::info!("Cleared the list waveform cache for a rebuild");
        self.persist(|sched| {
            let current = sched
                .job
                .as_ref()
                .is_some_and(|job| job.generation == generation);
            if !current {
                return ((), None);
            }
            sched.failures.clear();
            if let Err(error) = cleared {
                let saved = self.fail_job(
                    sched,
                    "IO",
                    format!("Cannot clear the list waveform cache: {error}"),
                );
                return (
                    (),
                    saved.map(|(record, last_error)| (Some(record), last_error)),
                );
            }
            let job = sched.job.as_mut().expect("checked");
            job.clearing = false;
            job.record.rebuild_cleared = true;
            let record = job.mark_saved();
            ((), Some((Some(record), sched.last_error.clone())))
        });
        self.wake.notify_all();
    }

    /// Write a job state change decided under the scheduler lock.
    fn flush_pending_save(&self) {
        let pending = self.sched().pending_save.take();
        if let Some((seq, record, last_error)) = pending {
            self.save_state(seq, Some(record), last_error);
        }
    }

    fn idle_wait(&self) {
        let sched = self.sched();
        let _ = self
            .wake
            .wait_timeout(sched, IDLE_TICK)
            .unwrap_or_else(PoisonError::into_inner);
    }

    fn take_prune(&self) -> bool {
        std::mem::take(&mut self.sched().prune)
    }
}

/// Replace every path in `message` by its file name (error messages never carry paths).
fn path_free(message: &str, paths: &[&Path]) -> String {
    let mut text = message.to_string();
    for path in paths {
        let full = path.display().to_string();
        if full.is_empty() {
            continue;
        }
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        text = text.replace(&full, &name);
    }
    text.chars().take(300).collect()
}

/// Start the background workers of `state`. They stop when the state is dropped or
/// [`ListWaveforms::shutdown`] is called.
pub fn spawn_workers(state: &AppState) {
    let list = state.list_waveforms().clone();
    for index in 0..list.workers {
        let weak = state.downgrade();
        let list = list.clone();
        let owner = list.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("list-waveforms-{index}"))
            .spawn(move || {
                loop {
                    if list.stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let Some(state) = weak.upgrade() else {
                        break;
                    };
                    list.cache.flush_due();
                    if let Some(wfdb) = &list.wfdb {
                        wfdb.flush_due();
                    }
                    if list.take_prune() {
                        list.prune();
                    }
                    list.requeue_deferred(&state);
                    let job = list.next_job(&state);
                    list.flush_pending_save();
                    if let Some(job) = job {
                        list.run(&state, job);
                        continue;
                    }
                    drop(state);
                    list.idle_wait();
                }
                list.cache.flush_all();
            });
        match spawned {
            Ok(handle) => owner
                .worker_handles
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(handle),
            Err(error) => tracing::warn!(%error, "Cannot start a list waveform worker"),
        }
    }
}

// ---------------------------------------------------------------------------------------
// `POST /waveforms`

/// Validated `POST /waveforms` body.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BatchRequest {
    /// Unique ids in first-seen order.
    pub ids: Vec<i64>,
    /// Client-cached entity tags by id (only ids of `ids`).
    pub known: HashMap<i64, String>,
    /// Requested `encoding` (§9.4).
    pub encoding: ListEncoding,
}

impl BatchRequest {
    /// Validate a decoded JSON body; every problem is 400 `VALIDATION`.
    pub fn parse(body: &serde_json::Value) -> std::result::Result<Self, ApiError> {
        let object = body
            .as_object()
            .ok_or_else(|| ApiError::validation("The request body must be a JSON object"))?;
        let raw_ids = object
            .get("ids")
            .and_then(|ids| ids.as_array())
            .ok_or_else(|| ApiError::validation("ids must be an array of audio ids"))?;
        if raw_ids.is_empty() || raw_ids.len() > MAX_LIST_IDS {
            return Err(ApiError::validation(format!(
                "ids must contain 1..={MAX_LIST_IDS} audio ids, got {}",
                raw_ids.len()
            )));
        }
        let mut ids = Vec::with_capacity(raw_ids.len());
        let mut seen = HashSet::with_capacity(raw_ids.len());
        for raw in raw_ids {
            let id = raw.as_i64().filter(|id| *id > 0).ok_or_else(|| {
                ApiError::validation(format!("ids must be positive integers, got {raw}"))
            })?;
            if seen.insert(id) {
                ids.push(id);
            }
        }
        match object.get("buckets") {
            None | Some(serde_json::Value::Null) => {}
            Some(value) if value.as_u64() == Some(u64::from(LIST_BUCKETS)) => {}
            Some(_) => {
                return Err(ApiError::validation(format!(
                    "buckets must be {LIST_BUCKETS} for list waveforms"
                )));
            }
        }
        let mut known = HashMap::new();
        match object.get("known") {
            None | Some(serde_json::Value::Null) => {}
            Some(serde_json::Value::Object(map)) => {
                for (key, value) in map {
                    let id = key
                        .parse::<i64>()
                        .ok()
                        .filter(|id| *id > 0 && key.bytes().all(|b| b.is_ascii_digit()))
                        .ok_or_else(|| {
                            ApiError::validation(format!(
                                "known keys must be positive audio ids, got {key:?}"
                            ))
                        })?;
                    let etag = value.as_str().ok_or_else(|| {
                        ApiError::validation(format!("known[{key:?}] must be a string"))
                    })?;
                    if seen.contains(&id) {
                        known.insert(id, etag.to_string());
                    }
                }
            }
            Some(_) => return Err(ApiError::validation("known must be an object")),
        }
        let encoding = match object.get("encoding") {
            None | Some(serde_json::Value::Null) => ListEncoding::default(),
            Some(serde_json::Value::String(name)) => ListEncoding::parse(name)?,
            Some(_) => return Err(ApiError::validation("encoding must be a string")),
        };
        Ok(Self {
            ids,
            known,
            encoding,
        })
    }
}

/// Per-item `status`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    /// Cached for the current file version.
    Ok,
    /// Same entity tag as `known`.
    NotModified,
    /// Queued; retry after `retry_after`.
    Pending,
    /// No such record, marked missing, or the file is gone.
    Missing,
    /// The record's root is offline.
    RootOffline,
    /// Cannot be decoded (not retried until the file changes).
    Error,
}

/// One item of a `POST /waveforms` reply.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchItem {
    /// Record id.
    pub id: i64,
    /// Outcome.
    pub status: ItemStatus,
    /// Entity tag (`ok`, `not_modified`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    /// Columns (`ok`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n: Option<usize>,
    /// base64 int16 LE min/max pairs (`ok`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peaks: Option<String>,
    /// Root name (`root_offline`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    /// `AUDIO` or `IO` (`error`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Path-free reason (`error`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl BatchItem {
    fn bare(id: i64, status: ItemStatus) -> Self {
        Self {
            id,
            status,
            etag: None,
            n: None,
            peaks: None,
            root: None,
            error_code: None,
            message: None,
        }
    }

    fn root_offline(id: i64, root: String) -> Self {
        Self {
            root: Some(root),
            ..Self::bare(id, ItemStatus::RootOffline)
        }
    }

    fn error(id: i64, code: &str, message: String) -> Self {
        Self {
            error_code: Some(code.into()),
            message: Some(message),
            ..Self::bare(id, ItemStatus::Error)
        }
    }
}

/// `POST /waveforms` reply.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchResponse {
    /// Identity of the library that answered.
    pub library_id: String,
    /// Always 128.
    pub buckets: u32,
    /// Encoding of every `peaks`; equals the request's (§9.4).
    pub encoding: String,
    /// Suggested retry interval for `pending` items (seconds), `null` without any.
    pub retry_after: Option<u32>,
    /// One item per unique requested id, in request order.
    pub items: Vec<BatchItem>,
}

/// Blocking. Answer one item from file metadata and the cache (never decodes).
fn lookup(
    state: &AppState,
    id: i64,
    path: &str,
    index: Option<usize>,
    known: Option<&str>,
    encoding: ListEncoding,
) -> BatchItem {
    let list = state.list_waveforms();
    let hinted = list.hint(id).map(|hint| (hint.canonical, hint.file_etag));
    let from_hint = hinted.is_some();
    let stat = match hinted {
        Some(found) => Ok(found),
        None => state.stat_audio(id, path, index),
    };
    let (canonical, file_etag) = match stat {
        Ok(found) => found,
        Err(StatFailure::Missing) => return BatchItem::bare(id, ItemStatus::Missing),
        Err(StatFailure::RootOffline { root }) => return BatchItem::root_offline(id, root),
        Err(StatFailure::Other(error)) => {
            return BatchItem::error(id, "IO", path_free(&error.to_string(), &[Path::new(path)]));
        }
    };
    let hash = WaveformKey::new(
        state.library_id(),
        id,
        &canonical,
        &file_etag,
        &list_params(),
    )
    .hash();
    match list.cache.get(id) {
        Some((cached, values)) if cached == hash => {
            let etag = encoding.etag(hash);
            if encoding == ListEncoding::I8 {
                list.store_i8(id, hash, &canonical, &values);
            }
            if known == Some(etag.as_str()) {
                BatchItem {
                    etag: Some(etag),
                    ..BatchItem::bare(id, ItemStatus::NotModified)
                }
            } else {
                BatchItem {
                    etag: Some(etag),
                    n: Some(values.len() / 2),
                    peaks: Some(encoding.encode(&values)),
                    ..BatchItem::bare(id, ItemStatus::Ok)
                }
            }
        }
        _ => match list.failure(id, hash) {
            Some(failure) => BatchItem::error(id, failure.code, failure.message),
            None => {
                if !from_hint {
                    list.store_hint(id, canonical, file_etag);
                }
                BatchItem::bare(id, ItemStatus::Pending)
            }
        },
    }
}

/// `POST /waveforms` (§9.1): at most [`MAX_BATCH_REQUESTS`] at once (503 `BUSY`), a
/// 2 s metadata budget with 8 parallel `stat`s, misses queued for the workers.
pub async fn batch(
    state: AppState,
    request: BatchRequest,
) -> std::result::Result<BatchResponse, ApiError> {
    let list = state.list_waveforms().clone();
    let _admission = list
        .requests
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::list_waveforms_busy())?;
    let BatchRequest {
        ids,
        known,
        encoding,
    } = request;
    let started = Instant::now();
    let records = {
        let list = list.clone();
        let ids = ids.clone();
        tokio::task::spawn_blocking(move || list.records(&ids))
            .await
            .map_err(|error| ApiError::internal(format!("Waveform task failed: {error}")))??
    };
    let monitor = state.root_monitor();
    let unfinished_limit = state
        .open_slot_count()
        .saturating_mul(UNFINISHED_STATS_PER_SLOT);
    let db_ms = started.elapsed().as_millis() as u64;
    let mut items: Vec<Option<BatchItem>> = vec![None; ids.len()];
    let mut budget_pending = false;
    let mut budget_exhausted = false;
    let mut open_limited = 0_usize;
    let mut deduped = 0_usize;
    let mut capped = 0_usize;
    let mut paused_for_playback = 0_usize;
    let pause_at_start = list.stat_pause_remaining();
    // Diagnostics (one log line per batch): records per root name.
    let mut per_root: BTreeMap<String, usize> = BTreeMap::new();
    // Reads run as detached tasks: those unfinished when the budget runs out keep going
    // in the background (bounded by `stat_slots`, `MAX_OUTSTANDING_STATS` and the
    // per-root limit) and leave a hint the workers and later requests reuse.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut stats = 0_usize;
    for (position, id) in ids.iter().copied().enumerate() {
        let Some((path, missing)) = records.get(&id) else {
            items[position] = Some(BatchItem::bare(id, ItemStatus::Missing));
            continue;
        };
        if *missing {
            items[position] = Some(BatchItem::bare(id, ItemStatus::Missing));
            continue;
        }
        let index = monitor.root_index_for(Path::new(path));
        let root_name = index.map_or("-", |index| monitor.roots()[index].name.as_str());
        *per_root.entry(root_name.to_string()).or_default() += 1;
        if let Some(index) = index {
            // Offline roots answer from the cached status: no disk access, no recheck.
            if monitor.status(index).state == crate::RootState::Offline {
                items[position] = Some(BatchItem::root_offline(
                    id,
                    monitor.roots()[index].name.clone(),
                ));
                continue;
            }
            if monitor.opens_in_progress(index) >= unfinished_limit {
                budget_pending = true;
                open_limited += 1;
                items[position] = Some(BatchItem::bare(id, ItemStatus::Pending));
                continue;
            }
        }
        if pause_at_start.is_some() {
            // Prebuffer window of a starting stream/preview: no metadata reads now.
            paused_for_playback += 1;
            items[position] = Some(BatchItem::bare(id, ItemStatus::Pending));
            continue;
        }
        if list.stats.contains(id) {
            // Already being read (an earlier request's background read).
            deduped += 1;
            items[position] = Some(BatchItem::bare(id, ItemStatus::Pending));
            continue;
        }
        if !list.stats.try_begin(id) {
            // Too many reads outstanding: left to the on-demand workers.
            capped += 1;
            items[position] = Some(BatchItem::bare(id, ItemStatus::Pending));
            continue;
        }
        let claim = StatClaim {
            list: list.clone(),
            id,
        };
        let state = state.clone();
        let path = path.clone();
        let known = known.get(&id).cloned();
        let tx = tx.clone();
        stats += 1;
        tokio::spawn(async move {
            let Ok(_slot) = claim.list.stat_slots.clone().acquire_owned().await else {
                return;
            };
            // A stream/preview started meanwhile: wait out its prebuffer window.
            while let Some(left) = claim.list.stat_pause_remaining() {
                tokio::time::sleep(left).await;
            }
            if let Some(index) = index
                && state.root_monitor().opens_in_progress(index) >= unfinished_limit
            {
                // The root has too many unfinished opens: the workers retry it later.
                let _ = tx.send((position, None));
                return;
            }
            let result = tokio::task::spawn_blocking(move || {
                let started = Instant::now();
                let item = lookup(&state, id, &path, index, known.as_deref(), encoding);
                drop(claim);
                (item, started.elapsed())
            })
            .await;
            if let Ok(result) = result {
                // The request may have answered already; the hint stays either way.
                let _ = tx.send((position, Some(result)));
            }
        });
    }
    drop(tx);
    let stat_started = Instant::now();
    let mut received = 0_usize;
    let mut finished = 0_usize;
    let mut slowest = Duration::ZERO;
    let deadline = tokio::time::Instant::now() + STAT_BUDGET;
    while received < stats {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some((position, result))) => {
                received += 1;
                match result {
                    Some((item, took)) => {
                        items[position] = Some(item);
                        finished += 1;
                        slowest = slowest.max(took);
                    }
                    None => open_limited += 1,
                }
            }
            Ok(None) => break,
            Err(_) => {
                budget_exhausted = true;
                break;
            }
        }
    }
    let stat_elapsed = stat_started.elapsed();
    let stat_ms = stat_elapsed.as_millis() as u64;
    let unfinished = stats - received;
    if open_limited > 0 {
        budget_pending = true;
    }
    let items: Vec<BatchItem> = items
        .into_iter()
        .zip(&ids)
        .map(|(item, id)| item.unwrap_or_else(|| BatchItem::bare(*id, ItemStatus::Pending)))
        .collect();
    let pending: Vec<i64> = items
        .iter()
        .filter(|item| item.status == ItemStatus::Pending)
        .map(|item| item.id)
        .collect();
    list.enqueue_on_demand(&pending);
    let pause_wait = pause_at_start
        .or_else(|| list.stat_pause_remaining())
        .map(|left| left.as_secs_f64());
    let stat_wait = if budget_pending {
        // A root hit its unfinished-open limit: give it the longest interval.
        Some(f64::INFINITY)
    } else if budget_exhausted {
        Some(stat_wait(unfinished, finished, stat_elapsed))
    } else {
        None
    };
    let stat_wait = match (stat_wait, pause_wait) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };
    let retry_after = (!pending.is_empty()).then(|| list.retry_after_for(&pending, stat_wait));
    let count = |status: ItemStatus| items.iter().filter(|item| item.status == status).count();
    tracing::info!(
        ids = ids.len(),
        db_ms,
        stat_ms,
        slowest_stat_ms = slowest.as_millis() as u64,
        stats,
        unfinished_stats = unfinished,
        deduped,
        capped,
        paused_for_playback,
        outstanding_stats = list.outstanding_stats(),
        budget_exhausted,
        open_limited,
        ok = count(ItemStatus::Ok),
        not_modified = count(ItemStatus::NotModified),
        pending = pending.len(),
        missing = count(ItemStatus::Missing),
        root_offline = count(ItemStatus::RootOffline),
        error = count(ItemStatus::Error),
        retry_after = ?retry_after,
        queued_on_demand = list.queued_on_demand(),
        on_demand_per_sec = ?list.on_demand_rate().map(|rate| (rate * 10.0).round() / 10.0),
        total_ms = started.elapsed().as_millis() as u64,
        roots = ?per_root,
        "List waveform batch"
    );
    Ok(BatchResponse {
        library_id: state.library_id().to_string(),
        buckets: LIST_BUCKETS,
        encoding: encoding.name().to_string(),
        retry_after,
        items,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peak(min: f32, max: f32) -> WaveformPeak {
        WaveformPeak {
            min,
            max,
            ..WaveformPeak::default()
        }
    }

    #[test]
    fn single_open_compute_matches_the_probed_two_pass_summary() {
        fn chunk(id: &[u8; 4], data: &[u8], out: &mut Vec<u8>) {
            out.extend_from_slice(id);
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(data);
            if data.len() % 2 == 1 {
                out.push(0);
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let cancel = AtomicBool::new(false);
        for (frames, trailing) in [(0_u32, true), (1, false), (127, true), (99_991, true)] {
            let mut fmt = Vec::new();
            for value in [1_u16, 2] {
                fmt.extend_from_slice(&value.to_le_bytes());
            }
            fmt.extend_from_slice(&48_000_u32.to_le_bytes());
            fmt.extend_from_slice(&192_000_u32.to_le_bytes());
            fmt.extend_from_slice(&4_u16.to_le_bytes());
            fmt.extend_from_slice(&16_u16.to_le_bytes());
            let samples: Vec<u8> = (0..frames as usize * 4)
                .map(|i| ((i * 2_654_435_761) >> 11) as u8)
                .collect();
            let mut body = Vec::new();
            chunk(b"fmt ", &fmt, &mut body);
            chunk(b"data", &samples, &mut body);
            if trailing {
                chunk(b"SMED", &[3; 4097], &mut body);
                chunk(b"iXML", b"<BWFXML/>", &mut body);
            }
            let mut wav = b"RIFF".to_vec();
            wav.extend_from_slice(&(body.len() as u32 + 4).to_le_bytes());
            wav.extend_from_slice(b"WAVE");
            wav.extend_from_slice(&body);
            let path = dir.path().join(format!("{frames}.wav"));
            std::fs::write(&path, wav).unwrap();
            assert_eq!(
                compute(&path, &cancel).unwrap(),
                compute_probed(&path, &cancel).unwrap(),
                "frames={frames}"
            );
        }
    }

    #[test]
    fn malformed_or_unsupported_sources_fail_both_compute_paths() {
        // Valid bundled source formats now reach both summary paths. Retain
        // rejection coverage for malformed audio and the unsupported Opus decoder.
        let dir = tempfile::tempdir().unwrap();
        let cancel = AtomicBool::new(false);
        let mut flac = b"fLaC".to_vec();
        flac.extend_from_slice(&[0x80, 0, 0, 34]);
        flac.extend_from_slice(&[0; 34]);
        let mut mp3 =
            include_bytes!("../../../crates/audiolib-audio/src/fixtures/mp3/cbr-info.mp3").to_vec();
        mp3.pop();
        let opus =
            include_bytes!("../../../crates/audiolib-audio/src/fixtures/vorbis/comments.opus")
                .to_vec();
        for (name, bytes) in [("a.flac", flac), ("a.mp3", mp3), ("a.opus", opus)] {
            let path = dir.path().join(name);
            std::fs::write(&path, bytes).unwrap();
            assert!(compute(&path, &cancel).is_err(), "{name}");
            assert!(compute_probed(&path, &cancel).is_err(), "{name}");
        }
    }

    #[test]
    fn supported_mp3_has_equivalent_list_compute_paths() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("owned.mp3");
        std::fs::write(
            &path,
            include_bytes!("../../../crates/audiolib-audio/src/fixtures/mp3/cbr-info.mp3"),
        )
        .unwrap();
        let cancel = AtomicBool::new(false);
        let actual = compute(&path, &cancel).unwrap();
        assert!(actual.iter().any(|peak| *peak != 0));
        assert_eq!(actual, compute_probed(&path, &cancel).unwrap());
    }

    #[test]
    fn base64_matches_rfc4648_vectors() {
        for (input, output) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), output);
        }
        assert_eq!(base64(&[0xfb, 0xff]), "+/8=");
    }

    #[test]
    fn channels_merge_into_a_quantized_envelope_union() {
        let left = vec![peak(-0.5, 0.25), peak(-2.0, 0.0)];
        let right = vec![peak(-0.25, 0.75), peak(0.0, 1.5)];
        assert_eq!(
            merge_channels(&[left, right]),
            vec![-16384, 24575, -32767, 32767]
        );
        assert_eq!(quantize(f32::NAN), 0);
        assert!(merge_channels(&[]).is_empty());
        let long = vec![peak(0.0, 0.0); 200];
        assert_eq!(merge_channels(&[long]).len(), 256);
        assert_eq!(encode_peaks(&[1, -1]), base64(&[1, 0, 0xff, 0xff]));
    }

    #[test]
    fn entity_tags_share_the_detail_key_and_add_the_list_version() {
        let key = WaveformKey::new("lib", 7, Path::new("a.wav"), "\"x\"", &list_params());
        let detail = key.etag(false);
        let list = list_etag(key.hash());
        assert_eq!(list, format!("{}-l1\"", detail.trim_end_matches('"')));
    }

    #[test]
    fn shards_round_trip_and_reject_corruption() {
        let mut entries = BTreeMap::new();
        entries.insert(
            5,
            Entry {
                hash: 9,
                values: vec![1, 2, -3, 4].into_boxed_slice(),
            },
        );
        entries.insert(
            6,
            Entry {
                hash: 10,
                values: Vec::new().into_boxed_slice(),
            },
        );
        let bytes = encode_shard(&entries);
        assert_eq!(bytes.len(), SHARD_HEADER + 2 * RECORD_HEADER + 8);
        assert_eq!(decode_shard(&bytes), Some(entries));
        assert_eq!(decode_shard(&bytes[..bytes.len() - 1]), None);
        assert_eq!(decode_shard(b"nope"), None);
    }

    #[test]
    fn cache_writes_atomically_and_evicts_least_recently_used_shards() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ListCache::open(dir.path().join("lib"), 1);
        let values = vec![7_i16; 256];
        // A full shard is ~536 KiB, so a 1 MiB limit holds only one of them.
        for shard in 0..3_i64 {
            for slot in 0..1024_i64 {
                cache.put(shard * 1024 + slot, 1, values.clone());
            }
            cache.flush_all();
        }
        assert!(cache.over_budget());
        assert!(cache.bytes() <= cache.limit_bytes());
        assert!(cache.get(2 * 1024).is_some(), "the newest shard stays");
        let files: Vec<String> = fs::read_dir(cache.dir())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            files.iter().all(|name| name.starts_with("shard-")),
            "{files:?}"
        );
        // A reopened cache indexes what is on disk.
        let reopened = ListCache::open(dir.path().join("lib"), 1);
        assert_eq!(reopened.bytes(), cache.bytes());
        assert_eq!(reopened.entries(), cache.entries());
    }

    #[test]
    fn requests_are_validated() {
        let parse = |text: &str| BatchRequest::parse(&serde_json::from_str(text).unwrap());
        let ok = parse(r#"{"ids":[3,1,3],"buckets":128,"known":{"1":"a","9":"b"}}"#).unwrap();
        assert_eq!(ok.ids, vec![3, 1]);
        assert_eq!(ok.known, HashMap::from([(1, "a".to_string())]));
        assert!(parse(r#"{"ids":[1]}"#).is_ok());
        let too_many = format!(
            r#"{{"ids":[{}]}}"#,
            (1..=201)
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(",")
        );
        for bad in [
            r#"{"ids":[]}"#,
            r#"[1]"#,
            r#"{"ids":[0]}"#,
            r#"{"ids":[-1]}"#,
            r#"{"ids":[1.5]}"#,
            r#"{"ids":["1"]}"#,
            r#"{"ids":[1],"buckets":64}"#,
            r#"{"ids":[1],"known":{"x":"a"}}"#,
            r#"{"ids":[1],"known":{"1":5}}"#,
            r#"{"ids":[1],"known":[]}"#,
            too_many.as_str(),
        ] {
            let error = parse(bad).unwrap_err();
            assert_eq!(error.code(), "VALIDATION", "{bad}");
        }
    }

    #[test]
    fn on_demand_queue_takes_newest_first_and_drops_the_oldest() {
        let mut queue = OnDemand::default();
        for id in 1..=(ON_DEMAND_CAPACITY as i64 + 5) {
            queue.push_newest(id);
        }
        assert_eq!(queue.len(), ON_DEMAND_CAPACITY);
        queue.push_newest(10);
        assert_eq!(queue.pop_newest(), Some(10));
        assert_eq!(queue.pop_newest(), Some(ON_DEMAND_CAPACITY as i64 + 5));
        assert!(!queue.seq_of.contains_key(&1));
    }

    #[test]
    fn queue_depth_counts_up_to_the_last_taken_id() {
        let mut queue = OnDemand::default();
        for id in 1..=10 {
            queue.push_newest(id);
        }
        // Taken newest first: 10, 9, ..., 1.
        assert_eq!(queue.depth_of(&[10]), 1);
        assert_eq!(queue.depth_of(&[9, 3]), 8);
        assert_eq!(queue.depth_of(&[42]), 0);
        assert_eq!(queue.depth_of(&[]), 0);
    }

    #[test]
    fn throughput_ignores_idle_time_and_smooths() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut throughput = Throughput::default();
        assert_eq!(throughput.per_second(2), 2.0 * DEFAULT_ON_DEMAND_RATE);
        throughput.record(at(0), at(100));
        assert!((throughput.per_second(1) - 10.0).abs() < 1e-6);
        // Taken long after the previous completion: only its own 100 ms count.
        throughput.record(at(60_000), at(60_100));
        assert!((throughput.per_second(1) - 10.0).abs() < 1e-6);
        // Back-to-back completions 50 ms apart pull the average up.
        throughput.record(at(60_000), at(60_150));
        assert!(throughput.per_second(1) > 10.0);
    }

    #[test]
    fn retry_after_follows_the_queue_position_within_one_to_five_seconds() {
        assert_eq!(estimate_retry_after(0, 14.0, None), 1);
        assert_eq!(estimate_retry_after(14, 14.0, None), 1);
        assert_eq!(estimate_retry_after(15, 14.0, None), 2);
        assert_eq!(estimate_retry_after(42, 14.0, None), 3);
        assert_eq!(estimate_retry_after(200, 14.0, None), 5);
        assert_eq!(estimate_retry_after(10_000, 0.0, None), 5);
        // Background metadata reads still running: at least their expected time.
        assert_eq!(estimate_retry_after(1, 1000.0, Some(f64::INFINITY)), 5);
        assert_eq!(estimate_retry_after(1, 1000.0, Some(2.2)), 3);
        assert_eq!(estimate_retry_after(42, 14.0, Some(0.5)), 3);
    }

    #[test]
    fn background_stat_wait_follows_the_measured_rate() {
        let two = Duration::from_secs(2);
        assert_eq!(stat_wait(0, 0, two), 0.0);
        assert!(stat_wait(10, 0, two).is_infinite());
        assert!((stat_wait(147, 53, two) - 5.547).abs() < 0.01);
        assert!((stat_wait(20, 100, two) - 0.4).abs() < 1e-9);
    }

    #[test]
    fn stat_tracker_dedupes_and_bounds_outstanding_reads() {
        let tracker = StatTracker::default();
        assert!(tracker.try_begin(1));
        assert!(!tracker.try_begin(1), "already being read");
        for id in 2..=MAX_OUTSTANDING_STATS as i64 {
            assert!(tracker.try_begin(id));
        }
        assert_eq!(tracker.len(), MAX_OUTSTANDING_STATS);
        assert!(!tracker.try_begin(10_000), "bounded");
        tracker.end(1);
        assert!(!tracker.contains(1));
        assert!(tracker.try_begin(10_000));
    }

    fn job_service(dir: &Path) -> ListWaveforms {
        ListWaveforms::new(
            ListOptions {
                cache_root: dir.join("cache"),
                cache_mb: 16,
                workers: 1,
                rate: 10,
                pause_stats_on_playback: false,
            },
            "lib-test",
            &dir.join("absent.db"),
        )
    }

    #[test]
    fn update_waits_for_cache_writer_and_refuses_deployment_while_it_is_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let list = job_service(dir.path());
        let (finished, wait) = std::sync::mpsc::channel::<()>();
        let worker = std::thread::spawn(move || {
            let _ = wait.recv();
        });
        list.worker_handles.lock().unwrap().push(worker);
        list.shutdown();
        assert!(matches!(
            list.drain_for_update(Duration::ZERO),
            Err(Error::Busy(_))
        ));
        finished.send(()).unwrap();
        list.drain_for_update(Duration::from_secs(2)).unwrap();
        assert!(list.worker_handles.lock().unwrap().is_empty());
    }

    fn running_record() -> JobRecord {
        JobRecord {
            id: "wfj-1-abcd".into(),
            mode: JobMode::Missing,
            mode_effective: JobMode::Missing,
            state: JobState::Running,
            paused: false,
            started_at: 1,
            updated_at: 1,
            finished_at: None,
            cursor: 0,
            max_id: 100,
            total: 100,
            done: 0,
            generated: 0,
            skipped: 0,
            failed: 0,
            skipped_offline: 0,
            rebuild_cleared: false,
        }
    }

    fn saved_state(list: &ListWaveforms) -> JobState {
        match wf_job::load(list.state_dir(), "lib-test") {
            wf_job::Loaded::State(file) => file.job.expect("job").state,
            _ => panic!("no state file"),
        }
    }

    /// A worker's progress snapshot taken before a pause / stop but written after it
    /// must not bring the running state back (and the job stays paused / stopped
    /// after a restart).
    #[test]
    fn stale_job_state_snapshots_are_not_written() {
        let dir = tempfile::tempdir().unwrap();
        for (action, expected) in [("pause", JobState::Paused), ("stop", JobState::Cancelled)] {
            let list = job_service(dir.path());
            {
                let mut sched = list.sched();
                sched.job_generation += 1;
                let generation = sched.job_generation;
                sched.job = Some(ActiveJob::new(generation, running_record(), false, None));
            }
            // A worker finished a record and snapshots the running job...
            let (seq, stale) = {
                let mut sched = list.sched();
                let record = sched.job.as_mut().unwrap().mark_saved();
                (sched.next_save_seq(), record)
            };
            assert_eq!(stale.state, JobState::Running);
            // ...the client pauses / stops before the worker writes it...
            match action {
                "pause" => list.pause_job(true).unwrap(),
                _ => list.stop_job().unwrap(),
            }
            assert_eq!(saved_state(&list), expected, "{action}");
            // ...and the late write is skipped.
            list.save_state(seq, Some(stale), None);
            assert_eq!(saved_state(&list), expected, "{action}");
            list.shutdown();
            drop(list);
            let list = job_service(dir.path());
            let restored = list.sched().job.as_ref().unwrap().record.state;
            assert_eq!(restored, expected, "{action} after a restart");
            assert_eq!(list.job_active(), expected == JobState::Paused);
            fs::remove_file(list.state_dir().join(wf_job::STATE_FILE)).unwrap();
        }
    }

    /// Records skipped for an offline root are requeued (and counted again) once the
    /// root is online, and taken before the walk continues.
    #[test]
    fn offline_records_are_requeued_when_their_root_is_online() {
        let dir = tempfile::tempdir().unwrap();
        let list = job_service(dir.path());
        let mut job = ActiveJob::new(1, running_record(), false, None);
        for id in [5, 6] {
            job.in_flight.insert(id, 0);
            job.finish_item(id, &Outcome::Offline("SFX".into()));
        }
        job.in_flight.insert(7, 0);
        job.finish_item(7, &Outcome::Offline("NAS".into()));
        assert_eq!((job.record.done, job.record.skipped_offline), (3, 3));
        job.requeue_online(|root| root == "SFX");
        assert_eq!(job.requeued, VecDeque::from([5, 6]));
        assert_eq!((job.record.done, job.record.skipped_offline), (1, 1));
        assert!(job.deferred.contains_key("NAS") && !job.deferred.contains_key("SFX"));
        // Taken first, without reading the library.
        job.exhausted = false;
        let mut sched = list.sched();
        sched.job = Some(job);
        assert!(matches!(
            list.next_job_item(&mut sched),
            JobStep::Take(Job::Cache(5, 1))
        ));
        assert!(matches!(
            list.next_job_item(&mut sched),
            JobStep::Take(Job::Cache(6, 1))
        ));
        // Then the walk asks for the next ids (read outside the lock).
        assert!(matches!(
            list.next_job_item(&mut sched),
            JobStep::Fetch {
                generation: 1,
                after: 0
            }
        ));
        // Only one worker reads at a time.
        assert!(matches!(list.next_job_item(&mut sched), JobStep::Idle));
    }
}
