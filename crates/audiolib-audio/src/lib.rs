// crates/audiolib-audio/src/lib.rs
//! Streaming WAV metadata, playback, and multichannel Waveform analysis.
#[cfg(feature = "playback")]
mod audition;
// Decoder/resampler paths are only reached through the playback engine; the headless build keeps them for tests.
#[cfg_attr(not(feature = "playback"), allow(dead_code))]
mod decoder;
#[cfg(feature = "playback")]
mod engine;
mod list_waveform;
mod source;
mod spectral;
#[cfg(test)]
mod tests;
mod wav;
mod wav_metadata;
mod waveform;
#[cfg(feature = "playback")]
pub use audition::AuditionSettings;
pub use list_waveform::{
    LIST_WAVEFORM_ZOOM_LEVEL, list_samples_per_pixel, list_waveform_with_cancel,
    merge_list_channels, quantize_list_peak,
};
pub use source::{
    ByteSource, SourceBuffer, SourceOpener, is_stream_source, register_buffered_stream_source,
    register_stream_source, unregister_stream_source,
};
pub use spectral::{SPECTRAL_PEAK_BASE, spectral_color};

#[cfg(feature = "playback")]
pub use engine::{
    AudioBackend, AudioCommand, AudioDevice, AudioEngine, AudioEvent, AudioHandle,
    CommandCompletion, PlaybackConfig, PlaybackState, PlaybackStatus, RatePolicy, list_devices,
};
/// Compatibility name for the persisted output-device configuration contract.
#[cfg(feature = "playback")]
pub type AudioOutputConfig = PlaybackConfig;
pub use wav::{probe, probe_file_with_artwork, probe_with_artwork};
pub use wav_metadata::{Artwork, WAV_METADATA_VERSION};
pub use waveform::{
    CACHE_VERSION as WAVEFORM_CACHE_VERSION, WaveformData, WaveformPeak, WaveformRequest,
    cached_waveform, cached_waveform_with_cancel, summary_waveform_with_read_buffer, waveform,
    waveform_with_cancel, waveform_with_read_buffer,
};
