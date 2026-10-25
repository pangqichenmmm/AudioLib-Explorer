// crates/audiolib-audio/src/spectral.rs
//! Bounded spectral-peak analysis. Hue follows frequency; non-tonal content fades to the normal black peak.
use rustfft::{Fft, FftPlanner, num_complex::Complex};
use std::sync::Arc;

/// Frequency and tonal concentration of one Hann-windowed analysis frame.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SpectralFeature {
    pub frequency: f32,
    pub tonality: f32,
}

/// Normal peak color used for non-tonal content and zero spectral opacity.
pub const SPECTRAL_PEAK_BASE: [u8; 3] = [14; 3];

/// Map the supplied REAPER reference's logarithmic frequency anchors to vivid RGB.
/// Noise threshold gates weak tonal evidence; variance controls the fade from black.
/// This changes only the displayed RGB, never amplitude geometry or playback volume.
pub fn spectral_color(frequency: f32, tonality: f32, noise: f32, variance: f32) -> [u8; 3] {
    let anchors = [14.0_f32, 36.0, 93.0, 241.0, 628.0, 1632.0, 4241.0, 11025.0];
    // Calibrated to the supplied REAPER color strip: magenta, red, yellow,
    // green, turquoise, blue and violet. Full-tone colors use the full RGB
    // range; tonality and opacity still provide the existing fade to black.
    // The final pink endpoint continues the strip above its last visible label.
    // Keep the Peaks Display Settings legend in sync with these endpoints.
    let palette = [
        [251.0_f32, 0.0, 255.0],
        [255.0, 0.0, 24.0],
        [255.0, 211.0, 0.0],
        [61.0, 255.0, 0.0],
        [0.0, 255.0, 172.0],
        [0.0, 100.0, 255.0],
        [134.0, 0.0, 255.0],
        [255.0, 0.0, 148.0],
    ];
    let frequency = if frequency.is_finite() {
        frequency.clamp(14.0, 11025.0)
    } else {
        14.0
    };
    let band = anchors
        .windows(2)
        .position(|pair| frequency <= pair[1])
        .unwrap_or(6);
    let fraction = (frequency / anchors[band]).ln() / (anchors[band + 1] / anchors[band]).ln();
    let rgb: [f32; 3] = std::array::from_fn(|channel| {
        palette[band][channel] * (1.0 - fraction) + palette[band + 1][channel] * fraction
    });
    // A noise window always has a largest FFT bin. Do not turn that random
    // winner into a colored peak, or boost its tiny concentration with sqrt().
    let concentration = if tonality.is_finite() {
        tonality.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let noise = if noise.is_finite() {
        noise.clamp(0.1, 2.0)
    } else {
        1.0
    };
    let variance = if variance.is_finite() {
        variance.clamp(0.05, 1.0)
    } else {
        0.38
    };
    let floor = 0.06 * noise;
    let tonal_weight = ((concentration - floor) / (variance * (1.0 - floor)))
        .clamp(0.0, 1.0)
        .powi(2);
    rgb.map(|value| {
        (f32::from(SPECTRAL_PEAK_BASE[0]) * (1.0 - tonal_weight) + value * tonal_weight).round()
            as u8
    })
}

struct SpectralTransform {
    fft: Arc<dyn Fft<f32>>,
    buffer: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
    window: Vec<f32>,
}
impl SpectralTransform {
    fn new(size: usize) -> Self {
        let fft = FftPlanner::new().plan_fft_forward(size);
        Self {
            scratch: vec![Complex::default(); fft.get_inplace_scratch_len()],
            fft,
            buffer: vec![Complex::default(); size],
            window: (0..size)
                .map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / (size - 1) as f32).cos())
                .collect(),
        }
    }
}

pub(crate) struct SpectralAnalyzer {
    samples: Vec<Vec<f32>>,
    index: usize,
    frames: usize,
    rate: f32,
    long: SpectralTransform,
    short: SpectralTransform,
    low_features: Vec<Option<(usize, SpectralFeature)>>,
}
impl SpectralAnalyzer {
    pub fn new(rate: u32, channels: usize) -> Self {
        // A short (~20–40 ms) window preserves brief mid/high resonances in
        // impacts and squelches. The long window resolves the low-frequency
        // palette without smearing every transient over roughly 170 ms.
        let size = (rate / 10).next_power_of_two().clamp(2048, 65536) as usize;
        let short_size = (rate / 50).next_power_of_two().clamp(512, 8192) as usize;
        Self {
            samples: vec![vec![0.0; size]; channels],
            index: 0,
            frames: 0,
            rate: rate as f32,
            long: SpectralTransform::new(size),
            short: SpectralTransform::new(short_size),
            low_features: vec![None; channels],
        }
    }
    pub fn size(&self) -> usize {
        self.long.buffer.len()
    }
    pub fn hop(&self) -> usize {
        self.short.buffer.len() / 4
    }
    pub fn push(&mut self, frame: &[f32]) {
        for (samples, sample) in self.samples.iter_mut().zip(frame) {
            samples[self.index] = *sample;
        }
        self.index = (self.index + 1) % self.size();
        self.frames += 1;
    }
    pub fn analyze(&mut self, channel: usize) -> SpectralFeature {
        let feature = analyze_window(
            &self.samples[channel],
            self.index,
            self.rate,
            &mut self.short,
        );
        let low_limit = 4.0 * self.rate / self.short.buffer.len() as f32;
        if feature.frequency >= low_limit {
            return feature;
        }
        // Low frequencies evolve more slowly. Reuse their long-window result
        // for at most a quarter window, bounding the expensive FFT work.
        if let Some((frame, feature)) = self.low_features[channel]
            && self.frames - frame < self.size() / 4
        {
            return feature;
        }
        let feature = analyze_window(
            &self.samples[channel],
            self.index,
            self.rate,
            &mut self.long,
        );
        self.low_features[channel] = Some((self.frames, feature));
        feature
    }
}

fn analyze_window(
    samples: &[f32],
    index: usize,
    rate: f32,
    transform: &mut SpectralTransform,
) -> SpectralFeature {
    let size = transform.buffer.len();
    let offset = (samples.len() - size) / 2;
    let mean = (0..size)
        .map(|i| samples[(index + offset + i) % samples.len()])
        .sum::<f32>()
        / size as f32;
    for i in 0..size {
        transform.buffer[i] = Complex::new(
            (samples[(index + offset + i) % samples.len()] - mean) * transform.window[i],
            0.0,
        );
    }
    transform
        .fft
        .process_with_scratch(&mut transform.buffer, &mut transform.scratch);
    let spectrum = &transform.buffer[..size / 2 + 1];
    let (bin, power) = spectrum
        .iter()
        .enumerate()
        .skip(1)
        .map(|(i, x)| (i, x.norm_sqr()))
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .unwrap();
    let total: f32 = spectrum.iter().skip(1).map(|x| x.norm_sqr()).sum();
    if total < 1e-12 || !total.is_finite() {
        return SpectralFeature::default();
    }
    let left = spectrum[bin.saturating_sub(1)].norm_sqr().max(1e-20).ln();
    let middle = power.max(1e-20).ln();
    let right = spectrum[(bin + 1).min(spectrum.len() - 1)]
        .norm_sqr()
        .max(1e-20)
        .ln();
    let denominator = left - 2.0 * middle + right;
    let offset = if denominator.abs() > 1e-8 {
        (0.5 * (left - right) / denominator).clamp(-0.5, 0.5)
    } else {
        0.0
    };
    let local: f32 = spectrum[bin.saturating_sub(2).max(1)..=(bin + 2).min(spectrum.len() - 1)]
        .iter()
        .map(|x| x.norm_sqr())
        .sum();
    // Estimate broadband energy beside the Hann main lobe. Subtracting
    // that floor stops broad noise humps from masquerading as pure tones.
    let local_start = bin.saturating_sub(2).max(1);
    let local_end = (bin + 2).min(spectrum.len() - 1);
    let neighbors = bin.saturating_sub(12).max(1)..=(bin + 12).min(spectrum.len() - 1);
    let mut noise_power = 0.0_f32;
    let mut noise_bins = 0;
    for neighbor in neighbors {
        if !(local_start..=local_end).contains(&neighbor) {
            noise_power += spectrum[neighbor].norm_sqr();
            noise_bins += 1;
        }
    }
    let noise_floor = noise_power / noise_bins.max(1) as f32;
    let tonal_power = (local - noise_floor * (local_end - local_start + 1) as f32).max(0.0);
    SpectralFeature {
        frequency: (bin as f32 + offset) * rate / size as f32,
        tonality: (tonal_power / total).clamp(0.0, 1.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reference_hues_follow_log_frequency_and_noise_fades_to_black() {
        for (hz, rgb) in [
            (14.0, [251, 0, 255]),
            (36.0, [255, 0, 24]),
            (93.0, [255, 211, 0]),
            (241.0, [61, 255, 0]),
            (628.0, [0, 255, 172]),
            (1632.0, [0, 100, 255]),
            (4241.0, [134, 0, 255]),
            (11025.0, [255, 0, 148]),
        ] {
            assert_eq!(spectral_color(hz, 1.0, 1.0, 0.38), rgb);
        }
        assert_eq!(spectral_color(619.0, 0.0, 1.0, 0.38), SPECTRAL_PEAK_BASE);
    }
    #[test]
    fn dominant_tone_is_resolved_at_high_sample_rates_and_noise_is_less_tonal() {
        for rate in [44100, 48000, 192000] {
            for frequency in [36.0, 92.0, 239.0, 619.0, 1604.0, 4155.0] {
                let mut analyzer = SpectralAnalyzer::new(rate, 2);
                let mut random = 12345_u32;
                for frame in 0..analyzer.size() {
                    random = random.wrapping_mul(1664525).wrapping_add(1013904223);
                    analyzer.push(&[
                        (std::f32::consts::TAU * frequency * frame as f32 / rate as f32).sin()
                            * 0.5,
                        random as f32 / u32::MAX as f32 - 0.5,
                    ]);
                }
                let tone = analyzer.analyze(0);
                let noise = analyzer.analyze(1);
                assert!(
                    (tone.frequency - frequency).abs() < 3.0,
                    "{rate}: {frequency} -> {}",
                    tone.frequency
                );
                assert!(tone.tonality > 0.9 && noise.tonality < 0.06);
            }
        }
    }
    #[test]
    fn broadband_noise_and_impulses_are_black_at_different_levels_and_rates() {
        for rate in [44100, 48000, 192000] {
            for seed in [7_u32, 73, 917, 10249] {
                let mut analyzer = SpectralAnalyzer::new(rate, 3);
                let mut random = seed;
                for frame in 0..analyzer.size() {
                    random = random.wrapping_mul(1664525).wrapping_add(1013904223);
                    let noise = random as f32 / u32::MAX as f32 * 2.0 - 1.0;
                    analyzer.push(&[
                        noise * 0.8,
                        noise * 0.08,
                        if frame == analyzer.size() / 2 {
                            0.8
                        } else {
                            0.0
                        },
                    ]);
                }
                let loud = analyzer.analyze(0);
                let quiet = analyzer.analyze(1);
                assert!((loud.tonality - quiet.tonality).abs() < 1e-5);
                for feature in [loud, quiet, analyzer.analyze(2)] {
                    assert_eq!(
                        spectral_color(feature.frequency, feature.tonality, 1.0, 0.38),
                        SPECTRAL_PEAK_BASE,
                        "rate={rate}, seed={seed}, feature={feature:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn mixed_signal_color_tracks_tonal_evidence_without_colorizing_noise() {
        for rate in [48000, 192000] {
            let mut previous_brightness = 0;
            for snr in [-30.0_f32, -20.0, -10.0, 0.0, 10.0, 30.0] {
                let tone_gain = 10.0_f32.powf(snr / 20.0);
                let mut analyzer = SpectralAnalyzer::new(rate, 1);
                let mut random = 179_u32;
                for frame in 0..analyzer.size() {
                    random = random.wrapping_mul(1664525).wrapping_add(1013904223);
                    let noise = (random as f32 / u32::MAX as f32 * 2.0 - 1.0) * 3.0_f32.sqrt();
                    let tone = (std::f32::consts::TAU * 628.0 * frame as f32 / rate as f32).sin()
                        * 2.0_f32.sqrt();
                    analyzer.push(&[(tone * tone_gain + noise) * 0.4 / (1.0 + tone_gain)]);
                }
                let feature = analyzer.analyze(0);
                let color = spectral_color(feature.frequency, feature.tonality, 1.0, 0.38);
                let brightness = *color.iter().max().unwrap();
                assert!(
                    brightness >= previous_brightness,
                    "snr={snr}, feature={feature:?}"
                );
                previous_brightness = brightness;
                if snr <= -10.0 {
                    assert!(
                        brightness <= 20,
                        "noise-dominated signal colored: {color:?}"
                    );
                }
                if snr >= 10.0 {
                    assert!((feature.frequency - 628.0).abs() < 3.0);
                    assert!(color[1] > color[0] + 70 && color[2] > color[0] + 70);
                }
            }
        }
    }
}
