// crates/audiolib-audio/src/audition.rs
//! Streaming audition transforms; all SoundTouch work stays on the control worker.
use crate::decoder::{Decoder, ResampledDecoder};
use audiolib_core::{Error, Result};
use serde::{Deserialize, Serialize};
#[path = "soundtouch_processor.rs"]
mod soundtouch_processor;
use soundtouch_processor::SoundTouchProcessor;

/// Independent semitone pitch and pitch-preserving speed, applied to the monitor pair.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AuditionSettings {
    pub pitch_semitones: f32,
    pub rate: f32,
    pub mono: bool,
    pub loop_enabled: bool,
}
impl Default for AuditionSettings {
    fn default() -> Self {
        Self {
            pitch_semitones: 0.0,
            rate: 1.0,
            mono: false,
            loop_enabled: false,
        }
    }
}
impl AuditionSettings {
    /// Reject non-finite or unsupported transform values before altering playback.
    pub fn validate(self) -> Result<()> {
        if !self.pitch_semitones.is_finite()
            || !(-24.0..=24.0).contains(&self.pitch_semitones)
            || !self.rate.is_finite()
            || !(0.25..=4.0).contains(&self.rate)
        {
            return Err(Error::Validation(
                "Pitch must be -24..24 semitones; Rate must be 0.25..4.0".into(),
            ));
        }
        Ok(())
    }
}

pub(crate) struct AuditionDecoder {
    source: ResampledDecoder,
    channels: usize,
    pair: usize,
    pub settings: AuditionSettings,
    processor: Option<SoundTouchProcessor>,
    flushed: bool,
    remaining: u64,
}
impl AuditionDecoder {
    pub fn new(
        decoder: Decoder,
        output_rate: u32,
        start_frame: u64,
        pair: u16,
        settings: AuditionSettings,
    ) -> Result<Self> {
        let end_frame = decoder.info.metadata.frames;
        Self::new_bounded(decoder, output_rate, start_frame, end_frame, pair, settings)
    }
    /// Only selected source frames reach the resampler and SoundTouch input.
    pub fn new_bounded(
        decoder: Decoder,
        output_rate: u32,
        start_frame: u64,
        end_frame: u64,
        pair: u16,
        settings: AuditionSettings,
    ) -> Result<Self> {
        settings.validate()?;
        let metadata = &decoder.info.metadata;
        let channels = usize::from(metadata.channels);
        if usize::from(pair) * 2 >= channels {
            return Err(Error::Validation(
                "Source channel pair is outside the loaded WAV channels".into(),
            ));
        }
        if start_frame > end_frame || end_frame > metadata.frames {
            return Err(Error::Validation("Invalid audition frame range".into()));
        }
        let remaining = ((end_frame - start_frame) as f64 * f64::from(output_rate)
            / f64::from(metadata.sample_rate)
            / f64::from(settings.rate))
        .round() as u64;
        let processor = if settings.pitch_semitones != 0.0 || settings.rate != 1.0 {
            Some(SoundTouchProcessor::new(
                output_rate,
                settings.rate,
                settings.pitch_semitones,
            )?)
        } else {
            None
        };
        Ok(Self {
            source: if end_frame == metadata.frames {
                ResampledDecoder::new(decoder, output_rate, start_frame)?
            } else {
                ResampledDecoder::new_bounded(decoder, output_rate, start_frame, end_frame)?
            },
            channels,
            pair: usize::from(pair),
            settings,
            processor,
            flushed: false,
            remaining,
        })
    }
    pub fn is_finished(&self) -> bool {
        self.remaining == 0
    }
    /// Whether producing `frames` more output frames will not wait for network data
    /// (Rate scales the source consumed; SoundTouch keeps a few thousand frames queued).
    pub fn is_buffered(&self, frames: u64) -> bool {
        let source = if self.processor.is_some() {
            (frames as f64 * f64::from(self.settings.rate)).ceil() as u64 + 8192
        } else {
            frames
        };
        self.source.is_buffered(source)
    }
    pub fn next(&mut self) -> Result<Vec<f32>> {
        if self.remaining == 0 {
            return Ok(Vec::new());
        }
        loop {
            if let Some(processor) = &mut self.processor {
                let frames = self.remaining.min(2048) as usize;
                let mut output = vec![0.0; frames * 2];
                let received = processor.receive_samples(&mut output);
                if received > 0 {
                    output.truncate(received * 2);
                    self.remaining -= received as u64;
                    return Ok(output);
                }
                if self.flushed {
                    // SoundTouch may round its duration by a frame. Pad only the
                    // bounded final block, retaining the exact source-time contract.
                    self.remaining -= frames as u64;
                    return Ok(output);
                }
            }
            let source = if self.source.is_finished() {
                Vec::new()
            } else {
                self.source.next()?
            };
            if source.is_empty() {
                if let Some(processor) = &mut self.processor {
                    processor.flush();
                    self.flushed = true;
                    continue;
                }
                self.remaining = 0;
                return Ok(Vec::new());
            }
            let mut stereo = Vec::with_capacity(source.len() / self.channels * 2);
            for frame in source.chunks_exact(self.channels) {
                let (left, right) = crate::engine::monitor_pair(frame, self.pair);
                let pair = if self.settings.mono {
                    let mono = (left + right) * 0.5;
                    [mono, mono]
                } else {
                    [left, right]
                };
                stereo.extend_from_slice(&pair);
            }
            if let Some(processor) = &mut self.processor {
                processor.put_samples(&stereo)?;
            } else {
                stereo.truncate(self.remaining.min((stereo.len() / 2) as u64) as usize * 2);
                self.remaining -= (stereo.len() / 2) as u64;
                return Ok(stereo);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn tone(path: &std::path::Path) {
        let mut pcm = Vec::new();
        for frame in 0..96000 {
            let value = (std::f32::consts::TAU * 440.0 * frame as f32 / 48000.0).sin() * 0.4;
            pcm.extend_from_slice(&value.to_le_bytes());
            pcm.extend_from_slice(&(-value).to_le_bytes());
        }
        crate::tests::write_wav(path, 48000, 2, 32, 3, &pcm, false, false);
    }
    #[test]
    fn pitch_and_speed_are_independent_with_exact_tail_and_bounded_blocks() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("tone.wav");
        tone(&path);
        for (pitch, rate, frequency) in [
            (0.0, 0.5, 440.0),
            (12.0, 1.0, 880.0),
            (-12.0, 2.0, 220.0),
            (7.0, 1.5, 659.26),
            (24.0, 4.0, 1760.0),
            (-24.0, 0.25, 110.0),
        ] {
            let settings = AuditionSettings {
                pitch_semitones: pitch,
                rate,
                ..Default::default()
            };
            let mut decoder =
                AuditionDecoder::new(Decoder::open(&path).unwrap(), 48000, 12000, 0, settings)
                    .unwrap();
            let mut result = Vec::new();
            while !decoder.is_finished() {
                let block = decoder.next().unwrap();
                assert!(block.len() <= 4096);
                result.extend(block);
            }
            assert_eq!(result.len() / 2, (84000.0 / rate).round() as usize);
            let mono: Vec<_> = result
                .chunks_exact(2)
                .skip(8000)
                .take(16000)
                .map(|p| p[0])
                .collect();
            let crossings = mono
                .windows(2)
                .filter(|p| p[0] <= 0.0 && p[1] > 0.0)
                .count();
            let measured = crossings as f32 * 48000.0 / mono.len() as f32;
            assert!(
                (measured - frequency).abs() < 8.0,
                "pitch {pitch} rate {rate}: {measured}"
            );
        }
        let mut mono = AuditionDecoder::new(
            Decoder::open(&path).unwrap(),
            48000,
            0,
            0,
            AuditionSettings {
                mono: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(mono.next().unwrap().iter().all(|v| v.abs() < 1e-6));
    }
}
