// crates/audiolib-core/src/audio_source.rs
//! Source-audio families shared by scanning, path validation and playback adapters.
use std::path::Path;

/// Source container family selected by a supported filename extension.
/// Header probing remains authoritative for the actual container and encoding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceAudioFormat {
    /// Little-endian RIFF, RF64 or BW64, including the `.bwf` alias.
    Wav,
    /// Native FLAC.
    Flac,
    /// Ogg Vorbis.
    Ogg,
    /// MPEG audio.
    Mp3,
    /// AIFF or AIFF-C.
    Aiff,
}

/// Extensions admitted by the shipped source-audio pipeline, without leading dots.
pub const SUPPORTED_AUDIO_EXTENSIONS: &[&str] =
    &["wav", "flac", "ogg", "oga", "bwf", "mp3", "aif", "aiff"];

/// Classify a supported extension without accessing or trusting source-file contents.
pub fn source_audio_format(path: &Path) -> Option<SourceAudioFormat> {
    let extension = path.extension()?.to_str()?;
    if extension.eq_ignore_ascii_case("wav") || extension.eq_ignore_ascii_case("bwf") {
        Some(SourceAudioFormat::Wav)
    } else if extension.eq_ignore_ascii_case("flac") {
        Some(SourceAudioFormat::Flac)
    } else if extension.eq_ignore_ascii_case("ogg") || extension.eq_ignore_ascii_case("oga") {
        Some(SourceAudioFormat::Ogg)
    } else if extension.eq_ignore_ascii_case("mp3") {
        Some(SourceAudioFormat::Mp3)
    } else if extension.eq_ignore_ascii_case("aif") || extension.eq_ignore_ascii_case("aiff") {
        Some(SourceAudioFormat::Aiff)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_extensions_share_one_case_insensitive_family_contract() {
        for extension in SUPPORTED_AUDIO_EXTENSIONS {
            assert!(source_audio_format(Path::new(&format!("sound.{extension}"))).is_some());
            assert!(
                source_audio_format(Path::new(&format!("sound.{}", extension.to_uppercase())))
                    .is_some()
            );
        }
        assert_eq!(
            source_audio_format(Path::new("sound.BWF")),
            Some(SourceAudioFormat::Wav)
        );
        assert_eq!(
            source_audio_format(Path::new("sound.AIF")),
            Some(SourceAudioFormat::Aiff)
        );
        for path in ["sound", "sound.wav.tmp", "sound.mp4", "sound.aifc"] {
            assert_eq!(source_audio_format(Path::new(path)), None);
        }
    }
}
