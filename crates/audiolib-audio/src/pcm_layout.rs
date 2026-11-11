// crates/audiolib-audio/src/pcm_layout.rs
//! Explicit PCM storage properties shared by WAV and AIFF decoding.
use audiolib_core::{Error, Result};

#[derive(Clone, Copy, Debug)]
pub(crate) enum ByteOrder {
    Little,
    Big,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum SampleKind {
    Signed,
    Unsigned,
    Float,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PcmLayout {
    pub endian: ByteOrder,
    pub kind: SampleKind,
    pub bytes_per_sample: u8,
}

impl PcmLayout {
    pub fn validate(self) -> Result<()> {
        if !matches!(
            (self.kind, self.bytes_per_sample),
            (SampleKind::Signed, 1..=4) | (SampleKind::Unsigned, 1) | (SampleKind::Float, 4 | 8)
        ) {
            return Err(Error::Audio("Unsupported PCM storage layout".into()));
        }
        Ok(())
    }

    // The caller validates the layout once and supplies exactly one stored sample.
    pub fn decode(self, bytes: &[u8]) -> f32 {
        match self.kind {
            SampleKind::Unsigned => (f32::from(bytes[0]) - 128.0) / 128.0,
            SampleKind::Signed => {
                let width = bytes.len();
                let mut word = [0_u8; 4];
                let value = match self.endian {
                    ByteOrder::Little => {
                        word[..width].copy_from_slice(bytes);
                        i32::from_le_bytes(word)
                    }
                    ByteOrder::Big => {
                        word[4 - width..].copy_from_slice(bytes);
                        i32::from_be_bytes(word)
                    }
                };
                let shift = 32 - width * 8;
                ((value << shift) >> shift) as f32 / (1_u64 << (width * 8 - 1)) as f32
            }
            SampleKind::Float => match (self.endian, bytes.len()) {
                (ByteOrder::Little, 4) => f32::from_le_bytes(bytes.try_into().unwrap()),
                (ByteOrder::Big, 4) => f32::from_be_bytes(bytes.try_into().unwrap()),
                (ByteOrder::Little, 8) => f64::from_le_bytes(bytes.try_into().unwrap()) as f32,
                (ByteOrder::Big, 8) => f64::from_be_bytes(bytes.try_into().unwrap()) as f32,
                _ => unreachable!("validated PCM layout"),
            },
        }
    }
}
