// app/audiolib-desktop/src/waveform_view.rs
//! Waveform viewport and pixel rendering, independent of transport processing.
use audiolib_audio::{SPECTRAL_PEAK_BASE, WaveformData, spectral_color};

const SPECTRAL_BACKGROUND: [u8; 4] = [132, 144, 148, 255];
const NORMAL_BACKGROUND: [u8; 4] = [29, 35, 40, 255];

#[derive(Clone, Copy, Debug)]
pub(crate) struct WaveView {
    pub zoom: f64,
    pub scroll: f64,
    pub gain_db: f32,
    pub opacity: f32,
    pub noise: f32,
    pub variance: f32,
    pub spectral: bool,
}
impl Default for WaveView {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            scroll: 0.0,
            gain_db: 0.0,
            opacity: 0.85,
            noise: 1.0,
            variance: 0.38,
            spectral: true,
        }
    }
}
impl WaveView {
    pub fn range(self, duration: f64) -> (f64, f64) {
        let span = duration / self.zoom.clamp(1.0, 128.0);
        let start = (duration - span).max(0.0) * self.scroll.clamp(0.0, 1.0);
        (start, start + span)
    }
    pub fn zoom_at(&mut self, factor: f64, position: f64, duration: f64) {
        let (start, end) = self.range(duration);
        let anchor = if (start..=end).contains(&position) {
            position
        } else {
            (start + end) * 0.5
        };
        self.zoom = (self.zoom * factor).clamp(1.0, 128.0);
        let span = duration / self.zoom;
        self.scroll = if duration > span {
            (anchor - span * 0.5).clamp(0.0, duration - span) / (duration - span)
        } else {
            0.0
        };
    }

    /// Accumulates zoom around a source time from the displayed Waveform.
    /// The caller resolves `anchor` from the displayed range, which can differ
    /// from this pending view while rendering. The anchor keeps its pointer
    /// fraction unless a source boundary constrains it; invalid inputs are ignored.
    pub fn zoom_at_anchor(&mut self, factor: f64, anchor: f64, fraction: f64, duration: f64) {
        if !factor.is_finite()
            || factor <= 0.0
            || !anchor.is_finite()
            || !fraction.is_finite()
            || !duration.is_finite()
            || duration <= 0.0
            || !self.zoom.is_finite()
            || !self.scroll.is_finite()
        {
            return;
        }
        let anchor = anchor.clamp(0.0, duration);
        let fraction = fraction.clamp(0.0, 1.0);
        let zoom = (self.zoom.clamp(1.0, 128.0) * factor).clamp(1.0, 128.0);
        let span = duration / zoom;
        let travel = (duration - span).max(0.0);
        let scroll = if travel > 0.0 {
            (anchor - span * fraction).clamp(0.0, travel) / travel
        } else {
            0.0
        };
        self.zoom = zoom;
        self.scroll = scroll;
    }

    /// Pages a zoomed view so a playing `position` stays visible: once playback
    /// passes [`FOLLOW_EDGE`] of the window (or lies outside it), the window moves
    /// so the playhead sits [`FOLLOW_LEAD`] from its left edge. Returns whether the
    /// view moved; an unzoomed view, the source end and invalid input never move it.
    pub fn follow(&mut self, position: f64, duration: f64) -> bool {
        if !position.is_finite()
            || !duration.is_finite()
            || duration <= 0.0
            || !self.zoom.is_finite()
            || !self.scroll.is_finite()
            || self.zoom <= 1.0
        {
            return false;
        }
        let (start, end) = self.range(duration);
        let span = end - start;
        let travel = duration - span;
        if travel <= 0.0 || (position >= start && position <= start + span * FOLLOW_EDGE) {
            return false;
        }
        let scroll = (position - span * FOLLOW_LEAD).clamp(0.0, travel) / travel;
        if (scroll - self.scroll).abs() < 1e-9 {
            return false;
        }
        self.scroll = scroll;
        true
    }
}

/// Share of a zoomed window the playhead may cross before the view pages on.
pub(crate) const FOLLOW_EDGE: f64 = 0.9;
/// Where a paged window places the playhead, as a share of its width.
pub(crate) const FOLLOW_LEAD: f64 = 0.05;

/// One time-ruler tick: its position as a fraction of the visible range and, on
/// major ticks, its time label.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RulerMark {
    pub fraction: f64,
    pub label: Option<String>,
}

/// Label steps in seconds, each with the minor divisions drawn between labels.
const RULER_STEPS: &[(f64, u32)] = &[
    (0.001, 5),
    (0.002, 4),
    (0.005, 5),
    (0.01, 5),
    (0.02, 4),
    (0.05, 5),
    (0.1, 5),
    (0.2, 4),
    (0.5, 5),
    (1.0, 5),
    (2.0, 4),
    (5.0, 5),
    (10.0, 5),
    (15.0, 3),
    (30.0, 6),
    (60.0, 6),
    (120.0, 4),
    (300.0, 5),
    (600.0, 5),
    (900.0, 3),
    (1800.0, 6),
    (3600.0, 6),
];
/// At most this many labelled intervals fit the visible range.
const RULER_MAX_LABELS: f64 = 10.0;

/// Ticks on round times for the visible `start..end` seconds, so labels name
/// the time under their tick exactly (`0:03`, `0:03.5`, `1:02:00`).
pub(crate) fn ruler_marks(start: f64, end: f64) -> Vec<RulerMark> {
    let span = end - start;
    if !start.is_finite() || !end.is_finite() || span <= 0.0 || start < 0.0 {
        return Vec::new();
    }
    let (step, divisions) = RULER_STEPS
        .iter()
        .copied()
        .find(|(step, _)| span / step <= RULER_MAX_LABELS)
        .unwrap_or_else(|| ((span / RULER_MAX_LABELS / 3600.0).ceil() * 3600.0, 6));
    let minor = step / f64::from(divisions);
    // A small tolerance keeps ticks that land on the edges despite rounding.
    let first = (start / minor - 1e-9).ceil() as i64;
    let last = (end / minor + 1e-9).floor() as i64;
    let decimals = if step < 0.01 {
        3
    } else if step < 0.1 {
        2
    } else if step < 1.0 {
        1
    } else {
        0
    };
    let hours = end >= 3600.0;
    (first..=last)
        .map(|index| {
            let time = index as f64 * minor;
            let major = index.rem_euclid(i64::from(divisions)) == 0;
            RulerMark {
                fraction: ((time - start) / span).clamp(0.0, 1.0),
                label: major.then(|| ruler_label(time, decimals, hours)),
            }
        })
        .collect()
}

fn ruler_label(seconds: f64, decimals: usize, hours: bool) -> String {
    let scale = 10_u64.pow(decimals as u32);
    let units = (seconds.max(0.0) * scale as f64).round() as u64;
    let whole = units / scale;
    let mut text = if hours {
        format!(
            "{}:{:02}:{:02}",
            whole / 3600,
            (whole / 60) % 60,
            whole % 60
        )
    } else {
        format!("{}:{:02}", whole / 60, whole % 60)
    };
    if decimals > 0 {
        text.push_str(&format!(".{:0width$}", units % scale, width = decimals));
    }
    text
}

pub(crate) fn render_waveform(
    data: &WaveformData,
    width: u32,
    height: u32,
    view: WaveView,
) -> Vec<u8> {
    let mut rgba = vec![0_u8; (width * height * 4) as usize];
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.copy_from_slice(if view.spectral {
            &SPECTRAL_BACKGROUND
        } else {
            &NORMAL_BACKGROUND
        });
    }
    if width == 0 || height == 0 {
        return rgba;
    }
    let channels = data.peaks.len().max(1);
    let lane = height as f32 / channels as f32;
    let gain = 10.0_f32.powf(view.gain_db.clamp(-12.0, 36.0) / 20.0);
    for (channel, peaks) in data.peaks.iter().enumerate() {
        let center = lane * (channel as f32 + 0.5);
        let amplitude = (lane * 0.44).max(1.0);
        let top = (channel as f32 * lane) as u32;
        let bottom = (((channel + 1) as f32 * lane) as u32)
            .saturating_sub(1)
            .min(height - 1);
        for x in 0..width {
            let put = |rgba: &mut Vec<u8>, y: u32, color: [u8; 3]| {
                let index = ((y * width + x) * 4) as usize;
                rgba[index..index + 4].copy_from_slice(&[color[0], color[1], color[2], 255]);
            };
            put(
                &mut rgba,
                center.min((height - 1) as f32) as u32,
                if view.spectral {
                    [98, 111, 116]
                } else {
                    [55, 66, 72]
                },
            );
            let Some(peak) = peaks.get(x as usize * peaks.len() / width as usize) else {
                continue;
            };
            if peak.max.abs().max(peak.min.abs()) * gain < 0.00001 {
                continue;
            }
            let high = (center - (peak.max * gain).clamp(-1.0, 1.0) * amplitude)
                .clamp(top as f32, bottom as f32) as u32;
            let low = (center - (peak.min * gain).clamp(-1.0, 1.0) * amplitude)
                .clamp(top as f32, bottom as f32) as u32;
            let rgb = if view.spectral {
                spectral_color(peak.dominant_hz, peak.tonality, view.noise, view.variance)
            } else {
                [143, 174, 166]
            };
            // Spectral opacity blends with the normal BLACK peak, not light
            // gray: neither low tonality nor opacity may brighten the noise.
            let color = if view.spectral {
                let opacity = view.opacity.clamp(0.0, 1.0);
                rgb.map(|v| {
                    (f32::from(v) * opacity + f32::from(SPECTRAL_PEAK_BASE[0]) * (1.0 - opacity))
                        .round() as u8
                })
            } else {
                rgb
            };
            for y in high.min(low)..=high.max(low) {
                // Slightly darker RMS core leaves transients readable without
                // manufacturing a spectrogram or changing amplitude geometry.
                let core = (y as f32 - center).abs() < peak.rms * gain * amplitude;
                put(
                    &mut rgba,
                    y,
                    if core {
                        color.map(|v| (v as f32 * 0.78) as u8)
                    } else {
                        color
                    },
                );
            }
        }
    }
    rgba
}

/// Neutral alpha coverage of the same source peaks, channel lanes and display gain.
/// Studio recolors and clips this layer on playback ticks without reading audio or
/// rerendering peaks; the default and spectral frequency renderers remain unchanged.
pub(crate) fn render_waveform_mask(
    data: &WaveformData,
    width: u32,
    height: u32,
    view: WaveView,
) -> Vec<u8> {
    let mut pixels = render_waveform(
        data,
        width,
        height,
        WaveView {
            spectral: false,
            ..view
        },
    );
    for pixel in pixels.chunks_exact_mut(4) {
        let alpha = if pixel == NORMAL_BACKGROUND {
            0
        } else if pixel[..3] == [55, 66, 72] {
            45
        } else {
            255
        };
        pixel.copy_from_slice(&[255, 255, 255, alpha]);
    }
    pixels
}

pub(crate) fn db_from_gain(gain: f32) -> f32 {
    if gain <= 0.001 {
        -60.0
    } else {
        (20.0 * gain.log10()).clamp(-60.0, 0.0)
    }
}
pub(crate) fn gain_from_db(db: f32) -> f32 {
    if db <= -60.0 {
        0.0
    } else {
        10.0_f32.powf(db.clamp(-60.0, 0.0) / 20.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follow_pages_only_when_playback_leaves_the_zoomed_window() {
        let duration = 100.0;
        let mut view = WaveView {
            zoom: 4.0,
            ..Default::default()
        };
        // Inside the first 90% of 0..25: no move.
        assert!(!view.follow(10.0, duration));
        assert!(!view.follow(22.5, duration));
        // Past the edge: the playhead lands 5% into the next window.
        assert!(view.follow(22.6, duration));
        let (start, end) = view.range(duration);
        assert!((start - (22.6 - 25.0 * FOLLOW_LEAD)).abs() < 1e-9);
        assert!((end - start - 25.0).abs() < 1e-9);
        // A position behind the window (repeat wrap, seek) brings it back.
        assert!(view.follow(1.0, duration));
        assert_eq!(view.range(duration).0, 0.0);
        // At the source end the window cannot move further.
        view.scroll = 1.0;
        assert!(!view.follow(99.0, duration));
        // Unzoomed and invalid input never move.
        let mut full = WaveView::default();
        assert!(!full.follow(50.0, duration));
        assert!(!view.follow(f64::NAN, duration));
        assert!(!view.follow(10.0, 0.0));
    }

    #[test]
    fn ruler_marks_use_round_times_and_label_major_ticks() {
        let labels = |start: f64, end: f64| {
            ruler_marks(start, end)
                .into_iter()
                .filter_map(|mark| mark.label)
                .collect::<Vec<_>>()
        };
        // 30.21 s: 5 s labels, never the odd 3.021 s steps of an even split.
        assert_eq!(
            labels(0.0, 30.21),
            ["0:00", "0:05", "0:10", "0:15", "0:20", "0:25", "0:30"]
        );
        // Zoomed into 5.2..5.9 s: tenths.
        assert_eq!(
            labels(5.2, 5.9),
            [
                "0:05.2", "0:05.3", "0:05.4", "0:05.5", "0:05.6", "0:05.7", "0:05.8", "0:05.9"
            ]
        );
        assert_eq!(
            labels(0.0, 0.004),
            ["0:00.000", "0:00.001", "0:00.002", "0:00.003", "0:00.004"]
        );
        assert_eq!(
            labels(3540.0, 3720.0),
            [
                "0:59:00", "0:59:30", "1:00:00", "1:00:30", "1:01:00", "1:01:30", "1:02:00"
            ]
        );
        // Every mark lies inside the range, in order, with minor ticks between labels.
        let marks = ruler_marks(12.5, 62.5);
        assert!(
            marks
                .windows(2)
                .all(|pair| pair[0].fraction < pair[1].fraction)
        );
        assert!(
            marks
                .iter()
                .all(|mark| (0.0..=1.0).contains(&mark.fraction))
        );
        assert!(marks.iter().filter(|mark| mark.label.is_none()).count() > marks.len() / 2);
        let first = marks.iter().find(|mark| mark.label.is_some()).unwrap();
        assert_eq!(first.label.as_deref(), Some("0:15"));
        assert!((first.fraction - 0.05).abs() < 1e-9);
        // Empty or invalid ranges draw nothing.
        assert!(ruler_marks(5.0, 5.0).is_empty());
        assert!(ruler_marks(0.0, f64::INFINITY).is_empty());
    }

    #[test]
    fn mouse_zoom_keeps_source_anchor_when_zooming_in_and_out() {
        let duration = 137.5;
        for fraction in [0.1, 0.27, 0.5, 0.85] {
            let mut view = WaveView {
                zoom: 4.0,
                scroll: 0.43,
                ..Default::default()
            };
            let initial = view.range(duration);
            let anchor = initial.0 + (initial.1 - initial.0) * fraction;
            for factor in [2.0, 0.25, 2.0] {
                view.zoom_at_anchor(factor, anchor, fraction, duration);
                let (start, end) = view.range(duration);
                let actual_anchor = start + (end - start) * fraction;
                assert!((actual_anchor - anchor).abs() < 1e-10);
            }
            assert!((view.scroll - 0.43).abs() < 1e-10);
            assert_eq!(view.zoom, 4.0);
        }
    }

    #[test]
    fn mouse_zoom_anchors_to_displayed_range_while_prior_render_is_pending() {
        let duration = 100.0;
        let displayed = (0.0, 100.0);
        let source_at = |fraction| displayed.0 + (displayed.1 - displayed.0) * fraction;
        let mut pending = WaveView::default();
        pending.zoom_at_anchor(2.0, source_at(0.25), 0.25, duration);
        assert_eq!(pending.range(duration), (12.5, 62.5));

        // The first render is still pending, so moving the pointer to 75%
        // refers to 75 seconds in the displayed range, not 50 in the pending one.
        pending.zoom_at_anchor(2.0, source_at(0.75), 0.75, duration);
        assert_eq!(pending.zoom, 4.0);
        assert_eq!(pending.range(duration), (56.25, 81.25));
        let (start, end) = pending.range(duration);
        assert_eq!(start + (end - start) * 0.75, 75.0);

        // Reversing a wheel step also accumulates zoom while anchoring to what
        // remains visible, even when the pointer has moved to the other side.
        let mut pending = WaveView {
            zoom: 4.0,
            scroll: 0.5,
            ..Default::default()
        };
        let displayed = pending.range(duration);
        let source_at = |fraction| displayed.0 + (displayed.1 - displayed.0) * fraction;
        pending.zoom_at_anchor(2.0, source_at(0.25), 0.25, duration);
        assert_eq!(pending.range(duration), (40.625, 53.125));
        pending.zoom_at_anchor(0.5, source_at(0.75), 0.75, duration);
        assert_eq!(pending.zoom, 4.0);
        assert_eq!(pending.range(duration), displayed);
    }

    #[test]
    fn mouse_zoom_clamps_edges_and_restores_full_source() {
        for (fraction, scroll) in [(-20.0, 0.0), (0.0, 0.0), (1.0, 1.0), (20.0, 1.0)] {
            let mut view = WaveView::default();
            let anchor = if scroll == 0.0 { -20.0 } else { 120.0 };
            view.zoom_at_anchor(f64::MAX, anchor, fraction, 100.0);
            assert_eq!(view.zoom, 128.0);
            assert_eq!(view.scroll, scroll);
            let (start, end) = view.range(100.0);
            assert!(start >= 0.0 && end <= 100.0);
            view.zoom_at_anchor(1.0 / 4096.0, anchor, fraction, 100.0);
            assert_eq!(view.zoom, 1.0);
            assert_eq!(view.scroll, 0.0);
            assert_eq!(view.range(100.0), (0.0, 100.0));
        }

        let mut right = WaveView {
            zoom: 8.0,
            scroll: 1.0,
            ..Default::default()
        };
        right.zoom_at_anchor(0.5, 87.5, 0.0, 100.0);
        assert_eq!(right.range(100.0), (75.0, 100.0));
        assert_eq!(right.scroll, 1.0);
        let mut left = WaveView {
            zoom: 8.0,
            scroll: 0.0,
            ..Default::default()
        };
        left.zoom_at_anchor(0.5, 12.5, 1.0, 100.0);
        assert_eq!(left.range(100.0), (0.0, 25.0));
        assert_eq!(left.scroll, 0.0);
    }

    #[test]
    fn mouse_zoom_ignores_invalid_parameters() {
        let initial = WaveView {
            zoom: 8.0,
            scroll: 0.4,
            ..Default::default()
        };
        for (factor, anchor, fraction, duration) in [
            (0.0, 50.0, 0.5, 100.0),
            (-1.0, 50.0, 0.5, 100.0),
            (f64::NAN, 50.0, 0.5, 100.0),
            (f64::INFINITY, 50.0, 0.5, 100.0),
            (f64::NEG_INFINITY, 50.0, 0.5, 100.0),
            (2.0, f64::NAN, 0.5, 100.0),
            (2.0, f64::INFINITY, 0.5, 100.0),
            (2.0, f64::NEG_INFINITY, 0.5, 100.0),
            (2.0, 50.0, f64::NAN, 100.0),
            (2.0, 50.0, f64::INFINITY, 100.0),
            (2.0, 50.0, f64::NEG_INFINITY, 100.0),
            (2.0, 50.0, 0.5, 0.0),
            (2.0, 50.0, 0.5, -100.0),
            (2.0, 50.0, 0.5, f64::NAN),
            (2.0, 50.0, 0.5, f64::INFINITY),
            (2.0, 50.0, 0.5, f64::NEG_INFINITY),
        ] {
            let mut view = initial;
            view.zoom_at_anchor(factor, anchor, fraction, duration);
            assert_eq!(view.zoom, initial.zoom);
            assert_eq!(view.scroll, initial.scroll);
            assert_eq!(view.range(100.0), initial.range(100.0));
        }
    }

    #[test]
    fn zoom_and_pan_stay_within_source_and_gain_db_roundtrips() {
        let mut view = WaveView::default();
        view.zoom_at(8.0, 98.0, 100.0);
        let (start, end) = view.range(100.0);
        assert!(start >= 0.0 && end <= 100.0 && end == 100.0);
        view.zoom_at(1.0 / 128.0, 98.0, 100.0);
        assert_eq!(view.range(100.0), (0.0, 100.0));
        assert!((gain_from_db(-12.1) - 0.2483).abs() < 0.0001);
        assert!((db_from_gain(0.5) + 6.0206).abs() < 0.001);
        assert_eq!(gain_from_db(-60.0), 0.0);
    }
    #[test]
    fn noise_stays_black_through_opacity_and_display_gain_on_visible_background() {
        use audiolib_audio::WaveformPeak;
        let data = WaveformData {
            sample_rate: 48000,
            channels: 1,
            frames: 100,
            samples_per_pixel: 100,
            peaks: vec![vec![WaveformPeak {
                min: -0.5,
                max: 0.5,
                rms: 0.0,
                dominant_hz: 619.0,
                tonality: 0.003,
                ..Default::default()
            }]],
        };
        for opacity in [0.0, 0.5, 0.85, 1.0] {
            for gain_db in [-6.0, 0.0, 6.0] {
                let pixels = render_waveform(
                    &data,
                    1,
                    20,
                    WaveView {
                        opacity,
                        gain_db,
                        ..Default::default()
                    },
                );
                assert_eq!(&pixels[10 * 4..11 * 4], &[14, 14, 14, 255]);
                assert_eq!(&pixels[0..4], &SPECTRAL_BACKGROUND);
            }
        }
        let normal = render_waveform(
            &data,
            1,
            20,
            WaveView {
                spectral: false,
                ..Default::default()
            },
        );
        assert_eq!(&normal[0..4], &NORMAL_BACKGROUND);
        assert_eq!(&normal[10 * 4..11 * 4], &[143, 174, 166, 255]);
    }

    #[test]
    fn progress_mask_preserves_channel_geometry_gain_and_transparent_background() {
        use audiolib_audio::WaveformPeak;

        let data = WaveformData {
            sample_rate: 48_000,
            channels: 2,
            frames: 128,
            samples_per_pixel: 16,
            peaks: vec![
                vec![WaveformPeak {
                    min: -0.6,
                    max: 0.8,
                    ..Default::default()
                }],
                vec![WaveformPeak {
                    min: -0.2,
                    max: 0.3,
                    ..Default::default()
                }],
            ],
        };
        for gain_db in [-6.0, 0.0, 6.0] {
            let view = WaveView {
                gain_db,
                ..Default::default()
            };
            let normal = render_waveform(
                &data,
                8,
                40,
                WaveView {
                    spectral: false,
                    ..view
                },
            );
            let mask = render_waveform_mask(&data, 8, 40, view);
            assert_eq!(mask.len(), normal.len());
            for (original, masked) in normal.chunks_exact(4).zip(mask.chunks_exact(4)) {
                assert_eq!(masked[..3], [255; 3]);
                assert_eq!(masked[3] == 0, original == NORMAL_BACKGROUND);
            }
            assert!(
                mask[..20 * 8 * 4]
                    .chunks_exact(4)
                    .any(|pixel| pixel[3] == 255)
            );
            assert!(
                mask[20 * 8 * 4..]
                    .chunks_exact(4)
                    .any(|pixel| pixel[3] == 255)
            );
        }
        assert!(render_waveform_mask(&data, 0, 40, WaveView::default()).is_empty());
    }
}
