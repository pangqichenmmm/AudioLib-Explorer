// crates/audiolib-core/src/audio_range.rs
//! Sample-accurate, half-open source-audio selections.
use crate::{Error, Result};
use serde::{Deserialize, Serialize};

/// A half-open interval in the original material's sample-frame timeline.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AudioFrameRange {
    /// First included frame in the original material.
    pub start_frame: u64,
    /// Exclusive end frame in the original material.
    pub end_frame: u64,
    /// Original-material sample rate, independent of the audition representation.
    pub sample_rate: u32,
}

impl AudioFrameRange {
    /// Validate a nonempty interval against the source's actual frame count.
    pub fn validate(self, total_frames: u64) -> Result<()> {
        if self.sample_rate == 0
            || self.start_frame >= self.end_frame
            || self.end_frame > total_frames
        {
            return Err(Error::Validation("Invalid source audio frame range".into()));
        }
        Ok(())
    }

    /// Snap ordered source seconds to their nearest frames, clamping to the source.
    pub fn from_seconds(start: f64, end: f64, sample_rate: u32, total_frames: u64) -> Result<Self> {
        if !start.is_finite() || !end.is_finite() || start < 0.0 || end <= start {
            return Err(Error::Validation("Invalid source audio time range".into()));
        }
        let range = Self {
            start_frame: ((start * f64::from(sample_rate)).round() as u64).min(total_frames),
            end_frame: ((end * f64::from(sample_rate)).round() as u64).min(total_frames),
            sample_rate,
        };
        range.validate(total_frames)?;
        Ok(range)
    }

    /// Start position in the original material's seconds.
    pub fn start_seconds(self) -> f64 {
        self.start_frame as f64 / f64::from(self.sample_rate)
    }

    /// Exclusive end position in the original material's seconds.
    pub fn end_seconds(self) -> f64 {
        self.end_frame as f64 / f64::from(self.sample_rate)
    }

    /// Map into a playback representation, keeping only complete frames inside it.
    ///
    /// A selection shorter than a frame of the representation is rejected; callers
    /// can prepare the original stream instead of widening the selected interval.
    /// Missing representation coverage is rejected rather than silently shortened.
    pub fn map_inward(self, sample_rate: u32, total_frames: u64) -> Result<Self> {
        self.validate(self.end_frame)?;
        if sample_rate == 0 {
            return Err(Error::Validation("Invalid playback sample rate".into()));
        }
        let denominator = u128::from(self.sample_rate);
        let start = (u128::from(self.start_frame) * u128::from(sample_rate)).div_ceil(denominator);
        let end = u128::from(self.end_frame) * u128::from(sample_rate) / denominator;
        let start_frame = u64::try_from(start)
            .map_err(|_| Error::Validation("Playback range frame conversion overflow".into()))?;
        let end_frame = u64::try_from(end)
            .map_err(|_| Error::Validation("Playback range frame conversion overflow".into()))?;
        if end_frame > total_frames || start_frame > total_frames {
            return Err(Error::Validation(
                "Playback range exceeds the current representation's source frames".into(),
            ));
        }
        if start_frame >= end_frame {
            return Err(Error::Validation(
                "Playback range is shorter than one frame of the current representation".into(),
            ));
        }
        Ok(Self {
            start_frame,
            end_frame,
            sample_rate,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selections_snap_validate_and_map_inward_without_widening() {
        let range = AudioFrameRange::from_seconds(0.125, 0.75, 48_000, 48_000).unwrap();
        assert_eq!((range.start_frame, range.end_frame), (6000, 36_000));
        assert_eq!((range.start_seconds(), range.end_seconds()), (0.125, 0.75));
        let mapped = AudioFrameRange {
            start_frame: 7,
            end_frame: 19,
            sample_rate: 96_000,
        }
        .map_inward(48_000, 100)
        .unwrap();
        assert_eq!((mapped.start_frame, mapped.end_frame), (4, 9));
        assert!(mapped.start_seconds() >= 7.0 / 96_000.0);
        assert!(mapped.end_seconds() <= 19.0 / 96_000.0);
        assert!(range.map_inward(48_000, 35_999).is_err());
        assert!(
            AudioFrameRange {
                start_frame: 1,
                end_frame: 2,
                sample_rate: 192_000
            }
            .map_inward(48_000, 100)
            .is_err()
        );
        for (start, end) in [
            (0.0, 0.0),
            (1.0, 0.5),
            (f64::NAN, 1.0),
            (0.0, f64::INFINITY),
        ] {
            assert!(AudioFrameRange::from_seconds(start, end, 48_000, 48_000).is_err());
        }
        assert!(
            AudioFrameRange {
                start_frame: 0,
                end_frame: 2,
                sample_rate: 0
            }
            .validate(2)
            .is_err()
        );
        assert!(range.validate(35_999).is_err());
    }
}
