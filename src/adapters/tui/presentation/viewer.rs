//! Shared vertical inspector navigation. The viewer never owns Stage state.

use std::{borrow::Cow, ops::Range};

use ratatui::text::Line;
use unicode_segmentation::UnicodeSegmentation;

use super::inspector_search::{CaselessMatcher, InspectorSearch};
use crate::adapters::tui::text_geometry;

/// Bytes are relative to a normalized, tab-expanded logical line, not the raw
/// tool record. Display-only paragraph separators deliberately have no origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowOrigin {
    pub logical_line: usize,
    pub bytes: Range<usize>,
}

pub struct LogicalLines<'a> {
    remaining: &'a str,
}

/// Mirror Literal's CRLF/bare-CR and trailing-newline policy without copying
/// clean source text. Only a logical line containing tabs needs an allocation.
pub fn logical_lines(source: &str) -> LogicalLines<'_> {
    LogicalLines {
        remaining: source.trim_end_matches(['\r', '\n']),
    }
}

impl<'a> Iterator for LogicalLines<'a> {
    type Item = Cow<'a, str>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }
        let end = self
            .remaining
            .find(['\r', '\n'])
            .unwrap_or(self.remaining.len());
        let line = &self.remaining[..end];
        let mut next = end;
        if next < self.remaining.len() {
            next += if self.remaining[next..].starts_with("\r\n") {
                2
            } else {
                1
            };
        }
        self.remaining = &self.remaining[next..];
        Some(text_geometry::expand_tabs(line, 0))
    }
}

fn span_prefix(source: &str, row: &Line<'_>) -> Option<usize> {
    let mut used = 0;
    for span in &row.spans {
        if !source[used..].starts_with(span.content.as_ref()) {
            return None;
        }
        used += span.content.len();
    }
    Some(used)
}

/// Literal's only non-identical painted content is a blank substitution for a
/// grapheme wider than the row. Compare it without reconstructing a row string.
fn substituted_prefix(source: &str, row: &Line<'_>) -> Option<usize> {
    let width = row.width();
    let mut original = source.grapheme_indices(true);
    let mut painted = row
        .spans
        .iter()
        .flat_map(|span| span.content.graphemes(true));
    let mut end = 0;
    while let Some(display) = painted.next() {
        let (at, grapheme) = original.next()?;
        if grapheme != display {
            if width == 0 || text_geometry::cells(grapheme) <= width || display != " " {
                return None;
            }
            for _ in 1..width {
                if painted.next() != Some(" ") {
                    return None;
                }
            }
        }
        end = at + grapheme.len();
    }
    Some(end)
}

/// Map the actual Literal output, rather than running a second wrapping
/// algorithm. Compare borrowed span chunks monotonically; wrapping may consume
/// whitespace between chunks but may never jump across non-whitespace content.
pub fn row_origins(source: &str, rows: &[Line<'_>]) -> Vec<Option<RowOrigin>> {
    let mut logical = logical_lines(source).enumerate();
    let mut current = logical.next();
    let mut offset = 0;
    let mut origins = Vec::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        if index + 1 == rows.len() && row.spans.iter().all(|s| s.content.is_empty()) {
            origins.push(None);
            continue;
        }
        loop {
            let Some((line, text)) = current.as_ref() else {
                origins.push(None);
                break;
            };
            if offset > 0 && text[offset..].chars().all(char::is_whitespace) {
                current = logical.next();
                offset = 0;
                continue;
            }
            if row.spans.iter().all(|s| s.content.is_empty()) {
                origins.push(Some(RowOrigin {
                    logical_line: *line,
                    bytes: offset..offset,
                }));
                current = logical.next();
                offset = 0;
                break;
            }
            if let Some(length) = span_prefix(&text[offset..], row) {
                origins.push(Some(RowOrigin {
                    logical_line: *line,
                    bytes: offset..offset + length,
                }));
                offset += length;
                break;
            }
            match text[offset..].chars().next() {
                Some(ch) if ch.is_whitespace() => {
                    offset = text[offset..]
                        .find(|ch: char| !ch.is_whitespace())
                        .map_or(text.len(), |next| offset + next);
                }
                None => {
                    current = logical.next();
                    offset = 0;
                }
                Some(_) => {
                    if let Some(length) = substituted_prefix(&text[offset..], row) {
                        origins.push(Some(RowOrigin {
                            logical_line: *line,
                            bytes: offset..offset + length,
                        }));
                        offset += length;
                    } else {
                        origins.push(None);
                    }
                    break;
                }
            }
        }
    }
    origins
}

#[derive(Debug, Clone)]
pub struct SearchMatch {
    pub logical_line: usize,
    pub bytes: Range<usize>,
    pub row: usize,
}

/// Maximum number of retained matches for a single committed query.
pub const MAX_SEARCH_MATCHES: usize = 100_000;

/// One committed search per frame, independent of the currently edited draft.
/// Matches retain offsets only: neither folded haystacks nor source snapshots.
#[derive(Debug, Default)]
pub struct ViewerSearch {
    pub editor: InspectorSearch,
    pub matches: Vec<SearchMatch>,
    pub focused: Option<usize>,
    committed: String,
    truncated: bool,
}

impl ViewerSearch {
    pub fn query(&self) -> &str {
        &self.committed
    }

    pub fn commit(&mut self, query: String) {
        self.committed = query;
        self.matches.clear();
        self.focused = None;
        self.truncated = false;
    }

    pub fn clear(&mut self) {
        self.editor.back();
        self.committed.clear();
        self.matches.clear();
        self.focused = None;
        self.truncated = false;
    }

    pub fn project(&mut self, source: &str, origins: &[Option<RowOrigin>], top: usize) {
        let focused = self
            .focused
            .and_then(|index| self.matches.get(index))
            .map(|found| (found.logical_line, found.bytes.start));
        self.matches.clear();
        self.focused = None;
        self.truncated = false;
        if self.committed.is_empty() {
            return;
        }
        let mut matcher = CaselessMatcher::new(&self.committed);
        let mut row = 0;
        let mut previous_row = None;
        for (logical_line, text) in logical_lines(source).enumerate() {
            matcher.ranges(&text, |bytes| {
                if self.matches.len() == MAX_SEARCH_MATCHES {
                    self.truncated = true;
                    return;
                }
                while row < origins.len()
                    && origins[row].as_ref().is_none_or(|origin| {
                        origin.logical_line < logical_line
                            || (origin.logical_line == logical_line
                                && origin.bytes.end <= bytes.start)
                    })
                {
                    if origins[row].is_some() {
                        previous_row = Some(row);
                    }
                    row += 1;
                }
                let mapped_row = if origins
                    .get(row)
                    .and_then(Option::as_ref)
                    .is_some_and(|origin| origin.logical_line == logical_line)
                {
                    row
                } else {
                    // A wrap can consume an entire whitespace match. It still
                    // belongs to the logical source and navigates to its row.
                    previous_row.unwrap_or(0)
                };
                if self.matches.len() == self.matches.capacity() {
                    let additional = self
                        .matches
                        .capacity()
                        .max(4)
                        .min(MAX_SEARCH_MATCHES - self.matches.len());
                    self.matches.reserve_exact(additional);
                }
                self.matches.push(SearchMatch {
                    logical_line,
                    bytes,
                    row: mapped_row,
                });
            });
        }
        if !self.matches.is_empty() {
            self.focused = Some(
                focused
                    .and_then(|(line, start)| {
                        self.matches.iter().position(|found| {
                            found.logical_line == line && found.bytes.start == start
                        })
                    })
                    .or_else(|| self.matches.iter().position(|m| m.row >= top))
                    .unwrap_or(0),
            );
        }
    }

    pub fn jump(&self, viewer: &mut Viewer, rows: usize, viewport: usize) {
        if let Some(found) = self.focused.and_then(|index| self.matches.get(index)) {
            viewer.top = found.row.min(rows.saturating_sub(viewport));
            viewer.follow = false;
        }
    }

    pub fn step(&mut self, viewer: &mut Viewer, forward: bool, rows: usize, viewport: usize) {
        if self.matches.is_empty() {
            return;
        }
        let current = self.focused.unwrap_or(0);
        self.focused = Some(if forward {
            (current + 1) % self.matches.len()
        } else {
            (current + self.matches.len() - 1) % self.matches.len()
        });
        self.jump(viewer, rows, viewport);
    }

    /// A grapheme is indivisible even when only one of its lowercased scalars
    /// matched (e.g. the combining dot produced by capital dotted I).
    pub fn highlight(&self, logical_line: usize, bytes: Range<usize>) -> Option<bool> {
        let first = self.matches.partition_point(|found| {
            found.logical_line < logical_line
                || (found.logical_line == logical_line && found.bytes.end <= bytes.start)
        });
        let mut highlighted = None;
        for (offset, found) in self.matches[first..].iter().enumerate() {
            if found.logical_line != logical_line || found.bytes.start >= bytes.end {
                break;
            }
            if found.bytes.end > bytes.start {
                let focused = self.focused == Some(first + offset);
                highlighted = Some(highlighted.unwrap_or(false) || focused);
            }
        }
        highlighted
    }

    pub fn status(&self) -> Option<String> {
        if self.committed.is_empty() {
            None
        } else if self.matches.is_empty() {
            Some(format!("Search: no match for \"{}\"", self.committed))
        } else {
            Some(format!(
                "Search {}/{}{}",
                self.focused.unwrap_or(0) + 1,
                self.matches.len(),
                if self.truncated { "+" } else { "" }
            ))
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Viewer {
    pub top: usize,
    pub follow: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ViewerCmd {
    LineDown,
    LineUp,
    HalfDown,
    HalfUp,
    PageDown,
    PageUp,
    Top,
    Tail,
}

/// Apply a registry command against the last fitted body dimensions.
pub fn apply(viewer: &mut Viewer, command: ViewerCmd, rows: usize, viewport: usize) {
    use ViewerCmd::*;
    let maximum = rows.saturating_sub(viewport);
    let top = viewer.top.min(maximum);
    viewer.top = match command {
        LineDown => top.saturating_add(1),
        LineUp => top.saturating_sub(1),
        HalfDown => top.saturating_add((viewport / 2).max(1)),
        HalfUp => top.saturating_sub((viewport / 2).max(1)),
        PageDown => top.saturating_add(viewport.saturating_sub(1).max(1)),
        PageUp => top.saturating_sub(viewport.saturating_sub(1).max(1)),
        Top => 0,
        Tail => maximum,
    }
    .min(maximum);
    viewer.follow = command == Tail;
}

/// Painting clamps again: keys can precede the first fitted paint or a resize.
pub fn slice(viewer: &Viewer, rows: usize, viewport: usize) -> std::ops::Range<usize> {
    let top = viewer.top.min(rows.saturating_sub(viewport));
    top..top.saturating_add(viewport).min(rows)
}

/// Fit a cached row width without rescanning its potentially unbounded spans.
/// Reserve the last cell before painting, so Buffer::set_line never paints half
/// of a wide grapheme beneath the continuation marker.
pub fn slice_line(row_width: usize, viewport: u16, wrap: bool) -> (u16, Option<&'static str>) {
    let clipped = !wrap && row_width > usize::from(viewport) && viewport > 0;
    if clipped {
        (viewport - 1, Some("→"))
    } else {
        (viewport, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_r2_1_long_dropped_space_run_is_bounded() {
        use crate::adapters::tui::{markdown, theme::Theme};
        use std::{sync::mpsc, time::Duration};
        let (send, recv) = mpsc::channel();
        std::thread::spawn(move || {
            let source = format!("x{}y", " ".repeat(200_000));
            let rows = markdown::render(
                &source,
                10,
                &Theme::dark(),
                &markdown::RenderOptions::literal(),
            )
            .lines;
            send.send(row_origins(&source, &rows)).unwrap();
        });
        let origins = recv
            .recv_timeout(Duration::from_secs(5))
            .expect("origin mapping must be linear");
        assert_eq!(
            origins,
            vec![
                Some(RowOrigin {
                    logical_line: 0,
                    bytes: 0..1
                }),
                Some(RowOrigin {
                    logical_line: 0,
                    bytes: 200_001..200_002
                }),
                None,
            ]
        );
    }

    #[test]
    fn review_r2_4_search_caps_matches_and_reports_overflow() {
        let source = "x".repeat(100_001);
        let origins = vec![Some(RowOrigin {
            logical_line: 0,
            bytes: 0..source.len(),
        })];
        let mut search = ViewerSearch::default();
        search.commit("x".into());
        search.project(&source, &origins, 0);
        assert_eq!(search.matches.len(), 100_000);
        assert_eq!(search.status().as_deref(), Some("Search 1/100000+"));
        search.project(&source[..100_000], &origins, 0);
        assert_eq!(search.matches.len(), MAX_SEARCH_MATCHES);
        assert_eq!(search.status().as_deref(), Some("Search 1/100000"));
        search.commit("z".into());
        search.project(&source, &origins, 0);
        assert!(search.matches.is_empty());
    }

    #[test]
    fn review_r2_10_no_match_and_empty_search_leave_follow_unchanged() {
        let mut search = ViewerSearch::default();
        let mut viewer = Viewer {
            top: 7,
            follow: true,
        };
        search.jump(&mut viewer, 20, 5);
        assert_eq!(
            viewer,
            Viewer {
                top: 7,
                follow: true
            }
        );
        search.commit("absent".into());
        search.project("source", &[], 7);
        search.jump(&mut viewer, 20, 5);
        assert_eq!(
            viewer,
            Viewer {
                top: 7,
                follow: true
            }
        );
    }

    #[test]
    fn story21_5_line_slice_reserves_only_a_real_overflow_marker() {
        assert_eq!(slice_line(200, 78, false), (77, Some("→")));
        assert_eq!(slice_line(78, 78, false), (78, None));
        assert_eq!(slice_line(77, 78, false), (78, None));
        assert_eq!(slice_line(200, 78, true), (78, None));
        assert_eq!(slice_line(2, 1, false), (0, Some("→")));
        assert_eq!(slice_line(1, 1, false), (1, None));
        assert_eq!(slice_line(200, 0, false), (0, None));
    }

    #[test]
    fn story21_5_logical_lines_normalize_without_copying_clean_text() {
        let source = "clean\nsecond";
        let lines: Vec<_> = logical_lines(source).collect();
        assert!(matches!(lines[0], Cow::Borrowed("clean")));
        assert_eq!(lines[0].as_ptr(), source.as_ptr());
        assert_eq!(
            logical_lines("a\r\nb\rc\tz\r\n\n").collect::<Vec<_>>(),
            ["a", "b", "c       z"],
        );
        assert!(logical_lines("\r\n\n").next().is_none());
    }

    #[test]
    fn story21_5_search_maps_the_real_literal_wrap_and_whole_graphemes() {
        use crate::adapters::tui::{markdown, theme::Theme};
        let source = "xxxxALPHA beta\r\nİ 界e\u{301}\tΣ σ ς";
        let rows = markdown::render(
            source,
            9,
            &Theme::dark(),
            &markdown::RenderOptions::literal(),
        )
        .lines;
        let origins = row_origins(source, &rows);
        assert_eq!(
            origins[0],
            Some(RowOrigin {
                logical_line: 0,
                bytes: 0..9
            })
        );
        assert_eq!(
            origins[1],
            Some(RowOrigin {
                logical_line: 0,
                bytes: 10..14
            })
        );
        assert_eq!(origins.last(), Some(&None));
        let mut search = ViewerSearch::default();
        search.commit("alpha beta".into());
        search.project(source, &origins, 0);
        assert_eq!(search.matches.len(), 1);
        assert_eq!(search.highlight(0, 4..9), Some(true));
        assert_eq!(search.highlight(0, 10..14), Some(true));
        assert_eq!(search.highlight(0, 0..4), None);
        search.commit("\u{307}".into());
        search.project(source, &origins, 0);
        assert_eq!(search.matches[0].bytes, 0.."İ".len());
        search.commit("σ".into());
        search.project(source, &origins, 0);
        assert_eq!(search.matches.len(), 3);
        search.commit("é".into());
        search.project(source, &origins, 0);
        assert!(search.matches.is_empty());
    }

    #[test]
    fn story21_5_origins_keep_empty_lines_and_replaced_wide_graphemes_monotonic() {
        use crate::adapters::tui::{markdown, theme::Theme};
        let source = "界x\n\nz";
        let rows = markdown::render(
            source,
            1,
            &Theme::dark(),
            &markdown::RenderOptions::literal(),
        )
        .lines;
        assert_eq!(
            row_origins(source, &rows),
            [
                Some(RowOrigin {
                    logical_line: 0,
                    bytes: 0..3
                }),
                Some(RowOrigin {
                    logical_line: 0,
                    bytes: 3..4
                }),
                Some(RowOrigin {
                    logical_line: 1,
                    bytes: 0..0
                }),
                Some(RowOrigin {
                    logical_line: 2,
                    bytes: 0..1
                }),
                None,
            ]
        );
    }

    #[test]
    fn story21_5_viewer_movements_and_follow_by_value() {
        use ViewerCmd::*;
        for (command, expected) in [
            (LineDown, 21),
            (LineUp, 19),
            (HalfDown, 26),
            (HalfUp, 14),
            (PageDown, 31),
            (PageUp, 9),
            (Top, 0),
            (Tail, 88),
        ] {
            let mut viewer = Viewer {
                top: 20,
                follow: true,
            };
            apply(&mut viewer, command, 100, 12);
            assert_eq!(viewer.top, expected, "{command:?}");
            assert_eq!(viewer.follow, command == Tail, "{command:?}");
        }
    }

    #[test]
    fn story21_5_viewer_clamps_short_empty_zero_and_overflow_edges() {
        use ViewerCmd::*;
        for command in [
            LineDown, LineUp, HalfDown, HalfUp, PageDown, PageUp, Top, Tail,
        ] {
            for (rows, viewport) in [(0, 12), (5, 12), (12, 12)] {
                let mut viewer = Viewer {
                    top: usize::MAX,
                    follow: false,
                };
                apply(&mut viewer, command, rows, viewport);
                assert_eq!(viewer.top, 0, "{command:?} {rows}/{viewport}");
                assert!(slice(&viewer, rows, viewport).end <= rows);
            }
            let mut viewer = Viewer {
                top: usize::MAX,
                follow: false,
            };
            apply(&mut viewer, command, usize::MAX, 1);
            assert!(viewer.top < usize::MAX);
        }
        let mut v = Viewer::default();
        apply(&mut v, HalfDown, 10, 0);
        assert_eq!(v.top, 1);
        apply(&mut v, PageDown, 10, 1);
        assert_eq!(v.top, 2);
    }

    #[test]
    fn story21_5_viewer_repeated_motion_stops_and_paint_clamps_again() {
        let mut v = Viewer::default();
        for _ in 0..200 {
            apply(&mut v, ViewerCmd::LineDown, 100, 12);
        }
        assert_eq!(v.top, 88);
        assert_eq!(slice(&v, 100, 12), 88..100);
        assert_eq!(slice(&v, 5, 12), 0..5);
        for _ in 0..200 {
            apply(&mut v, ViewerCmd::LineUp, 100, 12);
        }
        assert_eq!(v.top, 0);
    }
}
