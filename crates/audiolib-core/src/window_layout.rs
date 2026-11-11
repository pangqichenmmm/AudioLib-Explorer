// crates/audiolib-core/src/window_layout.rs
//! Persisted desktop window geometry and pane layout (client-only UI state).
//!
//! Every field is optional and parsed leniently: a missing, malformed or
//! out-of-range value only resets that field to the default layout, and never
//! discards the other preferences in `settings.json`.

use serde::{Deserialize, Serialize};

/// Smallest restored window width in logical pixels.
pub const WINDOW_MIN_WIDTH: f32 = 480.0;
/// Smallest restored window height in logical pixels.
pub const WINDOW_MIN_HEIGHT: f32 = 360.0;
/// Largest restored window edge in logical pixels.
pub const WINDOW_MAX_EDGE: f32 = 16384.0;
/// Saved window positions beyond this physical-pixel magnitude are discarded.
pub const WINDOW_MAX_COORDINATE: i64 = 100_000;
/// Library sidebar width range in unscaled logical pixels (interface scale 100%).
pub const SIDEBAR_WIDTH_RANGE: (f32, f32) = (120.0, 800.0);
/// Details panel width range in unscaled logical pixels (interface scale 100%).
pub const INSPECTOR_WIDTH_RANGE: (f32, f32) = (160.0, 1200.0);
/// Folder list share of the combined expanded sidebar list height.
pub const FOLDERS_SECTION_RATIO_RANGE: (f32, f32) = (0.1, 0.9);

/// Desktop window and pane layout restored on the next launch.
///
/// The window size is in logical pixels and the position in physical (virtual
/// screen) pixels, matching what the windowing system reports. Pane widths are
/// stored at interface scale 100% so a later scale change keeps their proportions.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct WindowLayoutPreferences {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub y: Option<i32>,
    pub maximized: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sidebar_width: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inspector_width: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inspector_visible: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filters_visible: Option<bool>,
    /// Whether the folder tree section is expanded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folders_section_expanded: Option<bool>,
    /// Whether the library/album section is expanded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub libraries_section_expanded: Option<bool>,
    /// Folder list share while both sections are expanded, independent of UI scale.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folders_section_ratio: Option<f32>,
}

impl<'de> Deserialize<'de> for WindowLayoutPreferences {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self::from_value(&serde_json::Value::deserialize(
            deserializer,
        )?))
    }
}

impl WindowLayoutPreferences {
    /// Parse leniently: a non-object yields the default layout, and each invalid
    /// field is dropped individually. The result is already [`normalized`](Self::normalized).
    pub fn from_value(value: &serde_json::Value) -> Self {
        let Some(object) = value.as_object() else {
            return Self::default();
        };
        let number = |key: &str| {
            object
                .get(key)
                .and_then(serde_json::Value::as_f64)
                .filter(|value| value.is_finite())
        };
        let coordinate = |key: &str| {
            number(key)
                .map(f64::round)
                .filter(|value| value.abs() <= WINDOW_MAX_COORDINATE as f64)
                .map(|value| value as i32)
        };
        let flag = |key: &str| object.get(key).and_then(serde_json::Value::as_bool);
        Self {
            width: number("width").map(|value| value as f32),
            height: number("height").map(|value| value as f32),
            x: coordinate("x"),
            y: coordinate("y"),
            maximized: flag("maximized").unwrap_or(false),
            sidebar_width: number("sidebar_width").map(|value| value as f32),
            inspector_width: number("inspector_width").map(|value| value as f32),
            inspector_visible: flag("inspector_visible"),
            filters_visible: flag("filters_visible"),
            folders_section_expanded: flag("folders_section_expanded"),
            libraries_section_expanded: flag("libraries_section_expanded"),
            folders_section_ratio: number("folders_section_ratio").map(|value| value as f32),
        }
        .normalized()
    }

    /// Clamp sizes into their supported ranges, drop non-finite values and keep a
    /// position only as a complete x/y pair.
    pub fn normalized(&self) -> Self {
        let edge = |value: Option<f32>, minimum: f32| {
            value
                .filter(|value| value.is_finite())
                .map(|value| value.clamp(minimum, WINDOW_MAX_EDGE))
        };
        let pane = |value: Option<f32>, (minimum, maximum): (f32, f32)| {
            value
                .filter(|value| value.is_finite())
                .map(|value| value.clamp(minimum, maximum))
        };
        let within = |value: Option<i32>| {
            value.filter(|value| i64::from(*value).abs() <= WINDOW_MAX_COORDINATE)
        };
        let (x, y) = match (within(self.x), within(self.y)) {
            (Some(x), Some(y)) => (Some(x), Some(y)),
            _ => (None, None),
        };
        Self {
            width: edge(self.width, WINDOW_MIN_WIDTH),
            height: edge(self.height, WINDOW_MIN_HEIGHT),
            x,
            y,
            maximized: self.maximized,
            sidebar_width: pane(self.sidebar_width, SIDEBAR_WIDTH_RANGE),
            inspector_width: pane(self.inspector_width, INSPECTOR_WIDTH_RANGE),
            inspector_visible: self.inspector_visible,
            filters_visible: self.filters_visible,
            folders_section_expanded: self.folders_section_expanded,
            libraries_section_expanded: self.libraries_section_expanded,
            folders_section_ratio: pane(self.folders_section_ratio, FOLDERS_SECTION_RATIO_RANGE),
        }
    }
}

/// A monitor (or work area) rectangle in physical virtual-screen pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScreenRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl ScreenRect {
    fn intersection_area(&self, other: &ScreenRect) -> i64 {
        let left = i64::from(self.x).max(i64::from(other.x));
        let top = i64::from(self.y).max(i64::from(other.y));
        let right = (i64::from(self.x) + i64::from(self.width))
            .min(i64::from(other.x) + i64::from(other.width));
        let bottom = (i64::from(self.y) + i64::from(self.height))
            .min(i64::from(other.y) + i64::from(other.height));
        (right - left).max(0) * (bottom - top).max(0)
    }
}

/// Height of the title-bar strip that must stay reachable, in physical pixels.
const GRAB_HEIGHT: u32 = 32;
/// Minimum visible title-bar width that keeps a window draggable, in physical pixels.
const GRAB_WIDTH: u32 = 96;

/// Fit a saved window onto the current monitors.
///
/// Returns the size clamped to the chosen monitor and a position that keeps the
/// title bar reachable. A window whose title bar is visible on some monitor keeps
/// its position; otherwise it moves onto the monitor it overlaps most (or the
/// first monitor) and is clamped inside it. Without monitor information the
/// position is `None`, letting the system place the window.
pub fn fit_window_to_monitors(
    window: ScreenRect,
    monitors: &[ScreenRect],
) -> (Option<(i32, i32)>, (u32, u32)) {
    let monitors: Vec<_> = monitors
        .iter()
        .filter(|monitor| monitor.width > 0 && monitor.height > 0)
        .copied()
        .collect();
    let Some(first) = monitors.first().copied() else {
        return (None, (window.width, window.height));
    };
    let grab = ScreenRect {
        height: window.height.min(GRAB_HEIGHT),
        ..window
    };
    let grab_needed = i64::from(window.width.min(GRAB_WIDTH)) * i64::from(grab.height.max(1));
    let reachable = monitors
        .iter()
        .map(|monitor| monitor.intersection_area(&grab))
        .sum::<i64>()
        >= grab_needed.max(1);
    let target = monitors
        .iter()
        .copied()
        .max_by_key(|monitor| monitor.intersection_area(&window))
        .filter(|monitor| monitor.intersection_area(&window) > 0)
        .unwrap_or(first);
    let size = (
        window.width.min(target.width),
        window.height.min(target.height),
    );
    if reachable {
        return (Some((window.x, window.y)), size);
    }
    let clamp = |position: i32, origin: i32, extent: u32, length: u32| {
        let maximum = i64::from(origin) + i64::from(extent) - i64::from(length);
        i64::from(position).clamp(i64::from(origin), maximum.max(i64::from(origin))) as i32
    };
    (
        Some((
            clamp(window.x, target.x, target.width, size.0),
            clamp(window.y, target.y, target.height, size.1),
        )),
        size,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const PRIMARY: ScreenRect = ScreenRect {
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
    };
    const LEFT: ScreenRect = ScreenRect {
        x: -2560,
        y: -200,
        width: 2560,
        height: 1440,
    };

    fn rect(x: i32, y: i32, width: u32, height: u32) -> ScreenRect {
        ScreenRect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn round_trip_preserves_a_complete_layout() {
        let layout = WindowLayoutPreferences {
            width: Some(1280.0),
            height: Some(800.0),
            x: Some(-1800),
            y: Some(40),
            maximized: true,
            sidebar_width: Some(240.0),
            inspector_width: Some(360.0),
            inspector_visible: Some(false),
            filters_visible: Some(true),
            folders_section_expanded: Some(false),
            libraries_section_expanded: Some(true),
            folders_section_ratio: Some(0.7),
        };
        let text = serde_json::to_string(&layout).unwrap();
        let parsed: WindowLayoutPreferences = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, layout);
    }

    #[test]
    fn default_layout_serializes_only_the_maximized_flag() {
        let value = serde_json::to_value(WindowLayoutPreferences::default()).unwrap();
        assert_eq!(value, json!({"maximized": false}));
        let parsed: WindowLayoutPreferences = serde_json::from_value(value).unwrap();
        assert_eq!(parsed, WindowLayoutPreferences::default());
    }

    #[test]
    fn invalid_fields_are_dropped_individually() {
        let parsed = WindowLayoutPreferences::from_value(&json!({
            "width": "wide",
            "height": 720,
            "x": 10,
            "y": null,
            "maximized": "yes",
            "sidebar_width": 260.5,
            "inspector_width": [1],
            "inspector_visible": 1,
            "filters_visible": true,
            "unknown": {"future": true}
        }));
        assert_eq!(
            parsed,
            WindowLayoutPreferences {
                width: None,
                height: Some(720.0),
                x: None,
                y: None,
                maximized: false,
                sidebar_width: Some(260.5),
                inspector_width: None,
                inspector_visible: None,
                filters_visible: Some(true),
                ..Default::default()
            }
        );
    }

    #[test]
    fn sidebar_sections_default_independently_and_clamp_the_saved_split() {
        let older = WindowLayoutPreferences::from_value(&json!({"sidebar_width": 260}));
        assert_eq!(older.folders_section_expanded, None);
        assert_eq!(older.libraries_section_expanded, None);
        assert_eq!(older.folders_section_ratio, None);

        let parsed = WindowLayoutPreferences::from_value(&json!({
            "sidebar_width": 260,
            "folders_section_expanded": false,
            "libraries_section_expanded": "yes",
            "folders_section_ratio": -1
        }));
        assert_eq!(parsed.sidebar_width, Some(260.0));
        assert_eq!(parsed.folders_section_expanded, Some(false));
        assert_eq!(parsed.libraries_section_expanded, None);
        assert_eq!(
            parsed.folders_section_ratio,
            Some(FOLDERS_SECTION_RATIO_RANGE.0)
        );

        for ratio in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let normalized = WindowLayoutPreferences {
                folders_section_ratio: Some(ratio),
                ..Default::default()
            }
            .normalized();
            assert_eq!(normalized.folders_section_ratio, None);
        }
        let too_large = WindowLayoutPreferences::from_value(&json!({
            "folders_section_ratio": 10
        }));
        assert_eq!(
            too_large.folders_section_ratio,
            Some(FOLDERS_SECTION_RATIO_RANGE.1)
        );
    }

    #[test]
    fn non_object_values_use_the_default_layout() {
        for value in [json!(null), json!("layout"), json!(42), json!([1, 2])] {
            assert_eq!(
                WindowLayoutPreferences::from_value(&value),
                WindowLayoutPreferences::default()
            );
        }
    }

    #[test]
    fn sizes_clamp_and_absurd_positions_are_discarded() {
        let parsed = WindowLayoutPreferences::from_value(&json!({
            "width": 10, "height": 1.0e9, "x": 5.0e9, "y": 3,
            "sidebar_width": 1, "inspector_width": 99999
        }));
        assert_eq!(parsed.width, Some(WINDOW_MIN_WIDTH));
        assert_eq!(parsed.height, Some(WINDOW_MAX_EDGE));
        assert_eq!((parsed.x, parsed.y), (None, None));
        assert_eq!(parsed.sidebar_width, Some(SIDEBAR_WIDTH_RANGE.0));
        assert_eq!(parsed.inspector_width, Some(INSPECTOR_WIDTH_RANGE.1));
        let manual = WindowLayoutPreferences {
            width: Some(f32::NAN),
            height: Some(f32::INFINITY),
            x: Some(1),
            y: None,
            ..Default::default()
        }
        .normalized();
        assert_eq!((manual.width, manual.height), (None, None));
        assert_eq!((manual.x, manual.y), (None, None));
    }

    #[test]
    fn visible_window_keeps_its_position() {
        let window = rect(100, 80, 1280, 800);
        assert_eq!(
            fit_window_to_monitors(window, &[PRIMARY]),
            (Some((100, 80)), (1280, 800))
        );
        // A window on a secondary monitor left of and above the primary stays there.
        let left = rect(-2400, -150, 1600, 900);
        assert_eq!(
            fit_window_to_monitors(left, &[PRIMARY, LEFT]),
            (Some((-2400, -150)), (1600, 900))
        );
        // Partly off the right edge but with a grabbable title bar: left alone.
        let partial = rect(1700, 100, 1280, 800);
        assert_eq!(
            fit_window_to_monitors(partial, &[PRIMARY]),
            (Some((1700, 100)), (1280, 800))
        );
    }

    #[test]
    fn offscreen_window_moves_onto_a_monitor() {
        // Saved on a monitor that is now disconnected.
        let window = rect(-2400, -150, 1600, 900);
        assert_eq!(
            fit_window_to_monitors(window, &[PRIMARY]),
            (Some((0, 0)), (1600, 900))
        );
        // Far below every monitor.
        let below = rect(300, 5000, 800, 600);
        assert_eq!(
            fit_window_to_monitors(below, &[PRIMARY]),
            (Some((300, 480)), (800, 600))
        );
        // Title bar above the top edge while the body is visible.
        let above = rect(200, -500, 800, 600);
        assert_eq!(
            fit_window_to_monitors(above, &[PRIMARY]),
            (Some((200, 0)), (800, 600))
        );
    }

    #[test]
    fn oversized_window_shrinks_to_its_monitor() {
        let window = rect(50, 50, 4000, 3000);
        assert_eq!(
            fit_window_to_monitors(window, &[PRIMARY]),
            (Some((50, 50)), (1920, 1080))
        );
        let offscreen = rect(9000, 9000, 4000, 3000);
        assert_eq!(
            fit_window_to_monitors(offscreen, &[PRIMARY]),
            (Some((0, 0)), (1920, 1080))
        );
    }

    #[test]
    fn missing_monitor_information_lets_the_system_place_the_window() {
        let window = rect(-5000, -5000, 1280, 800);
        assert_eq!(fit_window_to_monitors(window, &[]), (None, (1280, 800)));
        assert_eq!(
            fit_window_to_monitors(window, &[rect(0, 0, 0, 0)]),
            (None, (1280, 800))
        );
    }
}
