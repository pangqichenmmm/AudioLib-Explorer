// app/audiolib-desktop/src/result_page_navigation.rs
//! Published row ranges own their opaque continuation and backwards positions.
pub(super) use super::result_window::load_forward as search_visible_page;
use audiolib_core::{SearchPage, SearchRequest, SearchScope};

#[derive(Clone, Debug, PartialEq, Eq)]
struct Position {
    offset: usize,
    limit: usize,
    cursor: Option<String>,
}

impl Position {
    fn of(request: &SearchRequest) -> Self {
        Self {
            offset: request.offset,
            limit: request.limit,
            cursor: request.cursor.clone(),
        }
    }
    fn apply(&self, request: &mut SearchRequest) {
        request.offset = self.offset;
        request.limit = self.limit;
        request.cursor = self.cursor.clone();
        request.page = None;
    }
}

fn key(request: &SearchRequest) -> Vec<u8> {
    let mut identity = request.clone();
    identity.offset = 0;
    identity.limit = 0;
    identity.cursor = None;
    identity.page = None;
    serde_json::to_vec(&identity).expect("owned search request can be serialized")
}

#[derive(Clone)]
/// Provider facts captured before removal overlays change the displayed rows.
pub(super) struct SourcePage {
    returned: usize,
    total: u64,
    exact: bool,
    next_cursor: Option<String>,
}

impl From<&SearchPage> for SourcePage {
    fn from(page: &SearchPage) -> Self {
        Self {
            returned: page.returned_count,
            total: page.total_count,
            exact: page.total_count_exact,
            next_cursor: page.next_cursor.clone(),
        }
    }
}

impl SourcePage {
    /// An opaque continuation takes precedence over exact offset-based counts.
    pub(super) fn has_next(&self, request: &SearchRequest) -> bool {
        if self.next_cursor.is_some() {
            return true;
        }
        if !self.exact
            || (!request.query.trim().is_empty()
                && matches!(request.scope, SearchScope::All | SearchScope::Path))
        {
            return false;
        }
        (request.offset.saturating_add(self.returned) as u64) < self.total
    }
}

struct Transition {
    from: Position,
    forward: bool,
    popped: Option<Position>,
}

#[derive(Default)]
pub(super) struct ResultPageNavigation {
    key: Vec<u8>,
    current: Option<Position>,
    next_cursor: Option<String>,
    cursor_limit: usize,
    returned: usize,
    previous: Vec<Position>,
    requested: bool,
    pending: Option<Transition>,
    first_offset: Option<usize>,
}

impl ResultPageNavigation {
    /// Do not cache displayed rows under an unfulfilled next/previous request.
    pub(super) fn matches_current_request(&self, request: &SearchRequest) -> bool {
        self.pending.is_none()
            && self.current.as_ref().is_none_or(|position| {
                self.key == key(request) && *position == Position::of(request)
            })
    }

    /// Refreshes and restored queries start with their offset, never a stale cursor.
    pub(super) fn prepare_query(&mut self, request: &mut SearchRequest, reset: bool) {
        if reset || !std::mem::take(&mut self.requested) {
            self.clear();
            request.cursor = None;
        }
    }

    /// Range-prefetch changes its offset/limit and cannot reuse a page cursor.
    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }

    /// Request one next/previous page; keep a rollback position until publication.
    pub(super) fn step(&mut self, request: &mut SearchRequest, direction: i32, page_size: usize) {
        let from = Position::of(request);
        if self.key != key(request) || self.current.as_ref() != Some(&from) {
            self.clear();
            self.key = key(request);
            self.current = Some(from.clone());
            self.returned = request.limit;
        }
        let popped = if direction < 0 {
            self.previous.pop()
        } else {
            None
        };
        let target = if direction < 0 {
            popped.clone().unwrap_or(Position {
                offset: from.offset.saturating_sub(page_size),
                limit: page_size,
                cursor: None,
            })
        } else {
            self.previous.push(from.clone());
            Position {
                offset: from.offset.saturating_add(self.returned),
                limit: page_size,
                cursor: if self.cursor_limit == page_size {
                    self.next_cursor.clone()
                } else {
                    None
                },
            }
        };
        target.apply(request);
        self.pending = Some(Transition {
            from,
            forward: direction > 0,
            popped,
        });
        self.requested = true;
    }

    /// Navigation is based on the original provider range before removal overlays.
    #[cfg(test)]
    fn published(&mut self, request: &SearchRequest, page: &SourcePage) -> bool {
        self.install_window(
            request,
            &super::result_window::WindowCoverage {
                before_raw: request.offset,
                after_raw: request.offset + page.returned,
                has_next: page.has_next(request),
                next_cursor: page.next_cursor.clone(),
                cursor_limit: request.limit,
                first_offset: self.first_offset,
                ..Default::default()
            },
        )
    }

    /// Only a search starting at zero proves where its first visible range lies.
    /// Restored offsets keep their unknown earlier ranges available to Previous.
    #[cfg(test)]
    fn published_from(
        &mut self,
        request: &SearchRequest,
        page: &SourcePage,
        original_offset: usize,
    ) -> bool {
        self.install_window(
            request,
            &super::result_window::WindowCoverage {
                before_raw: request.offset,
                after_raw: request.offset + page.returned,
                has_next: page.has_next(request),
                next_cursor: page.next_cursor.clone(),
                cursor_limit: request.limit,
                first_offset: (original_offset == 0)
                    .then_some(request.offset)
                    .or(self.first_offset),
                ..Default::default()
            },
        )
    }

    /// Unknown restored prefixes remain navigable until their beginning is read.
    pub(super) fn can_previous(&self, request: &SearchRequest) -> bool {
        request.offset > self.first_offset.unwrap_or(0)
    }

    /// First raw offset established by a complete traversal from the beginning.
    pub(super) fn known_first(&self) -> Option<usize> {
        self.first_offset
    }

    /// A cold backwards step has no displayed position to restore. Search the
    /// preceding raw region instead of following an empty span forwards again.
    pub(super) fn backward_before(&self) -> Option<usize> {
        self.pending
            .as_ref()
            .filter(|transition| !transition.forward && transition.popped.is_none())
            .map(|transition| transition.from.offset)
    }

    /// Retain a displayed model only for a pending page of the same definition.
    pub(super) fn pending_for(&self, request: &SearchRequest) -> bool {
        self.pending.is_some() && self.key == key(request)
    }

    /// Restore the raw coverage together with its matching visible record model.
    /// Hidden raw spans may be wider than the resident row budget.
    pub(super) fn install_window(
        &mut self,
        request: &SearchRequest,
        coverage: &super::result_window::WindowCoverage,
    ) -> bool {
        let identity = key(request);
        if self.key != identity {
            self.previous.clear();
        }
        self.key = identity;
        self.current = Some(Position::of(request));
        self.returned = coverage.after_raw.saturating_sub(request.offset);
        self.next_cursor.clone_from(&coverage.next_cursor);
        self.cursor_limit = coverage.cursor_limit;
        self.first_offset = coverage.first_offset;
        self.requested = false;
        self.pending = None;
        coverage.has_next
    }

    /// A rejected/stale continuation must not move the still displayed range.
    pub(super) fn failed(&mut self, request: &mut SearchRequest) {
        if let Some(transition) = self.pending.take() {
            if transition.forward {
                self.previous.pop();
            } else if let Some(previous) = transition.popped {
                self.previous.push(previous);
            }
            transition.from.apply(request);
        }
        self.requested = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn reply(returned: usize, total: u64, exact: bool, cursor: Option<&str>) -> SourcePage {
        SourcePage::from(&SearchPage {
            returned_count: returned,
            total_count: total,
            total_count_exact: exact,
            next_cursor: cursor.map(str::to_owned),
            ..Default::default()
        })
    }

    #[test]
    fn provider_continuation_outweighs_totals_and_failed_navigation_restores_the_range() {
        let mut navigation = ResultPageNavigation::default();
        let mut request = SearchRequest {
            query: "wind".into(),
            scope: SearchScope::Path,
            limit: 200,
            ..Default::default()
        };
        // An overlay may hide every row of a page. Its raw provider range and
        // continuation still advance by 200, rather than skipping or stopping.
        assert!(navigation.published(&request, &reply(200, 0, false, Some("next"))));
        navigation.step(&mut request, 1, 200);
        assert_eq!(
            (request.offset, request.cursor.as_deref()),
            (200, Some("next"))
        );
        assert!(!navigation.matches_current_request(&request));
        navigation.prepare_query(&mut request, false);
        navigation.failed(&mut request);
        assert_eq!((request.offset, request.cursor.as_deref()), (0, None));
        assert!(navigation.matches_current_request(&request));
        navigation.step(&mut request, 1, 200);
        navigation.prepare_query(&mut request, false);
        assert!(!navigation.published(&request, &reply(51, 251, true, None)));
        navigation.step(&mut request, -1, 200);
        assert_eq!((request.offset, request.cursor.as_deref()), (0, None));
    }

    #[test]
    fn refresh_and_range_size_changes_do_not_reuse_an_unrelated_cursor() {
        let mut navigation = ResultPageNavigation::default();
        let mut request = SearchRequest {
            query: "wind".into(),
            scope: SearchScope::Path,
            limit: 600,
            ..Default::default()
        };
        assert!(navigation.published(&request, &reply(600, 601, false, Some("wide"))));
        navigation.step(&mut request, 1, 200);
        assert_eq!(
            (request.offset, request.limit, request.cursor.as_deref()),
            (600, 200, None)
        );
        navigation.prepare_query(&mut request, false);
        navigation.published(&request, &reply(200, 801, false, Some("narrow")));
        navigation.step(&mut request, 1, 200);
        assert_eq!(request.cursor.as_deref(), Some("narrow"));
        navigation.prepare_query(&mut request, false);
        navigation.prepare_query(&mut request, false);
        assert_eq!(request.offset, 800);
        assert!(request.cursor.is_none());
    }

    fn row(id: i64) -> audiolib_core::AudioFile {
        audiolib_core::AudioFile::from(audiolib_core::AudioFileData {
            id,
            ..Default::default()
        })
    }

    #[test]
    fn hidden_spans_follow_provider_cursors_without_recounting_final_totals() {
        let mut request = SearchRequest {
            query: "wind".into(),
            scope: SearchScope::Path,
            limit: 1,
            ..Default::default()
        };
        let mut observed = Vec::new();
        let reply = search_visible_page(
            &mut request,
            &AtomicBool::new(false),
            |request| {
                observed.push((request.offset, request.cursor.clone()));
                let id = observed.len() as i64;
                Ok(SearchPage {
                    results: vec![row(id)],
                    returned_count: 1,
                    total_count: 17,
                    total_count_exact: false,
                    next_cursor: (id < 3).then(|| format!("after-{id}")),
                    ..Default::default()
                })
            },
            |page| {
                page.results.retain(|file| file.id == 3);
                page.returned_count = page.results.len();
            },
        )
        .unwrap();
        let page = reply.page;
        assert_eq!(
            observed,
            [
                (0, None),
                (1, Some("after-1".into())),
                (2, Some("after-2".into()))
            ]
        );
        assert_eq!(page.results[0].id, 3);
        assert_eq!((page.total_count, page.total_count_exact), (17, false));
        assert!(!reply.coverage.has_next);
    }

    #[test]
    fn hidden_spans_reject_repeated_cursors_zero_progress_and_offset_overflow() {
        for (offset, returned, current_cursor) in [
            (0, 1, Some("same".to_owned())),
            (0, 0, None),
            (usize::MAX, 1, None),
        ] {
            let mut request = SearchRequest {
                query: "wind".into(),
                scope: SearchScope::Path,
                offset,
                cursor: current_cursor,
                ..Default::default()
            };
            let mut calls = 0;
            let result = search_visible_page(
                &mut request,
                &AtomicBool::new(false),
                |_| {
                    calls += 1;
                    Ok(SearchPage {
                        results: vec![row(1)],
                        returned_count: returned,
                        next_cursor: Some("same".into()),
                        ..Default::default()
                    })
                },
                |page| page.results.clear(),
            );
            assert!(matches!(result, Err(audiolib_core::Error::Validation(_))));
            assert_eq!(calls, 1);
        }
    }

    #[test]
    fn cancellation_prevents_hidden_span_continuation_and_terminal_publication() {
        let cancel = AtomicBool::new(false);
        let mut request = SearchRequest::default();
        let mut calls = 0;
        let result = search_visible_page(
            &mut request,
            &cancel,
            |_| {
                calls += 1;
                Ok(SearchPage {
                    results: vec![row(1)],
                    returned_count: 1,
                    total_count: 2,
                    total_count_exact: true,
                    ..Default::default()
                })
            },
            |page| {
                page.results.clear();
                cancel.store(true, Ordering::Release);
            },
        );
        assert!(matches!(result, Err(audiolib_core::Error::Cancelled)));
        assert_eq!(calls, 1);
        let result = search_visible_page(
            &mut request,
            &cancel,
            |_| panic!("a cancelled continuation must not query the provider"),
            |_| {},
        );
        assert!(matches!(result, Err(audiolib_core::Error::Cancelled)));

        cancel.store(false, Ordering::Release);
        let result = search_visible_page(
            &mut request,
            &cancel,
            |_| Ok(SearchPage::default()),
            |_| cancel.store(true, Ordering::Release),
        );
        assert!(matches!(result, Err(audiolib_core::Error::Cancelled)));
    }

    #[test]
    fn hidden_terminal_path_span_does_not_invent_an_offset_continuation() {
        let mut request = SearchRequest {
            query: "wind".into(),
            scope: SearchScope::Path,
            ..Default::default()
        };
        let mut calls = 0;
        let reply = search_visible_page(
            &mut request,
            &AtomicBool::new(false),
            |_| {
                calls += 1;
                Ok(SearchPage {
                    results: vec![row(1)],
                    returned_count: 1,
                    total_count: 17,
                    total_count_exact: true,
                    ..Default::default()
                })
            },
            |page| page.results.clear(),
        )
        .unwrap();
        let page = reply.page;
        assert!(page.results.is_empty());
        assert!(!reply.coverage.has_next);
        assert_eq!(calls, 1);
        assert_eq!(request.offset, 0);
    }

    #[test]
    fn partial_hidden_pages_do_not_reuse_a_different_probe_limit_cursor() {
        let mut request = SearchRequest {
            query: "wind".into(),
            scope: SearchScope::Path,
            limit: 200,
            ..Default::default()
        };
        let mut observed = Vec::new();
        let reply = search_visible_page(
            &mut request,
            &AtomicBool::new(false),
            |request| {
                observed.push((request.offset, request.limit, request.cursor.clone()));
                Ok(SearchPage {
                    results: (request.offset..request.offset + request.limit)
                        .map(|id| row(id as i64))
                        .collect(),
                    returned_count: request.limit,
                    total_count: 1_000,
                    total_count_exact: false,
                    next_cursor: Some(format!("limit-{}", request.limit)),
                    ..Default::default()
                })
            },
            |page| {
                page.results.retain(|file| file.id >= 150);
                page.returned_count = page.results.len();
            },
        )
        .unwrap();
        assert_eq!(observed, [(0, 200, None), (200, 150, None)]);
        assert_eq!(reply.page.results.len(), 200);
        assert!(reply.page.next_cursor.is_none());
        let mut navigation = ResultPageNavigation::default();
        assert!(navigation.install_window(&request, &reply.coverage));
        navigation.step(&mut request, 1, 200);
        assert_eq!((request.offset, request.limit), (350, 200));
        assert!(request.cursor.is_none());
    }

    #[test]
    fn hidden_prefix_and_middle_spans_keep_only_visible_backward_positions() {
        let mut navigation = ResultPageNavigation::default();
        let mut request = SearchRequest {
            query: "wind".into(),
            scope: SearchScope::Path,
            offset: 200,
            limit: 200,
            cursor: Some("after-prefix".into()),
            ..Default::default()
        };
        assert!(navigation.published_from(
            &request,
            &reply(200, 201, false, Some("after-first-visible")),
            0,
        ));
        assert!(!navigation.can_previous(&request));
        navigation.step(&mut request, 1, 200);
        // The worker skips a hidden middle span without adding it to history.
        request.offset = 600;
        request.cursor = Some("after-middle".into());
        navigation.published_from(&request, &reply(100, 100, false, None), 400);
        assert!(navigation.can_previous(&request));
        navigation.step(&mut request, -1, 200);
        assert_eq!(request.offset, 200);
        assert_eq!(request.cursor.as_deref(), Some("after-prefix"));
        navigation.published_from(
            &request,
            &reply(200, 201, false, Some("after-first-visible")),
            200,
        );
        assert!(!navigation.can_previous(&request));

        let mut restored = ResultPageNavigation::default();
        restored.published_from(&request, &reply(200, 201, false, None), 200);
        assert!(restored.can_previous(&request));
    }
}
