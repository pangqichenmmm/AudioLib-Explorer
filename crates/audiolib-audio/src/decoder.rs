// crates/audiolib-audio/src/decoder.rs
use crate::{
    source::{
        AudioRegion, ByteSource, MediaAdapter, PrefixedSource, SourceBuffer, Tracked,
        is_stream_source, open_source, source_buffer,
    },
    wav::{WavInfo, read_info},
};
use audioadapter_buffers::direct::InterleavedSlice;
use audiolib_core::{Error, Result};
use rubato::{Fft, FixedSync, Resampler};
use std::{
    io::{BufReader, Cursor, Read, Seek, SeekFrom},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use symphonia::core::{
    codecs::audio::AudioDecoder,
    common::Limit,
    formats::{FormatReader, SeekMode, SeekTo, TrackType, probe::Hint},
    io::MediaSourceStream,
    meta::MetadataOptions,
    packet::Packet,
    units::{Duration, Timestamp},
};

pub(crate) struct Decoder {
    pub info: WavInfo,
    source: Source,
    frame: u64,
    /// For a network source: what is in memory after the decoder's read position.
    buffer: Option<BufferProbe>,
}

/// Bytes past the estimate a buffered check also requires when Symphonia's format reader
/// (which reads ahead of the frames it returns) owns the source.
const READ_SLACK_BYTES: u64 = 64 * 1024;

struct BufferProbe {
    buffer: Arc<dyn SourceBuffer>,
    position: Arc<AtomicU64>,
    bytes_per_frame: f64,
    slack: u64,
}

/// Wrap `file` so its position is visible to the source's [`SourceBuffer`], if any.
fn tracked(
    file: Box<dyn ByteSource>,
    buffer: &Option<Arc<dyn SourceBuffer>>,
    (bytes_per_frame, slack): (f64, u64),
    probe: &mut Option<BufferProbe>,
) -> Box<dyn ByteSource> {
    let Some(buffer) = buffer else {
        return file;
    };
    let position = Arc::new(AtomicU64::new(0));
    *probe = Some(BufferProbe {
        buffer: buffer.clone(),
        position: position.clone(),
        bytes_per_frame,
        slack,
    });
    Box::new(Tracked::new(file, position))
}
enum Source {
    Pcm(Box<dyn ByteSource>),
    Symphonia {
        reader: Box<dyn FormatReader>,
        decoder: Box<dyn AudioDecoder>,
        track: u32,
        /// Bounded encoder delay gives a reset decoder enough pre-roll before
        /// the requested presentation frame, including seeks near frame zero.
        seek_preroll: u64,
        pending: Vec<f32>,
        offset: usize,
    },
    Blocks {
        file: Box<dyn ByteSource>,
        decoder: Box<dyn AudioDecoder>,
        track: u32,
        frames_per_block: u64,
        max_blocks_per_packet: u64,
        next_frame: u64,
        remaining_bytes: u64,
        pending: Vec<f32>,
        offset: usize,
    },
}
fn audio_error(error: impl std::fmt::Display) -> Error {
    Error::Audio(error.to_string())
}

/// Open `path`; with `read_buffer > 0` a plain file is read through a buffer of that many
/// bytes, so small decoder reads become large sequential reads (registered stream sources
/// keep their own buffering and are never wrapped).
fn open_bytes(path: &Path, read_buffer: usize) -> Result<(Box<dyn ByteSource>, u64)> {
    let (file, len) = open_source(path)?;
    if read_buffer == 0 || is_stream_source(path) {
        return Ok((file, len));
    }
    Ok((Box::new(BufReader::with_capacity(read_buffer, file)), len))
}

// Symphonia's WAV demuxer recognizes RIFF, not RF64/BW64. A small owned RIFF
// header supplies codec parameters while the block reader keeps the original
// validated 64-bit data range. No source length is narrowed to fit this header.
fn compressed_wave_header(info: &WavInfo) -> Vec<u8> {
    let mut bytes = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
    bytes.extend_from_slice(&(info.format_data.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&info.format_data);
    if !info.format_data.len().is_multiple_of(2) {
        bytes.push(0);
    }
    bytes.extend_from_slice(b"fact\x04\0\0\0");
    bytes.extend_from_slice(&(info.metadata.frames.min(u64::from(u32::MAX)) as u32).to_le_bytes());
    bytes.extend_from_slice(b"data\0\0\0\0");
    let size = (bytes.len() - 8) as u32;
    bytes[4..8].copy_from_slice(&size.to_le_bytes());
    bytes
}

impl Decoder {
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_read_buffer(path, 0)
    }

    /// [`Decoder::open`] reading a plain file through a `read_buffer`-byte buffer
    /// (0 = unbuffered). Decoded samples are identical either way.
    pub fn open_with_read_buffer(path: &Path, read_buffer: usize) -> Result<Self> {
        let info = read_info(path)?;
        Self::open_prepared(info, source_buffer(path), || open_bytes(path, read_buffer))
    }

    /// Decode only the consumed, already authorized local file; never consult the
    /// playback registry or reopen an untrusted path. `info` must describe this handle.
    pub(crate) fn open_local_file(file: std::fs::File, info: WavInfo) -> Result<Self> {
        let len = file.metadata()?.len();
        Self::open_prepared(info, None, || {
            let mut reader = file.try_clone()?;
            reader.seek(SeekFrom::Start(0))?;
            Ok((Box::new(reader), len))
        })
    }

    fn open_prepared(
        info: WavInfo,
        buffer: Option<Arc<dyn SourceBuffer>>,
        mut open: impl FnMut() -> Result<(Box<dyn ByteSource>, u64)>,
    ) -> Result<Self> {
        if info.metadata.encoding == "OPUS" {
            return Err(Error::Audio(
                "Opus Metadata is supported, but this build has no Opus playback decoder".into(),
            ));
        }
        if matches!(
            info.metadata.attributes["container"].as_str(),
            Some("RIFF" | "RF64" | "BW64")
        ) && matches!(info.format_tag, 1 | 3)
            && !matches!(
                (info.format_tag, info.metadata.bit_depth),
                (1, 8 | 16 | 24 | 32) | (3, 32 | 64)
            )
        {
            return Err(Error::Audio(
                "Unsupported PCM/float WAV sample width".into(),
            ));
        }
        let mut probe = None;
        let source = if let Some(layout) = info.pcm_layout {
            layout.validate()?;
            if u32::from(info.block_align)
                < u32::from(info.metadata.channels) * u32::from(layout.bytes_per_sample)
            {
                return Err(Error::Audio("Invalid PCM frame layout".into()));
            }
            let (file, _) = open()?;
            let mut file = tracked(file, &buffer, (f64::from(info.block_align), 0), &mut probe);
            file.seek(SeekFrom::Start(info.data_offset))?;
            Source::Pcm(file)
        } else {
            let large_wave = matches!(
                info.metadata.attributes["container"].as_str(),
                Some("RF64" | "BW64")
            );
            let (inner, len) = open()?;
            let bytes_per_frame = if info.metadata.frames > 0 {
                info.data_bytes as f64 / info.metadata.frames as f64
            } else {
                f64::from(info.block_align)
            };
            let mut inner = tracked(
                inner,
                &buffer,
                (bytes_per_frame, READ_SLACK_BYTES),
                &mut probe,
            );
            let mp3 = info.metadata.attributes["container"] == "MP3";
            let adapter = if large_wave {
                let bytes = compressed_wave_header(&info);
                let len = bytes.len() as u64;
                MediaAdapter::new(Box::new(Cursor::new(bytes)), len)
            } else if mp3 {
                let (prefix, skip) = crate::mp3_metadata::decoder_prefix(&mut inner, &info)?;
                let payload_len = info
                    .data_bytes
                    .checked_sub(skip)
                    .ok_or_else(|| Error::Audio("Invalid MP3 decoder prefix range".into()))?;
                let start = info
                    .data_offset
                    .checked_add(skip)
                    .ok_or_else(|| Error::Audio("MP3 decoder range overflow".into()))?;
                let region = AudioRegion::new(inner, start, payload_len)?;
                MediaAdapter::without_length(Box::new(PrefixedSource::new(
                    prefix,
                    Box::new(region),
                    payload_len,
                )?))
            } else {
                MediaAdapter::new(inner, len)
            };
            let stream = MediaSourceStream::new(Box::new(adapter), Default::default());
            let mut hint = Hint::new();
            hint.with_extension(if info.format_tag != 0 {
                "wav"
            } else if info.metadata.attributes["container"] == "Ogg" {
                "ogg"
            } else {
                "flac"
            });
            let reader: Box<dyn FormatReader> = if mp3 {
                Box::new(
                    symphonia::default::formats::MpaReader::try_new(stream, Default::default())
                        .map_err(audio_error)?,
                )
            } else {
                symphonia::default::get_probe()
                    .probe(
                        &hint,
                        stream,
                        Default::default(),
                        MetadataOptions::default()
                            .limit_tag_bytes(Limit::Maximum(1024 * 1024))
                            .limit_visual_bytes(Limit::Maximum(0)),
                    )
                    .map_err(audio_error)?
            };
            let track = reader
                .default_track(TrackType::Audio)
                .ok_or_else(|| Error::Audio("No supported audio track".into()))?;
            let params = track
                .codec_params
                .as_ref()
                .and_then(|p| p.audio())
                .ok_or_else(|| Error::Audio("Missing audio codec parameters".into()))?;
            let decoder = symphonia::default::get_codecs()
                .make_audio_decoder(params, &Default::default())
                .map_err(audio_error)?;
            let frames_per_block = params.frames_per_block.unwrap_or(1);
            if info.format_tag != 0 && (frames_per_block > 1 || large_wave) {
                // Symphonia 0.6.1's RIFF seek uses frame_ts * block_size for
                // ADPCM; byte positions actually require block_index * block_size.
                // Packetize complete WAV blocks here, preserving the codec decoder
                // and allowing bounded, sample-accurate seeks without a linear scan.
                // The format reader's own source is no longer read: track the block reader.
                probe = None;
                let (file, _) = open()?;
                // Whole blocks are read at a time: one block of slack.
                let mut file = tracked(
                    file,
                    &buffer,
                    (
                        f64::from(info.block_align) / frames_per_block as f64,
                        u64::from(info.block_align),
                    ),
                    &mut probe,
                );
                file.seek(SeekFrom::Start(info.data_offset))?;
                Source::Blocks {
                    file,
                    track: track.id,
                    decoder,
                    frames_per_block,
                    max_blocks_per_packet: params
                        .max_frames_per_packet
                        .unwrap_or(frames_per_block)
                        .div_euclid(frames_per_block)
                        .max(1),
                    next_frame: 0,
                    remaining_bytes: info.data_bytes,
                    pending: Vec::new(),
                    offset: 0,
                }
            } else {
                Source::Symphonia {
                    track: track.id,
                    seek_preroll: if mp3 {
                        // Layer III reservoirs and synthesis overlap need a bounded
                        // reset pre-roll even when the source has no delay tag.
                        u64::from(track.delay.unwrap_or_default()).max(32 * 1152)
                    } else {
                        u64::from(track.delay.unwrap_or_default())
                    },
                    reader,
                    decoder,
                    pending: Vec::new(),
                    offset: 0,
                }
            }
        };
        Ok(Self {
            info,
            source,
            frame: 0,
            buffer: probe,
        })
    }

    /// Whether the next `frames` source frames can be decoded without waiting for network
    /// data. Always true for files and sources without a [`SourceBuffer`].
    pub fn is_buffered(&self, frames: u64) -> bool {
        let Some(probe) = &self.buffer else {
            return true;
        };
        let frames = frames.min(self.info.metadata.frames.saturating_sub(self.frame));
        if frames == 0 {
            return true;
        }
        let bytes = (frames as f64 * probe.bytes_per_frame).ceil() as u64 + probe.slack;
        probe
            .buffer
            .is_buffered(probe.position.load(Ordering::Acquire), bytes)
    }

    pub fn seek(&mut self, frame: u64) -> Result<()> {
        let frame = frame.min(self.info.metadata.frames);
        // The presentation EOF is valid even when a format's timestamp seek
        // excludes that boundary. A later seek still resets source/pending state.
        if frame == self.info.metadata.frames {
            self.frame = frame;
            return Ok(());
        }
        let channels = usize::from(self.info.metadata.channels);
        let actual_frame = match &mut self.source {
            Source::Pcm(file) => {
                file.seek(SeekFrom::Start(
                    self.info.data_offset + frame * u64::from(self.info.block_align),
                ))?;
                frame
            }
            Source::Symphonia {
                reader,
                decoder,
                track,
                seek_preroll,
                pending,
                offset,
            } => {
                reader
                    .seek(
                        SeekMode::Accurate,
                        SeekTo::Timestamp {
                            ts: Timestamp::try_from(frame.saturating_sub(*seek_preroll))
                                .map_err(audio_error)?,
                            track_id: *track,
                        },
                    )
                    .map_err(audio_error)?;
                decoder.reset();
                pending.clear();
                *offset = 0;
                // The returned seek anchor is a container packet timestamp,
                // not the first PCM frame: Vorbis can return a negative primer,
                // and the first packet after reset decodes to no samples.
                // Gapless decoding has already removed packet.trim_start/end.
                // Derive the retained PCM's position from its own PTS + start
                // trim, physically discard any PCM before presentation zero,
                // then use the ordinary bounded read loop to reach `frame`.
                loop {
                    let packet = reader.next_packet().map_err(audio_error)?.ok_or_else(|| {
                        Error::Audio("Unexpected end of audio during seek".into())
                    })?;
                    if packet.track_id != *track {
                        continue;
                    }
                    let audio = decoder.decode(&packet).map_err(audio_error)?;
                    if audio.samples_interleaved() == 0 {
                        continue;
                    }
                    pending.resize(audio.samples_interleaved(), 0.0);
                    audio.copy_to_slice_interleaved(&mut *pending);
                    let timestamp = packet
                        .pts
                        .checked_add(packet.trim_start)
                        .ok_or_else(|| Error::Audio("Decoded PCM timestamp overflow".into()))?
                        .get();
                    if timestamp < 0 {
                        let decoded_frames = pending.len() / channels;
                        // The min proves this conversion and channel product
                        // fit the actual retained sample buffer on every host.
                        let skip = timestamp.unsigned_abs().min(decoded_frames as u64) as usize;
                        *offset = skip * channels;
                        if *offset == pending.len() {
                            pending.clear();
                            *offset = 0;
                            continue;
                        }
                        break 0;
                    }
                    break u64::try_from(timestamp).map_err(audio_error)?;
                }
            }
            Source::Blocks {
                file,
                decoder,
                frames_per_block,
                next_frame,
                remaining_bytes,
                pending,
                offset,
                ..
            } => {
                let block = frame / *frames_per_block;
                let byte_offset = block * u64::from(self.info.block_align);
                file.seek(SeekFrom::Start(self.info.data_offset + byte_offset))?;
                *remaining_bytes = self.info.data_bytes.saturating_sub(byte_offset);
                *next_frame = block * *frames_per_block;
                decoder.reset();
                pending.clear();
                *offset = 0;
                *next_frame
            }
        };
        if actual_frame > frame {
            return Err(Error::Audio(
                "Codec seek landed after the requested frame".into(),
            ));
        }
        self.frame = actual_frame;
        while self.frame < frame {
            let discard = (frame - self.frame).min(2048) as usize;
            if self.read(discard)?.len() < channels {
                return Err(Error::Audio("Unexpected end of WAV during seek".into()));
            }
        }
        Ok(())
    }

    /// Decode at most a bounded number of interleaved frames; never used in an output callback.
    pub fn read(&mut self, frames: usize) -> Result<Vec<f32>> {
        let channels = usize::from(self.info.metadata.channels);
        let frames = frames.min(self.info.metadata.frames.saturating_sub(self.frame) as usize);
        if frames == 0 {
            return Ok(Vec::new());
        }
        let mut output = Vec::with_capacity(frames * channels);
        match &mut self.source {
            Source::Pcm(file) => {
                let layout = self.info.pcm_layout.expect("validated PCM source");
                let width = usize::from(layout.bytes_per_sample);
                let align = usize::from(self.info.block_align);
                let mut bytes = vec![0_u8; frames * align];
                file.read_exact(&mut bytes)?;
                for frame in bytes.chunks_exact(align) {
                    for bytes in frame[..width * channels].chunks_exact(width) {
                        let value = layout.decode(bytes);
                        output.push(if value.is_finite() { value } else { 0.0 });
                    }
                }
            }
            Source::Symphonia {
                reader,
                decoder,
                track,
                pending,
                offset,
                ..
            } => {
                while output.len() < frames * channels {
                    if *offset < pending.len() {
                        let count = (frames * channels - output.len()).min(pending.len() - *offset);
                        output.extend_from_slice(&pending[*offset..*offset + count]);
                        *offset += count;
                    } else {
                        let Some(packet) = reader.next_packet().map_err(audio_error)? else {
                            break;
                        };
                        if packet.track_id != *track {
                            continue;
                        }
                        let audio = decoder.decode(&packet).map_err(audio_error)?;
                        pending.resize(audio.samples_interleaved(), 0.0);
                        audio.copy_to_slice_interleaved(&mut *pending);
                        *offset = 0;
                    }
                }
            }
            Source::Blocks {
                file,
                decoder,
                track,
                frames_per_block,
                max_blocks_per_packet,
                next_frame,
                remaining_bytes,
                pending,
                offset,
            } => {
                while output.len() < frames * channels {
                    if *offset < pending.len() {
                        let count = (frames * channels - output.len()).min(pending.len() - *offset);
                        output.extend_from_slice(&pending[*offset..*offset + count]);
                        *offset += count;
                    } else {
                        let block_size = u64::from(self.info.block_align);
                        let blocks = (*max_blocks_per_packet).min(*remaining_bytes / block_size);
                        if blocks == 0 {
                            break;
                        }
                        let packet_frames = blocks * *frames_per_block;
                        let mut bytes = vec![0; (blocks * block_size) as usize];
                        file.read_exact(&mut bytes)?;
                        *remaining_bytes -= bytes.len() as u64;
                        let packet = Packet::new(
                            *track,
                            Timestamp::try_from(*next_frame).map_err(audio_error)?,
                            Duration::from(packet_frames),
                            bytes,
                        );
                        *next_frame += packet_frames;
                        let audio = decoder.decode(&packet).map_err(audio_error)?;
                        pending.resize(audio.samples_interleaved(), 0.0);
                        audio.copy_to_slice_interleaved(&mut *pending);
                        *offset = 0;
                    }
                }
            }
        }
        self.frame += (output.len() / channels) as u64;
        Ok(output)
    }
}

/// Bounded FFT sample rate conversion, trimming its startup delay and padded tail.
pub(crate) struct ResampledDecoder {
    decoder: Decoder,
    resampler: Option<Fft<f32>>,
    output_remaining: u64,
    delay_remaining: usize,
    channels: usize,
    /// Source frames consumed per output frame.
    input_per_output: f64,
    input_remaining: u64,
}
impl ResampledDecoder {
    pub fn is_finished(&self) -> bool {
        self.output_remaining == 0
    }
    /// [`Decoder::is_buffered`] for `frames` output frames (plus one resampler block).
    pub fn is_buffered(&self, frames: u64) -> bool {
        let block = self
            .resampler
            .as_ref()
            .map_or(0, |resampler| resampler.input_frames_max() as u64);
        self.decoder.is_buffered(
            ((frames as f64 * self.input_per_output).ceil() as u64 + block)
                .min(self.input_remaining),
        )
    }
    pub fn new(decoder: Decoder, output_rate: u32, start_frame: u64) -> Result<Self> {
        let end_frame = decoder.info.metadata.frames;
        Self::new_bounded(decoder, output_rate, start_frame, end_frame)
    }
    /// Treat the exclusive source end as EOF before resampling, including its lookahead.
    pub fn new_bounded(
        mut decoder: Decoder,
        output_rate: u32,
        start_frame: u64,
        end_frame: u64,
    ) -> Result<Self> {
        if output_rate == 0 || start_frame > end_frame || end_frame > decoder.info.metadata.frames {
            return Err(Error::Validation("Invalid bounded decoder range".into()));
        }
        decoder.seek(start_frame)?;
        let metadata = &decoder.info.metadata;
        let channels = usize::from(metadata.channels);
        let input_remaining = end_frame - start_frame;
        let output_remaining = (input_remaining as f64 * f64::from(output_rate)
            / f64::from(metadata.sample_rate))
        .round() as u64;
        let resampler = if output_rate == metadata.sample_rate {
            None
        } else {
            Some(
                Fft::<f32>::new(
                    metadata.sample_rate as usize,
                    output_rate as usize,
                    1024,
                    channels,
                    FixedSync::Input,
                )
                .map_err(audio_error)?,
            )
        };
        let delay_remaining = resampler
            .as_ref()
            .map_or(0, |resampler| resampler.output_delay());
        let input_per_output = f64::from(metadata.sample_rate) / f64::from(output_rate.max(1));
        Ok(Self {
            decoder,
            resampler,
            output_remaining,
            delay_remaining,
            channels,
            input_per_output,
            input_remaining,
        })
    }
    pub fn next(&mut self) -> Result<Vec<f32>> {
        if self.output_remaining == 0 {
            return Ok(Vec::new());
        }
        let Some(resampler) = &mut self.resampler else {
            let samples = self.decoder.read(self.input_remaining.min(2048) as usize)?;
            self.input_remaining -= (samples.len() / self.channels) as u64;
            self.output_remaining = self
                .output_remaining
                .saturating_sub((samples.len() / self.channels) as u64);
            return Ok(samples);
        };
        let frames = resampler.input_frames_next();
        let mut input = self
            .decoder
            .read(self.input_remaining.min(frames as u64) as usize)?;
        self.input_remaining -= (input.len() / self.channels) as u64;
        input.resize(frames * self.channels, 0.0);
        let input = InterleavedSlice::new(&input, self.channels, frames).map_err(audio_error)?;
        let mut output = resampler
            .process(&input, None)
            .map_err(audio_error)?
            .take_data();
        let skip = self.delay_remaining.min(output.len() / self.channels);
        self.delay_remaining -= skip;
        output.drain(..skip * self.channels);
        output.truncate(
            (self
                .output_remaining
                .min((output.len() / self.channels) as u64) as usize)
                * self.channels,
        );
        self.output_remaining -= (output.len() / self.channels) as u64;
        if output.is_empty() && self.output_remaining > 0 {
            return self.next();
        }
        Ok(output)
    }
}
