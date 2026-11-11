// crates/audiolib-remote/src/route.rs
//! Which server endpoint auditions a record (doc/api/README.md §8, §8.1).
//!
//! Transcoding is the user's choice per server profile ("transcode auditions",
//! [`RouteOptions::transcode`]); the measured throughput never switches it on or off.
//!
//! * Transcoding off (the default, and what LAN profiles use): always the original
//!   `/audio/{id}/stream`; `/preview` is never requested.
//! * Non-WAV containers use their original `/stream` bytes and a bundled decoder.
//! * Transcoding on, WAV: `/audio/{id}/preview?codec=pcm&sr=48000&bits=16` (`sr=44100` for
//!   the 44.1 kHz family, never above the source rate) and `ch=2` for more than two
//!   channels. `sr` and `bits` are always sent: `codec=pcm` alone keeps the original 96k/24.
//! * `codec=flac` (same `sr`, 16-bit, stereo; mono below [`MONO_BELOW_BPS`]) when the
//!   rolling throughput is below [`FLAC_BELOW_BPS`] (assumed
//!   [`CONSERVATIVE_BANDWIDTH_BPS`] before the first measurement), or when `codec=pcm`
//!   answers `415`; when FLAC fails too (`415`, `503 TRANSCODER_UNAVAILABLE`) the raw
//!   `/stream`.
use audiolib_core::AudioFile;

/// Assumed throughput until a transfer has been measured.
pub const CONSERVATIVE_BANDWIDTH_BPS: f64 = 4_000_000.0;
/// Below this throughput FLAC (about 0.4–1.0 Mbit/s) replaces 16-bit PCM (1.4–1.5 Mbit/s).
pub const FLAC_BELOW_BPS: f64 = 2_500_000.0;
/// Below this throughput the FLAC preview is folded to mono.
pub const MONO_BELOW_BPS: f64 = 800_000.0;

/// What besides the record and the throughput decides the route.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RouteOptions {
    /// The player's source channel pair (0-based): a later pair needs every channel, so
    /// no channel fold then.
    pub source_pair: u16,
    /// The active server profile transcodes auditions (`/preview`); `false` (the
    /// default) always streams the original.
    pub transcode: bool,
    /// `codec=pcm` answered `415` for this record.
    pub pcm_unsupported: bool,
    /// `codec=flac` failed for this record (or the server has no transcoder).
    pub flac_unavailable: bool,
}

/// Output parameters of a preview; `None` keeps the source value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct PreviewParams {
    pub sample_rate: Option<u32>,
    pub bits: Option<u16>,
    pub channels: Option<u16>,
}

/// The endpoint (and parameters) an audition reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AudioRoute {
    /// `/audio/{id}/stream`: the original bytes.
    Stream,
    /// `/audio/{id}/preview?codec=pcm`: a decimated 16-bit WAV with a known length.
    Pcm(PreviewParams),
    /// `/audio/{id}/preview?codec=flac`: chunked FLAC, positioned with `t=`.
    Flac(PreviewParams),
}

impl AudioRoute {
    /// Query string after `/preview`, or `None` for `/stream`.
    pub fn preview_query(&self) -> Option<String> {
        let (codec, params) = match self {
            Self::Stream => return None,
            Self::Pcm(params) => ("pcm", params),
            Self::Flac(params) => ("flac", params),
        };
        let mut query = format!("codec={codec}");
        if let Some(rate) = params.sample_rate {
            query.push_str(&format!("&sr={rate}"));
        }
        if let Some(bits) = params.bits {
            query.push_str(&format!("&bits={bits}"));
        }
        if let Some(channels) = params.channels {
            query.push_str(&format!("&ch={channels}"));
        }
        Some(query)
    }

    /// Short stable tag for cache keys and logs, e.g. `stream`, `pcm-48000-16-2`.
    pub fn tag(&self) -> String {
        let part = |value: Option<u32>| value.map_or_else(|| "src".to_owned(), |v| v.to_string());
        match self {
            Self::Stream => "stream".into(),
            Self::Pcm(params) | Self::Flac(params) => format!(
                "{}-{}-{}-{}",
                if matches!(self, Self::Pcm(_)) {
                    "pcm"
                } else {
                    "flac"
                },
                part(params.sample_rate),
                part(params.bits.map(u32::from)),
                part(params.channels.map(u32::from)),
            ),
        }
    }

    pub fn is_preview(&self) -> bool {
        !matches!(self, Self::Stream)
    }

    /// Bytes per second of audio the player reads for `file` on this route (a FLAC
    /// preview is decoded to PCM of the same layout as `codec=pcm`).
    pub fn output_bytes_per_second(&self, file: &AudioFile) -> u64 {
        if matches!(self, Self::Stream)
            && matches!(
                file.attributes["container"].as_str(),
                Some("MP3" | "FLAC" | "Ogg")
            )
            && file.duration.is_finite()
            && file.duration > 0.0
        {
            return (file.file_size as f64 / file.duration).ceil() as u64;
        }
        let (rate, bits, channels) = match self {
            Self::Stream => (file.sample_rate, file.bit_depth, file.channels),
            Self::Pcm(params) | Self::Flac(params) => (
                params.sample_rate.map_or(file.sample_rate, |rate| {
                    pcm_output_rate(file.sample_rate, rate)
                }),
                params.bits.unwrap_or(file.bit_depth),
                params.channels.unwrap_or(file.channels),
            ),
        };
        u64::from(rate) * u64::from(bits.div_ceil(8)) * u64::from(channels)
    }
}

/// Whether the actual known source container is WAV; old records fall back to name.
pub fn is_wav(file: &AudioFile) -> bool {
    match file.attributes["container"].as_str() {
        Some("RIFF" | "RF64" | "BW64") => return true,
        Some("MP3" | "AIFF" | "AIFC" | "FLAC" | "Ogg") => return false,
        _ => {}
    }
    let name = if file.filename.is_empty() {
        file.file_path.as_str()
    } else {
        file.filename.as_str()
    };
    let lower = name.to_ascii_lowercase();
    [".wav", ".wave", ".bwf", ".rf64"]
        .iter()
        .any(|extension| lower.ends_with(extension))
}

/// Raw bitrate of the record in bits per second (0 when the metadata is unknown).
pub fn raw_bitrate(file: &AudioFile) -> f64 {
    f64::from(file.sample_rate) * f64::from(file.bit_depth) * f64::from(file.channels)
}

/// The rate `codec=pcm` produces for `requested` from a `source` rate: the source divided
/// by the smallest integer `n` with `source / n <= requested` (§8.1; never upsampled).
pub fn pcm_output_rate(source: u32, requested: u32) -> u32 {
    if source == 0 || requested >= source {
        return source;
    }
    let n = source.div_ceil(requested.max(1));
    (n..=source)
        .find(|n| source.is_multiple_of(*n))
        .map_or(source, |n| source / n)
}

/// The preview rate for a `source` rate: 44.1 kHz for the 44.1 kHz family, else 48 kHz,
/// never above the source (the server only decimates).
pub fn preview_rate(source: u32) -> u32 {
    let target = if source > 0 && source.is_multiple_of(44_100) {
        44_100
    } else {
        48_000
    };
    if source == 0 {
        target
    } else {
        target.min(source)
    }
}

/// Pick the route for `file` (see the module docs).
pub fn choose_route(
    file: &AudioFile,
    bandwidth_bps: Option<f64>,
    options: RouteOptions,
) -> AudioRoute {
    if !options.transcode || !is_wav(file) || raw_bitrate(file) <= 0.0 {
        return AudioRoute::Stream;
    }
    let bandwidth = bandwidth_bps.unwrap_or(CONSERVATIVE_BANDWIDTH_BPS);
    let first_pair = options.source_pair == 0;
    let stereo = (file.channels > 2 && first_pair).then_some(2);
    let pcm = AudioRoute::Pcm(PreviewParams {
        sample_rate: Some(preview_rate(file.sample_rate)),
        bits: Some(16),
        channels: stereo,
    });
    let flac = AudioRoute::Flac(PreviewParams {
        sample_rate: Some(preview_rate(file.sample_rate)),
        bits: Some(16),
        channels: if bandwidth < MONO_BELOW_BPS && file.channels > 1 && first_pair {
            Some(1)
        } else {
            stereo
        },
    });
    match (options.pcm_unsupported, options.flac_unavailable) {
        (false, false) if bandwidth < FLAC_BELOW_BPS => flac,
        (false, _) => pcm,
        (true, false) => flac,
        (true, true) => AudioRoute::Stream,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav(sample_rate: u32, bit_depth: u16, channels: u16) -> AudioFile {
        AudioFile::from(audiolib_core::AudioFileData {
            id: 1,
            filename: "x.wav".into(),
            file_path: "/S/x.wav".into(),
            sample_rate,
            bit_depth,
            channels,
            ..Default::default()
        })
    }

    fn pcm(rate: u32, channels: Option<u16>) -> AudioRoute {
        AudioRoute::Pcm(PreviewParams {
            sample_rate: Some(rate),
            bits: Some(16),
            channels,
        })
    }

    fn flac(rate: u32, channels: Option<u16>) -> AudioRoute {
        AudioRoute::Flac(PreviewParams {
            sample_rate: Some(rate),
            bits: Some(16),
            channels,
        })
    }

    /// Transcoding on (the route the tests below exercise unless they say otherwise).
    fn on() -> RouteOptions {
        RouteOptions {
            transcode: true,
            ..Default::default()
        }
    }

    fn route(file: &AudioFile, bandwidth: Option<f64>) -> AudioRoute {
        choose_route(file, bandwidth, on())
    }

    #[test]
    fn transcoding_off_always_streams_the_original_whatever_the_throughput() {
        let off = RouteOptions::default();
        assert!(!off.transcode);
        for file in [wav(96_000, 24, 2), wav(192_000, 24, 6), wav(44_100, 16, 1)] {
            for bandwidth in [None, Some(0.3e6), Some(2e6), Some(5e6), Some(100e6)] {
                assert_eq!(
                    choose_route(&file, bandwidth, off),
                    AudioRoute::Stream,
                    "{bandwidth:?}"
                );
            }
        }
        // Remembered refusals do not matter either: nothing asks /preview.
        let refused = RouteOptions {
            pcm_unsupported: true,
            flac_unavailable: true,
            ..off
        };
        assert_eq!(
            choose_route(&wav(96_000, 24, 2), Some(1e6), refused),
            AudioRoute::Stream
        );
    }

    #[test]
    fn decimation_follows_the_servers_integer_rule() {
        assert_eq!(pcm_output_rate(96_000, 48_000), 48_000);
        assert_eq!(pcm_output_rate(192_000, 48_000), 48_000);
        assert_eq!(pcm_output_rate(88_200, 48_000), 44_100);
        assert_eq!(pcm_output_rate(44_100, 48_000), 44_100);
        assert_eq!(pcm_output_rate(96_000, 44_100), 32_000);
        assert_eq!(preview_rate(96_000), 48_000);
        assert_eq!(preview_rate(192_000), 48_000);
        assert_eq!(preview_rate(48_000), 48_000);
        assert_eq!(preview_rate(88_200), 44_100);
        assert_eq!(preview_rate(44_100), 44_100);
        assert_eq!(preview_rate(32_000), 32_000);
    }

    #[test]
    fn transcoding_on_uses_the_16_bit_preview_even_on_a_fast_link() {
        let stereo96 = wav(96_000, 24, 2); // 4.608 Mbit/s
        // Unmeasured (conservative 4 Mbit/s) up to a LAN: the preview, always with sr and
        // bits (codec=pcm alone would keep 96k/24). The throughput never picks /stream.
        for bandwidth in [None, Some(4e6), Some(5e6), Some(9.3e6), Some(100e6)] {
            assert_eq!(
                route(&stereo96, bandwidth),
                pcm(48_000, None),
                "{bandwidth:?}"
            );
        }
        assert_eq!(
            pcm(48_000, None).preview_query().unwrap(),
            "codec=pcm&sr=48000&bits=16"
        );
        // 192k/24 stereo (9.2 Mbit/s) at 10 Mbit/s: preview; 44.1k family keeps 44.1k.
        assert_eq!(route(&wav(192_000, 24, 2), Some(10e6)), pcm(48_000, None));
        assert_eq!(route(&wav(88_200, 24, 2), Some(5e6)), pcm(44_100, None));
        // A 48k/16 stereo source (1.5 Mbit/s) at 2.6 Mbit/s still gets the clean preview.
        assert_eq!(route(&wav(48_000, 16, 2), Some(2.6e6)), pcm(48_000, None));
    }

    #[test]
    fn multichannel_files_fold_to_stereo_unless_a_later_pair_is_selected() {
        let six = wav(96_000, 24, 6); // 13.8 Mbit/s
        assert_eq!(route(&six, Some(5e6)), pcm(48_000, Some(2)));
        assert_eq!(route(&six, Some(20e6)), pcm(48_000, Some(2)));
        assert_eq!(
            pcm(48_000, Some(2)).preview_query().unwrap(),
            "codec=pcm&sr=48000&bits=16&ch=2"
        );
        // Pair 3/4 selected: all channels are needed.
        let later = RouteOptions {
            source_pair: 1,
            ..on()
        };
        assert_eq!(choose_route(&six, Some(15e6), later), pcm(48_000, None));
        assert_eq!(route(&six, Some(30e6)), pcm(48_000, Some(2)));
    }

    #[test]
    fn slow_links_get_flac_and_refusals_fall_back_to_flac_then_the_raw_stream() {
        let stereo96 = wav(96_000, 24, 2);
        // Below 2.5 Mbit/s: FLAC stereo; below 0.8 Mbit/s: FLAC mono.
        assert_eq!(route(&stereo96, Some(2.4e6)), flac(48_000, None));
        assert_eq!(route(&stereo96, Some(0.7e6)), flac(48_000, Some(1)));
        assert_eq!(
            route(&wav(96_000, 24, 6), Some(1.5e6)),
            flac(48_000, Some(2))
        );
        assert_eq!(route(&wav(96_000, 24, 1), Some(0.5e6)), flac(48_000, None));
        assert_eq!(
            flac(48_000, Some(1)).preview_query().unwrap(),
            "codec=flac&sr=48000&bits=16&ch=1"
        );
        // 415 from codec=pcm: FLAC, even on a fast link.
        let pcm_refused = RouteOptions {
            pcm_unsupported: true,
            ..on()
        };
        assert_eq!(
            choose_route(&stereo96, Some(5e6), pcm_refused),
            flac(48_000, None)
        );
        // FLAC refused too: the original stream.
        let both = RouteOptions {
            pcm_unsupported: true,
            flac_unavailable: true,
            ..on()
        };
        assert_eq!(choose_route(&stereo96, Some(5e6), both), AudioRoute::Stream);
        // No transcoder on a slow link: PCM rather than the raw original.
        let no_flac = RouteOptions {
            flac_unavailable: true,
            ..on()
        };
        assert_eq!(
            choose_route(&stereo96, Some(1.5e6), no_flac),
            pcm(48_000, None)
        );
        // Non-WAV files never use a preview.
        let mut ogg = wav(48_000, 16, 2);
        ogg.filename = "x.ogg".into();
        assert_eq!(route(&ogg, Some(0.5e6)), AudioRoute::Stream);
        assert_eq!(AudioRoute::Stream.preview_query(), None);
        assert_eq!(AudioRoute::Stream.tag(), "stream");
        assert_eq!(flac(48_000, None).tag(), "flac-48000-16-src");
    }

    #[test]
    fn inspected_non_wave_containers_use_original_bytes_even_with_a_wave_name() {
        for container in ["MP3", "AIFF", "AIFC", "FLAC", "Ogg"] {
            let mut file = wav(48_000, 16, 2);
            file.attributes = serde_json::json!({"container":container});
            assert!(!is_wav(&file));
            assert_eq!(
                choose_route(&file, Some(100_000.0), on()),
                AudioRoute::Stream
            );
        }
        let mut mp3 = wav(48_000, 0, 2);
        mp3.attributes = serde_json::json!({"container":"MP3"});
        mp3.duration = 2.0;
        mp3.file_size = 32_000;
        assert_eq!(AudioRoute::Stream.output_bytes_per_second(&mp3), 16_000);
    }
}
