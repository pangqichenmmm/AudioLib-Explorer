// app/audiolib-desktop/src/playback_repaint_tests.rs
//! Playback cost guard for every theme. The window uses Slint's software renderer
//! with partial repaint, so a playback frame costs about the area it repaints.
//! While the playhead moves, a frame may repaint the playhead, the edge of the
//! played region and the changing labels, never the played waveform or the window.
//! The Studio gradient is the one exception: it re-spreads over the played waveform
//! each time the played width grows by 1/16, so such frames are rare.
use crate::AppWindow;
use audiolib_core::config::THEME_IDS;
use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType,
};
use slint::{ComponentHandle, Image, Rgba8Pixel, SharedPixelBuffer};
use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

const WIDTH: usize = 1600;
const HEIGHT: usize = 900;

struct Platform {
    window: Rc<MinimalSoftwareWindow>,
    now: Rc<Cell<Duration>>,
}

impl slint::platform::Platform for Platform {
    fn create_window_adapter(
        &self,
    ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }

    fn duration_since_start(&self) -> Duration {
        self.now.get()
    }
}

/// A synthetic peak image and its opaque mask.
fn waveform(width: u32, height: u32) -> (Image, Image) {
    let mut peaks = vec![0_u8; (width * height * 4) as usize];
    let mut mask = peaks.clone();
    for y in 0..height {
        for x in 0..width {
            let amplitude = ((x as f32 / 23.0).sin().abs() * 0.8 + 0.1) * height as f32 / 2.0;
            if ((y as f32) - height as f32 / 2.0).abs() < amplitude {
                let at = ((y * width + x) * 4) as usize;
                peaks[at..at + 4].copy_from_slice(&[200, 120, 60, 255]);
                mask[at..at + 4].copy_from_slice(&[255; 4]);
            }
        }
    }
    let image = |pixels: &[u8]| {
        Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
            pixels, width, height,
        ))
    };
    (image(&peaks), image(&mask))
}

#[test]
fn playback_repaints_only_around_the_playhead_in_every_theme() {
    // Platform selection is thread-local; no user window, database or audio exists.
    let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
    let now = Rc::new(Cell::new(Duration::ZERO));
    slint::platform::set_platform(Box::new(Platform {
        window: window.clone(),
        now: now.clone(),
    }))
    .unwrap();
    let ui = AppWindow::new().unwrap();
    ui.window()
        .set_size(slint::PhysicalSize::new(WIDTH as u32, HEIGHT as u32));
    ui.show().unwrap();
    let duration = 30.0_f32;
    ui.set_has_selection(true);
    ui.set_selected_row(0);
    ui.set_selected_id("1".into());
    ui.set_play_duration(duration);
    ui.set_play_end("00:30.000".into());
    ui.set_audio_ready(true);
    let (peaks, mask) = waveform(1880, 120);
    ui.set_waveform_image(peaks);
    ui.set_waveform_mask(mask);
    ui.set_waveform_ready(true);
    ui.set_view_start(0.0);
    ui.set_view_end(duration);
    crate::controller::publish_wave_target(
        &ui,
        crate::waveform_view::WaveView::default(),
        f64::from(duration),
    );
    ui.set_playback_state(3);
    ui.set_playback_clock("1:1".into());
    ui.set_confirmed_playback_rate(1.0);
    ui.set_playing(true);
    ui.set_playback_smooth(true);

    let mut buffer = vec![PremultipliedRgbaColor::default(); WIDTH * HEIGHT];
    // Advance one 60 Hz frame; returns the repainted area in pixels.
    let mut frame = || {
        now.set(now.get() + Duration::from_micros(16_667));
        slint::platform::update_timers_and_animations();
        let mut area = 0;
        window.draw_if_needed(|renderer| {
            area = renderer
                .render(&mut buffer, WIDTH)
                .iter()
                .map(|(_, size)| size.width as usize * size.height as usize)
                .sum();
        });
        area
    };
    // A repainted played waveform is several times this, the window 50 times.
    let budget = WIDTH * HEIGHT / 50;
    for theme in THEME_IDS {
        crate::theme::apply_theme(&ui, theme);
        for expanded in [false, true] {
            for peak_mode in [0, 1] {
                ui.set_audition_expanded(expanded);
                ui.set_peak_mode(peak_mode);
                // A late start makes the played waveform wide.
                let mut position = duration * 0.9;
                ui.set_play_position(position);
                // Let layout changes and transitions finish.
                for _ in 0..20 {
                    frame();
                }
                let mut areas = Vec::new();
                for step in 0..30 {
                    position += 1.0 / 60.0;
                    ui.set_play_position(position);
                    if step % 6 == 0 {
                        ui.set_play_time(format!("00:{position:06.3}").into());
                    }
                    areas.push(frame());
                }
                let case = format!("{theme}, expanded {expanded}, peak mode {peak_mode}");
                assert!(
                    areas.iter().any(|area| *area > 0),
                    "{case}: the playhead never repainted"
                );
                // 30 frames move the playhead less than one gradient step apart.
                let over = areas.iter().filter(|area| **area > budget).count();
                assert!(
                    over <= 1,
                    "{case}: {over} playback frames repainted more than {budget} px: {areas:?}"
                );
            }
        }
    }
}
