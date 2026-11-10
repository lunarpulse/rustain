//! The body model of the thread window (Story 21.8 slice 8-B): the page text as segments
//! (local facts as literal `Text`, one peer-authored `Body` per turn), the turn marks the
//! search filter keeps or drops, and the one row builder both modes paint from.
//!
//! Peer text reaches this module already escaped once (`recipient_view::escape_disclosable`:
//! controls and bidi characters as visible `\u{..}`), so the renderer never sees a raw
//! control character, and the rendered body is only ever painted behind the six-cell
//! gutter: identity rows, title and rail never come from a body.

use std::ops::Range;

use ratatui::text::{Line, Span};

use super::presentation::{
    inspector_search::InspectorSearch,
    markdown_body::{self, BodyMode, RENDERED_BODY_MAX_BYTES, RENDERED_BODY_NOTICE},
};
use super::state::ThreadInspectorState;
use super::theme::Theme;
use crate::adapters::tui::markdown::LinkPresentation;

/// Cells of the renderer-owned indent every body row sits behind.
pub const GUTTER: usize = 6;

const GUTTER_TEXT: &str = "      ";

/// A piece of a page: local facts (`Text`: whole lines, no trailing newline) or one
/// peer-authored body (`Body`: escaped text, newlines kept, no indent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadSegment {
    Text(String),
    Body(String),
}

impl ThreadSegment {
    pub fn text(&self) -> &str {
        match self {
            Self::Text(text) | Self::Body(text) => text,
        }
    }
}

/// Where a turn starts in the segment list; `keep` turns (the opened item and the thread
/// root) survive every filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnMark {
    pub first_segment: usize,
    pub keep: bool,
}

/// Rows of a laid-out body: the rows that scroll sideways and the widest block (cells,
/// gutter excluded).
#[derive(Debug, Clone, Default)]
pub struct BodyRows {
    pub lines: Vec<Line<'static>>,
    pub wide: Vec<Range<usize>>,
    pub widest: usize,
}

/// Split the legacy `content` into segments. `bodies` are the byte ranges of each body in
/// `content` (just after its six-space indent, up to the end of its last line) and
/// `turn_starts` the byte offsets where each turn begins; both ascending.
pub fn split_segments(
    content: &str,
    bodies: &[Range<usize>],
    turn_starts: &[(usize, bool)],
) -> (Vec<ThreadSegment>, Vec<TurnMark>) {
    fn text(
        segments: &mut Vec<ThreadSegment>,
        content: &str,
        range: Range<usize>,
        after_body: &mut bool,
    ) {
        let mut piece = &content[range];
        if std::mem::take(after_body) {
            piece = piece.strip_prefix('\n').unwrap_or(piece);
        }
        piece = piece.strip_suffix('\n').unwrap_or(piece);
        if !piece.is_empty() {
            segments.push(ThreadSegment::Text(piece.to_owned()));
        }
    }
    let mut segments = Vec::new();
    let mut marks = Vec::new();
    let mut at = 0;
    let mut after_body = false;
    let mut bodies = bodies.iter().peekable();
    let mut turns = turn_starts.iter().copied().peekable();
    loop {
        let body_cut = bodies
            .peek()
            .map(|range| range.start.saturating_sub(GUTTER));
        let turn_cut = turns.peek().map(|(start, _)| *start);
        match (body_cut, turn_cut) {
            (None, None) => break,
            (body, Some(turn)) if body.is_none_or(|body| turn <= body) => {
                let (start, keep) = turns.next().expect("peeked");
                text(&mut segments, content, at..start.max(at), &mut after_body);
                at = start.max(at);
                marks.push(TurnMark {
                    first_segment: segments.len(),
                    keep,
                });
            }
            (Some(body), _) => {
                let range = bodies.next().expect("peeked").clone();
                text(&mut segments, content, at..body.max(at), &mut after_body);
                segments.push(ThreadSegment::Body(
                    content[range.clone()].replace("\n      ", "\n"),
                ));
                at = range.end;
                after_body = true;
            }
            (None, Some(_)) => unreachable!("covered by the turn arm"),
        }
    }
    text(&mut segments, content, at..content.len(), &mut after_body);
    (segments, marks)
}

/// The segments of turn `index`.
fn turn_range(inspector: &ThreadInspectorState, index: usize) -> Range<usize> {
    let end = inspector
        .turns
        .get(index + 1)
        .map_or(inspector.segments.len(), |next| next.first_segment);
    inspector.turns[index].first_segment..end
}

/// Which turns the filter keeps: every kept turn, and every turn whose display text
/// contains the query (all turns while the query is empty).
pub fn shown_turns(inspector: &ThreadInspectorState) -> Vec<bool> {
    let search = &inspector.search;
    (0..inspector.turns.len())
        .map(|index| {
            inspector.turns[index].keep
                || search.query.is_empty()
                || inspector.segments[turn_range(inspector, index)]
                    .iter()
                    .any(|segment| search.matches(segment.text()))
        })
        .collect()
}

/// Whether the query matches anything of the page (the preamble included).
pub fn has_match(segments: &[ThreadSegment], search: &InspectorSearch) -> bool {
    search.query.is_empty()
        || segments
            .iter()
            .any(|segment| search.matches(segment.text()))
}

/// The segments painted now: the preamble, then every shown turn.
pub fn visible_segments(inspector: &ThreadInspectorState) -> Vec<&ThreadSegment> {
    let shown = shown_turns(inspector);
    let preamble = inspector
        .turns
        .first()
        .map_or(inspector.segments.len(), |mark| mark.first_segment);
    let mut parts: Vec<&ThreadSegment> = inspector.segments[..preamble].iter().collect();
    for (index, shown) in shown.into_iter().enumerate() {
        if shown {
            parts.extend(inspector.segments[turn_range(inspector, index)].iter());
        }
    }
    parts
}

/// The legacy literal text of `parts`: local lines as they are, each body line behind the
/// six-space indent. Unfiltered it equals the page's `content`.
pub fn source_text(parts: &[&ThreadSegment]) -> String {
    let mut out = String::new();
    for part in parts {
        match part {
            ThreadSegment::Text(text) => {
                out.push_str(text);
                out.push('\n');
            }
            ThreadSegment::Body(body) => {
                for line in body.split('\n') {
                    out.push_str(GUTTER_TEXT);
                    out.push_str(line);
                    out.push('\n');
                }
            }
        }
    }
    out
}

fn gutter(line: Line<'static>) -> Line<'static> {
    let mut spans = Vec::with_capacity(line.spans.len() + 1);
    spans.push(Span::raw(GUTTER_TEXT));
    spans.extend(line.spans);
    Line {
        style: line.style,
        alignment: line.alignment,
        spans,
    }
}

/// Lay the page out for `width`: Source = the literal text; Rendered = literal local rows
/// plus each body as markdown (links explicit) at `width - 6` behind the gutter.
pub fn build(inspector: &ThreadInspectorState, theme: &Theme, width: usize) -> BodyRows {
    let parts = visible_segments(inspector);
    if inspector.mode == BodyMode::Source {
        let text = source_text(&parts);
        return BodyRows {
            lines: markdown_body::layout(&text, width, theme, BodyMode::Source, true).lines,
            ..BodyRows::default()
        };
    }
    let mut rows = BodyRows::default();
    let inner = width.saturating_sub(GUTTER).max(1);
    for part in parts {
        match part {
            ThreadSegment::Text(text) => {
                let mut lines =
                    markdown_body::layout(text, width, theme, BodyMode::Source, true).lines;
                // The literal layout ends with one empty row of its own: only the source text
                // as a whole keeps it, never every local line group of the rendered page.
                if lines.last().is_some_and(|line| line.width() == 0) {
                    lines.pop();
                }
                rows.lines.extend(lines);
            }
            ThreadSegment::Body(body) if body.len() > RENDERED_BODY_MAX_BYTES => {
                rows.lines
                    .push(gutter(Line::from(RENDERED_BODY_NOTICE.to_owned())));
                rows.lines.extend(
                    markdown_body::layout(body, inner, theme, BodyMode::Source, true)
                        .lines
                        .into_iter()
                        .map(gutter),
                );
            }
            ThreadSegment::Body(body) => {
                let defused =
                    crate::infrastructure::runtime::recipient_view::defuse_char_references(body);
                let laid = markdown_body::layout_links(
                    &defused,
                    inner,
                    theme,
                    BodyMode::Rendered,
                    true,
                    LinkPresentation::Explicit,
                );
                let base = rows.lines.len();
                rows.wide
                    .extend(laid.wide.iter().map(|r| base + r.start..base + r.end));
                rows.widest = rows.widest.max(laid.widest);
                rows.lines.extend(laid.lines.into_iter().map(gutter));
            }
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn story21_8_split_segments_round_trips_the_legacy_content() {
        let mut content = String::from("  head\n");
        let first = content.len() + GUTTER;
        content.push_str("      one\n      two");
        let first = first..content.len();
        content.push('\n');
        let turn = content.len();
        content.push_str("  turn · received\n");
        let second = content.len() + GUTTER;
        content.push_str("      body\\u{1b}");
        let second = second..content.len();
        content.push('\n');
        let (segments, marks) = split_segments(&content, &[first, second], &[(turn, false)]);
        assert_eq!(
            segments,
            [
                ThreadSegment::Text("  head".into()),
                ThreadSegment::Body("one\ntwo".into()),
                ThreadSegment::Text("  turn · received".into()),
                ThreadSegment::Body("body\\u{1b}".into()),
            ]
        );
        assert_eq!(
            marks,
            [TurnMark {
                first_segment: 2,
                keep: false
            }]
        );
        let all: Vec<&ThreadSegment> = segments.iter().collect();
        assert_eq!(source_text(&all), content);
    }

    #[test]
    fn story21_8_a_body_ending_in_a_newline_round_trips() {
        let mut content = String::from("  head\n");
        let start = content.len() + GUTTER;
        content.push_str("      last line\n      ");
        let range = start..content.len();
        content.push('\n');
        let (segments, _) = split_segments(&content, &[range], &[]);
        assert_eq!(segments[1], ThreadSegment::Body("last line\n".into()));
        let all: Vec<&ThreadSegment> = segments.iter().collect();
        assert_eq!(source_text(&all), content);
    }

    #[test]
    fn story21_8_a_two_word_query_keeps_the_matching_follow_up_turn_and_the_opened_turn() {
        use crate::adapters::tui::state::{ThreadAccess, ThreadTarget};
        use crate::domain::models::SemanticMessageType;
        let mut inspector = ThreadInspectorState::new(
            ThreadTarget::Node("n".into()),
            ThreadAccess::LocalOnly,
            SemanticMessageType::Consultation,
        );
        let text = |t: &str| ThreadSegment::Text(t.into());
        inspector.segments = vec![
            text("  head"),
            text("  opened · received"),
            text("  f3 · received · follow-up to opened"),
            ThreadSegment::Body("line 3".into()),
            text("  f40 · received · follow-up to opened"),
            ThreadSegment::Body("line 40".into()),
        ];
        inspector.turns = vec![
            TurnMark {
                first_segment: 1,
                keep: true,
            },
            TurnMark {
                first_segment: 2,
                keep: false,
            },
            TurnMark {
                first_segment: 4,
                keep: false,
            },
        ];
        inspector.search.query.set("line 40");
        assert_eq!(shown_turns(&inspector), [true, false, true]);
        inspector.search.query.set("LINE 3");
        assert_eq!(shown_turns(&inspector), [true, true, false]);
        inspector.search.query.set("");
        assert_eq!(shown_turns(&inspector), [true, true, true]);
    }

    /// 8B-01: references decoded by the markdown parser (`&#x202e;`) are shown as visible
    /// escapes at the builder level, never as raw characters in a span.
    #[test]
    fn story21_8_entity_encoded_bidi_and_controls_are_visible_escapes_in_builder_spans() {
        use crate::adapters::tui::state::{ThreadAccess, ThreadTarget};
        use crate::domain::models::SemanticMessageType;
        let mut inspector = ThreadInspectorState::new(
            ThreadTarget::Node("n".into()),
            ThreadAccess::LocalOnly,
            SemanticMessageType::Consultation,
        );
        inspector.segments = vec![ThreadSegment::Body(
            "a&#x202e;b&#8238;c&#X2066;d&#x1b;e&#0;f &amp; &#x41; &#xZZ; &#x202e".into(),
        )];
        let laid = build(&inspector, &Theme::dark(), 80);
        let text: String = laid
            .lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(
            !text.chars().any(|c| c.is_control()
                || ('\u{202a}'..='\u{202e}').contains(&c)
                || ('\u{2066}'..='\u{2069}').contains(&c)),
            "{text:?}"
        );
        for visible in ["\\u{202e}", "\\u{2066}", "\\u{1b}", "\\u{0}"] {
            assert!(text.contains(visible), "{visible} in {text:?}");
        }
        // Positive controls: an ordinary reference still decodes, a malformed one is text.
        assert!(text.contains("& A"), "{text:?}");
        assert!(text.contains("&#xZZ;"), "{text:?}");
    }
}
