// app/audiolib-desktop/src/theme.rs
//! Appearance themes and font sizes. Built-in definitions live in core so settings
//! validation, bilingual names and presentation selection share one registry.
//! Font sizes are whole pixels; `Theme.font()` / `Theme.list-font()` combine them
//! with the interface scale.
use crate::AppWindow;
use audiolib_core::config::{
    BUILTIN_THEMES, DEFAULT_THEME, PlayerStyle, THEME_IDS, clamp_font_size, normalize_theme,
};
use slint::{Color, ComponentHandle, Image, ModelRc, SharedString, VecModel};

/// Color tokens of one theme, as `0xAARRGGBB`.
pub(crate) struct Palette {
    pub bg: u32,
    pub panel: u32,
    pub raised: u32,
    pub hover: u32,
    pub border: u32,
    pub strong_border: u32,
    pub ink: u32,
    pub dim: u32,
    pub muted: u32,
    pub accent: u32,
    pub accent_hover: u32,
    pub mint: u32,
    pub apricot: u32,
    pub coral: u32,
    pub on_accent: u32,
    pub selected: u32,
    pub checked: u32,
    pub alternate: u32,
    pub warning: u32,
    pub trough: u32,
    pub segment: u32,
    pub danger: u32,
    pub danger_surface: u32,
    pub player: u32,
    pub player_border: u32,
    pub tooltip: u32,
    pub overlay: u32,
}

/// The original soft charcoal palette; identical to the defaults in `ui/theme.slint`.
pub(crate) const DEFAULT_PALETTE: Palette = Palette {
    bg: 0xff18_1b20,
    panel: 0xff20_242a,
    raised: 0xff2a_3037,
    hover: 0xff35_3d45,
    border: 0xff35_3d45,
    strong_border: 0xff69_7581,
    ink: 0xffd9_dee3,
    dim: 0xffaa_b2bd,
    muted: 0xff95_a0ac,
    accent: 0xff8f_aea6,
    accent_hover: 0xffa8_c3bb,
    mint: 0xff8f_aea6,
    apricot: 0xffc3_b48e,
    coral: 0xffcf_9696,
    on_accent: 0xff20_2b28,
    selected: 0xff2b_3a38,
    checked: 0xff35_4744,
    alternate: 0xff22_282e,
    warning: 0xffc3_b48e,
    trough: 0xff15_181c,
    segment: 0xff3a_434c,
    danger: 0xffcf_9696,
    danger_surface: 0xff3a_2a2d,
    player: 0xff23_2a30,
    player_border: 0xff46_5059,
    tooltip: 0xff14_171c,
    overlay: 0xa800_0000,
};

/// The PR #185 workbench palette, retained as the opt-in Studio theme.
pub(crate) const STUDIO_PALETTE: Palette = Palette {
    bg: 0xff20_2226,
    panel: 0xff24_272b,
    raised: 0xff39_3c43,
    hover: 0xff34_383e,
    border: 0xff38_3c43,
    strong_border: 0xff62_6873,
    ink: 0xffec_ebe7,
    dim: 0xffad_b0b7,
    muted: 0xff85_8b94,
    accent: 0xff2b_bfbd,
    accent_hover: 0xff66_d2c9,
    mint: 0xff79_d5bc,
    apricot: 0xffef_c4a5,
    coral: 0xffea_8b75,
    on_accent: 0xff10_2a2a,
    selected: 0xff29_3e40,
    checked: 0xff30_3c3e,
    alternate: 0xff27_2a2f,
    warning: 0xffd6_ba98,
    trough: 0xff1e_2024,
    segment: 0xff3b_4148,
    danger: 0xffdf_9e98,
    danger_surface: 0xff40_3033,
    player: 0xff29_2c31,
    player_border: 0xff41_454c,
    tooltip: 0xff19_1b1f,
    overlay: 0xa800_0000,
};

/// Appearance combo index for a theme identifier; unknown themes select the default.
pub(crate) fn theme_index(id: &str) -> i32 {
    let id = normalize_theme(id);
    THEME_IDS
        .iter()
        .position(|candidate| *candidate == id)
        .unwrap_or(0) as i32
}

/// Theme identifier for an Appearance combo index; unknown indices use the default.
pub(crate) fn theme_id(index: i32) -> String {
    usize::try_from(index)
        .ok()
        .and_then(|position| THEME_IDS.get(position))
        .map_or_else(|| DEFAULT_THEME.to_owned(), |id| (*id).to_owned())
}

/// List and apply a complete theme (palette and presentation) on the UI thread.
pub(crate) fn apply_theme(ui: &AppWindow, id: &str) {
    let names = |chinese: bool| {
        let names = BUILTIN_THEMES
            .iter()
            .map(|entry| SharedString::from(if chinese { entry.name_cn } else { entry.name }))
            .collect::<Vec<_>>();
        ModelRc::new(VecModel::from(names))
    };
    ui.set_theme_names(names(false));
    ui.set_theme_names_cn(names(true));
    let id = normalize_theme(id);
    let index = theme_index(&id);
    ui.set_theme_index(index);
    let entry = &BUILTIN_THEMES[index as usize];
    // A new design gets its own palette and presentation. Preserve the original
    // default, while spectral frequency colors remain independent audio data.
    let palette = match entry.player_style {
        PlayerStyle::Classic => &DEFAULT_PALETTE,
        PlayerStyle::Studio => &STUDIO_PALETTE,
    };
    let theme = ui.global::<crate::Theme>();
    let modern_style = entry.player_style == PlayerStyle::Studio;
    let presentation_changed = theme.get_modern_style() != modern_style;
    if presentation_changed {
        ui.invoke_prepare_workbench_switch();
        // Clear transient events before Slint creates the new presenter and binds
        // its change trackers. Clearing on a deferred style-changed handler is late.
        ui.invoke_clear_audition_input_feedback();
    }
    if modern_style {
        install_studio_surfaces(&theme);
    }
    theme.set_modern_style(modern_style);
    let color = Color::from_argb_encoded;
    theme.set_bg(color(palette.bg));
    theme.set_panel(color(palette.panel));
    theme.set_raised(color(palette.raised));
    theme.set_hover(color(palette.hover));
    theme.set_border(color(palette.border));
    theme.set_strong_border(color(palette.strong_border));
    theme.set_ink(color(palette.ink));
    theme.set_dim(color(palette.dim));
    theme.set_muted(color(palette.muted));
    theme.set_accent(color(palette.accent));
    theme.set_accent_hover(color(palette.accent_hover));
    theme.set_mint(color(palette.mint));
    theme.set_apricot(color(palette.apricot));
    theme.set_coral(color(palette.coral));
    theme.set_on_accent(color(palette.on_accent));
    theme.set_selected(color(palette.selected));
    theme.set_checked(color(palette.checked));
    theme.set_alternate(color(palette.alternate));
    theme.set_warning(color(palette.warning));
    theme.set_trough(color(palette.trough));
    theme.set_segment(color(palette.segment));
    theme.set_danger(color(palette.danger));
    theme.set_danger_surface(color(palette.danger_surface));
    theme.set_player(color(palette.player));
    theme.set_player_border(color(palette.player_border));
    theme.set_tooltip(color(palette.tooltip));
    theme.set_overlay(color(palette.overlay));
    if presentation_changed {
        ui.invoke_restore_workbench_switch();
    }
}

/// Studio surface artwork in the order of the `Theme.studio-*` images.
const STUDIO_SURFACES: [&str; 7] = [
    include_str!("../ui/surfaces/toolbar.svg"),
    include_str!("../ui/surfaces/rail.svg"),
    include_str!("../ui/surfaces/library.svg"),
    include_str!("../ui/surfaces/inspector.svg"),
    include_str!("../ui/surfaces/player.svg"),
    include_str!("../ui/surfaces/transport.svg"),
    include_str!("../ui/surfaces/tab-indicator.svg"),
];

thread_local! {
    /// Rasterized once per UI thread; images share their pixels when cloned.
    static STUDIO_SURFACE_IMAGES: Vec<Image> =
        STUDIO_SURFACES.iter().map(|svg| rasterize_surface(svg)).collect();
}

/// Give the Studio backdrops bitmaps. The software renderer has no cache for
/// rasterized SVG: an SVG backdrop is rasterized at its full drawn size on every
/// repaint that touches it, which cost a whole frame budget per playback frame.
/// Bitmaps are only sampled inside the repainted region.
fn install_studio_surfaces(theme: &crate::Theme) {
    STUDIO_SURFACE_IMAGES.with(|images| {
        let [
            toolbar,
            rail,
            library,
            inspector,
            player,
            transport,
            tab_indicator,
        ] = images.as_slice()
        else {
            return;
        };
        theme.set_studio_toolbar(toolbar.clone());
        theme.set_studio_rail(rail.clone());
        theme.set_studio_library(library.clone());
        theme.set_studio_inspector(inspector.clone());
        theme.set_studio_player(player.clone());
        theme.set_studio_transport(transport.clone());
        theme.set_studio_tab_indicator(tab_indicator.clone());
    });
}

/// Rasterize one surface at twice its design size, so typical HiDPI windows
/// scale it down or barely up. The surfaces are smooth gradients that stretch
/// (`preserveAspectRatio="none"`), so a fixed raster keeps their look.
fn rasterize_surface(svg: &str) -> Image {
    let doubled = double_svg_size(svg);
    Image::load_from_svg_data(doubled.as_bytes())
        .ok()
        .and_then(|image| image.to_rgba8())
        .map(Image::from_rgba8)
        .unwrap_or_default()
}

/// Double the root element's `width` and `height`; the viewBox keeps the art.
fn double_svg_size(svg: &str) -> String {
    let Some(root_end) = svg.find('>') else {
        return svg.to_owned();
    };
    let (root, rest) = svg.split_at(root_end);
    let mut root = root.to_owned();
    for attribute in [" width=\"", " height=\""] {
        let Some(start) = root.find(attribute).map(|at| at + attribute.len()) else {
            continue;
        };
        let Some(length) = root[start..].find('"') else {
            continue;
        };
        if let Ok(value) = root[start..start + length].parse::<f64>() {
            root.replace_range(start..start + length, &(value * 2.0).to_string());
        }
    }
    root + rest
}

/// Show and apply the interface and list font sizes (clamped to the supported range).
pub(crate) fn apply_font_sizes(ui: &AppWindow, ui_font: u32, list_font: u32) {
    let ui_font = clamp_font_size(i64::from(ui_font)) as i32;
    let list_font = clamp_font_size(i64::from(list_font)) as i32;
    ui.set_ui_font_px(ui_font);
    ui.set_list_font_px(list_font);
    let theme = ui.global::<crate::Theme>();
    theme.set_ui_font_px(ui_font);
    theme.set_list_font_px(list_font);
    crate::detail_presentation::relayout(ui);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn studio_surfaces_rasterize_at_twice_their_design_size() {
        assert_eq!(
            double_svg_size(
                r#"<svg xmlns="x" width="56" height="668" viewBox="0 0 100 1000"><path/></svg>"#
            ),
            r#"<svg xmlns="x" width="112" height="1336" viewBox="0 0 100 1000"><path/></svg>"#
        );
        STUDIO_SURFACE_IMAGES.with(|images| {
            assert_eq!(images.len(), STUDIO_SURFACES.len());
            let sizes: Vec<_> = images
                .iter()
                .map(|image| (image.size().width, image.size().height))
                .collect();
            assert_eq!(
                sizes,
                [
                    (3832, 112),
                    (112, 1336),
                    (416, 1336),
                    (592, 1336),
                    (3832, 256),
                    (200, 200),
                    (448, 6)
                ]
            );
            for image in images {
                assert!(image.to_rgba8().is_some(), "a surface is a bitmap");
            }
        });
    }
}
