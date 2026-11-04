// app/audiolib-desktop/src/detail_presentation.rs
//! Owned, read-only presentation of the selected record's parameters and tag pills.
//! This module neither reads source files nor submits Metadata writes.

use crate::{AppWindow, DisplayTagRow};
use audiolib_core::AudioFile;
use slint::{Model, ModelRc, SharedString, VecModel};

/// Ordered display tags, preserving the core's user/source merge and multiword labels.
pub(crate) fn labels(file: &AudioFile) -> Vec<SharedString> {
    let mut output = file
        .tags
        .split(',')
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .map(SharedString::from)
        .collect::<Vec<_>>();
    let mut seen = output
        .iter()
        .map(|tag| tag.to_lowercase())
        .collect::<std::collections::BTreeSet<_>>();
    for tag in audiolib_core::source_tags(&file.attributes) {
        if seen.insert(tag.to_lowercase()) {
            output.push(tag.into());
        }
    }
    output
}

/// Publish parameters from the same validated DTO that supplies the selected file name.
/// Unknown technical values are shown as unavailable, never as fabricated defaults.
pub(crate) fn present(ui: &AppWindow, file: &AudioFile) {
    ui.set_selected_duration(
        if file.duration.is_finite() && file.duration >= 0.0 {
            crate::controller::format_time(file.duration)
        } else {
            "—".to_owned()
        }
        .into(),
    );
    ui.set_selected_sample_rate(
        if file.sample_rate == 0 {
            "—".to_owned()
        } else {
            let rate = f64::from(file.sample_rate) / 1000.0;
            if file.sample_rate.is_multiple_of(1000) {
                format!("{rate:.0} kHz")
            } else {
                format!("{rate} kHz")
            }
        }
        .into(),
    );
    ui.set_selected_bit_depth(
        if file.bit_depth == 0 {
            "—".to_owned()
        } else {
            format!("{} bit", file.bit_depth)
        }
        .into(),
    );
    ui.set_selected_channels(
        match file.channels {
            0 => "—".to_owned(),
            1 => "Mono".to_owned(),
            2 => "Stereo".to_owned(),
            channels => format!("{channels} ch"),
        }
        .into(),
    );
    ui.set_selected_tag_labels(ModelRc::new(VecModel::from(labels(file))));
    relayout(ui);
}

/// Greedily wrap a single selected record's tags using the actual Slint text metrics.
/// Called on selection, font and pane-width changes, never from playback ticks.
pub(crate) fn relayout(ui: &AppWindow) {
    let width = ui.get_detail_tag_area_width().max(1.0);
    let spacing = ui.get_tag_chip_spacing() * 1.5;
    let mut rows: Vec<Vec<SharedString>> = Vec::new();
    let mut used = 0.0;
    for tag in ui.get_selected_tag_labels().iter().take(64) {
        ui.set_detail_tag_probe_text(tag.clone());
        let measured = ui.get_detail_tag_probe_width().min(width);
        match rows.last_mut() {
            Some(row) if used + spacing + measured <= width - 1.0 => {
                row.push(tag);
                used += spacing + measured;
            }
            _ => {
                rows.push(vec![tag]);
                used = measured;
            }
        }
    }
    ui.set_detail_tag_probe_text("".into());
    ui.set_selected_tag_rows(ModelRc::new(VecModel::from(
        rows.into_iter()
            .map(|tags| DisplayTagRow {
                tags: ModelRc::new(VecModel::from(tags)),
            })
            .collect::<Vec<_>>(),
    )));
}

/// Remove all presentation fields together when a library/selection is invalidated.
pub(crate) fn clear(ui: &AppWindow) {
    ui.set_selected_duration("".into());
    ui.set_selected_sample_rate("".into());
    ui.set_selected_bit_depth("".into());
    ui.set_selected_channels("".into());
    ui.set_selected_tag_labels(ModelRc::default());
    ui.set_selected_tag_rows(ModelRc::default());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameters_and_tags_follow_the_selected_owned_record_and_clear_together() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().unwrap();
        let mut file = AudioFile {
            duration: 1.176,
            sample_rate: 192_000,
            bit_depth: 24,
            channels: 2,
            tags: "Metal, Field recording".into(),
            attributes: serde_json::json!({"wav_metadata":{"fields":{"keywords":"metal, Chain, Foley", "category":"Metal, heavy"}}}),
            ..Default::default()
        };
        present(&ui, &file);
        assert_eq!(ui.get_selected_duration(), "00:01.176");
        assert_eq!(ui.get_selected_sample_rate(), "192 kHz");
        assert_eq!(ui.get_selected_bit_depth(), "24 bit");
        assert_eq!(ui.get_selected_channels(), "Stereo");
        assert_eq!(
            ui.get_selected_tag_labels()
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
            ["Metal", "Field recording", "Chain", "Foley", "Metal, heavy"]
        );
        file.sample_rate = 44_100;
        file.channels = 6;
        file.tags.clear();
        file.attributes = serde_json::Value::Null;
        present(&ui, &file);
        assert_eq!(ui.get_selected_sample_rate(), "44.1 kHz");
        assert_eq!(ui.get_selected_channels(), "6 ch");
        assert_eq!(ui.get_selected_tag_rows().row_count(), 0);
        file.sample_rate = 0;
        file.bit_depth = 0;
        file.channels = 0;
        file.duration = f64::NAN;
        present(&ui, &file);
        assert_eq!(ui.get_selected_duration(), "—");
        assert_eq!(ui.get_selected_sample_rate(), "—");
        assert_eq!(ui.get_selected_bit_depth(), "—");
        assert_eq!(ui.get_selected_channels(), "—");
        clear(&ui);
        assert!(ui.get_selected_duration().is_empty());
        assert!(ui.get_selected_channels().is_empty());
    }

    #[test]
    fn narrower_detail_panes_wrap_tags_without_changing_their_data() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().unwrap();
        let tags: Vec<SharedString> = [
            "Metal",
            "Chain",
            "Foley",
            "多声道录音",
            "Long location field recording",
        ]
        .into_iter()
        .map(Into::into)
        .collect();
        ui.set_selected_tag_labels(ModelRc::new(VecModel::from(tags.clone())));
        ui.set_detail_tag_area_width(800.0);
        relayout(&ui);
        let wide = ui.get_selected_tag_rows().row_count();
        ui.set_detail_tag_area_width(120.0);
        relayout(&ui);
        let rows = ui.get_selected_tag_rows();
        assert!(rows.row_count() > wide);
        let shown: Vec<_> = rows
            .iter()
            .flat_map(|row| row.tags.iter().collect::<Vec<_>>())
            .collect();
        assert_eq!(shown, tags);
    }
}
