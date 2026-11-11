// app/audiolib-desktop/src/result_window.rs
//! Preserve the visible result range when an asynchronous window load completes.
use audiolib_core::{AudioFile, SearchPage, SearchRequest};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};

use super::result_page_navigation::SourcePage;

/// Raw query positions belonging to the bounded visible row model. This state
/// is memory-only and is cached under the same provider/revision as its records.
#[derive(Clone, Debug, Default)]
pub(super) struct WindowCoverage {
    /// Equal before/after provider revision for the displayed model, when proven.
    pub revision: Option<u64>,
    pub positions: Vec<usize>,
    pub before_raw: usize,
    pub after_raw: usize,
    pub has_next: bool,
    pub next_cursor: Option<String>,
    pub cursor_limit: usize,
    pub first_offset: Option<usize>,
}

impl WindowCoverage {
    /// A delta may be joined only to the exact provider image already displayed.
    /// This proof is required even when the delta uses a single provider query.
    pub(super) fn matches_revision(&self, before: Option<u64>, after: Option<u64>) -> bool {
        self.revision.is_some() && self.revision == before && before == after
    }
}

/// Owned visible records plus the raw coverage needed to continue their query.
/// Counts remain provider-derived or conservative observed lower bounds.
pub(super) struct WindowPage {
    pub page: SearchPage,
    pub coverage: WindowCoverage,
}

#[derive(Clone)]
/// One adjacent range load, bound to the already displayed window and direction.
pub(super) struct WindowPlan {
    pub coverage: WindowCoverage,
    pub direction: i32,
}

fn no_progress() -> audiolib_core::Error {
    audiolib_core::Error::Validation("Search continuation did not advance".into())
}

/// Collect a bounded number of visible rows with individually bounded provider
/// queries. Removal gaps consume raw positions, never synthetic model rows.
pub(super) fn load_forward(
    request: &mut SearchRequest,
    cancel: &AtomicBool,
    search: impl FnMut(&SearchRequest) -> audiolib_core::Result<SearchPage>,
    hide_removed: impl FnMut(&mut SearchPage),
) -> audiolib_core::Result<WindowPage> {
    load_forward_until(request, None, cancel, search, hide_removed)
}

fn load_forward_until(
    request: &mut SearchRequest,
    stop_before: Option<usize>,
    cancel: &AtomicBool,
    mut search: impl FnMut(&SearchRequest) -> audiolib_core::Result<SearchPage>,
    mut hide_removed: impl FnMut(&mut SearchPage),
) -> audiolib_core::Result<WindowPage> {
    let budget = request.limit.clamp(1, super::PAGE_SIZE * 3);
    let original_offset = request.offset;
    let mut probe = request.clone();
    probe.limit = budget.min(super::PAGE_SIZE);
    if probe.limit != request.limit {
        probe.cursor = None;
    }
    let mut cursors = HashSet::new();
    let mut records = Vec::new();
    let mut positions = Vec::new();
    let mut first_request = None;
    let mut total = 0;
    let mut exact = true;
    let mut elapsed = 0.0;
    let mut cached = true;
    loop {
        if cancel.load(Ordering::Acquire) {
            return Err(audiolib_core::Error::Cancelled);
        }
        let mut page = search(&probe)?;
        let source = SourcePage::from(&page);
        let after_raw = probe
            .offset
            .checked_add(page.returned_count)
            .ok_or_else(no_progress)?;
        let raw_ids = page.results.iter().map(|file| file.id).collect::<Vec<_>>();
        let next_cursor = page.next_cursor.clone();
        let has_next =
            source.has_next(&probe) && stop_before.is_none_or(|boundary| after_raw < boundary);
        hide_removed(&mut page);
        if cancel.load(Ordering::Acquire) {
            return Err(audiolib_core::Error::Cancelled);
        }
        if !page.results.is_empty() && first_request.is_none() {
            first_request = Some(probe.clone());
        }
        let mut raw_index = 0;
        for file in &page.results {
            while raw_ids.get(raw_index) != Some(&file.id) {
                raw_index += 1;
                if raw_index >= raw_ids.len() {
                    return Err(no_progress());
                }
            }
            positions.push(
                probe
                    .offset
                    .checked_add(raw_index)
                    .ok_or_else(no_progress)?,
            );
            raw_index += 1;
        }
        total = total.max(page.total_count);
        exact &= page.total_count_exact && probe.cursor.is_none();
        elapsed += page.execution_time_ms;
        cached &= page.cache_hit;
        records.append(&mut page.results);
        if records.len() >= budget || !has_next {
            let first = first_request.unwrap_or_else(|| probe.clone());
            request.offset = first.offset;
            request.cursor = (first.limit == budget).then_some(first.cursor).flatten();
            request.limit = budget;
            request.page = None;
            page.results = records;
            page.returned_count = page.results.len();
            page.total_count = total.max(page.returned_count as u64);
            page.total_count_exact = exact;
            page.execution_time_ms = elapsed;
            page.cache_hit = cached;
            page.next_cursor = (probe.limit == budget)
                .then_some(next_cursor.clone())
                .flatten();
            return Ok(WindowPage {
                page,
                coverage: WindowCoverage {
                    revision: None,
                    positions,
                    before_raw: request.offset,
                    after_raw,
                    has_next,
                    next_cursor,
                    cursor_limit: probe.limit,
                    first_offset: (original_offset == 0).then_some(request.offset),
                },
            });
        }
        if after_raw <= probe.offset {
            return Err(no_progress());
        }
        if let Some(cursor) = &probe.cursor {
            cursors.insert(cursor.clone());
        }
        if let Some(cursor) = &next_cursor
            && !cursors.insert(cursor.clone())
        {
            return Err(no_progress());
        }
        let limit = (budget - records.len())
            .min(super::PAGE_SIZE)
            .min(stop_before.map_or(usize::MAX, |boundary| boundary - after_raw));
        probe.cursor = (limit == probe.limit).then_some(next_cursor).flatten();
        probe.offset = after_raw;
        probe.limit = limit;
        probe.page = None;
    }
}

/// Read earlier raw spans in their ordinary provider order, then prepend their
/// visible rows. A hidden reverse span never follows a forward continuation past
/// the original boundary.
pub(super) fn load_backward(
    request: &mut SearchRequest,
    before_raw: usize,
    cancel: &AtomicBool,
    mut search: impl FnMut(&SearchRequest) -> audiolib_core::Result<SearchPage>,
    mut hide_removed: impl FnMut(&mut SearchPage),
) -> audiolib_core::Result<WindowPage> {
    let budget = request.limit.clamp(1, super::PAGE_SIZE * 3);
    let mut end = before_raw;
    let mut queried = false;
    let mut output = WindowPage {
        page: SearchPage::default(),
        coverage: WindowCoverage::default(),
    };
    while end > 0 && output.page.results.len() < budget {
        let start = end.saturating_sub((budget - output.page.results.len()).min(super::PAGE_SIZE));
        let mut probe = request.clone();
        probe.offset = start;
        probe.limit = end - start;
        probe.cursor = None;
        probe.page = None;
        let mut part = load_forward_until(
            &mut probe,
            Some(end),
            cancel,
            &mut search,
            &mut hide_removed,
        )?;
        part.page.results.append(&mut output.page.results);
        part.coverage
            .positions
            .append(&mut output.coverage.positions);
        part.page.total_count = part.page.total_count.max(output.page.total_count);
        part.page.total_count_exact &= !queried || output.page.total_count_exact;
        part.page.cache_hit &= !queried || output.page.cache_hit;
        part.page.execution_time_ms += output.page.execution_time_ms;
        output = part;
        queried = true;
        end = start;
    }
    if cancel.load(Ordering::Acquire) {
        return Err(audiolib_core::Error::Cancelled);
    }
    request.offset = output.coverage.positions.first().copied().unwrap_or(end);
    request.limit = budget;
    request.cursor = None;
    request.page = None;
    output.page.returned_count = output.page.results.len();
    output.page.total_count = output
        .page
        .total_count
        .max(output.page.returned_count as u64);
    output.coverage.before_raw = request.offset;
    output.coverage.after_raw = before_raw;
    output.coverage.has_next = true;
    output.coverage.next_cursor = None;
    output.coverage.first_offset = (end == 0).then_some(request.offset);
    Ok(output)
}

/// Join an adjacent visible chunk and trim only the opposite edge. The caller
/// still applies the existing visible-anchor contract before installing it.
pub(super) fn join(
    current: &[AudioFile],
    coverage: &WindowCoverage,
    mut added: WindowPage,
    direction: i32,
) -> Option<WindowPage> {
    if current.len() != coverage.positions.len() {
        return None;
    }
    let mut records = current.to_vec();
    let mut positions = coverage.positions.clone();
    let mut joined = if direction > 0 {
        records.append(&mut added.page.results);
        positions.append(&mut added.coverage.positions);
        added.coverage.positions = positions;
        added.coverage.before_raw = coverage.before_raw;
        added.coverage.first_offset = coverage.first_offset;
        added.page.results = records;
        added
    } else {
        added.page.results.append(&mut records);
        added.coverage.positions.append(&mut positions);
        added.coverage.after_raw = coverage.after_raw;
        added.coverage.has_next = coverage.has_next;
        added.coverage.next_cursor.clone_from(&coverage.next_cursor);
        added.coverage.cursor_limit = coverage.cursor_limit;
        added.coverage.first_offset = added.coverage.first_offset.or(coverage.first_offset);
        added
    };
    let max = super::PAGE_SIZE * 3;
    if joined.page.results.len() > max {
        if direction > 0 {
            let trim = joined.page.results.len() - max;
            joined.page.results.drain(..trim);
            joined.coverage.positions.drain(..trim);
            joined.coverage.before_raw = joined.coverage.positions[0];
        } else {
            joined.page.results.truncate(max);
            joined.coverage.positions.truncate(max);
            joined.coverage.after_raw = joined.coverage.positions[max - 1].saturating_add(1);
            joined.coverage.has_next = true;
            joined.coverage.next_cursor = None;
        }
    }
    joined.page.returned_count = joined.page.results.len();
    joined.page.total_count = joined
        .page
        .total_count
        .max(joined.page.returned_count as u64);
    Some(joined)
}

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
    fn delta_requires_a_proven_matching_revision_before_and_after_one_query() {
        let coverage = WindowCoverage {
            revision: Some(42),
            ..Default::default()
        };
        assert!(coverage.matches_revision(Some(42), Some(42)));
        assert!(!coverage.matches_revision(Some(42), Some(43)));
        assert!(!coverage.matches_revision(Some(43), Some(43)));
        assert!(!coverage.matches_revision(Some(42), None));
        assert!(!WindowCoverage::default().matches_revision(Some(42), Some(42)));
    }

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
            .map(|id| {
                AudioFile::from(audiolib_core::AudioFileData {
                    id,
                    ..Default::default()
                })
            })
            .collect()
    }
}
