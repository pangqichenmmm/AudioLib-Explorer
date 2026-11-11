// app/audiolib-desktop/src/waveform_cursor.rs
//! Waveform pointer feedback through Slint's public winit cursor APIs.
use crate::AppWindow;
use slint::winit_030::{
    CustomApplicationHandler, EventResult, WinitWindowAccessor,
    winit::{
        event::WindowEvent,
        event_loop::ActiveEventLoop,
        window::{Cursor, CursorIcon, CustomCursor, WindowId},
    },
};
use slint::{ComponentHandle, Image, Timer, TimerMode};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::{Rc, Weak};
use std::time::Duration;

const SCALE_STEPS: f64 = 64.0;
const LOGICAL_SIDE: u16 = 20;
const MAX_SIDE: u32 = 256;

thread_local! {
    // The desktop has one main window. The callback owns State, which only keeps
    // a weak component handle; neither this registration nor the timer retains it.
    static CURRENT: RefCell<Weak<State>> = const { RefCell::new(Weak::new()) };
}

fn current() -> Option<Rc<State>> {
    CURRENT.with(|state| state.borrow().upgrade())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ScaleKey(u16);

impl ScaleKey {
    fn new(window_scale: f64) -> Self {
        let scale = if window_scale.is_finite() {
            window_scale.clamp(0.5, 8.0)
        } else {
            1.0
        };
        Self((scale * SCALE_STEPS).round() as u16)
    }

    fn factor(self) -> f64 {
        f64::from(self.0) / SCALE_STEPS
    }
}

struct CursorCache<K, V> {
    scale: Option<ScaleKey>,
    entries: BTreeMap<i32, (K, V)>,
}

impl<K, V> Default for CursorCache<K, V> {
    fn default() -> Self {
        Self {
            scale: None,
            entries: BTreeMap::new(),
        }
    }
}

impl<K: PartialEq, V> CursorCache<K, V> {
    fn prepare_scale(&mut self, scale: ScaleKey) {
        if self.scale != Some(scale) {
            self.entries.clear();
            self.scale = Some(scale);
        }
    }

    fn get(&self, kind: i32, key: &K) -> Option<&V> {
        self.entries
            .get(&kind)
            .and_then(|(cached, cursor)| (cached == key).then_some(cursor))
    }

    fn insert(&mut self, kind: i32, key: K, cursor: V) {
        // Only these eight UI states allocate native cursor handles.
        if (1..=8).contains(&kind) {
            self.entries.insert(kind, (key, cursor));
        }
    }
}

#[derive(PartialEq)]
struct ImageKey {
    image: Image,
    hotspot_x: u32,
    hotspot_y: u32,
}

struct State {
    ui: slint::Weak<AppWindow>,
    timer: Timer,
    pending: Cell<bool>,
    suspended: Cell<bool>,
    pointer_in_window: Cell<bool>,
    fallback_hidden: Cell<bool>,
    cache: RefCell<CursorCache<ImageKey, CustomCursor>>,
    last_applied: RefCell<Option<Cursor>>,
}

impl State {
    fn schedule(self: &Rc<Self>) {
        if self.pending.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        self.timer
            .start(TimerMode::SingleShot, Duration::ZERO, move || {
                if let Some(state) = weak.upgrade()
                    && let Some(ui) = state.ui.upgrade()
                {
                    // about_to_wait also consumes pending directly. This wake covers
                    // feedback changed by another timer while the pointer is still.
                    ui.window().request_redraw();
                }
            });
    }

    fn flush(&self, event_loop: &ActiveEventLoop) {
        if !self.pending.replace(false) {
            return;
        }
        self.timer.stop();
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        if ui.get_background_blocked() && !self.suspended.get() {
            // A modal can appear under a stationary pointer. Do not leave an
            // export cursor over that new surface while waiting for mouse input.
            ui.window().with_winit_window(|window| {
                window.set_cursor_visible(true);
                window.set_cursor(CursorIcon::Default);
            });
            self.last_applied.borrow_mut().take();
            self.fallback_hidden.set(false);
            return;
        }
        let kind = ui.get_wave_pointer_kind();
        if self.suspended.get() || !self.pointer_in_window.get() || kind == 11 {
            self.last_applied.borrow_mut().take();
            return;
        }
        if kind == 0 {
            // Input events already invalidate last_applied and let Slint pick
            // the next control's stock cursor. A loss of ownership without input
            // (for example a presenter switch) must also release our old image.
            let restore_fallback = self.fallback_hidden.replace(false);
            if self.last_applied.borrow_mut().take().is_some() || restore_fallback {
                ui.window().with_winit_window(|window| {
                    window.set_cursor_visible(true);
                    window.set_cursor(CursorIcon::Default);
                });
            }
            return;
        }
        let Some(window_scale) = ui
            .window()
            .with_winit_window(|window| window.scale_factor())
        else {
            ui.set_wave_native_cursor(false);
            return;
        };
        let cursor = match kind {
            1..=8 => {
                // Pointer size follows the OS DPI, independently of GUI zoom.
                let scale = ScaleKey::new(window_scale);
                let hotspot_x = finite_hotspot(ui.get_wave_pointer_hotspot_x());
                let hotspot_y = finite_hotspot(ui.get_wave_pointer_hotspot_y());
                let key = ImageKey {
                    image: ui.get_wave_pointer_image(),
                    hotspot_x: hotspot_x.to_bits(),
                    hotspot_y: hotspot_y.to_bits(),
                };
                let cached = {
                    let mut cache = self.cache.borrow_mut();
                    cache.prepare_scale(scale);
                    cache.get(kind, &key).cloned()
                };
                let custom = cached.or_else(|| {
                    let raster = rasterize_svg(cursor_svg(kind)?, hotspot_x, hotspot_y, scale)?;
                    let source = CustomCursor::from_rgba(
                        raster.rgba,
                        raster.width,
                        raster.height,
                        raster.hotspot_x,
                        raster.hotspot_y,
                    )
                    .ok()?;
                    let cursor = event_loop.create_custom_cursor(source);
                    // A cache miss is bounded by the eight kinds at the current
                    // DPI; pointer motion itself does not emit cursor logs.
                    tracing::debug!(target: "audiolib_cursor", kind, window_scale,
                        width = raster.width, height = raster.height,
                        hotspot_x = raster.hotspot_x, hotspot_y = raster.hotspot_y,
                        "Created Waveform system cursor");
                    self.cache.borrow_mut().insert(kind, key, cursor.clone());
                    Some(cursor)
                });
                let Some(custom) = custom else {
                    ui.set_wave_native_cursor(false);
                    self.fallback_hidden.set(true);
                    self.last_applied.borrow_mut().take();
                    // The same SVG remains painted by Slint on this fallback.
                    ui.window()
                        .with_winit_window(|window| window.set_cursor_visible(false));
                    return;
                };
                Cursor::Custom(custom)
            }
            9 => CursorIcon::Pointer.into(),
            10 => CursorIcon::Default.into(),
            _ => return,
        };
        let changed = self.last_applied.borrow().as_ref() != Some(&cursor)
            || self.fallback_hidden.replace(false);
        if changed {
            ui.window().with_winit_window(|window| {
                window.set_cursor_visible(true);
                window.set_cursor(cursor.clone());
            });
            *self.last_applied.borrow_mut() = Some(cursor);
        }
        if !ui.get_wave_native_cursor() {
            ui.set_wave_native_cursor(true);
        }
    }
}

/// Register feedback on the main UI window; headless backends keep paint fallback.
pub(super) fn install(ui: &AppWindow) {
    ui.set_wave_native_cursor(false);
    let state = Rc::new(State {
        ui: ui.as_weak(),
        timer: Timer::default(),
        pending: Cell::new(false),
        suspended: Cell::new(false),
        pointer_in_window: Cell::new(false),
        fallback_hidden: Cell::new(false),
        cache: RefCell::new(CursorCache::default()),
        last_applied: RefCell::new(None),
    });
    CURRENT.with(|current| *current.borrow_mut() = Rc::downgrade(&state));
    let callback_state = state.clone();
    ui.on_wave_pointer_feedback_changed(move || callback_state.schedule());
    state.schedule();
}

/// Supply this handler to BackendSelector before the first Slint window is made.
/// It propagates every event and preserves the selected backend and renderer.
pub(super) fn application_handler() -> impl CustomApplicationHandler {
    ApplicationHandler
}

struct ApplicationHandler;

impl CustomApplicationHandler for ApplicationHandler {
    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        window_id: WindowId,
        winit_window: Option<&slint::winit_030::winit::window::Window>,
        _slint_window: Option<&slint::Window>,
        event: &WindowEvent,
    ) -> EventResult {
        if let Some(state) = current()
            && let Some(ui) = state.ui.upgrade()
            && ui.window().with_winit_window(|window| window.id()) == Some(window_id)
        {
            let refresh = match event {
                WindowEvent::CursorEntered { .. } => {
                    state.pointer_in_window.set(true);
                    true
                }
                WindowEvent::CursorMoved { position, .. } => {
                    let inside = winit_window.is_some_and(|window| {
                        let size = window.inner_size();
                        position.x >= 0.0
                            && position.y >= 0.0
                            && position.x < f64::from(size.width)
                            && position.y < f64::from(size.height)
                    });
                    state.pointer_in_window.set(inside);
                    true
                }
                WindowEvent::CursorLeft { .. } => {
                    state.pointer_in_window.set(false);
                    true
                }
                WindowEvent::MouseInput { .. } | WindowEvent::ScaleFactorChanged { .. } => true,
                _ => false,
            };
            if refresh {
                // Slint may apply its stock Crosshair during this event even
                // when the latest custom-image kind has not changed.
                state.last_applied.borrow_mut().take();
                state.schedule();
            }
        }
        EventResult::Propagate
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) -> EventResult {
        // Slint flushes coalesced pointer moves before invoking this hook.
        if let Some(state) = current() {
            state.flush(event_loop);
        }
        EventResult::Propagate
    }
}

/// Refresh cursor feedback after a window DPI or application-scale change.
/// Application-scale changes reuse the same DPI-sized native cursor pixels.
pub(super) fn scale_factor_changed() {
    if let Some(state) = current() {
        state.last_applied.borrow_mut().take();
        state.schedule();
    }
}

/// Restore a visible stock cursor immediately before entering native drag-and-drop.
/// Pending feedback cannot overwrite OLE's copy/no-drop feedback while suspended.
pub(super) fn suspend_for_native_drag(ui: &AppWindow) {
    tracing::debug!(target: "audiolib_cursor", "Native drag owns the system cursor");
    if let Some(state) = current() {
        state.suspended.set(true);
        state.pending.set(false);
        state.timer.stop();
        state.last_applied.borrow_mut().take();
        state.fallback_hidden.set(false);
    }
    ui.window().with_winit_window(|window| {
        window.set_cursor_visible(true);
        window.set_cursor(CursorIcon::Default);
    });
}

/// Re-read the current UI feedback after native drag-and-drop has returned.
pub(super) fn resume_after_native_drag(_ui: &AppWindow) {
    tracing::debug!(target: "audiolib_cursor", "Restoring Waveform pointer feedback after native drag");
    if let Some(state) = current() {
        state.suspended.set(false);
        state.last_applied.borrow_mut().take();
        state.schedule();
    }
}

fn finite_hotspot(value: f32) -> f32 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}

#[derive(Debug, PartialEq)]
struct Raster {
    rgba: Vec<u8>,
    width: u16,
    height: u16,
    hotspot_x: u16,
    hotspot_y: u16,
}

fn cursor_svg(kind: i32) -> Option<&'static str> {
    Some(match kind {
        1 => include_str!("../ui/icons/wave-cursor-crosshair.svg"),
        2 => include_str!("../ui/icons/wave-cursor-select-left.svg"),
        3 => include_str!("../ui/icons/wave-cursor-select-right.svg"),
        4 => include_str!("../ui/icons/wave-cursor-resize-start.svg"),
        5 => include_str!("../ui/icons/wave-cursor-resize-end.svg"),
        6 => include_str!("../ui/icons/wave-cursor-hand-open.svg"),
        7 => include_str!("../ui/icons/wave-cursor-hand-closed.svg"),
        8 => include_str!("../ui/icons/wave-cursor-hand-wait.svg"),
        _ => return None,
    })
}

fn rasterize_svg(source: &str, hotspot_x: f32, hotspot_y: f32, scale: ScaleKey) -> Option<Raster> {
    let side = (f64::from(LOGICAL_SIDE) * scale.factor())
        .round()
        .clamp(1.0, f64::from(MAX_SIDE)) as u16;
    // The outer SVG's intrinsic size is the final physical pixel size. Slint's
    // public to_rgba8 renders that vector tree directly at this size; it never
    // enlarges a previously rasterized 20-pixel bitmap. The original SVG stays
    // a nested viewport, sharing the exact asset used by the paint fallback.
    let sized = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{side}\" height=\"{side}\" viewBox=\"0 0 {LOGICAL_SIDE} {LOGICAL_SIDE}\">{source}</svg>"
    );
    let pixels = Image::load_from_svg_data(sized.as_bytes())
        .ok()?
        .to_rgba8()?;
    if pixels.width() != u32::from(side) || pixels.height() != u32::from(side) {
        return None;
    }
    Some(Raster {
        rgba: pixels.as_bytes().to_vec(),
        width: side,
        height: side,
        hotspot_x: (f64::from(finite_hotspot(hotspot_x)) * f64::from(side)
            / f64::from(LOGICAL_SIDE))
        .round()
        .clamp(0.0, f64::from(side - 1)) as u16,
        hotspot_y: (f64::from(finite_hotspot(hotspot_y)) * f64::from(side)
            / f64::from(LOGICAL_SIDE))
        .round()
        .clamp(0.0, f64::from(side - 1)) as u16,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vector_assets_have_dpi_sized_pixels_and_matching_hotspots() {
        for (dpi, side, hotspot) in [
            (1.0, 20, 10),
            (1.25, 25, 13),
            (1.5, 30, 15),
            (2.0, 40, 20),
            (3.0, 60, 30),
        ] {
            for kind in 1..=8 {
                let raster =
                    rasterize_svg(cursor_svg(kind).unwrap(), 10.0, 10.0, ScaleKey::new(dpi))
                        .unwrap();
                assert_eq!(
                    (raster.width, raster.height),
                    (side, side),
                    "DPI={dpi}, kind={kind}"
                );
                assert_eq!(
                    (raster.hotspot_x, raster.hotspot_y),
                    (hotspot, hotspot),
                    "DPI={dpi}, kind={kind}"
                );
                assert_eq!(raster.rgba.len(), usize::from(side) * usize::from(side) * 4);
                assert!(raster.rgba.chunks_exact(4).any(|pixel| pixel[3] > 0));
            }
        }
    }

    #[test]
    fn a_one_pixel_vector_line_is_sharp_at_double_dpi() {
        let source = r#"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="20" viewBox="0 0 20 20"><path d="M9.5 2V18" stroke="white" stroke-width="1"/></svg>"#;
        let raster = rasterize_svg(source, 10.0, 10.0, ScaleKey::new(2.0)).unwrap();
        let alpha: Vec<u8> = (0..raster.width)
            .map(|x| raster.rgba[(20 * usize::from(raster.width) + usize::from(x)) * 4 + 3])
            .collect();
        // Direct vector sampling covers exactly two opaque columns. Enlarging
        // the 20-pixel raster bilinearly would instead produce four soft ones.
        assert!(alpha[..18].iter().all(|&value| value == 0));
        assert_eq!(&alpha[18..20], &[255, 255]);
        assert!(alpha[20..].iter().all(|&value| value == 0));
    }

    #[test]
    fn malformed_svg_and_extreme_dpi_are_bounded() {
        assert!(rasterize_svg("<svg><broken>", 0.0, 0.0, ScaleKey::new(1.0)).is_none());
        assert!(cursor_svg(0).is_none());
        assert!(cursor_svg(11).is_none());
        for (dpi, expected) in [
            (f64::MIN, 10),
            (0.0, 10),
            (f64::MAX, 160),
            (f64::NAN, 20),
            (f64::INFINITY, 20),
            (f64::NEG_INFINITY, 20),
        ] {
            let raster = rasterize_svg(
                cursor_svg(1).unwrap(),
                f32::INFINITY,
                1000.0,
                ScaleKey::new(dpi),
            )
            .unwrap();
            assert_eq!((raster.width, raster.height), (expected, expected));
            assert_eq!((raster.hotspot_x, raster.hotspot_y), (0, expected - 1));
            assert_eq!(
                raster.rgba.len(),
                usize::from(expected) * usize::from(expected) * 4
            );
        }
    }

    #[test]
    fn cache_drops_previous_scale_handles_and_replaces_changed_images() {
        struct Handle(Rc<Cell<usize>>);
        impl Drop for Handle {
            fn drop(&mut self) {
                self.0.set(self.0.get() - 1);
            }
        }
        let live = Rc::new(Cell::new(0));
        let mut cache = CursorCache::default();
        for step in 64..100 {
            cache.prepare_scale(ScaleKey(step));
            assert_eq!(live.get(), 0);
            for kind in 1..=8 {
                live.set(live.get() + 1);
                cache.insert(kind, 1, Handle(live.clone()));
            }
            assert_eq!(live.get(), 8);
            assert_eq!(cache.entries.len(), 8);
            live.set(live.get() + 1);
            cache.insert(1, 2, Handle(live.clone()));
            assert!(cache.get(1, &1).is_none());
            assert!(cache.get(1, &2).is_some());
            assert_eq!(live.get(), 8);
        }
        drop(cache);
        assert_eq!(live.get(), 0);
    }
}
