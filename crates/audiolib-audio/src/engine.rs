// crates/audiolib-audio/src/engine.rs
use crate::audition::{AuditionDecoder, AuditionSettings};
use crate::decoder::Decoder;
use audiolib_core::{Error, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

// SDK lifecycle/control calls are serialized across UI discovery and engine workers.
// The data callback never acquires this lock. While a stream exists, discovery returns
// owned cached descriptions and never loads another global ASIO driver.
static ASIO_ACCESS: Mutex<()> = Mutex::new(());
static ACTIVE_ASIO_STREAMS: AtomicUsize = AtomicUsize::new(0);
static ASIO_DEVICE_CACHE: Mutex<Option<Vec<AudioDevice>>> = Mutex::new(None);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioBackend {
    #[default]
    Wasapi,
    Asio,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RatePolicy {
    #[default]
    DeviceDefault,
    Native,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct PlaybackConfig {
    pub backend: AudioBackend,
    pub device_id: Option<String>,
    pub buffer_frames: u32,
    pub output_pair: u16,
    pub source_pair: u16,
    pub volume: f32,
    pub rate_policy: RatePolicy,
    pub audition: AuditionSettings,
}
impl Default for PlaybackConfig {
    fn default() -> Self {
        Self {
            backend: AudioBackend::Wasapi,
            device_id: None,
            buffer_frames: 0,
            output_pair: 0,
            source_pair: 0,
            volume: 0.8,
            rate_policy: RatePolicy::DeviceDefault,
            audition: AuditionSettings::default(),
        }
    }
}
impl PlaybackConfig {
    fn validate(&self) -> Result<()> {
        self.audition.validate()?;
        if !self.volume.is_finite() || !(0.0..=1.0).contains(&self.volume) {
            return Err(Error::Validation("Volume must be 0..=1".into()));
        }
        if self.buffer_frames > 65536 || self.output_pair > 127 || self.source_pair > 127 {
            return Err(Error::Validation(
                "Invalid output buffer or channel pair".into(),
            ));
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackState {
    #[default]
    Empty,
    Loading,
    Ready,
    Playing,
    Paused,
    Stopped,
    Ended,
    Error,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PlaybackStatus {
    /// Confirmed Pitch, Rate, mono and repeat settings.
    pub audition: AuditionSettings,
    pub generation: u64,
    /// Revision of the confirmed playback clock, independent of the loaded source
    /// generation and playback history session. Successful source/stream resets,
    /// Seek (including loops), Stop, Pause and Play transitions advance it so GUI
    /// prediction can discard an old clock even when the position stays the same.
    /// Ordinary progress, volume changes and rejected commands retain the revision.
    #[serde(default)]
    pub transport_revision: u64,
    /// Successful playback sessions in this loaded generation. Pause, Seek and
    /// device reconfiguration retain the session; Stop/EOF followed by Play starts another.
    #[serde(default)]
    pub play_session_id: u64,
    pub state: PlaybackState,
    pub file_path: Option<PathBuf>,
    pub duration: f64,
    pub position: f64,
    pub sample_rate: u32,
    pub device_sample_rate: u32,
    pub channels: u16,
    pub volume: f32,
    pub underruns: u64,
    pub error: Option<String>,
    pub active_backend: Option<AudioBackend>,
    pub active_device_id: Option<String>,
    pub actual_buffer_frames: u32,
    /// Source pair used by the currently retained output stream, including while paused.
    pub active_source_pair: Option<u16>,
    /// Hardware output pair used by that stream; None after the stream is closed.
    pub active_output_pair: Option<u16>,
    /// Rate policy of that stream, rather than an unconfirmed settings selection.
    pub active_rate_policy: Option<RatePolicy>,
    /// Playing, but paused until a network source has buffered enough audio (before the
    /// first sound, after a seek, or after the buffer ran dry). Always false for files.
    #[serde(default)]
    pub buffering: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AudioDevice {
    pub id: String,
    pub name: String,
    pub backend: AudioBackend,
    pub max_output_channels: u16,
    pub default_sample_rate: u32,
    pub is_default: bool,
    pub buffer_min: Option<u32>,
    pub buffer_max: Option<u32>,
    pub buffer_preferred: Option<u32>,
    pub buffer_granularity: Option<i32>,
    pub output_channel_names: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AudioCommand {
    Load(PathBuf),
    Play,
    Pause,
    Stop,
    Seek(f64),
    /// Seek and then start/resume playback as one ordered control operation.
    /// A failed seek must never fall through to playback at the old position.
    SeekAndPlay(f64),
    SetVolume(f32),
    SetAudition(AuditionSettings),
    /// Acknowledge after all preceding control commands have completed.
    Synchronize,
    Configure(PlaybackConfig),
    /// Replace a representation of the same source while reconfiguring output.
    /// The generation/path guard rejects stale asynchronous preparations. Unlike
    /// Load, this retains the logical source generation, position and play session.
    ConfigureSource {
        expected_generation: u64,
        expected_path: PathBuf,
        path: PathBuf,
        config: PlaybackConfig,
    },
    ControlPanel,
    Shutdown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AudioEvent {
    /// A successful new playback session, with its immutable source identity.
    /// The cumulative ID also lets status consumers recover a dropped bounded event.
    PlaybackStarted {
        generation: u64,
        play_session_id: u64,
        file_path: PathBuf,
    },
    StateChanged {
        generation: u64,
        state: PlaybackState,
    },
    Position {
        generation: u64,
        position: f64,
    },
    Error {
        generation: u64,
        message: String,
    },
    DeviceLost {
        generation: u64,
        message: String,
    },
}
struct EventSubscriber {
    sender: SyncSender<AudioEvent>,
    playback_only: bool,
}
type Subscribers = Arc<Mutex<Vec<EventSubscriber>>>;
struct CommandRequest {
    command: AudioCommand,
    acknowledgment: Option<SyncSender<Result<PlaybackStatus>>>,
}

fn audio_error(error: impl std::fmt::Display) -> Error {
    Error::Audio(error.to_string())
}
fn host(backend: AudioBackend) -> Result<cpal::Host> {
    match backend {
        AudioBackend::Wasapi => {
            #[cfg(windows)]
            {
                cpal::host_from_id(cpal::HostId::Wasapi).map_err(audio_error)
            }
            #[cfg(not(windows))]
            {
                Ok(cpal::default_host())
            }
        }
        AudioBackend::Asio => {
            #[cfg(all(windows, feature = "asio"))]
            {
                cpal::host_from_id(cpal::HostId::Asio).map_err(audio_error)
            }
            #[cfg(not(all(windows, feature = "asio")))]
            {
                Err(Error::Audio("ASIO is not enabled in this build".into()))
            }
        }
    }
}
fn identity(device: &cpal::Device) -> Result<String> {
    Ok(device.id().map_err(audio_error)?.to_string())
}

/// Enumerate stable CPAL device IDs, x64 ASIO aliases, and available buffer ranges.
pub fn list_devices(backend: AudioBackend) -> Result<Vec<AudioDevice>> {
    let _driver_guard = if backend == AudioBackend::Asio {
        Some(
            ASIO_ACCESS
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        )
    } else {
        None
    };
    if backend == AudioBackend::Asio && ACTIVE_ASIO_STREAMS.load(Ordering::Acquire) > 0 {
        return Ok(ASIO_DEVICE_CACHE
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .unwrap_or_default());
    }
    let host = host(backend)?;
    let default = host
        .default_output_device()
        .as_ref()
        .and_then(|device| identity(device).ok());
    let aliases = if backend == AudioBackend::Asio {
        audiolib_windows::asio_drivers()?
    } else {
        Vec::new()
    };
    let mut seen = std::collections::HashSet::new();
    let mut devices = Vec::new();
    for device in host.output_devices().map_err(audio_error)? {
        let name = device
            .description()
            .map_err(audio_error)?
            .name()
            .to_string();
        let dedupe = aliases
            .iter()
            .find(|driver| driver.name == name || driver.aliases.contains(&name))
            .map(|driver| driver.clsid.clone())
            .unwrap_or_else(|| name.clone());
        if backend == AudioBackend::Asio && !seen.insert(dedupe) {
            continue;
        }
        let id = identity(&device)?;
        let Ok(config) = device.default_output_config() else {
            continue;
        };
        let max_output_channels = device
            .supported_output_configs()
            .ok()
            .and_then(|configs| configs.map(|config| config.channels()).max())
            .unwrap_or(config.channels());
        let (buffer_min, buffer_max) = match config.buffer_size() {
            cpal::SupportedBufferSize::Range { min, max } => (Some(*min), Some(*max)),
            _ => (None, None),
        };
        let range = if backend == AudioBackend::Asio {
            // SAFETY: ASIO_ACCESS is held and the current iterator retains this driver.
            unsafe { audiolib_windows::loaded_asio_buffer_range() }.ok()
        } else {
            None
        };
        let output_channel_names = if backend == AudioBackend::Asio {
            // SAFETY: The iterator retains this serialized driver's reported output count.
            unsafe { audiolib_windows::loaded_asio_channel_names(max_output_channels) }
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        devices.push(AudioDevice {
            is_default: default.as_ref() == Some(&id),
            id,
            name,
            backend,
            max_output_channels,
            default_sample_rate: config.sample_rate(),
            buffer_min: range.as_ref().map(|range| range.min).or(buffer_min),
            buffer_max: range.as_ref().map(|range| range.max).or(buffer_max),
            buffer_preferred: range.as_ref().map(|range| range.preferred),
            buffer_granularity: range.as_ref().map(|range| range.granularity),
            output_channel_names,
        });
    }
    if backend == AudioBackend::Asio {
        *ASIO_DEVICE_CACHE
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(devices.clone());
    }
    Ok(devices)
}

/// The owned result of one queued command, independent of later playback status.
pub struct CommandCompletion {
    acknowledgment: Receiver<Result<PlaybackStatus>>,
}

impl CommandCompletion {
    /// Wait for this command's result on a worker thread, never the GUI thread.
    pub fn wait(self, timeout: Duration) -> Result<PlaybackStatus> {
        self.acknowledgment
            .recv_timeout(timeout)
            .map_err(|error| Error::Audio(format!("Audio acknowledgment: {error}")))?
    }

    /// Wait for a definitive result before disposing transactional source resources.
    /// Call only from a worker thread; source interruption remains controlled by
    /// the owner, and disconnection still returns an error. A timeout cannot prove
    /// whether a queued source replacement committed, so it cannot safely roll back.
    pub fn wait_until_completed(self) -> Result<PlaybackStatus> {
        self.acknowledgment
            .recv()
            .map_err(|error| Error::Audio(format!("Audio acknowledgment: {error}")))?
    }
}

#[derive(Clone)]
pub struct AudioHandle {
    commands: SyncSender<CommandRequest>,
    status: Arc<Mutex<PlaybackStatus>>,
    subscribers: Subscribers,
}
impl AudioHandle {
    /// Enqueue a bounded control command without accessing a driver on the UI thread.
    pub fn send(&self, command: AudioCommand) -> Result<()> {
        self.commands
            .try_send(CommandRequest {
                command,
                acknowledgment: None,
            })
            .map_err(|error| Error::Audio(format!("Audio command queue: {error}")))
    }
    pub fn status(&self) -> PlaybackStatus {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    /// Enqueue without waiting and return this command's exact worker result.
    /// This allows callers to order commands under a short lock, then wait outside it.
    pub fn enqueue_with_ack(&self, command: AudioCommand) -> Result<CommandCompletion> {
        let (sender, receiver) = mpsc::sync_channel(1);
        self.commands
            .try_send(CommandRequest {
                command,
                acknowledgment: Some(sender),
            })
            .map_err(|error| Error::Audio(format!("Audio command queue: {error}")))?;
        Ok(CommandCompletion {
            acknowledgment: receiver,
        })
    }
    /// Wait for the worker to confirm a command, suitable for CLI/IPC workers, never the GUI thread.
    pub fn send_and_wait(
        &self,
        command: AudioCommand,
        timeout: Duration,
    ) -> Result<PlaybackStatus> {
        self.enqueue_with_ack(command)?.wait(timeout)
    }
    /// Subscribe to bounded, best-effort events. Status snapshots remain authoritative if an
    /// inactive subscriber fills its 256-event queue; audio/control threads never block on it.
    pub fn subscribe(&self) -> Receiver<AudioEvent> {
        self.subscribe_filtered(false)
    }
    /// Subscribe to playback starts without Position/StateChanged traffic filling
    /// the queue while a consumer is busy. This queue is bounded to 256 starts;
    /// the cumulative status session ID can recover gaps for the loaded source.
    pub fn subscribe_playback_starts(&self) -> Receiver<AudioEvent> {
        self.subscribe_filtered(true)
    }
    fn subscribe_filtered(&self, playback_only: bool) -> Receiver<AudioEvent> {
        let (sender, receiver) = mpsc::sync_channel(256);
        self.subscribers
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(EventSubscriber {
                sender,
                playback_only,
            });
        receiver
    }
}
pub struct AudioEngine {
    handle: AudioHandle,
    worker: Option<JoinHandle<()>>,
}
impl AudioEngine {
    /// Create the audio control worker; no output stream is opened until Play.
    pub fn new(config: PlaybackConfig) -> Result<Self> {
        config.validate()?;
        let (tx, rx) = mpsc::sync_channel(64);
        let status = Arc::new(Mutex::new(PlaybackStatus {
            volume: config.volume,
            audition: config.audition,
            ..Default::default()
        }));
        let worker_status = status.clone();
        let subscribers = Arc::new(Mutex::new(Vec::new()));
        let worker_subscribers = subscribers.clone();
        let worker = thread::Builder::new()
            .name("audio-control".into())
            .spawn(move || Worker::new(config, worker_status, worker_subscribers).run(rx))?;
        Ok(Self {
            handle: AudioHandle {
                commands: tx,
                status,
                subscribers,
            },
            worker: Some(worker),
        })
    }
    pub fn handle(&self) -> AudioHandle {
        self.handle.clone()
    }
    pub fn events(&self) -> Receiver<AudioEvent> {
        self.handle.subscribe()
    }
}
impl Drop for AudioEngine {
    fn drop(&mut self) {
        let _ = self.handle.commands.send(CommandRequest {
            command: AudioCommand::Shutdown,
            acknowledgment: None,
        });
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

// The PCM ring has one producer (control worker) and one consumer (output callback).
// Atomic payload slots make a seek flush safe even if a driver invokes a late callback.
struct Ring {
    slots: Box<[AtomicU32]>,
    read: AtomicUsize,
    write: AtomicUsize,
}
impl Ring {
    fn new(capacity: usize) -> Self {
        Self {
            slots: (0..capacity).map(|_| AtomicU32::new(0)).collect(),
            read: AtomicUsize::new(0),
            write: AtomicUsize::new(0),
        }
    }
    fn len(&self) -> usize {
        self.write
            .load(Ordering::Acquire)
            .wrapping_sub(self.read.load(Ordering::Acquire))
    }
    fn capacity(&self) -> usize {
        self.slots.len()
    }
    #[cfg(test)]
    fn push(&self, input: &[f32]) -> usize {
        self.push_aligned(input, 1)
    }
    fn push_aligned(&self, input: &[f32], alignment: usize) -> usize {
        let write = self.write.load(Ordering::Relaxed);
        let available = self
            .capacity()
            .saturating_sub(write.wrapping_sub(self.read.load(Ordering::Acquire)));
        let count = input.len().min(available) / alignment * alignment;
        for (offset, value) in input[..count].iter().enumerate() {
            self.slots[write.wrapping_add(offset) % self.capacity()]
                .store(value.to_bits(), Ordering::Relaxed);
        }
        self.write
            .store(write.wrapping_add(count), Ordering::Release);
        count
    }
    fn pop(&self) -> Option<f32> {
        let read = self.read.load(Ordering::Relaxed);
        if read == self.write.load(Ordering::Acquire) {
            return None;
        }
        let value = self.slots[read % self.capacity()].load(Ordering::Relaxed);
        self.read.store(read.wrapping_add(1), Ordering::Release);
        Some(f32::from_bits(value))
    }
    fn clear_suspended(&self) {
        self.read
            .store(self.write.load(Ordering::Acquire), Ordering::Release);
    }
}
struct Realtime {
    ring: Ring,
    running: AtomicBool,
    gain: AtomicU32,
    consumed: AtomicU64,
    underruns: AtomicU64,
    failed: AtomicBool,
    active: AtomicBool,
    callback_latency_nanos: AtomicU64,
    source_finished: AtomicBool,
    transition: SourceTransition,
}
/// Seek, pause, stop and source-change boundary envelope duration. The envelope
/// is time based: a fixed frame count shrinks with the device rate (32 frames is
/// 0.33 ms at 96 kHz), and such a short ramp is itself an audible click.
const SEEK_FADE_SECONDS: f64 = 0.008;
/// Per-channel output frames, including the zero and unity endpoints, for very
/// low device rates (and hardware-free fixtures using the default envelope).
const SEEK_FADE_MIN_FRAMES: u32 = 32;

/// Boundary envelope frames for a device rate (see [`SEEK_FADE_SECONDS`]).
fn seek_fade_frames(sample_rate: u32) -> u32 {
    ((f64::from(sample_rate) * SEEK_FADE_SECONDS).round() as u32).max(SEEK_FADE_MIN_FRAMES)
}

/// Raised-cosine envelope gain for `level` in `0..frames`. Unlike a linear ramp it
/// has zero slope at both endpoints, so neither the start nor the end of a fade
/// forms a corner in the waveform. Exact 0 and 1 endpoints; no allocation.
fn seek_envelope_gain(level: u32, frames: u32) -> f32 {
    let last = frames.saturating_sub(1).max(1);
    if level == 0 {
        0.0
    } else if level >= last {
        1.0
    } else {
        0.5 - 0.5 * (std::f32::consts::PI * level as f32 / last as f32).cos()
    }
}

#[derive(Default)]
struct SourceTransition {
    fade_out: AtomicBool,
    faded_out: AtomicU64,
    fade_in_revision: AtomicU64,
    /// Full silent buffers submitted after the most recent audible frame.
    silent_callbacks: AtomicUsize,
}

impl Realtime {
    fn arm_fade_in(&self) {
        self.transition.fade_out.store(false, Ordering::Release);
        self.transition
            .fade_in_revision
            .fetch_add(1, Ordering::Release);
    }

    /// Wait only on the control worker. The callback acknowledges a fully muted
    /// buffer and holds silence until the replacement source has been published.
    fn fade_out_before_reset(&self) -> Result<()> {
        let revision = self.transition.fade_in_revision.load(Ordering::Acquire);
        self.transition
            .faded_out
            .store(revision.wrapping_sub(1), Ordering::Relaxed);
        self.transition.fade_out.store(true, Ordering::Release);
        // Allow two device periods, including large legal buffers, even before
        // the first callback. A dead device must still fail instead of hanging.
        let timeout = Duration::from_nanos(self.callback_latency_nanos.load(Ordering::Relaxed))
            .saturating_mul(2)
            .saturating_add(Duration::from_millis(100));
        let deadline = std::time::Instant::now() + timeout;
        while self.transition.faded_out.load(Ordering::Acquire) != revision {
            if self.failed.load(Ordering::Acquire) {
                return Err(Error::Audio("Output device failed during seek fade".into()));
            }
            if std::time::Instant::now() >= deadline {
                return Err(Error::Audio(
                    "Output callback did not finish seek fade".into(),
                ));
            }
            thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    }
}

struct SeekEnvelope {
    revision: u64,
    level: u32,
    /// Envelope length in output frames, fixed for the stream's device rate.
    frames: u32,
    /// Last real stereo PCM frame, before the volume and seek envelopes. Retained
    /// as fallback only during a fade, never across source changes or ordinary silence.
    last_pcm: [f32; 2],
}

impl SeekEnvelope {
    fn new(frames: u32) -> Self {
        let frames = frames.max(2);
        Self {
            revision: 0,
            level: frames - 1,
            frames,
            last_pcm: [0.0; 2],
        }
    }
}

#[cfg(test)]
impl Default for SeekEnvelope {
    fn default() -> Self {
        Self::new(SEEK_FADE_MIN_FRAMES)
    }
}
/// Audio a network source must hold in memory before playback starts or resumes after a
/// seek (or the rest of the file, if shorter).
const PREBUFFER_SECONDS: f64 = 0.5;
/// After the buffer ran dry, playback resumes only with this much audio in memory (or the
/// rest of the file), so a slow link pauses once instead of stuttering between bursts and
/// server/NAS jitter has a cushion.
const REBUFFER_SECONDS: f64 = 2.0;
/// Audio that must be in memory before the fill worker decodes its next block, so the
/// decoder never waits for the network while the output plays.
const FILL_LOOKAHEAD_SECONDS: f64 = 0.1;
/// The callback ramps the gain to zero in 5 ms; pause the stream only after that.
const BUFFERING_FADE: Duration = Duration::from_millis(12);

/// Network buffering of the current source (see [`PREBUFFER_SECONDS`]).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Buffering {
    Off,
    /// The buffer ran dry: the gain ramps to zero until this instant, then playback pauses.
    Fading(std::time::Instant),
    /// Paused until this many output frames are buffered (or the source end is).
    Waiting(u64),
}

struct Output {
    stream: Option<cpal::Stream>,
    /// Hardware-free control tests can opt into the real callback handshake.
    #[cfg(test)]
    callback_available: bool,
    asio: bool,
    realtime: Arc<Realtime>,
    decoder: AuditionDecoder,
    sample_rate: u32,
    /// Output gain to restore after buffering.
    volume: f32,
    buffering: Buffering,
    pending: Vec<f32>,
    pending_offset: usize,
    eof: bool,
    pending_last: bool,
    start_position: f64,
    drain_deadline: Option<std::time::Instant>,
}
impl Output {
    fn stream(&self) -> &cpal::Stream {
        self.stream
            .as_ref()
            .expect("Output owns a stream until Drop")
    }
    fn play_stream(&self) -> Result<()> {
        // Unit tests retain the real decoder/ring/control logic and substitute
        // only the device syscall, without constructing or playing a CPAL stream.
        #[cfg(test)]
        if self.stream.is_none() {
            return Ok(());
        }
        self.stream().play().map_err(audio_error)
    }
    fn pause_stream(&self) -> Result<()> {
        #[cfg(test)]
        if self.stream.is_none() {
            return Ok(());
        }
        self.stream().pause().map_err(audio_error)
    }
    /// Output frames for `seconds`, never less than one fill block ([`Self::lookahead`]),
    /// so a started or resumed output can always fill.
    fn frames_for(&self, seconds: f64) -> u64 {
        ((seconds * f64::from(self.sample_rate)).ceil() as u64).max(self.lookahead())
    }
    /// What must be in memory before the next decoder block: covers one decode call
    /// (2048 source frames, or a resampler block) at any rate.
    fn lookahead(&self) -> u64 {
        ((FILL_LOOKAHEAD_SECONDS * f64::from(self.sample_rate)).ceil() as u64).max(8192)
    }
    /// Top the ring up. Returns true when it stopped because a network source does not
    /// have the next block in memory yet (it never waits for the network).
    fn fill(&mut self) -> Result<bool> {
        let lookahead = self.lookahead();
        while self.realtime.ring.len() < self.realtime.ring.capacity() / 2 && !self.eof {
            if self.pending_offset >= self.pending.len() {
                if !self.decoder.is_buffered(lookahead) {
                    return Ok(true);
                }
                let source = self.decoder.next()?;
                if source.is_empty() {
                    self.eof = true;
                    self.realtime.source_finished.store(true, Ordering::Release);
                    break;
                }
                self.pending_last = self.decoder.is_finished();
                self.pending = source;
                self.pending_offset = 0;
            }
            self.pending_offset += self
                .realtime
                .ring
                .push_aligned(&self.pending[self.pending_offset..], 2);
            if self.pending_last && self.pending_offset == self.pending.len() {
                self.eof = true;
                self.realtime.source_finished.store(true, Ordering::Release);
            }
            if self.realtime.ring.len() == self.realtime.ring.capacity() {
                break;
            }
        }
        Ok(false)
    }
    /// Start (or resume) the output: at once when the source has the prebuffer in memory,
    /// otherwise silent until it has ([`Buffering::Waiting`]).
    fn start_running(&mut self) -> Result<()> {
        if self.realtime.transition.fade_out.load(Ordering::Acquire) {
            self.realtime.arm_fade_in();
        }
        if self.eof || self.decoder.is_buffered(self.frames_for(PREBUFFER_SECONDS)) {
            self.realtime
                .gain
                .store(self.volume.to_bits(), Ordering::Relaxed);
            self.realtime.running.store(true, Ordering::Release);
            self.buffering = Buffering::Off;
        } else {
            self.realtime
                .gain
                .store(0.0_f32.to_bits(), Ordering::Relaxed);
            self.realtime.running.store(false, Ordering::Release);
            self.buffering = Buffering::Waiting(self.frames_for(PREBUFFER_SECONDS));
        }
        self.play_stream()
    }
    /// Network buffering while playing: when the ring runs low and the next block is not in
    /// memory, ramp the gain down and pause (no glitch, no noise); resume with a ramp once
    /// [`REBUFFER_SECONDS`] are buffered. Files are never starved, so this is a no-op.
    fn regulate(&mut self, starved: bool) -> Result<()> {
        let low = (self.sample_rate as usize / 16).max(64) * 2;
        match self.buffering {
            Buffering::Off => {
                if starved && !self.eof && self.realtime.ring.len() < low {
                    self.realtime
                        .gain
                        .store(0.0_f32.to_bits(), Ordering::Relaxed);
                    self.buffering = Buffering::Fading(std::time::Instant::now() + BUFFERING_FADE);
                }
            }
            Buffering::Fading(until) => {
                if !starved && self.realtime.ring.len() >= low {
                    // The data arrived during the fade: carry on.
                    self.realtime
                        .gain
                        .store(self.volume.to_bits(), Ordering::Relaxed);
                    self.buffering = Buffering::Off;
                } else if std::time::Instant::now() >= until {
                    self.realtime.running.store(false, Ordering::Release);
                    self.buffering = Buffering::Waiting(self.frames_for(REBUFFER_SECONDS));
                }
            }
            Buffering::Waiting(frames) => {
                if self.eof || self.decoder.is_buffered(frames) {
                    self.fill()?;
                    self.realtime
                        .gain
                        .store(self.volume.to_bits(), Ordering::Relaxed);
                    self.realtime.running.store(true, Ordering::Release);
                    self.buffering = Buffering::Off;
                }
            }
        }
        Ok(())
    }
    fn is_waiting(&self) -> bool {
        matches!(self.buffering, Buffering::Waiting(_))
    }
    fn position(&self) -> f64 {
        self.start_position
            + self.realtime.consumed.load(Ordering::Relaxed) as f64 / f64::from(self.sample_rate)
                * f64::from(self.decoder.settings.rate)
    }
    /// Silence and quiesce the consumer before any potentially blocking source
    /// operation. The stream keeps submitting silence while the worker does I/O.
    fn quiesce_source(&self) -> Result<Option<std::time::Instant>> {
        let mut fade_drain_deadline = None;
        if self.realtime.running.load(Ordering::Acquire) {
            // Hardware-free transport fixtures have no callback to acknowledge.
            #[cfg(test)]
            let has_callback = self.callback_available;
            #[cfg(not(test))]
            let has_callback = true;
            if has_callback {
                self.realtime.fade_out_before_reset()?;
                fade_drain_deadline = Some(
                    std::time::Instant::now()
                        + Duration::from_nanos(
                            self.realtime.callback_latency_nanos.load(Ordering::Relaxed),
                        ),
                );
            }
        } else if self.is_waiting() {
            // Network rebuffering keeps the device playing silence even though
            // PCM consumption is stopped. Its previously queued audio must drain.
            fade_drain_deadline = Some(
                std::time::Instant::now()
                    + Duration::from_nanos(
                        self.realtime.callback_latency_nanos.load(Ordering::Relaxed),
                    ),
            );
        }
        self.realtime.running.store(false, Ordering::SeqCst);
        // Keep an active device emitting silence: pausing here can discard its
        // queued fade tail. A previously paused device remains paused until Play.
        let deadline = std::time::Instant::now() + Duration::from_millis(100);
        while self.realtime.active.load(Ordering::SeqCst) {
            if std::time::Instant::now() >= deadline {
                return Err(Error::Audio("Output callback did not suspend".into()));
            }
            thread::yield_now();
        }
        Ok(fade_drain_deadline)
    }

    fn pause_after_drain(&self, deadline: Option<std::time::Instant>) -> Result<()> {
        if let Some(deadline) = deadline {
            if self.asio {
                // CPAL's ASIO pause stops filling, rather than clearing, its two
                // hardware buffers. Both must contain silence before pausing.
                let timeout = std::time::Instant::now()
                    + Duration::from_nanos(
                        self.realtime.callback_latency_nanos.load(Ordering::Relaxed),
                    )
                    .saturating_mul(2)
                    .saturating_add(Duration::from_millis(100));
                while self
                    .realtime
                    .transition
                    .silent_callbacks
                    .load(Ordering::Acquire)
                    < 2
                {
                    if self.realtime.failed.load(Ordering::Acquire)
                        || std::time::Instant::now() >= timeout
                    {
                        return Err(Error::Audio("Output did not drain silent buffers".into()));
                    }
                    thread::sleep(Duration::from_millis(1));
                }
            }
            thread::sleep(deadline.saturating_duration_since(std::time::Instant::now()));
        }
        self.pause_stream()
    }

    fn reset_source(
        &mut self,
        open_decoder: impl FnOnce() -> Result<Decoder>,
        position: f64,
        source_pair: u16,
        playing: bool,
        audition: AuditionSettings,
    ) -> Result<()> {
        let fade_drain_deadline = self.quiesce_source()?;
        self.buffering = Buffering::Off;
        let result = (|| {
            // Opening a WAV (including its headers on a NAS) can block longer than
            // the old PCM ring lasts. Never open it before the callback fades out.
            let decoder = open_decoder()?;
            let source_channels = usize::from(decoder.info.metadata.channels);
            if usize::from(source_pair) * 2 >= source_channels {
                return Err(Error::Audio(
                    "Source channel pair is outside the loaded WAV channels".into(),
                ));
            }
            let start_frame = (position * f64::from(decoder.info.metadata.sample_rate)) as u64;
            self.decoder = AuditionDecoder::new(
                decoder,
                self.sample_rate,
                start_frame,
                source_pair,
                audition,
            )?;
            self.realtime.ring.clear_suspended();
            self.realtime.consumed.store(0, Ordering::Relaxed);
            self.realtime
                .source_finished
                .store(false, Ordering::Release);
            self.realtime.underruns.store(0, Ordering::Relaxed);
            self.pending.clear();
            self.pending_offset = 0;
            self.eof = false;
            self.pending_last = false;
            self.drain_deadline = None;
            self.start_position = position;
            self.fill()?;
            // Publish after releasing the old mute, so its late acknowledgment can
            // never carry the revision of the new source.
            self.realtime.arm_fade_in();
            if playing {
                self.start_running()?;
            } else {
                // Stop must leave the device paused (including before an ASIO panel).
                // Wait only on the worker, after decoder preparation has already used
                // part of the drain time, so pause cannot discard the queued fade tail.
                self.pause_after_drain(fade_drain_deadline)?;
            }
            Ok(())
        })();
        if result.is_err() {
            // Command failure closes the stream next. Preserve its queued fade
            // even when reopening, seeking or filling the source failed.
            self.realtime.running.store(false, Ordering::SeqCst);
            let _ = self.pause_after_drain(fade_drain_deadline);
        }
        result
    }
}
impl Drop for Output {
    fn drop(&mut self) {
        self.realtime.running.store(false, Ordering::Release);
        if self.asio {
            let _guard = ASIO_ACCESS
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            drop(self.stream.take());
            ACTIVE_ASIO_STREAMS.fetch_sub(1, Ordering::AcqRel);
        } else {
            drop(self.stream.take());
        }
    }
}
pub(crate) fn monitor_pair(input: &[f32], pair: usize) -> (f32, f32) {
    let left = input[pair * 2];
    (left, input.get(pair * 2 + 1).copied().unwrap_or(left))
}

fn build_stream<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    realtime: Arc<Realtime>,
    output_pair: u16,
) -> Result<cpal::Stream>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    let error_realtime = realtime.clone();
    let mut callback = RenderCallback::new(
        realtime,
        usize::from(config.channels),
        usize::from(output_pair),
        config.sample_rate,
    );
    device
        .build_output_stream::<T, _, _>(
            config,
            move |output, info| {
                let timestamp = info.timestamp();
                let delay = timestamp
                    .playback
                    .saturating_duration_since(timestamp.callback)
                    + Duration::from_secs_f64(
                        output.len() as f64
                            / f64::from(config.channels)
                            / f64::from(config.sample_rate),
                    );
                callback.realtime.callback_latency_nanos.store(
                    delay.as_nanos().min(u128::from(u64::MAX)) as u64,
                    Ordering::Relaxed,
                );
                callback.render(output);
            },
            move |error| {
                record_stream_error(&error_realtime, error.kind(), cfg!(target_os = "macos"));
            },
            None,
        )
        .map_err(audio_error)
}

fn record_stream_error(realtime: &Realtime, kind: cpal::ErrorKind, macos: bool) {
    // CoreAudio reports processor overloads from its real-time listener. CPAL
    // also documents denied real-time scheduling as nonfatal: audio continues.
    // Keep this path allocation-, lock- and logging-free, and never clear an
    // earlier fatal error. Other hosts retain their existing fatal handling.
    match (macos, kind) {
        (true, cpal::ErrorKind::Xrun) => {
            realtime.underruns.fetch_add(1, Ordering::Relaxed);
        }
        (true, cpal::ErrorKind::RealtimeDenied) => {}
        _ => realtime.failed.store(true, Ordering::Release),
    }
}

struct RenderCallback {
    realtime: Arc<Realtime>,
    channels: usize,
    pair: usize,
    ramp_step: f32,
    gain: f32,
    envelope: SeekEnvelope,
}
impl RenderCallback {
    /// Callback state for a device stream; all buffers are allocated here, never
    /// in [`RenderCallback::render`].
    fn new(realtime: Arc<Realtime>, channels: usize, pair: usize, sample_rate: u32) -> Self {
        Self {
            realtime,
            channels,
            pair,
            ramp_step: 1.0 / (sample_rate as f32 * 0.005).max(1.0),
            gain: 0.0,
            envelope: SeekEnvelope::new(seek_fade_frames(sample_rate)),
        }
    }
    fn render<T: cpal::SizedSample + cpal::FromSample<f32>>(&mut self, output: &mut [T]) {
        self.realtime.active.store(true, Ordering::SeqCst);
        // This application callback only reads/writes atomics and a preallocated ring.
        // There are no locks, allocations, file operations, Python calls, or logging.
        let running = self.realtime.running.load(Ordering::SeqCst);
        let revision = self
            .realtime
            .transition
            .fade_in_revision
            .load(Ordering::Acquire);
        if revision != self.envelope.revision {
            self.envelope.revision = revision;
            self.envelope.level = 0;
            self.envelope.last_pcm = [0.0; 2];
        }
        let fade_out = self.realtime.transition.fade_out.load(Ordering::Acquire);
        let target = f32::from_bits(self.realtime.gain.load(Ordering::Relaxed));
        let mut underrun = false;
        let mut faded_out = false;
        let mut silent = true;
        let mut consumed = 0_u64;
        for frame in output.chunks_mut(self.channels) {
            self.gain += (target - self.gain).clamp(-self.ramp_step, self.ramp_step);
            let muted = fade_out && self.envelope.level == 0;
            faded_out |= muted;
            let complete = running && !muted && self.realtime.ring.len() >= 2;
            let gain = self.gain * seek_envelope_gain(self.envelope.level, self.envelope.frames);
            let pcm = if complete {
                consumed += 1;
                let pcm = [
                    self.realtime.ring.pop().unwrap_or(0.0),
                    self.realtime.ring.pop().unwrap_or(0.0),
                ];
                self.envelope.last_pcm = pcm;
                pcm
            } else if running && fade_out && !muted {
                // A short EOF/network tail can run dry before the fade finishes.
                // Decay the last real frame instead of jumping straight to zero;
                // synthesized output never advances the source playback cursor.
                self.envelope.last_pcm
            } else {
                if running && !fade_out {
                    underrun = true;
                }
                self.envelope.last_pcm = [0.0; 2];
                [0.0; 2]
            };
            let [left, right] = pcm.map(|sample| sample * gain);
            silent &= left == 0.0 && right == 0.0;
            if fade_out {
                self.envelope.level = self.envelope.level.saturating_sub(1);
            } else if complete {
                self.envelope.level = (self.envelope.level + 1).min(self.envelope.frames - 1);
            }
            for (channel, sample) in frame.iter_mut().enumerate() {
                let value = if channel == self.pair * 2 {
                    left
                } else if channel == self.pair * 2 + 1 {
                    right
                } else {
                    0.0
                };
                *sample = T::from_sample(value.clamp(-1.0, 1.0));
            }
        }
        self.realtime
            .consumed
            .fetch_add(consumed, Ordering::Relaxed);
        if underrun && !self.realtime.source_finished.load(Ordering::Acquire) {
            self.realtime.underruns.fetch_add(1, Ordering::Relaxed);
        }
        if silent && !output.is_empty() {
            self.realtime
                .transition
                .silent_callbacks
                .fetch_add(1, Ordering::Release);
        } else {
            self.realtime
                .transition
                .silent_callbacks
                .store(0, Ordering::Release);
        }
        if faded_out {
            self.realtime
                .transition
                .faded_out
                .store(revision, Ordering::Release);
        }
        self.realtime.active.store(false, Ordering::SeqCst);
    }
}

// Keep rapid auditions on the prepared stream, but do not retain a stopped WASAPI
// client indefinitely while a DAW uses another backend on the same audio interface.
const WASAPI_IDLE_RELEASE: Duration = Duration::from_millis(500);

struct Worker {
    config: PlaybackConfig,
    status: Arc<Mutex<PlaybackStatus>>,
    output: Option<Output>,
    output_idle_since: Option<std::time::Instant>,
    needs_recovery: bool,
    session_open: AtomicBool,
    subscribers: Subscribers,
}
impl Worker {
    fn new(
        config: PlaybackConfig,
        status: Arc<Mutex<PlaybackStatus>>,
        subscribers: Subscribers,
    ) -> Self {
        let initial = status.lock().unwrap_or_else(|error| error.into_inner());
        let session_open = initial.play_session_id > 0
            && !matches!(
                initial.state,
                PlaybackState::Empty
                    | PlaybackState::Loading
                    | PlaybackState::Stopped
                    | PlaybackState::Ended
            );
        drop(initial);
        Self {
            config,
            status,
            output: None,
            output_idle_since: None,
            needs_recovery: false,
            session_open: AtomicBool::new(session_open),
            subscribers,
        }
    }
    fn snapshot(&self) -> PlaybackStatus {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    fn publish(&self, event: AudioEvent) {
        self.subscribers
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .retain(|subscriber| {
                if subscriber.playback_only && !matches!(&event, AudioEvent::PlaybackStarted { .. })
                {
                    return true;
                }
                match subscriber.sender.try_send(event.clone()) {
                    Ok(()) | Err(mpsc::TrySendError::Full(_)) => true,
                    Err(mpsc::TrySendError::Disconnected(_)) => false,
                }
            });
    }
    fn update(&self, action: impl FnOnce(&mut PlaybackStatus)) {
        let (before, after, started) = {
            let mut status = self.status.lock().unwrap_or_else(|e| e.into_inner());
            let before = status.clone();
            action(&mut status);
            if before.generation != status.generation
                || matches!(
                    status.state,
                    PlaybackState::Empty
                        | PlaybackState::Loading
                        | PlaybackState::Stopped
                        | PlaybackState::Ended
                )
            {
                self.session_open.store(false, Ordering::Relaxed);
            }
            // Playing is published only after stream.play() succeeds. A paused
            // or reconfigured session remains open, including device recovery.
            let started = status.state == PlaybackState::Playing
                && status.file_path.is_some()
                && !self.session_open.swap(true, Ordering::Relaxed);
            if started {
                status.play_session_id = status.play_session_id.saturating_add(1);
            }
            (before, status.clone(), started)
        };
        if started {
            self.publish(AudioEvent::PlaybackStarted {
                generation: after.generation,
                play_session_id: after.play_session_id,
                file_path: after
                    .file_path
                    .clone()
                    .expect("Playing session has a source"),
            });
        }
        if before.state != after.state || before.generation != after.generation {
            self.publish(AudioEvent::StateChanged {
                generation: after.generation,
                state: after.state,
            });
        }
        if before.position != after.position {
            self.publish(AudioEvent::Position {
                generation: after.generation,
                position: after.position,
            });
        }
        if before.error != after.error
            && let Some(message) = after.error
        {
            self.publish(AudioEvent::Error {
                generation: after.generation,
                message,
            });
        }
    }
    fn update_transport(&self, action: impl FnOnce(&mut PlaybackStatus)) {
        self.update(|status| {
            let revision = status.transport_revision;
            action(status);
            status.transport_revision = revision.wrapping_add(1);
        });
    }
    fn close_output(&mut self) {
        self.output_idle_since = None;
        if let Some(output) = self.output.take() {
            output.realtime.running.store(false, Ordering::Release);
            if let Some(stream) = &output.stream {
                let _ = stream.pause();
            }
        }
        self.update(|status| {
            status.active_backend = None;
            status.active_device_id = None;
            status.actual_buffer_frames = 0;
            status.device_sample_rate = 0;
            status.active_source_pair = None;
            status.active_output_pair = None;
            status.active_rate_policy = None;
            status.buffering = false;
        });
    }
    /// WASAPI Stop pauses streaming but retains the audio client and its driver
    /// resources. Release it after a short idle grace; Play reconstructs output
    /// from the retained position/configuration without changing a paused session.
    /// The control worker owns this timer and stream destruction, never the callback.
    fn release_idle_output(&mut self, now: std::time::Instant) {
        let idle = matches!(
            self.status
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .state,
            PlaybackState::Ready
                | PlaybackState::Paused
                | PlaybackState::Stopped
                | PlaybackState::Ended
        );
        if !idle || self.output.as_ref().is_none_or(|output| output.asio) {
            self.output_idle_since = None;
            return;
        }
        let idle_since = self.output_idle_since.get_or_insert(now);
        if now.saturating_duration_since(*idle_since) >= WASAPI_IDLE_RELEASE {
            self.close_output();
        }
    }
    fn suspend_output(&self) -> Result<()> {
        if let Some(output) = &self.output {
            let deadline = output.quiesce_source()?;
            output.pause_after_drain(deadline)?;
        }
        Ok(())
    }
    fn check_output_failure(&mut self) -> bool {
        let Some(output) = &self.output else {
            return false;
        };
        if !output.realtime.failed.swap(false, Ordering::AcqRel) {
            return false;
        }
        let position = output.position();
        // A prepared, paused stream can also be invalidated. Close it before any
        // subsequent command can acknowledge Play on an already failed driver.
        self.close_output();
        self.needs_recovery = true;
        self.publish(AudioEvent::DeviceLost {
            generation: self.snapshot().generation,
            message: "Output device reset or disconnected".into(),
        });
        self.update_transport(|status| {
            status.state = PlaybackState::Paused;
            status.position = position.min(status.duration);
            status.error = Some(
                "Output device was lost or reset. Select an available device and press Play."
                    .into(),
            );
        });
        true
    }
    /// Stop interrupted a read blocked on network data (server mode): this is a stop,
    /// not a failure. The source is rewound like Stop does; if that is impossible the
    /// output is closed (Play opens it again), still without an error.
    fn stop_after_interrupted_read(&mut self) {
        if self.snapshot().state == PlaybackState::Loading {
            // The new file never finished loading: nothing usable is loaded.
            self.close_output();
            self.update_transport(|status| {
                status.state = PlaybackState::Empty;
                status.file_path = None;
                status.position = 0.0;
                status.error = None;
            });
            return;
        }
        if self.command(AudioCommand::Stop).is_err() {
            self.close_output();
            self.update_transport(|status| {
                status.position = 0.0;
                status.state = if status.file_path.is_some() {
                    PlaybackState::Stopped
                } else {
                    PlaybackState::Empty
                };
                status.error = None;
            });
        }
    }
    /// A failure of `output.fill()` in the worker loop.
    fn fill_failed(&mut self, error: &Error) {
        if error.is_read_interrupted() {
            self.stop_after_interrupted_read();
            return;
        }
        self.close_output();
        self.update(|status| {
            status.state = PlaybackState::Error;
            status.error = Some(error.to_string());
        });
    }
    fn execute_command(&mut self, command: AudioCommand) -> Result<()> {
        let configuring = matches!(
            &command,
            AudioCommand::Configure(_) | AudioCommand::ConfigureSource { .. }
        );
        let result = self.command(command);
        if let Err(error) = &result
            && error.is_read_interrupted()
        {
            self.stop_after_interrupted_read();
            return Err(Error::Cancelled);
        }
        if let Err(error) = &result {
            if !matches!(error, Error::Validation(_)) {
                self.close_output();
                self.update(|status| {
                    status.state = if configuring && status.file_path.is_some() {
                        PlaybackState::Paused
                    } else {
                        PlaybackState::Error
                    };
                });
            }
            // Invalid input is reported without discarding a valid source/stream
            // or changing the transport state. The acknowledgment still fails.
            self.update(|status| status.error = Some(error.to_string()));
        }
        result
    }
    fn select_wasapi_fallback(&mut self, error: &Error) {
        self.config.backend = AudioBackend::Wasapi;
        self.config.device_id = None;
        self.config.output_pair = 0;
        self.config.buffer_frames = 0;
        self.config.rate_policy = RatePolicy::DeviceDefault;
        self.needs_recovery = false;
        self.update(|status| {
            status.state = PlaybackState::Paused;
            status.error = Some(format!(
                "ASIO could not recover ({error}). WASAPI device default with resampling is selected; press Play to resume."
            ));
        });
    }
    fn with_asio_panel_driver<T>(
        &self,
        discover: impl FnOnce() -> Result<T>,
        panel: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        let _driver_guard = ASIO_ACCESS
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        // A live stream retains the selected global driver. Discovery must never
        // load another registration while that driver is alive.
        let retained_device_iterator = if self.output.as_ref().is_some_and(|output| output.asio) {
            None
        } else {
            Some(discover()?)
        };
        let result = panel();
        drop(retained_device_iterator);
        result
    }
    fn start_output(&mut self) -> Result<()> {
        self.start_output_at(None)
    }
    fn start_output_at(&mut self, position: Option<f64>) -> Result<()> {
        self.close_output();
        let mut driver_guard = if self.config.backend == AudioBackend::Asio {
            Some(
                ASIO_ACCESS
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()),
            )
        } else {
            None
        };
        let mut status = self.snapshot();
        if let Some(position) = position {
            status.position = position.clamp(0.0, status.duration);
        }
        let path = status
            .file_path
            .ok_or_else(|| Error::Audio("No audio file is loaded".into()))?;
        let decoder = Decoder::open(&path)?;
        let host = host(self.config.backend)?;
        // CPAL ASIO's iterator owns the driver's strong reference until a Stream exists.
        // Retain it through capability/shim queries and stream creation; a temporary
        // default_output_device()/find() drops the driver and makes SDK queries ASE_NotPresent.
        let mut device_iterator = host.output_devices().map_err(audio_error)?;
        let device = if let Some(id) = &self.config.device_id {
            device_iterator
                .find(|device| identity(device).is_ok_and(|candidate| candidate == *id))
                .ok_or_else(|| Error::Audio("Selected output device is unavailable".into()))?
        } else if self.config.backend == AudioBackend::Asio {
            device_iterator
                .next()
                .ok_or_else(|| Error::Audio("No output device is available".into()))?
        } else {
            host.default_output_device()
                .ok_or_else(|| Error::Audio("No default output device is available".into()))?
        };
        let default = device.default_output_config().map_err(audio_error)?;
        let rate = if self.config.rate_policy == RatePolicy::Native {
            decoder.info.metadata.sample_rate
        } else {
            default.sample_rate()
        };
        let mut config = default.config();
        config.sample_rate = rate;
        if self.config.backend == AudioBackend::Asio {
            config.channels = device
                .supported_output_configs()
                .map_err(audio_error)?
                .map(|supported| supported.channels())
                .max()
                .unwrap_or(config.channels);
        }
        if usize::from(self.config.output_pair) * 2 >= usize::from(config.channels)
            || (config.channels > 1
                && usize::from(self.config.output_pair) * 2 + 1 >= usize::from(config.channels))
        {
            return Err(Error::Audio(
                "Selected output channel pair is outside this device's channels".into(),
            ));
        }
        if self.config.buffer_frames > 0 {
            if self.config.backend == AudioBackend::Asio {
                // SAFETY: ASIO_ACCESS and the retained device iterator cover this query.
                let range = unsafe { audiolib_windows::loaded_asio_buffer_range() }?;
                if !range.supports(self.config.buffer_frames) {
                    return Err(Error::Audio(format!(
                        "ASIO buffer {} violates min={}, max={}, granularity={}",
                        self.config.buffer_frames, range.min, range.max, range.granularity
                    )));
                }
            }
            if let cpal::SupportedBufferSize::Range { min, max } = default.buffer_size()
                && (self.config.buffer_frames < *min || self.config.buffer_frames > *max)
            {
                return Err(Error::Audio(format!(
                    "Buffer must be in device range {min}..={max}"
                )));
            }
            config.buffer_size = cpal::BufferSize::Fixed(self.config.buffer_frames);
        }
        let source_channels = usize::from(decoder.info.metadata.channels);
        if usize::from(self.config.source_pair) * 2 >= source_channels {
            return Err(Error::Audio(
                "Source channel pair is outside the loaded WAV channels".into(),
            ));
        }
        let start_frame = (status.position * f64::from(decoder.info.metadata.sample_rate)) as u64;
        let decoder = AuditionDecoder::new(
            decoder,
            rate,
            start_frame,
            self.config.source_pair,
            self.config.audition,
        )?;
        let realtime = Arc::new(Realtime {
            ring: Ring::new(2 * (rate as usize / 4).max(8192)),
            running: AtomicBool::new(false),
            gain: AtomicU32::new(self.config.volume.to_bits()),
            consumed: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            failed: AtomicBool::new(false),
            active: AtomicBool::new(false),
            callback_latency_nanos: AtomicU64::new(match config.buffer_size {
                cpal::BufferSize::Fixed(frames) => {
                    (f64::from(frames) / f64::from(rate) * 1_000_000_000.0).ceil() as u64
                }
                cpal::BufferSize::Default => 100_000_000,
            }),
            source_finished: AtomicBool::new(false),
            transition: SourceTransition::default(),
        });
        let stream = match default.sample_format() {
            cpal::SampleFormat::F32 => {
                build_stream::<f32>(&device, config, realtime.clone(), self.config.output_pair)
            }
            cpal::SampleFormat::F64 => {
                build_stream::<f64>(&device, config, realtime.clone(), self.config.output_pair)
            }
            cpal::SampleFormat::I16 => {
                build_stream::<i16>(&device, config, realtime.clone(), self.config.output_pair)
            }
            cpal::SampleFormat::I24 => build_stream::<cpal::I24>(
                &device,
                config,
                realtime.clone(),
                self.config.output_pair,
            ),
            cpal::SampleFormat::I32 => {
                build_stream::<i32>(&device, config, realtime.clone(), self.config.output_pair)
            }
            cpal::SampleFormat::U16 => {
                build_stream::<u16>(&device, config, realtime.clone(), self.config.output_pair)
            }
            _ => Err(Error::Audio(
                "Unsupported output device sample format".into(),
            )),
        }?;
        let actual_buffer_frames = stream.buffer_size().unwrap_or(self.config.buffer_frames);
        realtime.callback_latency_nanos.fetch_max(
            (f64::from(actual_buffer_frames) / f64::from(rate) * 1_000_000_000.0).ceil() as u64,
            Ordering::Relaxed,
        );
        let device_id = identity(&device)?;
        let asio = self.config.backend == AudioBackend::Asio;
        if asio {
            ACTIVE_ASIO_STREAMS.fetch_add(1, Ordering::AcqRel);
            // SAFETY: The serialized iterator plus newly built Stream retain the driver.
            let range = unsafe { audiolib_windows::loaded_asio_buffer_range() }.ok();
            let description = AudioDevice {
                id: device_id.clone(),
                name: device
                    .description()
                    .map(|description| description.name().to_string())
                    .unwrap_or_else(|_| device_id.clone()),
                backend: AudioBackend::Asio,
                max_output_channels: config.channels,
                default_sample_rate: rate,
                is_default: self.config.device_id.is_none(),
                buffer_min: range.as_ref().map(|range| range.min),
                buffer_max: range.as_ref().map(|range| range.max),
                buffer_preferred: range.as_ref().map(|range| range.preferred),
                buffer_granularity: range.as_ref().map(|range| range.granularity),
                // SAFETY: The reported config output count belongs to this retained driver.
                output_channel_names: unsafe {
                    audiolib_windows::loaded_asio_channel_names(config.channels)
                }
                .unwrap_or_default(),
            };
            let mut cache = ASIO_DEVICE_CACHE
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let devices = cache.get_or_insert_with(Vec::new);
            if let Some(existing) = devices.iter_mut().find(|existing| existing.id == device_id) {
                *existing = description;
            } else {
                devices.push(description);
            }
        }
        let mut output = Output {
            stream: Some(stream),
            #[cfg(test)]
            callback_available: true,
            asio,
            realtime,
            decoder,
            sample_rate: rate,
            volume: self.config.volume,
            buffering: Buffering::Off,
            pending: Vec::new(),
            pending_offset: 0,
            eof: false,
            pending_last: false,
            start_position: status.position,
            drain_deadline: None,
        };
        let started = output.fill().and_then(|_| output.start_running());
        if let Err(error) = started {
            drop(device_iterator);
            drop(driver_guard.take());
            drop(output);
            return Err(error);
        }
        self.output = Some(output);
        self.needs_recovery = false;
        let position = status.position;
        self.update_transport(|status| {
            status.position = position;
            status.device_sample_rate = rate;
            status.state = PlaybackState::Playing;
            status.error = None;
            status.active_backend = Some(self.config.backend);
            status.active_device_id = Some(device_id);
            status.actual_buffer_frames = actual_buffer_frames;
            status.active_source_pair = Some(self.config.source_pair);
            status.active_output_pair = Some(self.config.output_pair);
            status.active_rate_policy = Some(self.config.rate_policy);
        });
        Ok(())
    }
    fn command(&mut self, command: AudioCommand) -> Result<()> {
        match command {
            AudioCommand::Load(path) => {
                let generation = self.snapshot().generation.wrapping_add(1);
                self.suspend_output()?;
                self.needs_recovery = false;
                let previous = self.snapshot();
                self.update(|status| {
                    *status = PlaybackStatus {
                        generation,
                        transport_revision: previous.transport_revision,
                        state: PlaybackState::Loading,
                        file_path: Some(path.clone()),
                        volume: self.config.volume,
                        audition: self.config.audition,
                        active_backend: previous.active_backend,
                        active_device_id: previous.active_device_id.clone(),
                        actual_buffer_frames: previous.actual_buffer_frames,
                        device_sample_rate: previous.device_sample_rate,
                        active_source_pair: previous.active_source_pair,
                        active_output_pair: previous.active_output_pair,
                        active_rate_policy: previous.active_rate_policy,
                        ..Default::default()
                    };
                });
                // Opening the decoder verifies the actual codec before Ready is reported; its
                // layout (headers up to the data chunk) is all Load needs, read once.
                let decoder = Decoder::open(&path)?;
                let metadata = decoder.info.metadata.clone();
                if self.output.as_ref().is_some_and(|output| {
                    self.config.rate_policy == RatePolicy::DeviceDefault
                        || output.sample_rate == metadata.sample_rate
                }) {
                    self.output.as_mut().unwrap().reset_source(
                        || Ok(decoder),
                        0.0,
                        self.config.source_pair,
                        false,
                        self.config.audition,
                    )?;
                } else {
                    self.close_output();
                }
                let prepared = self.snapshot();
                self.update_transport(|status| {
                    *status = PlaybackStatus {
                        generation,
                        state: PlaybackState::Ready,
                        file_path: Some(path),
                        duration: metadata.duration,
                        sample_rate: metadata.sample_rate,
                        channels: metadata.channels,
                        volume: self.config.volume,
                        audition: self.config.audition,
                        active_backend: prepared.active_backend,
                        active_device_id: prepared.active_device_id,
                        actual_buffer_frames: prepared.actual_buffer_frames,
                        active_source_pair: prepared.active_source_pair,
                        active_output_pair: prepared.active_output_pair,
                        active_rate_policy: prepared.active_rate_policy,
                        device_sample_rate: self
                            .output
                            .as_ref()
                            .map_or(0, |output| output.sample_rate),
                        ..Default::default()
                    }
                });
            }
            AudioCommand::Play => {
                if self.snapshot().state == PlaybackState::Playing {
                    return Ok(());
                }
                if self.snapshot().state == PlaybackState::Ended {
                    let path = self.snapshot().file_path.unwrap();
                    if let Some(output) = &mut self.output {
                        output.reset_source(
                            || Decoder::open(&path),
                            0.0,
                            self.config.source_pair,
                            false,
                            self.config.audition,
                        )?;
                    }
                    self.update(|status| status.position = 0.0);
                }
                if let Some(output) = &mut self.output {
                    output.start_running()?;
                    let buffering = output.is_waiting();
                    self.update_transport(|status| {
                        status.state = PlaybackState::Playing;
                        status.error = None;
                        status.buffering = buffering;
                    });
                } else {
                    if self.snapshot().state == PlaybackState::Ended {
                        self.update(|status| status.position = 0.0);
                    }
                    if let Err(error) = self.start_output() {
                        if self.needs_recovery && self.config.backend == AudioBackend::Asio {
                            self.select_wasapi_fallback(&error);
                        } else {
                            return Err(error);
                        }
                    }
                }
            }
            AudioCommand::Pause => {
                if self.snapshot().state == PlaybackState::Playing
                    && let Some(output) = &self.output
                {
                    self.suspend_output()?;
                    let position = output.position();
                    let ended = output.eof && output.realtime.ring.len() == 0;
                    self.update_transport(|status| {
                        status.position = position.min(status.duration);
                        status.buffering = false;
                        status.state = if ended {
                            PlaybackState::Ended
                        } else {
                            PlaybackState::Paused
                        };
                    });
                }
            }
            AudioCommand::Stop => {
                if let Some(output) = &mut self.output {
                    let path = self
                        .status
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .file_path
                        .clone()
                        .unwrap();
                    output.reset_source(
                        || Decoder::open(&path),
                        0.0,
                        self.config.source_pair,
                        false,
                        self.config.audition,
                    )?;
                }
                self.update_transport(|status| {
                    status.position = 0.0;
                    status.error = None;
                    status.buffering = false;
                    if status.file_path.is_some() {
                        status.state = PlaybackState::Stopped;
                    }
                });
            }
            AudioCommand::Seek(seconds) => {
                if !seconds.is_finite() {
                    return Err(Error::Validation("Seek position must be finite".into()));
                }
                let status = self.snapshot();
                let playing = status.state == PlaybackState::Playing;
                let position = seconds.clamp(0.0, status.duration);
                if let Some(output) = &mut self.output {
                    output.reset_source(
                        || Decoder::open(status.file_path.as_ref().unwrap()),
                        position,
                        self.config.source_pair,
                        playing,
                        self.config.audition,
                    )?;
                } else if playing {
                    self.start_output_at(Some(position))?;
                    return Ok(());
                }
                self.update_transport(|current| {
                    current.position = position;
                    if status.state == PlaybackState::Ended && position < status.duration {
                        current.state = PlaybackState::Paused;
                    }
                });
            }
            AudioCommand::SeekAndPlay(seconds) => {
                self.command(AudioCommand::Seek(seconds))?;
                self.command(AudioCommand::Play)?;
            }
            AudioCommand::SetVolume(volume) => {
                if !volume.is_finite() || !(0.0..=1.0).contains(&volume) {
                    return Err(Error::Validation("Volume must be 0..=1".into()));
                }
                self.config.volume = volume;
                if let Some(output) = &mut self.output {
                    output.volume = volume;
                    // While buffering the gain stays at zero; resuming restores the volume.
                    if output.buffering == Buffering::Off {
                        output
                            .realtime
                            .gain
                            .store(volume.to_bits(), Ordering::Relaxed);
                    }
                }
                self.update(|status| status.volume = volume);
            }
            AudioCommand::SetAudition(settings) => {
                settings.validate()?;
                let old = self.config.audition;
                let position = if (settings.pitch_semitones, settings.rate, settings.mono)
                    != (old.pitch_semitones, old.rate, old.mono)
                {
                    let status = self.snapshot();
                    let position = self
                        .output
                        .as_ref()
                        .map_or(status.position, |output| output.position())
                        .min(status.duration);
                    if let Some(output) = &mut self.output {
                        output.reset_source(
                            || Decoder::open(status.file_path.as_ref().unwrap()),
                            position,
                            self.config.source_pair,
                            status.state == PlaybackState::Playing,
                            settings,
                        )?;
                    }
                    Some(position)
                } else {
                    None
                };
                self.config.audition = settings;
                let confirm = |status: &mut PlaybackStatus| {
                    if let Some(position) = position {
                        status.position = position;
                    }
                    status.audition = settings;
                    status.error = None;
                };
                if position.is_some() {
                    self.update_transport(confirm);
                } else {
                    self.update(confirm);
                }
            }
            AudioCommand::Configure(config) => {
                config.validate()?;
                let previous = self.config.clone();
                let playing = self.snapshot().state == PlaybackState::Playing;
                let position = self.output.as_ref().map(|output| output.position());
                self.close_output();
                self.config = config;
                self.update(|status| {
                    status.volume = self.config.volume;
                    status.audition = self.config.audition;
                });
                self.needs_recovery = false;
                if playing && let Err(error) = self.start_output_at(position) {
                    self.config = previous;
                    self.update(|status| {
                        status.volume = self.config.volume;
                        if let Some(position) = position {
                            status.position = position.min(status.duration);
                        }
                    });
                    return Err(error);
                }
                if !playing {
                    self.update_transport(|status| {
                        if let Some(position) = position {
                            status.position = position.min(status.duration);
                        }
                    });
                }
            }
            AudioCommand::ConfigureSource {
                expected_generation,
                expected_path,
                path,
                mut config,
            } => {
                let before = self.snapshot();
                if before.generation != expected_generation
                    || before.file_path.as_ref() != Some(&expected_path)
                {
                    return Err(Error::Validation(
                        "The loaded source changed while output settings were prepared".into(),
                    ));
                }
                // Output settings do not edit audition transforms. Pause/seek/
                // transform commands admitted while the route downloaded win.
                config.audition = before.audition;
                config.validate()?;
                // Validate the new representation before closing valid output. All
                // decoder/network work stays on this worker, never in the callback.
                let decoder = Decoder::open(&path)?;
                let metadata = decoder.info.metadata.clone();
                if usize::from(config.source_pair) * 2 >= usize::from(metadata.channels) {
                    return Err(Error::Validation(
                        "Source channel pair is outside the replacement WAV channels".into(),
                    ));
                }
                let position = self
                    .output
                    .as_ref()
                    .map_or(before.position, |output| output.position());
                let previous_config = self.config.clone();
                self.close_output();
                self.config = config;
                self.needs_recovery = false;
                // Do not publish Loading/Ready: this is still the same logical
                // record/session and a paused audition must remain paused.
                self.update(|status| {
                    status.file_path = Some(path);
                    status.sample_rate = metadata.sample_rate;
                    status.channels = metadata.channels;
                    status.duration = metadata.duration;
                    status.position = position.min(metadata.duration);
                    status.volume = self.config.volume;
                    status.audition = self.config.audition;
                    status.buffering = false;
                    status.error = None;
                });
                if before.state == PlaybackState::Playing {
                    if let Err(error) = self.start_output_at(Some(position)) {
                        self.config = previous_config;
                        // Retain the previous representation for a later resume;
                        // execute_command reports failure as Paused safely.
                        self.update(|status| {
                            *status = PlaybackStatus { position, ..before };
                        });
                        return Err(error);
                    }
                } else {
                    self.update_transport(|_| {});
                }
            }
            AudioCommand::ControlPanel => {
                if self.config.backend != AudioBackend::Asio {
                    return Err(Error::Audio("Driver control panel requires ASIO".into()));
                }
                // Some vendor panels are modal. Pause before opening one so a blocked
                // control worker cannot starve the ring; retain the stream/driver for queries.
                if self.snapshot().state == PlaybackState::Playing {
                    self.command(AudioCommand::Pause)?;
                }
                // A live ASIO stream already owns the correct initialized global driver.
                // Re-enumerating from the first registration would fail for a selected
                // non-first driver because CPAL stops on DriverAlreadyExists.
                self.with_asio_panel_driver(
                    || {
                        let host = host(AudioBackend::Asio)?;
                        let mut device_iterator = host.output_devices().map_err(audio_error)?;
                        let device = if let Some(id) = &self.config.device_id {
                            device_iterator
                                .find(|device| {
                                    identity(device).is_ok_and(|candidate| candidate == *id)
                                })
                                .ok_or_else(|| {
                                    Error::Audio("Selected ASIO driver is unavailable".into())
                                })?
                        } else {
                            device_iterator
                                .next()
                                .ok_or_else(|| Error::Audio("No ASIO driver is available".into()))?
                        };
                        let _ = device.default_output_config().map_err(audio_error)?;
                        Ok(device_iterator)
                    },
                    || {
                        // SAFETY: with_asio_panel_driver holds ASIO_ACCESS and retains
                        // the initialized driver through this stream or its iterator.
                        unsafe { audiolib_windows::loaded_asio_control_panel() }
                    },
                )?;
            }
            AudioCommand::Synchronize | AudioCommand::Shutdown => {}
        }
        Ok(())
    }
    fn run(mut self, receiver: Receiver<CommandRequest>) {
        loop {
            match receiver.recv_timeout(Duration::from_millis(1)) {
                Ok(CommandRequest {
                    command: AudioCommand::Shutdown,
                    ..
                })
                | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Ok(request) => {
                    self.check_output_failure();
                    let result = self.execute_command(request.command);
                    if let Some(sender) = request.acknowledgment {
                        let _ = sender.try_send(result.map(|_| self.snapshot()));
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if self.check_output_failure() {
                continue;
            }
            self.release_idle_output(std::time::Instant::now());
            if self.snapshot().state != PlaybackState::Playing {
                continue;
            }
            let Some(output) = &mut self.output else {
                continue;
            };
            if let Err(error) = output.fill().and_then(|starved| output.regulate(starved)) {
                self.fill_failed(&error);
                continue;
            }
            let buffering = output.is_waiting();
            let position = output.position();
            let underruns = output.realtime.underruns.load(Ordering::Relaxed);
            let empty = output.eof && output.realtime.ring.len() == 0;
            if empty && output.drain_deadline.is_none() {
                let delay = Duration::from_nanos(
                    output
                        .realtime
                        .callback_latency_nanos
                        .load(Ordering::Relaxed),
                );
                output.drain_deadline = Some(std::time::Instant::now() + delay);
            }
            let ended = empty
                && output
                    .drain_deadline
                    .is_some_and(|deadline| std::time::Instant::now() >= deadline);
            self.update(|status| {
                status.position = position.min(status.duration);
                status.underruns = underruns;
                status.buffering = buffering;
            });
            if ended && self.config.audition.loop_enabled && self.snapshot().duration > 0.0 {
                if let Err(error) = self.execute_command(AudioCommand::Seek(0.0)) {
                    self.update(|status| status.error = Some(error.to_string()));
                }
                continue;
            }
            if ended {
                if let Some(output) = &self.output {
                    output.realtime.running.store(false, Ordering::Release);
                    let _ = output.pause_stream();
                }
                self.update_transport(|status| {
                    status.state = PlaybackState::Ended;
                    status.position = status.duration;
                });
            }
        }
        self.close_output();
    }
}

#[cfg(test)]
#[path = "seek_tests.rs"]
mod seek_tests;

#[cfg(test)]
#[path = "transport_tests.rs"]
mod transport_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn stream_error_realtime() -> Realtime {
        Realtime {
            ring: Ring::new(8),
            running: AtomicBool::new(true),
            gain: AtomicU32::new(0.8_f32.to_bits()),
            consumed: AtomicU64::new(17),
            underruns: AtomicU64::new(0),
            failed: AtomicBool::new(false),
            active: AtomicBool::new(true),
            callback_latency_nanos: AtomicU64::new(0),
            source_finished: AtomicBool::new(false),
            transition: SourceTransition::default(),
        }
    }

    #[test]
    fn coreaudio_recoverable_notifications_preserve_playback_and_count_overloads() {
        let realtime = stream_error_realtime();
        record_stream_error(&realtime, cpal::ErrorKind::Xrun, true);
        record_stream_error(&realtime, cpal::ErrorKind::RealtimeDenied, true);
        record_stream_error(&realtime, cpal::ErrorKind::Xrun, true);
        assert_eq!(realtime.underruns.load(Ordering::Relaxed), 2);
        assert!(!realtime.failed.load(Ordering::Acquire));
        assert!(realtime.running.load(Ordering::Acquire));
        assert_eq!(realtime.consumed.load(Ordering::Relaxed), 17);
    }

    #[test]
    fn coreaudio_fatal_errors_remain_sticky_after_recoverable_notifications() {
        for kind in [
            cpal::ErrorKind::DeviceNotAvailable,
            cpal::ErrorKind::StreamInvalidated,
            cpal::ErrorKind::Other,
        ] {
            let realtime = stream_error_realtime();
            record_stream_error(&realtime, kind, true);
            assert!(realtime.failed.load(Ordering::Acquire), "{kind:?}");
            assert_eq!(realtime.underruns.load(Ordering::Relaxed), 0);
            record_stream_error(&realtime, cpal::ErrorKind::RealtimeDenied, true);
            record_stream_error(&realtime, cpal::ErrorKind::Xrun, true);
            assert!(realtime.failed.load(Ordering::Acquire), "{kind:?}");
            assert_eq!(realtime.underruns.load(Ordering::Relaxed), 1);
        }
    }

    #[test]
    fn other_hosts_retain_fatal_stream_error_handling() {
        for kind in [
            cpal::ErrorKind::Xrun,
            cpal::ErrorKind::RealtimeDenied,
            cpal::ErrorKind::DeviceNotAvailable,
            cpal::ErrorKind::StreamInvalidated,
            cpal::ErrorKind::Other,
        ] {
            let realtime = stream_error_realtime();
            record_stream_error(&realtime, kind, false);
            assert!(realtime.failed.load(Ordering::Acquire), "{kind:?}");
            assert_eq!(realtime.underruns.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn queued_completion_keeps_load_failure_after_a_later_stop_clears_status() {
        let temp = tempfile::tempdir().unwrap();
        let engine = AudioEngine::new(PlaybackConfig::default()).unwrap();
        let handle = engine.handle();
        let load = handle
            .enqueue_with_ack(AudioCommand::Load(temp.path().join("missing.wav")))
            .unwrap();
        let stop = handle.enqueue_with_ack(AudioCommand::Stop).unwrap();
        let stopped = stop.wait(Duration::from_secs(4)).unwrap();
        assert_eq!(stopped.state, PlaybackState::Stopped);
        assert_eq!(stopped.error, None);
        assert_eq!(handle.status().error, None);
        assert!(matches!(
            load.wait(Duration::from_secs(4)),
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound
        ));
    }

    fn worker(status: PlaybackStatus) -> Worker {
        Worker::new(
            PlaybackConfig::default(),
            Arc::new(Mutex::new(status)),
            Arc::new(Mutex::new(Vec::new())),
        )
    }
    // No CPAL stream is constructed: these fixtures exercise the actual ring,
    // source decoder and control transitions without accessing audio hardware.
    fn prepared_output(path: &std::path::Path) -> Output {
        crate::tests::write_wav(path, 1000, 1, 16, 1, &vec![0; 2000], false, false);
        Output {
            stream: None,
            callback_available: false,
            asio: false,
            realtime: Arc::new(Realtime {
                ring: Ring::new(16),
                running: AtomicBool::new(true),
                gain: AtomicU32::new(0.8_f32.to_bits()),
                consumed: AtomicU64::new(0),
                underruns: AtomicU64::new(0),
                failed: AtomicBool::new(false),
                active: AtomicBool::new(false),
                callback_latency_nanos: AtomicU64::new(0),
                source_finished: AtomicBool::new(false),
                transition: SourceTransition::default(),
            }),
            decoder: AuditionDecoder::new(
                Decoder::open(path).unwrap(),
                1000,
                0,
                0,
                AuditionSettings::default(),
            )
            .unwrap(),
            sample_rate: 1000,
            volume: 0.8,
            buffering: Buffering::Off,
            pending: Vec::new(),
            pending_offset: 0,
            eof: false,
            pending_last: false,
            start_position: 0.0,
            drain_deadline: None,
        }
    }
    #[test]
    fn idle_wasapi_output_releases_after_grace_without_changing_transport() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("idle-wasapi.wav");
        for state in [
            PlaybackState::Ready,
            PlaybackState::Paused,
            PlaybackState::Stopped,
            PlaybackState::Ended,
        ] {
            let output = prepared_output(&path);
            output.realtime.running.store(false, Ordering::Release);
            let before = PlaybackStatus {
                generation: 4,
                transport_revision: 11,
                play_session_id: 7,
                state,
                file_path: Some(path.clone()),
                position: 0.25,
                duration: 1.0,
                sample_rate: 1000,
                channels: 1,
                volume: 0.8,
                active_backend: Some(AudioBackend::Wasapi),
                active_device_id: Some("retained-device".into()),
                actual_buffer_frames: 128,
                device_sample_rate: 1000,
                active_source_pair: Some(0),
                active_output_pair: Some(0),
                active_rate_policy: Some(RatePolicy::DeviceDefault),
                ..Default::default()
            };
            let mut worker = worker(before.clone());
            worker.output = Some(output);
            let now = std::time::Instant::now();
            worker.release_idle_output(now);
            worker.release_idle_output(now + WASAPI_IDLE_RELEASE - Duration::from_millis(1));
            assert!(worker.output.is_some(), "{state:?} lost fast stream reuse");
            worker.release_idle_output(now + WASAPI_IDLE_RELEASE);
            assert!(
                worker.output.is_none(),
                "{state:?} retained its audio client"
            );
            let expected = PlaybackStatus {
                active_backend: None,
                active_device_id: None,
                actual_buffer_frames: 0,
                device_sample_rate: 0,
                active_source_pair: None,
                active_output_pair: None,
                active_rate_policy: None,
                ..before
            };
            assert_eq!(
                serde_json::to_value(worker.snapshot()).unwrap(),
                serde_json::to_value(expected).unwrap(),
                "{state:?} changed its logical transport when releasing output"
            );
            worker.release_idle_output(now + Duration::from_secs(20));
            assert!(
                worker.output.is_none(),
                "idle output must not reopen itself"
            );
        }
    }

    #[test]
    fn playing_and_network_buffering_cancel_wasapi_idle_release() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("idle-resume.wav");
        let output = prepared_output(&path);
        let mut worker = worker(PlaybackStatus {
            state: PlaybackState::Paused,
            file_path: Some(path),
            duration: 1.0,
            play_session_id: 1,
            ..Default::default()
        });
        worker.output = Some(output);
        let now = std::time::Instant::now();
        worker.release_idle_output(now);
        worker.execute_command(AudioCommand::Play).unwrap();
        worker.release_idle_output(now + WASAPI_IDLE_RELEASE);
        assert!(worker.output.is_some());
        assert!(worker.output_idle_since.is_none());
        // Waiting for network data is still an active audition. The callback's
        // running flag is false here, but its stream must remain available.
        let output = worker.output.as_mut().unwrap();
        output.buffering = Buffering::Waiting(1000);
        output.realtime.running.store(false, Ordering::Release);
        worker.update(|status| status.buffering = true);
        worker.release_idle_output(now + Duration::from_secs(20));
        assert!(worker.output.is_some(), "network buffering released output");
        worker.execute_command(AudioCommand::Pause).unwrap();
        let paused_at = now + Duration::from_secs(21);
        worker.release_idle_output(paused_at);
        worker.release_idle_output(paused_at + WASAPI_IDLE_RELEASE - Duration::from_millis(1));
        assert!(
            worker.output.is_some(),
            "resume did not reset the idle grace"
        );
        worker.release_idle_output(paused_at + WASAPI_IDLE_RELEASE);
        assert!(worker.output.is_none());
    }

    #[test]
    fn idle_release_preserves_asio_stream_lifetime() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("idle-asio.wav");
        let mut output = prepared_output(&path);
        // This fake owns no CPAL driver. Keep the real ownership counter balanced
        // so Output::drop follows the same counted-stream invariant as production.
        ACTIVE_ASIO_STREAMS.fetch_add(1, Ordering::AcqRel);
        output.asio = true;
        let mut worker = worker(PlaybackStatus {
            state: PlaybackState::Paused,
            ..Default::default()
        });
        worker.output = Some(output);
        let now = std::time::Instant::now();
        worker.release_idle_output(now);
        worker.release_idle_output(now + Duration::from_secs(60));
        assert!(
            worker.output.is_some(),
            "ASIO lifetime changed with WASAPI policy"
        );
        assert!(worker.output_idle_since.is_none());
    }

    #[test]
    fn released_wasapi_output_retains_seek_transforms_and_playback_session() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("idle-session.wav");
        for state in [
            PlaybackState::Paused,
            PlaybackState::Stopped,
            PlaybackState::Ended,
        ] {
            let output = prepared_output(&path);
            let mut worker = worker(PlaybackStatus {
                generation: 4,
                play_session_id: 7,
                state,
                file_path: Some(path.clone()),
                position: if state == PlaybackState::Ended {
                    1.0
                } else {
                    0.0
                },
                duration: 1.0,
                sample_rate: 1000,
                channels: 1,
                ..Default::default()
            });
            worker.output = Some(output);
            let now = std::time::Instant::now();
            worker.release_idle_output(now);
            worker.release_idle_output(now + WASAPI_IDLE_RELEASE);
            assert!(worker.output.is_none());
            worker.execute_command(AudioCommand::Seek(0.25)).unwrap();
            let settings = AuditionSettings {
                mono: true,
                pitch_semitones: 2.0,
                rate: 0.8,
                ..Default::default()
            };
            worker
                .execute_command(AudioCommand::SetAudition(settings))
                .unwrap();
            worker
                .execute_command(AudioCommand::Configure(PlaybackConfig {
                    volume: 0.4,
                    audition: settings,
                    ..Default::default()
                }))
                .unwrap();
            assert!(worker.output.is_none());
            assert_eq!(worker.snapshot().position, 0.25);
            assert_eq!(worker.snapshot().audition, settings);
            // Substitute only device creation: reconstruct the decoder at the same
            // logical position/configuration as start_output, then use real Play.
            let mut resumed = prepared_output(&path);
            resumed
                .reset_source(|| Decoder::open(&path), 0.25, 0, false, settings)
                .unwrap();
            resumed.volume = worker.config.volume;
            worker.output = Some(resumed);
            worker.execute_command(AudioCommand::Play).unwrap();
            let status = worker.snapshot();
            assert_eq!(status.state, PlaybackState::Playing);
            assert_eq!(status.position, 0.25);
            assert_eq!(status.generation, 4);
            assert_eq!(status.audition, settings);
            assert_eq!(status.volume, 0.4);
            assert_eq!(
                status.play_session_id,
                if state == PlaybackState::Paused { 7 } else { 8 },
                "{state:?} changed playback history semantics after releasing output"
            );
        }
    }

    #[test]
    fn an_interrupted_read_is_a_stop_not_an_error() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("interrupted.wav");
        let output = prepared_output(&path);
        let mut worker = worker(PlaybackStatus::default());
        worker
            .execute_command(AudioCommand::Load(path.clone()))
            .unwrap();
        worker.output = Some(output);
        worker.execute_command(AudioCommand::Play).unwrap();
        // The worker loop's fill fails with the interrupt: Stopped, output kept, no error.
        worker.fill_failed(&Error::Io(audiolib_core::read_interrupted_error()));
        let status = worker.snapshot();
        assert_eq!(status.state, PlaybackState::Stopped);
        assert_eq!(status.error, None);
        assert_eq!(status.position, 0.0);
        assert!(worker.output.is_some());
        // A decoder that wraps the I/O error into its own message is recognised too.
        worker.execute_command(AudioCommand::Play).unwrap();
        worker.fill_failed(&Error::Audio(format!(
            "decode failed: {}",
            audiolib_core::AUDIO_READ_INTERRUPTED
        )));
        assert_eq!(worker.snapshot().state, PlaybackState::Stopped);
        assert!(worker.output.is_some());
        // Any other failure still closes the output and reports the error.
        worker.execute_command(AudioCommand::Play).unwrap();
        worker.fill_failed(&Error::Audio("device exploded".into()));
        assert_eq!(worker.snapshot().state, PlaybackState::Error);
        assert!(worker.output.is_none());
        // Stop clears the error of the failed playback.
        worker.execute_command(AudioCommand::Stop).unwrap();
        let status = worker.snapshot();
        assert_eq!(status.state, PlaybackState::Stopped);
        assert_eq!(status.error, None);
    }

    #[test]
    fn a_command_interrupted_by_stop_does_not_close_the_output() {
        use crate::source::{ByteSource, SourceOpener, register_stream_source};
        use std::sync::atomic::AtomicBool;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("streamed.wav");
        let output = prepared_output(&path);
        let bytes = std::fs::read(&path).unwrap();
        // The stream path's reads fail with the interrupt once `interrupt` is set.
        let virtual_path = temp.path().join("remote").join("streamed.wav");
        let interrupt = Arc::new(AtomicBool::new(false));
        let opener: SourceOpener = {
            let interrupt = interrupt.clone();
            Arc::new(move || {
                if interrupt.load(Ordering::SeqCst) {
                    return Err(audiolib_core::read_interrupted_error());
                }
                let source: Box<dyn ByteSource> = Box::new(std::io::Cursor::new(bytes.clone()));
                Ok((source, bytes.len() as u64))
            })
        };
        register_stream_source(&virtual_path, opener);
        let mut worker = worker(PlaybackStatus::default());
        worker
            .execute_command(AudioCommand::Load(virtual_path.clone()))
            .unwrap();
        worker.output = Some(output);
        worker.execute_command(AudioCommand::Play).unwrap();
        interrupt.store(true, Ordering::SeqCst);
        // A seek re-opens the source, which is interrupted: Cancelled, never Error.
        let result = worker.execute_command(AudioCommand::SeekAndPlay(1.5));
        assert!(matches!(result, Err(Error::Cancelled)));
        let status = worker.snapshot();
        assert_ne!(status.state, PlaybackState::Error);
        assert_eq!(status.error, None);
        // An interrupted load leaves nothing half-loaded behind.
        let result = worker.execute_command(AudioCommand::Load(virtual_path.clone()));
        assert!(matches!(result, Err(Error::Cancelled)));
        let status = worker.snapshot();
        assert_eq!(status.state, PlaybackState::Empty);
        assert_eq!(status.file_path, None);
        assert_eq!(status.error, None);
        crate::source::unregister_stream_source(&virtual_path);
    }

    #[test]
    fn a_network_source_buffers_before_playing_and_after_running_dry() {
        use crate::source::{
            ByteSource, SourceBuffer, SourceOpener, register_buffered_stream_source,
            unregister_stream_source,
        };
        struct Available {
            total: u64,
            available: AtomicU64,
        }
        impl SourceBuffer for Available {
            fn is_buffered(&self, offset: u64, len: u64) -> bool {
                offset.saturating_add(len).min(self.total) <= self.available.load(Ordering::SeqCst)
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let output = prepared_output(&temp.path().join("device.wav"));
        // 20 s at 1 kHz mono 16-bit: the decoder needs 8192 frames (16 KiB) per fill block.
        let path = temp.path().join("source.wav");
        crate::tests::write_wav(&path, 1000, 1, 16, 1, &vec![0; 40_000], false, false);
        let bytes = Arc::new(std::fs::read(&path).unwrap());
        let total = bytes.len() as u64;
        let buffer = Arc::new(Available {
            total,
            available: AtomicU64::new(44),
        });
        let opener: SourceOpener = {
            let bytes = bytes.clone();
            Arc::new(move || {
                let source: Box<dyn ByteSource> = Box::new(std::io::Cursor::new(bytes.to_vec()));
                Ok((source, bytes.len() as u64))
            })
        };
        let virtual_path = temp.path().join("remote").join("source.wav");
        register_buffered_stream_source(&virtual_path, opener, buffer.clone());
        let mut worker = worker(PlaybackStatus::default());
        worker.output = Some(output);
        worker
            .execute_command(AudioCommand::Load(virtual_path.clone()))
            .unwrap();
        // Only the header is in memory: Play waits silently (prebuffer), nothing is decoded.
        worker.execute_command(AudioCommand::Play).unwrap();
        let status = worker.snapshot();
        assert_eq!(status.state, PlaybackState::Playing);
        assert!(status.buffering);
        let output = worker.output.as_mut().unwrap();
        assert!(!output.realtime.running.load(Ordering::SeqCst));
        assert_eq!(
            f32::from_bits(output.realtime.gain.load(Ordering::SeqCst)),
            0.0
        );
        assert_eq!(output.realtime.ring.len(), 0);
        // The prebuffer arrives: playback starts at the configured volume.
        buffer.available.store(44 + 2 * 10_000, Ordering::SeqCst);
        output.regulate(false).unwrap();
        assert_eq!(output.buffering, Buffering::Off);
        assert!(output.realtime.running.load(Ordering::SeqCst));
        let volume = output.volume;
        assert_eq!(
            f32::from_bits(output.realtime.gain.load(Ordering::SeqCst)),
            volume
        );
        // Play until the buffered data runs out: the fill worker reports starvation instead
        // of blocking on the missing bytes.
        let mut starved = false;
        for _ in 0..10_000 {
            while output.realtime.ring.pop().is_some() {}
            if output.fill().unwrap() {
                starved = true;
                break;
            }
        }
        assert!(starved);
        assert!(!output.eof);
        // Ring empty and starved: fade out, then pause until the rebuffer is in memory.
        output.regulate(true).unwrap();
        assert!(matches!(output.buffering, Buffering::Fading(_)));
        assert_eq!(
            f32::from_bits(output.realtime.gain.load(Ordering::SeqCst)),
            0.0
        );
        assert!(output.realtime.running.load(Ordering::SeqCst));
        std::thread::sleep(BUFFERING_FADE + Duration::from_millis(5));
        output.regulate(true).unwrap();
        assert!(output.is_waiting());
        assert!(!output.realtime.running.load(Ordering::SeqCst));
        // A volume change while buffering keeps the output silent and applies on resume.
        worker
            .execute_command(AudioCommand::SetVolume(0.5))
            .unwrap();
        let output = worker.output.as_mut().unwrap();
        assert_eq!(
            f32::from_bits(output.realtime.gain.load(Ordering::SeqCst)),
            0.0
        );
        output.regulate(true).unwrap();
        assert!(output.is_waiting(), "nothing new arrived");
        buffer.available.store(total, Ordering::SeqCst);
        output.regulate(false).unwrap();
        assert_eq!(output.buffering, Buffering::Off);
        assert!(output.realtime.running.load(Ordering::SeqCst));
        assert_eq!(
            f32::from_bits(output.realtime.gain.load(Ordering::SeqCst)),
            0.5
        );
        assert!(output.realtime.ring.len() > 0);
        // Pausing clears the reported buffering state.
        worker.execute_command(AudioCommand::Pause).unwrap();
        assert!(!worker.snapshot().buffering);
        unregister_stream_source(&virtual_path);
    }

    #[test]
    fn small_and_identical_seek_advance_transport_without_new_source_or_history_session() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("small-seek.wav");
        let output = prepared_output(&path);
        let mut worker = worker(PlaybackStatus {
            transport_revision: 7,
            ..Default::default()
        });
        worker.execute_command(AudioCommand::Load(path)).unwrap();
        assert_eq!(worker.snapshot().transport_revision, 8);
        worker.output = Some(output);
        worker.execute_command(AudioCommand::Play).unwrap();
        let playing = worker.snapshot();
        assert_eq!(playing.transport_revision, 9);
        for position in [0.05, 0.05] {
            let revision = worker.snapshot().transport_revision;
            worker
                .execute_command(AudioCommand::Seek(position))
                .unwrap();
            let status = worker.snapshot();
            assert_eq!(status.transport_revision, revision + 1);
            assert_eq!(status.position, position);
            assert_eq!(status.state, PlaybackState::Playing);
            assert_eq!(status.generation, playing.generation);
            assert_eq!(status.play_session_id, playing.play_session_id);
        }
        let revision = worker.snapshot().transport_revision;
        worker.execute_command(AudioCommand::Play).unwrap();
        worker
            .execute_command(AudioCommand::SetVolume(0.5))
            .unwrap();
        worker
            .execute_command(AudioCommand::SetAudition(AuditionSettings {
                loop_enabled: true,
                ..Default::default()
            }))
            .unwrap();
        worker.update(|status| status.position = 0.08);
        assert_eq!(worker.snapshot().transport_revision, revision);
    }

    #[test]
    fn representation_replacement_retains_cursor_audition_and_history_session() {
        let temp = tempfile::tempdir().unwrap();
        let stereo = temp.path().join("stereo-preview.wav");
        let multichannel = temp.path().join("all-channel-preview.wav");
        crate::tests::write_wav(&stereo, 1000, 2, 16, 1, &vec![0; 4000], false, false);
        crate::tests::write_wav(&multichannel, 1000, 4, 16, 1, &vec![0; 8000], false, false);
        for state in [
            PlaybackState::Ready,
            PlaybackState::Paused,
            PlaybackState::Stopped,
            PlaybackState::Ended,
        ] {
            let audition = AuditionSettings {
                rate: 0.5,
                pitch_semitones: 3.0,
                mono: true,
                loop_enabled: true,
            };
            let mut worker = worker(PlaybackStatus {
                generation: 8,
                transport_revision: 13,
                play_session_id: 4,
                state,
                file_path: Some(stereo.clone()),
                position: 0.375,
                duration: 1.0,
                channels: 2,
                sample_rate: 1000,
                audition,
                ..Default::default()
            });
            worker
                .execute_command(AudioCommand::ConfigureSource {
                    expected_generation: 8,
                    expected_path: stereo.clone(),
                    path: multichannel.clone(),
                    config: PlaybackConfig {
                        source_pair: 1,
                        audition,
                        ..Default::default()
                    },
                })
                .unwrap();
            let replaced = worker.snapshot();
            assert_eq!(replaced.file_path, Some(multichannel.clone()));
            assert_eq!(replaced.generation, 8);
            assert_eq!(replaced.play_session_id, 4);
            assert_eq!(replaced.transport_revision, 14);
            assert_eq!(replaced.position, 0.375);
            assert_eq!(replaced.channels, 4);
            assert_eq!(replaced.state, state);
            assert_eq!(replaced.audition, audition);
            if state == PlaybackState::Paused {
                // A hardware-free prepared output exercises the actual session
                // publication after resuming the replaced paused source.
                worker.output = Some(prepared_output(&temp.path().join("device.wav")));
                worker.execute_command(AudioCommand::Play).unwrap();
                assert_eq!(worker.snapshot().play_session_id, 4);
            }
        }
    }

    #[test]
    fn stale_or_wrong_channel_representation_never_replaces_the_source() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("stereo-preview.wav");
        crate::tests::write_wav(&source, 1000, 2, 16, 1, &vec![0; 4000], false, false);
        for (generation, expected, pair) in [
            (7, source.clone(), 0),
            (8, temp.path().join("other.wav"), 0),
            (8, source.clone(), 1),
        ] {
            let mut worker = worker(PlaybackStatus {
                generation: 8,
                transport_revision: 13,
                play_session_id: 4,
                state: PlaybackState::Paused,
                file_path: Some(source.clone()),
                position: 0.375,
                duration: 1.0,
                channels: 2,
                ..Default::default()
            });
            let result = worker.execute_command(AudioCommand::ConfigureSource {
                expected_generation: generation,
                expected_path: expected,
                path: source.clone(),
                config: PlaybackConfig {
                    source_pair: pair,
                    ..Default::default()
                },
            });
            assert!(matches!(result, Err(Error::Validation(_))));
            let retained = worker.snapshot();
            assert_eq!(retained.file_path, Some(source.clone()));
            assert_eq!(retained.generation, 8);
            assert_eq!(retained.position, 0.375);
            assert_eq!(retained.play_session_id, 4);
            assert_eq!(retained.state, PlaybackState::Paused);
            assert_eq!(retained.transport_revision, 13);
        }
    }

    #[test]
    fn engine_eof_loop_restarts_transport_without_new_history_session() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("loop.wav");
        let mut output = prepared_output(&path);
        output.eof = true;
        output.start_position = 1.0;
        output.drain_deadline = Some(std::time::Instant::now());
        let mut worker = worker(PlaybackStatus {
            generation: 5,
            transport_revision: 12,
            play_session_id: 2,
            state: PlaybackState::Playing,
            file_path: Some(path),
            duration: 1.0,
            position: 1.0,
            ..Default::default()
        });
        worker.config.audition.loop_enabled = true;
        worker.output = Some(output);
        let status = worker.status.clone();
        let (commands, receiver) = mpsc::sync_channel(1);
        let thread = thread::spawn(move || worker.run(receiver));
        // The worker completes EOF handling after acknowledging each command.
        // The second barrier therefore observes the actual automatic loop path.
        for _ in 0..2 {
            let (acknowledgment, received) = mpsc::sync_channel(1);
            commands
                .send(CommandRequest {
                    command: AudioCommand::Synchronize,
                    acknowledgment: Some(acknowledgment),
                })
                .unwrap();
            received
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .unwrap();
        }
        let looped = status.lock().unwrap().clone();
        drop(commands);
        thread.join().unwrap();
        assert_eq!(looped.transport_revision, 13);
        assert_eq!(looped.position, 0.0);
        assert_eq!(looped.state, PlaybackState::Playing);
        assert_eq!(looped.generation, 5);
        assert_eq!(looped.play_session_id, 2);
    }

    #[test]
    fn clock_transitions_advance_transport_while_history_session_semantics_are_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("clock-transitions.wav");
        let output = prepared_output(&path);
        let mut worker = worker(PlaybackStatus::default());
        worker.execute_command(AudioCommand::Load(path)).unwrap();
        worker.output = Some(output);
        worker.execute_command(AudioCommand::Play).unwrap();
        let playing = worker.snapshot();
        worker.execute_command(AudioCommand::Pause).unwrap();
        assert_eq!(
            worker.snapshot().transport_revision,
            playing.transport_revision + 1
        );
        worker.execute_command(AudioCommand::Play).unwrap();
        assert_eq!(
            worker.snapshot().transport_revision,
            playing.transport_revision + 2
        );
        assert_eq!(worker.snapshot().play_session_id, playing.play_session_id);
        worker
            .execute_command(AudioCommand::SetAudition(AuditionSettings {
                rate: 0.25,
                ..Default::default()
            }))
            .unwrap();
        assert_eq!(
            worker.snapshot().transport_revision,
            playing.transport_revision + 3
        );
        worker.execute_command(AudioCommand::Pause).unwrap();
        worker
            .execute_command(AudioCommand::Configure(PlaybackConfig::default()))
            .unwrap();
        assert_eq!(
            worker.snapshot().transport_revision,
            playing.transport_revision + 5
        );
        worker.execute_command(AudioCommand::Stop).unwrap();
        assert_eq!(
            worker.snapshot().transport_revision,
            playing.transport_revision + 6
        );
        worker.output = Some(prepared_output(
            worker.snapshot().file_path.as_ref().unwrap(),
        ));
        worker.execute_command(AudioCommand::Play).unwrap();
        let restarted = worker.snapshot();
        assert_eq!(restarted.transport_revision, playing.transport_revision + 7);
        assert_eq!(restarted.generation, playing.generation);
        assert_eq!(restarted.play_session_id, playing.play_session_id + 1);
    }

    #[test]
    fn rejected_and_failed_source_commands_do_not_advance_transport() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("prepared.wav");
        for (command, state, has_output) in [
            (AudioCommand::Seek(f64::NAN), PlaybackState::Playing, true),
            (AudioCommand::Seek(0.15), PlaybackState::Playing, true),
            (AudioCommand::Seek(0.15), PlaybackState::Playing, false),
            (AudioCommand::Stop, PlaybackState::Playing, true),
            (AudioCommand::Play, PlaybackState::Ready, false),
            (
                AudioCommand::SetAudition(AuditionSettings {
                    mono: true,
                    ..Default::default()
                }),
                PlaybackState::Playing,
                true,
            ),
            (
                AudioCommand::Configure(PlaybackConfig::default()),
                PlaybackState::Playing,
                false,
            ),
            (
                AudioCommand::Load(temp.path().join("missing-load.wav")),
                PlaybackState::Playing,
                true,
            ),
        ] {
            let mut worker = worker(PlaybackStatus {
                generation: 5,
                transport_revision: 17,
                play_session_id: 2,
                state,
                file_path: Some(temp.path().join("missing.wav")),
                duration: 1.0,
                position: 0.1,
                ..Default::default()
            });
            if has_output {
                worker.output = Some(prepared_output(&source));
            }
            assert!(worker.execute_command(command).is_err());
            assert_eq!(worker.snapshot().transport_revision, 17);
        }
    }
    #[test]
    fn playback_sessions_change_only_after_successful_new_playback() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sessions.wav");
        let output = prepared_output(&path);
        let mut worker = worker(PlaybackStatus::default());
        let (sender, events) = mpsc::sync_channel(256);
        worker.subscribers.lock().unwrap().push(EventSubscriber {
            sender,
            playback_only: false,
        });
        worker
            .execute_command(AudioCommand::Load(path.clone()))
            .unwrap();
        assert_eq!(worker.snapshot().play_session_id, 0);
        let generation = worker.snapshot().generation;
        worker.output = Some(output);
        worker.execute_command(AudioCommand::Play).unwrap();
        assert_eq!(worker.snapshot().play_session_id, 1);
        worker.execute_command(AudioCommand::Play).unwrap();
        worker.execute_command(AudioCommand::Pause).unwrap();
        worker.execute_command(AudioCommand::Seek(0.25)).unwrap();
        worker
            .execute_command(AudioCommand::SetAudition(AuditionSettings {
                mono: true,
                ..Default::default()
            }))
            .unwrap();
        // Paused reconfiguration closes the output, then a retained test stream
        // exercises the same successful resume publication without opening hardware.
        worker
            .execute_command(AudioCommand::Configure(PlaybackConfig::default()))
            .unwrap();
        worker.output = Some(prepared_output(&path));
        worker.execute_command(AudioCommand::Play).unwrap();
        worker.execute_command(AudioCommand::Seek(0.5)).unwrap();
        assert_eq!(worker.snapshot().play_session_id, 1);
        worker.execute_command(AudioCommand::Stop).unwrap();
        assert_eq!(worker.snapshot().play_session_id, 1);
        worker.execute_command(AudioCommand::Play).unwrap();
        assert_eq!(worker.snapshot().play_session_id, 2);
        // Exercise Pause's real EOF transition: no queued PCM remains to resume.
        let output = worker.output.as_mut().unwrap();
        output.eof = true;
        output.realtime.ring.clear_suspended();
        worker.execute_command(AudioCommand::Pause).unwrap();
        assert_eq!(worker.snapshot().state, PlaybackState::Ended);
        worker.execute_command(AudioCommand::Seek(0.2)).unwrap();
        assert_eq!(worker.snapshot().state, PlaybackState::Paused);
        worker.execute_command(AudioCommand::Play).unwrap();
        assert_eq!(worker.snapshot().play_session_id, 3);
        assert_eq!(worker.snapshot().generation, generation);
        let starts: Vec<_> = events
            .try_iter()
            .filter_map(|event| match event {
                AudioEvent::PlaybackStarted {
                    generation,
                    play_session_id,
                    file_path,
                } => Some((generation, play_session_id, file_path)),
                _ => None,
            })
            .collect();
        assert_eq!(
            starts,
            vec![
                (generation, 1, path.clone()),
                (generation, 2, path.clone()),
                (generation, 3, path)
            ]
        );
    }

    #[test]
    fn ended_play_starts_again_and_load_changes_event_source_atomically() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first.wav");
        let second = temp.path().join("second.wav");
        let output = prepared_output(&first);
        let _ = prepared_output(&second);
        let mut worker = worker(PlaybackStatus::default());
        let (sender, events) = mpsc::sync_channel(256);
        worker.subscribers.lock().unwrap().push(EventSubscriber {
            sender,
            playback_only: false,
        });
        worker
            .execute_command(AudioCommand::Load(first.clone()))
            .unwrap();
        worker.output = Some(output);
        worker.execute_command(AudioCommand::Play).unwrap();
        let output = worker.output.as_mut().unwrap();
        output.eof = true;
        output.realtime.ring.clear_suspended();
        worker.execute_command(AudioCommand::Pause).unwrap();
        worker.execute_command(AudioCommand::Play).unwrap();
        assert_eq!(worker.snapshot().play_session_id, 2);
        worker
            .execute_command(AudioCommand::Load(second.clone()))
            .unwrap();
        assert_eq!(worker.snapshot().generation, 2);
        assert_eq!(worker.snapshot().play_session_id, 0);
        worker.execute_command(AudioCommand::Play).unwrap();
        let confirmed = worker.snapshot();
        assert_eq!(confirmed.generation, 2);
        assert_eq!(confirmed.play_session_id, 1);
        let starts: Vec<_> = events
            .try_iter()
            .filter_map(|event| match event {
                AudioEvent::PlaybackStarted {
                    generation,
                    play_session_id,
                    file_path,
                } => Some((generation, play_session_id, file_path)),
                _ => None,
            })
            .collect();
        assert_eq!(
            starts,
            vec![(1, 1, first.clone()), (1, 2, first), (2, 1, second)]
        );
    }

    #[test]
    fn failed_play_does_not_allocate_a_playback_session() {
        let temp = tempfile::tempdir().unwrap();
        let mut worker = worker(PlaybackStatus {
            generation: 5,
            state: PlaybackState::Ready,
            file_path: Some(temp.path().join("missing.wav")),
            ..Default::default()
        });
        let (sender, events) = mpsc::sync_channel(256);
        worker.subscribers.lock().unwrap().push(EventSubscriber {
            sender,
            playback_only: false,
        });
        // Missing source fails before driver discovery, even with the ASIO feature.
        assert!(worker.execute_command(AudioCommand::Play).is_err());
        assert_eq!(worker.snapshot().play_session_id, 0);
        assert!(
            !events
                .try_iter()
                .any(|event| matches!(event, AudioEvent::PlaybackStarted { .. }))
        );
    }

    #[test]
    fn playback_start_subscription_is_not_starved_by_position_events() {
        let worker = worker(PlaybackStatus::default());
        let (commands, _) = mpsc::sync_channel(1);
        let handle = AudioHandle {
            commands,
            status: worker.status.clone(),
            subscribers: worker.subscribers.clone(),
        };
        let standard = handle.subscribe();
        let starts = handle.subscribe_playback_starts();
        for frame in 0..512 {
            worker.publish(AudioEvent::Position {
                generation: 1,
                position: frame as f64,
            });
        }
        worker.publish(AudioEvent::PlaybackStarted {
            generation: 1,
            play_session_id: 1,
            file_path: PathBuf::from("old.wav"),
        });
        worker.publish(AudioEvent::PlaybackStarted {
            generation: 2,
            play_session_id: 1,
            file_path: PathBuf::from("new.wav"),
        });
        assert_eq!(standard.try_iter().count(), 256);
        let sources: Vec<_> = starts
            .try_iter()
            .map(|event| match event {
                AudioEvent::PlaybackStarted { file_path, .. } => file_path,
                _ => panic!("Playback-only subscription received unrelated traffic"),
            })
            .collect();
        assert_eq!(sources, ["old.wav", "new.wav"].map(PathBuf::from));
    }

    #[test]
    fn waveform_seek_and_play_starts_at_target_and_preserves_session_rules() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("click-play.wav");
        let output = prepared_output(&path);
        let mut worker = worker(PlaybackStatus::default());
        worker.execute_command(AudioCommand::Load(path)).unwrap();
        worker.output = Some(output);
        let generation = worker.snapshot().generation;
        for (state, position, session) in [
            (PlaybackState::Ready, 0.25, 1),
            (PlaybackState::Playing, 0.55, 1),
            (PlaybackState::Paused, 0.15, 1),
            (PlaybackState::Stopped, 0.65, 2),
            (PlaybackState::Ended, 0.35, 3),
        ] {
            match state {
                PlaybackState::Paused => worker.execute_command(AudioCommand::Pause).unwrap(),
                PlaybackState::Stopped => worker.execute_command(AudioCommand::Stop).unwrap(),
                PlaybackState::Ended => {
                    let output = worker.output.as_mut().unwrap();
                    output.eof = true;
                    output.realtime.ring.clear_suspended();
                    worker.execute_command(AudioCommand::Pause).unwrap();
                }
                _ => {}
            }
            assert_eq!(worker.snapshot().state, state);
            let revision = worker.snapshot().transport_revision;
            worker
                .execute_command(AudioCommand::SeekAndPlay(position))
                .unwrap();
            let status = worker.snapshot();
            assert_eq!(status.state, PlaybackState::Playing);
            assert!((status.position - position).abs() < 0.001);
            assert!((worker.output.as_ref().unwrap().position() - position).abs() < 0.001);
            assert_eq!(status.play_session_id, session);
            assert_eq!(status.generation, generation);
            assert!(status.transport_revision > revision);
        }
        worker.execute_command(AudioCommand::Pause).unwrap();
        let position = worker.snapshot().position;
        assert!(
            worker
                .execute_command(AudioCommand::SeekAndPlay(f64::NAN))
                .is_err()
        );
        assert_eq!(worker.snapshot().state, PlaybackState::Paused);
        assert_eq!(worker.snapshot().position, position);
        std::fs::remove_file(worker.snapshot().file_path.unwrap()).unwrap();
        assert!(
            worker
                .execute_command(AudioCommand::SeekAndPlay(0.2))
                .is_err()
        );
        assert_eq!(worker.snapshot().state, PlaybackState::Error);
        assert_eq!(worker.snapshot().play_session_id, 3);
    }

    #[test]
    fn seek_from_ended_preserves_requested_resume_position() {
        let mut worker = worker(PlaybackStatus {
            state: PlaybackState::Ended,
            duration: 3.0,
            position: 3.0,
            file_path: Some(PathBuf::from("loaded.wav")),
            ..Default::default()
        });
        worker.execute_command(AudioCommand::Seek(1.25)).unwrap();
        assert_eq!(worker.snapshot().state, PlaybackState::Paused);
        assert_eq!(worker.snapshot().position, 1.25);
        worker.update(|status| {
            status.state = PlaybackState::Ended;
            status.position = status.duration;
        });
        worker.execute_command(AudioCommand::Pause).unwrap();
        assert_eq!(worker.snapshot().state, PlaybackState::Ended);
    }
    #[test]
    fn closing_output_clears_actual_device_information_without_a_stream() {
        let mut worker = worker(PlaybackStatus {
            active_backend: Some(AudioBackend::Asio),
            active_device_id: Some("removed-driver".into()),
            actual_buffer_frames: 256,
            device_sample_rate: 192000,
            active_source_pair: Some(2),
            active_output_pair: Some(3),
            active_rate_policy: Some(RatePolicy::Native),
            ..Default::default()
        });
        worker.close_output();
        let status = worker.snapshot();
        assert_eq!(status.active_backend, None);
        assert_eq!(status.active_device_id, None);
        assert_eq!(status.actual_buffer_frames, 0);
        assert_eq!(status.device_sample_rate, 0);
        assert_eq!(status.active_source_pair, None);
        assert_eq!(status.active_output_pair, None);
        assert_eq!(status.active_rate_policy, None);
    }
    #[test]
    fn asio_fallback_selects_default_resampling_without_claiming_an_active_route() {
        let mut worker = worker(PlaybackStatus {
            state: PlaybackState::Paused,
            file_path: Some(PathBuf::from("loaded.wav")),
            duration: 1.0,
            position: 0.5,
            ..Default::default()
        });
        worker.config.backend = AudioBackend::Asio;
        worker.config.device_id = Some("unavailable-driver".into());
        worker.config.output_pair = 3;
        worker.config.source_pair = 2;
        worker.config.buffer_frames = 128;
        worker.config.rate_policy = RatePolicy::Native;
        worker.needs_recovery = true;
        worker.select_wasapi_fallback(&Error::Audio("driver unavailable".into()));
        assert_eq!(worker.config.backend, AudioBackend::Wasapi);
        assert_eq!(worker.config.device_id, None);
        assert_eq!(worker.config.output_pair, 0);
        assert_eq!(worker.config.source_pair, 2);
        assert_eq!(worker.config.rate_policy, RatePolicy::DeviceDefault);
        assert!(!worker.needs_recovery);
        let status = worker.snapshot();
        assert_eq!(status.state, PlaybackState::Paused);
        assert_eq!(status.position, 0.5);
        assert_eq!(status.active_backend, None);
        assert_eq!(status.active_output_pair, None);
        assert_eq!(status.active_rate_policy, None);
        assert!(
            status
                .error
                .unwrap()
                .contains("device default with resampling")
        );
    }
    #[test]
    fn active_non_first_asio_panel_reuses_driver_and_unlocks_after_error() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("source.wav");
        let mut output = prepared_output(&path);
        // Model an ASIO lease without constructing CPAL/SDK resources. Its Drop
        // still exercises the real serialization lock and active-lease counter.
        output.asio = true;
        ACTIVE_ASIO_STREAMS.fetch_add(1, Ordering::AcqRel);
        let mut worker = worker(PlaybackStatus::default());
        worker.config.backend = AudioBackend::Asio;
        worker.config.device_id = Some("non-first-driver".into());
        worker.output = Some(output);
        let invoked = AtomicUsize::new(0);
        let result = worker.with_asio_panel_driver(
            || -> Result<()> { panic!("active ASIO driver must not be re-enumerated") },
            || {
                invoked.fetch_add(1, Ordering::Relaxed);
                Err(Error::Audio("simulated panel failure".into()))
            },
        );
        assert!(result.is_err());
        assert_eq!(invoked.load(Ordering::Relaxed), 1);
        // Cleanup takes the same SDK lock, verifying that the panel-error path
        // released it before the retained output lease is destroyed.
        worker.close_output();
        assert!(worker.output.is_none());
    }
    #[test]
    fn rejected_parameters_preserve_playback_and_prepared_pcm() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("source.wav");
        let output = prepared_output(&path);
        output.realtime.ring.push_aligned(&[0.1, 0.2], 2);
        let mut worker = worker(PlaybackStatus {
            state: PlaybackState::Playing,
            file_path: Some(path),
            duration: 1.0,
            position: 0.25,
            volume: 0.8,
            ..Default::default()
        });
        worker.output = Some(output);
        for command in [
            AudioCommand::Seek(f64::NAN),
            AudioCommand::SetVolume(1.1),
            AudioCommand::Configure(PlaybackConfig {
                buffer_frames: 65537,
                ..Default::default()
            }),
        ] {
            assert!(worker.execute_command(command).is_err());
            let status = worker.snapshot();
            assert_eq!(status.state, PlaybackState::Playing);
            assert_eq!(status.position, 0.25);
            assert_eq!(status.volume, 0.8);
            let output = worker.output.as_ref().unwrap();
            assert!(output.realtime.running.load(Ordering::Acquire));
            assert_eq!(output.realtime.ring.len(), 2);
        }
    }
    #[test]
    fn paused_driver_failure_closes_output_and_emits_generation_event() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("source.wav");
        let output = prepared_output(&path);
        output.realtime.failed.store(true, Ordering::Release);
        output.realtime.consumed.store(125, Ordering::Relaxed);
        let mut worker = worker(PlaybackStatus {
            generation: 7,
            state: PlaybackState::Paused,
            file_path: Some(path),
            duration: 1.0,
            active_backend: Some(AudioBackend::Asio),
            active_device_id: Some("driver".into()),
            actual_buffer_frames: 128,
            device_sample_rate: 1000,
            ..Default::default()
        });
        let (sender, events) = mpsc::sync_channel(16);
        worker.subscribers.lock().unwrap().push(EventSubscriber {
            sender,
            playback_only: false,
        });
        worker.output = Some(output);
        assert!(worker.check_output_failure());
        assert!(worker.output.is_none());
        assert!(worker.needs_recovery);
        let status = worker.snapshot();
        assert_eq!(status.state, PlaybackState::Paused);
        assert_eq!(status.position, 0.125);
        assert_eq!(status.active_backend, None);
        assert_eq!(status.actual_buffer_frames, 0);
        assert!(
            events
                .try_iter()
                .any(|event| matches!(event, AudioEvent::DeviceLost { generation: 7, .. }))
        );
    }
    #[test]
    fn failed_load_suspends_previous_pcm_and_resets_new_source_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("source.wav");
        let output = prepared_output(&path);
        let realtime = output.realtime.clone();
        let mut worker = worker(PlaybackStatus {
            generation: 2,
            state: PlaybackState::Playing,
            file_path: Some(path),
            duration: 1.0,
            position: 0.75,
            sample_rate: 1000,
            channels: 1,
            active_backend: Some(AudioBackend::Wasapi),
            actual_buffer_frames: 256,
            device_sample_rate: 1000,
            ..Default::default()
        });
        worker.output = Some(output);
        let missing = temp.path().join("missing.wav");
        assert!(
            worker
                .execute_command(AudioCommand::Load(missing.clone()))
                .is_err()
        );
        assert!(!realtime.running.load(Ordering::Acquire));
        assert!(worker.output.is_none());
        let status = worker.snapshot();
        assert_eq!(status.generation, 3);
        assert_eq!(status.state, PlaybackState::Error);
        assert_eq!(status.file_path, Some(missing));
        assert_eq!(status.duration, 0.0);
        assert_eq!(status.position, 0.0);
        assert_eq!(status.sample_rate, 0);
        assert_eq!(status.channels, 0);
        assert_eq!(status.active_backend, None);
    }
    #[test]
    fn configure_open_failure_restores_previous_settings_and_pauses() {
        let temp = tempfile::tempdir().unwrap();
        let mut worker = worker(PlaybackStatus {
            state: PlaybackState::Playing,
            file_path: Some(temp.path().join("missing.wav")),
            duration: 1.0,
            position: 0.5,
            volume: 0.8,
            ..Default::default()
        });
        let config = PlaybackConfig {
            backend: AudioBackend::Asio,
            device_id: Some("unavailable-driver".into()),
            volume: 0.2,
            buffer_frames: 128,
            ..Default::default()
        };
        // Decoder opening fails before host discovery, so this test cannot open
        // a real device or produce audio, including on machines with ASIO.
        assert!(
            worker
                .execute_command(AudioCommand::Configure(config))
                .is_err()
        );
        assert_eq!(worker.config.backend, AudioBackend::Wasapi);
        assert_eq!(worker.config.device_id, None);
        assert_eq!(worker.config.buffer_frames, 0);
        assert_eq!(worker.config.volume, 0.8);
        let status = worker.snapshot();
        assert_eq!(status.state, PlaybackState::Paused);
        assert_eq!(status.position, 0.5);
        assert_eq!(status.volume, 0.8);
        assert_eq!(status.active_backend, None);
        assert!(status.error.is_some());
    }
    #[test]
    fn bounded_pcm_ring_preserves_order_and_wraps() {
        let ring = Ring::new(4);
        assert_eq!(ring.push(&[0.1, 0.2, 0.3, 0.4, 0.5]), 4);
        assert_eq!(ring.pop(), Some(0.1));
        assert_eq!(ring.pop(), Some(0.2));
        assert_eq!(ring.push(&[0.5, 0.6]), 2);
        for value in [0.3, 0.4, 0.5, 0.6] {
            assert_eq!(ring.pop(), Some(value));
        }
        assert_eq!(ring.pop(), None);
    }
    #[test]
    fn config_and_monitor_mapping() {
        assert!(PlaybackConfig::default().validate().is_ok());
        assert_eq!(monitor_pair(&[0.5], 0), (0.5, 0.5));
        assert_eq!(monitor_pair(&[0.1, 0.2], 0), (0.1, 0.2));
        assert_eq!(monitor_pair(&[0.1, 0.2, 0.3, 0.4], 1), (0.3, 0.4));
    }
    #[test]
    fn stereo_ring_routes_to_hardware_pair_and_preserves_frame_alignment() {
        let realtime = Arc::new(Realtime {
            ring: Ring::new(8),
            running: AtomicBool::new(true),
            gain: AtomicU32::new(1.0_f32.to_bits()),
            consumed: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            failed: AtomicBool::new(false),
            active: AtomicBool::new(false),
            callback_latency_nanos: AtomicU64::new(0),
            source_finished: AtomicBool::new(false),
            transition: SourceTransition::default(),
        });
        assert_eq!(realtime.ring.push_aligned(&[0.2, -0.3, 0.4], 2), 2);
        let mut callback = RenderCallback {
            realtime: realtime.clone(),
            channels: 16,
            pair: 2,
            ramp_step: 1.0,
            gain: 1.0,
            envelope: SeekEnvelope::default(),
        };
        let mut output = [9.0_f32; 32];
        callback.render(&mut output);
        assert_eq!(output[4], 0.2);
        assert_eq!(output[5], -0.3);
        for (index, sample) in output.iter().enumerate() {
            if ![4, 5].contains(&index) {
                assert_eq!(*sample, 0.0);
            }
        }
        assert_eq!(realtime.consumed.load(Ordering::Relaxed), 1);
        assert_eq!(realtime.ring.len(), 0);
    }
    #[test]
    fn callback_live_gain_ramp_pause_resume_and_underrun() {
        let realtime = Arc::new(Realtime {
            ring: Ring::new(64),
            running: AtomicBool::new(true),
            gain: AtomicU32::new(0.8_f32.to_bits()),
            consumed: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            failed: AtomicBool::new(false),
            active: AtomicBool::new(false),
            callback_latency_nanos: AtomicU64::new(0),
            source_finished: AtomicBool::new(false),
            transition: SourceTransition::default(),
        });
        realtime.ring.push(&[1.0; 32]);
        let mut callback = RenderCallback {
            realtime: realtime.clone(),
            channels: 2,
            pair: 0,
            ramp_step: 0.2,
            gain: 0.0,
            envelope: SeekEnvelope::default(),
        };
        let mut output = [0.0_f32; 8];
        callback.render(&mut output);
        assert!((output[0] - 0.2).abs() < 1e-6);
        assert!((output[6] - 0.8).abs() < 1e-6);
        realtime.gain.store(0.2_f32.to_bits(), Ordering::Relaxed);
        callback.render(&mut output);
        assert!((output[0] - 0.6).abs() < 1e-6);
        assert!((output[6] - 0.2).abs() < 1e-6);
        let consumed = realtime.consumed.load(Ordering::Relaxed);
        realtime.running.store(false, Ordering::Release);
        callback.render(&mut output);
        assert_eq!(output, [0.0; 8]);
        assert_eq!(realtime.consumed.load(Ordering::Relaxed), consumed);
        realtime.running.store(true, Ordering::Release);
        callback.render(&mut output);
        assert_eq!(realtime.consumed.load(Ordering::Relaxed), consumed + 4);
        callback.render(&mut output);
        callback.render(&mut output);
        assert!(realtime.underruns.load(Ordering::Relaxed) > 0);
    }
}
