// crates/audiolib-audio/src/slice.rs
//! Bounded, source-frame WAV export from real local originals, independent of audition.
use crate::{
    decoder::Decoder,
    pcm_layout::{ByteOrder, SampleKind},
    wav::{WavInfo, read_info_reader},
};
use audiolib_core::{AudioFrameRange, Error, Result};
use std::{
    fs::{File, Metadata, OpenOptions},
    io::{BufWriter, Read, Seek, SeekFrom, Write},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};
use symphonia::core::{
    common::Limit,
    formats::{SeekMode, SeekTo, TrackType, probe::Hint},
    io::MediaSourceStream,
    meta::MetadataOptions,
    units::Timestamp,
};

const COPY_BYTES: usize = 256 * 1024;
const DECODE_FRAMES: usize = 2048;

fn invalid(message: &str) -> Error {
    Error::Audio(message.into())
}

fn audio_error(error: impl std::fmt::Display) -> Error {
    Error::Audio(error.to_string())
}

fn cancelled(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Acquire) {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

fn open_original(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    // The export owns a stable original while reading it. Permit other readers,
    // but prevent Windows writers/deleters from replacing bytes beneath the handle.
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(1);
    }
    Ok(options.open(path)?)
}

fn same_source(left: &Metadata, right: &Metadata) -> bool {
    let unchanged = left.is_file()
        && right.is_file()
        && left.len() == right.len()
        && left.modified().ok() == right.modified().ok()
        && left.created().ok() == right.created().ok();
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        unchanged
            && left.dev() == right.dev()
            && left.ino() == right.ino()
            && left.ctime() == right.ctime()
            && left.ctime_nsec() == right.ctime_nsec()
    }
    #[cfg(not(unix))]
    {
        unchanged
    }
}

/// A new file exclusively created by this export. Failed/cancelled work removes only
/// this file, after closing the writer; callers keep the containing directory owned.
struct PartialOutput<'a> {
    path: &'a Path,
    committed: bool,
}

impl Drop for PartialOutput<'_> {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(self.path);
        }
    }
}

#[derive(Clone, Copy)]
struct OutputFormat {
    channels: u16,
    rate: u32,
    bytes: u16,
    valid_bits: u16,
    mask: u32,
    float: bool,
}

impl OutputFormat {
    fn for_source(info: &WavInfo) -> Result<Self> {
        let metadata = &info.metadata;
        let (bytes, float, valid_bits) = if let Some(layout) = info.pcm_layout {
            layout.validate()?;
            (
                u16::from(layout.bytes_per_sample),
                matches!(layout.kind, SampleKind::Float),
                metadata.attributes["valid_bits_per_sample"]
                    .as_u64()
                    .and_then(|bits| u16::try_from(bits).ok())
                    .unwrap_or(metadata.bit_depth),
            )
        } else if metadata.encoding == "FLAC" {
            if !(1..=32).contains(&metadata.bit_depth) {
                return Err(invalid("Unsupported FLAC integer sample width"));
            }
            (metadata.bit_depth.div_ceil(8), false, metadata.bit_depth)
        } else {
            (4, true, 32)
        };
        let bits = bytes * 8;
        if metadata.sample_rate == 0
            || !(1..=256).contains(&metadata.channels)
            || valid_bits == 0
            || valid_bits > bits
            || (float && valid_bits != bits)
        {
            return Err(invalid("Invalid source audio layout for slice export"));
        }
        let mask = metadata.attributes["channel_mask"]
            .as_u64()
            .map(u32::try_from)
            .transpose()
            .map_err(audio_error)?
            .unwrap_or(0);
        if mask != 0 && mask.count_ones() != u32::from(metadata.channels) {
            return Err(invalid(
                "Source WAV channel mask does not match its channels",
            ));
        }
        Ok(Self {
            channels: metadata.channels,
            rate: metadata.sample_rate,
            bytes,
            valid_bits,
            mask,
            float,
        })
    }

    fn align(self) -> u16 {
        self.bytes * self.channels
    }

    fn header(self, frames: u64) -> Result<Vec<u8>> {
        let bits = self.bytes * 8;
        let extensible = self.channels > 2 || self.mask != 0 || self.valid_bits != bits;
        let fmt_bytes = if extensible { 40_u64 } else { 16 };
        let data_bytes = frames
            .checked_mul(u64::from(self.align()))
            .ok_or_else(|| invalid("Export WAV data size overflow"))?;
        let riff_bytes = 4_u64
            .checked_add(8 + fmt_bytes + if self.float { 12 } else { 0 } + 8)
            .and_then(|bytes| bytes.checked_add(data_bytes))
            .and_then(|bytes| bytes.checked_add(data_bytes & 1))
            .ok_or_else(|| invalid("Export WAV container size overflow"))?;
        let rf64 = riff_bytes > u64::from(u32::MAX);
        let riff_bytes = riff_bytes
            .checked_add(if rf64 { 36 } else { 0 })
            .ok_or_else(|| invalid("Export RF64 container size overflow"))?;
        let rate_bytes = self
            .rate
            .checked_mul(u32::from(self.align()))
            .ok_or_else(|| invalid("Export WAV byte rate overflow"))?;
        let tag = if self.float { 3_u16 } else { 1 };
        let mut header = Vec::with_capacity(104);
        header.extend_from_slice(if rf64 { b"RF64" } else { b"RIFF" });
        header.extend_from_slice(&(if rf64 { u32::MAX } else { riff_bytes as u32 }).to_le_bytes());
        header.extend_from_slice(b"WAVE");
        if rf64 {
            header.extend_from_slice(b"ds64\x1c\0\0\0");
            header.extend_from_slice(&riff_bytes.to_le_bytes());
            header.extend_from_slice(&data_bytes.to_le_bytes());
            header.extend_from_slice(&frames.to_le_bytes());
            header.extend_from_slice(&0_u32.to_le_bytes());
        }
        header.extend_from_slice(b"fmt ");
        header.extend_from_slice(&(fmt_bytes as u32).to_le_bytes());
        header.extend_from_slice(&(if extensible { 0xfffe } else { tag }).to_le_bytes());
        header.extend_from_slice(&self.channels.to_le_bytes());
        header.extend_from_slice(&self.rate.to_le_bytes());
        header.extend_from_slice(&rate_bytes.to_le_bytes());
        header.extend_from_slice(&self.align().to_le_bytes());
        header.extend_from_slice(&bits.to_le_bytes());
        if extensible {
            header.extend_from_slice(&22_u16.to_le_bytes());
            header.extend_from_slice(&self.valid_bits.to_le_bytes());
            header.extend_from_slice(&self.mask.to_le_bytes());
            header.extend_from_slice(&u32::from(tag).to_le_bytes());
            header.extend_from_slice(&[0, 0, 0x10, 0, 0x80, 0, 0, 0xaa, 0, 0x38, 0x9b, 0x71]);
        }
        if self.float {
            header.extend_from_slice(b"fact\x04\0\0\0");
            header.extend_from_slice(&(frames.min(u64::from(u32::MAX)) as u32).to_le_bytes());
        }
        header.extend_from_slice(b"data");
        header.extend_from_slice(&(if rf64 { u32::MAX } else { data_bytes as u32 }).to_le_bytes());
        Ok(header)
    }
}

/// Export an end-exclusive source-frame selection as an untransformed WAV file.
///
/// Reads only the real file at `source`, never a registered audition/preview source.
/// Retains the source rate and every channel; PCM/float frames retain their stored
/// precision, AIFF changes byte order/sign representation losslessly, FLAC retains
/// integer precision, and other supported codecs decode to float32. Timing-dependent
/// source Metadata is omitted instead of copied with invalid offsets or durations.
/// `destination` must be a new file in a caller-owned directory; it is never replaced.
/// Cancellation/errors remove only the newly created output. Progress counts exported
/// frames, and all decoding/file I/O stays on the caller's worker thread.
pub fn export_audio_slice(
    source: &Path,
    destination: &Path,
    range: AudioFrameRange,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<()> {
    cancelled(cancel)?;
    let source = std::fs::canonicalize(source)?;
    let mut file = open_original(&source)?;
    let before = file.metadata()?;
    if !before.is_file() || !same_source(&before, &std::fs::metadata(&source)?) {
        return Err(invalid("Slice source is not a stable regular file"));
    }
    let info = read_info_reader(&mut file, before.len(), false, false)?;
    if range.sample_rate != info.metadata.sample_rate
        || range.start_frame >= range.end_frame
        || range.end_frame > info.metadata.frames
    {
        return Err(Error::Validation(
            "Slice range is outside the original source frames".into(),
        ));
    }
    let format = OutputFormat::for_source(&info)?;
    let frames = range.end_frame - range.start_frame;
    let header = format.header(frames)?;
    cancelled(cancel)?;
    let output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    let mut partial = PartialOutput {
        path: destination,
        committed: false,
    };
    // Declare after the cleanup guard so the writer closes first on every exit.
    let mut output = BufWriter::with_capacity(COPY_BYTES, output);
    output.write_all(&header)?;
    progress(0, frames);
    if info.pcm_layout.is_some() {
        copy_pcm(
            &mut file,
            &mut output,
            &info,
            format,
            range,
            cancel,
            progress,
        )?;
    } else if info.metadata.encoding == "FLAC" {
        decode_flac(
            file.try_clone()?,
            &mut output,
            &info,
            format,
            range,
            cancel,
            progress,
        )?;
    } else {
        decode_float(
            file.try_clone()?,
            &mut output,
            info,
            range,
            cancel,
            progress,
        )?;
    }
    cancelled(cancel)?;
    if !same_source(&before, &file.metadata()?)
        || !same_source(&before, &std::fs::metadata(&source)?)
    {
        return Err(invalid("Original audio changed during slice export"));
    }
    if (frames * u64::from(format.align())) & 1 != 0 {
        output.write_all(&[0])?;
    }
    output.flush()?;
    output.get_ref().sync_all()?;
    cancelled(cancel)?;
    partial.committed = true;
    Ok(())
}

fn copy_pcm(
    file: &mut File,
    output: &mut impl Write,
    info: &WavInfo,
    format: OutputFormat,
    range: AudioFrameRange,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<()> {
    let layout = info.pcm_layout.expect("PCM export was selected");
    let align = usize::from(info.block_align);
    let width = usize::from(layout.bytes_per_sample);
    let output_align = usize::from(format.align());
    if align < output_align {
        return Err(invalid("Invalid original PCM frame alignment"));
    }
    let start = range
        .start_frame
        .checked_mul(align as u64)
        .and_then(|offset| info.data_offset.checked_add(offset))
        .ok_or_else(|| invalid("Slice PCM byte offset overflow"))?;
    file.seek(SeekFrom::Start(start))?;
    let frames = range.end_frame - range.start_frame;
    let batch = (COPY_BYTES / align).max(1);
    let mut bytes = vec![0; batch * align];
    let mut done = 0;
    while done < frames {
        cancelled(cancel)?;
        let count = (frames - done).min(batch as u64) as usize;
        file.read_exact(&mut bytes[..count * align])?;
        for frame in bytes[..count * align].chunks_exact_mut(align) {
            let samples = &mut frame[..output_align];
            if matches!(layout.endian, ByteOrder::Big) {
                for sample in samples.chunks_exact_mut(width) {
                    sample.reverse();
                }
            }
            if width == 1 && matches!(layout.kind, SampleKind::Signed) {
                for sample in &mut *samples {
                    *sample ^= 0x80;
                }
            }
            output.write_all(samples)?;
        }
        done += count as u64;
        progress(done, frames);
    }
    Ok(())
}

fn decode_float(
    file: File,
    output: &mut impl Write,
    info: WavInfo,
    range: AudioFrameRange,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<()> {
    let channels = usize::from(info.metadata.channels);
    let mut decoder = Decoder::open_local_file(file, info)?;
    decoder.seek(range.start_frame)?;
    let frames = range.end_frame - range.start_frame;
    let mut done = 0;
    let mut bytes = Vec::with_capacity(DECODE_FRAMES * channels * 4);
    while done < frames {
        cancelled(cancel)?;
        let count = (frames - done).min(DECODE_FRAMES as u64) as usize;
        let samples = decoder.read(count)?;
        if samples.len() != count * channels {
            return Err(invalid("Original audio ended before the selected range"));
        }
        bytes.clear();
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        output.write_all(&bytes)?;
        done += count as u64;
        progress(done, frames);
    }
    Ok(())
}

fn decode_flac(
    mut file: File,
    output: &mut impl Write,
    info: &WavInfo,
    format: OutputFormat,
    range: AudioFrameRange,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<()> {
    file.seek(SeekFrom::Start(0))?;
    let stream = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    hint.with_extension(if info.metadata.attributes["container"] == "Ogg" {
        "ogg"
    } else {
        "flac"
    });
    let mut reader = symphonia::default::get_probe()
        .probe(
            &hint,
            stream,
            Default::default(),
            MetadataOptions::default()
                .limit_tag_bytes(Limit::Maximum(1024 * 1024))
                .limit_visual_bytes(Limit::Maximum(0)),
        )
        .map_err(audio_error)?;
    let track = reader
        .default_track(TrackType::Audio)
        .ok_or_else(|| invalid("FLAC has no supported audio track"))?;
    let track_id = track.id;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|params| params.audio())
        .ok_or_else(|| invalid("FLAC has no audio codec parameters"))?;
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(params, &Default::default())
        .map_err(audio_error)?;
    if range.start_frame != 0 {
        reader
            .seek(
                SeekMode::Accurate,
                SeekTo::Timestamp {
                    ts: Timestamp::try_from(range.start_frame).map_err(audio_error)?,
                    track_id,
                },
            )
            .map_err(audio_error)?;
        decoder.reset();
    }
    let channels = usize::from(format.channels);
    let frames = range.end_frame - range.start_frame;
    let mut done = 0_u64;
    let mut samples = Vec::<i32>::new();
    let mut bytes = Vec::new();
    while done < frames {
        cancelled(cancel)?;
        let packet = reader
            .next_packet()
            .map_err(audio_error)?
            .ok_or_else(|| invalid("FLAC ended before the selected range"))?;
        if packet.track_id != track_id {
            continue;
        }
        let audio = decoder.decode(&packet).map_err(audio_error)?;
        if audio.spec().rate() != format.rate || audio.spec().channels().count() != channels {
            return Err(invalid("FLAC sample layout changed during export"));
        }
        let packet_start = packet
            .pts
            .checked_add(packet.trim_start)
            .ok_or_else(|| invalid("FLAC presentation timestamp overflow"))?
            .get();
        if audio.frames() == 0 {
            continue;
        }
        let next = range.start_frame + done;
        let skip = if packet_start < 0 {
            next.saturating_add(packet_start.unsigned_abs())
        } else {
            let start = packet_start as u64;
            if start > next {
                return Err(invalid("FLAC seek left a gap in the selection"));
            }
            next - start
        };
        if skip >= audio.frames() as u64 {
            continue;
        }
        let count = (audio.frames() as u64 - skip).min(frames - done) as usize;
        samples.resize(audio.samples_interleaved(), 0);
        audio.copy_to_slice_interleaved(&mut samples);
        bytes.clear();
        for &sample in &samples[skip as usize * channels..(skip as usize + count) * channels] {
            // Symphonia normalizes integer samples to the full i32 range. Keep
            // the high storage bytes, including low valid bits (never through f32).
            let normalized = sample >> (32 - u32::from(format.bytes) * 8);
            if format.bytes == 1 {
                bytes.push((normalized + 128) as u8);
            } else {
                bytes.extend_from_slice(&normalized.to_le_bytes()[..usize::from(format.bytes)]);
            }
        }
        output.write_all(&bytes)?;
        done += count as u64;
        progress(done, frames);
    }
    Ok(())
}

#[cfg(test)]
#[path = "slice_tests.rs"]
mod tests;
