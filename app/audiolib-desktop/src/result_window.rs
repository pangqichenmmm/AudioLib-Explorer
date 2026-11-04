// app/audiolib-desktop/src/result_window.rs
//! Preserve the visible result range when an asynchronous window load completes.
use audiolib_core::AudioFile;

/// Locate both currently visible endpoints in the arriving rows before moving
/// the viewport. Reject a window that would discard content the user scrolled
/// back to while the load was pending.
pub(super) fn result_window_scroll_y(
    current: &[AudioFile],
    next: &[AudioFile],
    y: f32,
    row_height: f32,
    visible_height: f32,
) -> Option<f32> {
    if current.is_empty() {
        return Some(0.0);
    }
    if !y.is_finite() || !row_height.is_finite() || !visible_height.is_finite() {
        return None;
    }
    let row_height = row_height.max(1.0);
    let scroll = (-y).max(0.0);
    let top = ((scroll / row_height).floor() as usize).min(current.len() - 1);
    let last = (((scroll + visible_height.max(0.0)) / row_height).ceil() - 1.0).max(0.0) as usize;
    let last = last.clamp(top, current.len() - 1);
    let new_top = next.iter().position(|file| file.id == current[top].id)?;
    if !next.iter().any(|file| file.id == current[last].id) {
        return None;
    }
    let fraction = scroll % row_height;
    Some(-(new_top as f32 * row_height + fraction))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appending_a_range_keeps_the_current_scroll_position() {
        let current = rows(0, 200);
        let next = rows(0, 400);
        assert_eq!(
            result_window_scroll_y(&current, &next, -1_783.5, 22.0, 440.0),
            Some(-1_783.5)
        );
    }

    #[test]
    fn sliding_the_window_preserves_the_visible_id_and_fractional_row_offset() {
        let current = rows(0, 600);
        let next = rows(200, 600);
        let old_y = -350.0 * 22.0 - 7.25;
        assert_eq!(
            result_window_scroll_y(&current, &next, old_y, 22.0, 440.0),
            Some(old_y + 200.0 * 22.0)
        );
    }

    #[test]
    fn scrolling_back_during_a_forward_load_rejects_an_evicted_top_anchor() {
        assert_eq!(
            result_window_scroll_y(&rows(0, 600), &rows(200, 600), -2_200.0, 22.0, 440.0),
            None
        );
    }

    #[test]
    fn loading_previous_rows_rejects_a_window_that_discards_the_visible_tail() {
        let current = rows(200, 600);
        let next = rows(0, 600);
        // The top anchor is ID 590, which survives. The visible tail reaches
        // ID 609, so admitting this previous window would still remove content.
        assert_eq!(
            result_window_scroll_y(&current, &next, -390.0 * 22.0, 22.0, 440.0),
            None
        );
    }

    #[test]
    fn a_partially_visible_last_row_must_survive_but_the_next_hidden_row_need_not() {
        let current = rows(0, 10);
        let next = rows(0, 5);
        // At exactly the row boundary, ID 4 is the last visible row. Crossing
        // that boundary by half a pixel exposes ID 5, which the next range lacks.
        assert_eq!(
            result_window_scroll_y(&current, &next, -60.5, 20.0, 39.5),
            Some(-60.5)
        );
        assert_eq!(
            result_window_scroll_y(&current, &next, -60.5, 20.0, 40.0),
            None
        );
    }

    #[test]
    fn empty_and_minimum_height_inputs_keep_the_anchor_contract() {
        assert_eq!(
            result_window_scroll_y(&[], &rows(0, 10), -20.0, 22.0, 440.0),
            Some(0.0)
        );
        assert_eq!(
            result_window_scroll_y(&rows(0, 10), &[], 0.0, 22.0, 440.0),
            None
        );
        assert_eq!(
            result_window_scroll_y(&rows(0, 10), &rows(0, 20), -3.25, 0.0, 2.0),
            Some(-3.25)
        );
    }

    fn rows(start: i64, count: i64) -> Vec<AudioFile> {
        (start..start + count)
            .map(|id| AudioFile {
                id,
                ..Default::default()
            })
            .collect()
    }
}
