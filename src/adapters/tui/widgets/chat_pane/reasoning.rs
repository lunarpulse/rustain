//! Stage rows of reasoning (Story 21.9 D4-D6): the one-row `◇ Thinking · 4.2s ·
//! 1.3k tokens` line, the live `◇ Thinking… · 3.1s · ~120 tokens` line and the
//! expanded body under the header. One owner for the facts text, the width fit
//! and the row count, so the height pass and the paint cannot drift (AD-2113).

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::{LinkSink, stage_options};
use crate::adapters::tui::markdown::{self, RenderOptions};
use crate::adapters::tui::text_geometry as geom;
use crate::adapters::tui::theme::Theme;
use crate::adapters::tui::widgets::status_bar::format_token_count;
use crate::domain::models::ReasoningDisplay;
use crate::domain::models::context_bundle::estimate_tokens;

/// The lead span of a reasoning row; the focus marker replaces exactly this.
pub(super) const LEAD: &str = "◇ ";

const SEPARATOR: &str = " · ";

/// `<0.1s`, `4.2s`, `42s`, `1m 12s`, `1h 02m`.
pub(crate) fn format_duration(ms: u64) -> String {
    if ms < 100 {
        return "<0.1s".to_owned();
    }
    let tenths = ms / 100;
    if tenths < 100 {
        return format!("{}.{}s", tenths / 10, tenths % 10);
    }
    let seconds = ms / 1000;
    if seconds < 60 {
        return format!("{seconds}s");
    }
    if seconds < 3600 {
        return format!("{}m {:02}s", seconds / 60, seconds % 60);
    }
    format!("{}h {:02}m", seconds / 3600, (seconds % 3600) / 60)
}

/// `1.3k tokens` (provider-reported) or `~1.3k tokens` (estimated).
fn token_phrase(count: u32, estimated: bool) -> String {
    format!(
        "{}{} {}",
        if estimated { "~" } else { "" },
        format_token_count(count),
        if count == 1 { "token" } else { "tokens" }
    )
}

fn estimate(text: &str) -> u32 {
    u32::try_from(estimate_tokens(text)).unwrap_or(u32::MAX)
}

/// `4.2s · 1.3k tokens`: the facts after the label, shared by the Stage row
/// and the window title. The duration is omitted when unknown; the token
/// segment is the provider's count, else an estimate of `text`.
pub(crate) fn facts(text: &str, duration_ms: Option<u64>, tokens: Option<u32>) -> String {
    let tokens = tokens.map_or_else(
        || token_phrase(estimate(text), true),
        |count| token_phrase(count, false),
    );
    match duration_ms {
        Some(ms) => format!("{}{SEPARATOR}{tokens}", format_duration(ms)),
        None => tokens,
    }
}

/// Whether a committed reasoning part paints any row at all: empty text with
/// no recorded facts has nothing to say.
pub(super) fn visible(text: &str, duration_ms: Option<u64>, tokens: Option<u32>) -> bool {
    !text.is_empty() || duration_ms.is_some() || tokens.is_some()
}

/// Case-insensitive, non-overlapping matches of `query` in `text`: what the
/// message search counts for a reasoning part the Stage does not paint
/// (folded), so later matches keep their ordinal.
pub(super) fn match_count(text: &str, query: &str) -> usize {
    let query: String = query.chars().flat_map(char::to_lowercase).collect();
    if query.is_empty() {
        return 0;
    }
    let lower: String = text.chars().flat_map(char::to_lowercase).collect();
    lower.matches(query.as_str()).count()
}

/// The label cut to `budget` cells: drop the last segment, then the next,
/// then ellipsize the head (never wraps, never splits a grapheme).
fn fit(head: &str, segments: &[String], budget: usize) -> String {
    for keep in (0..=segments.len()).rev() {
        let mut label = head.to_owned();
        for segment in &segments[..keep] {
            label.push_str(SEPARATOR);
            label.push_str(segment);
        }
        if geom::cells(&label) <= budget {
            return label;
        }
    }
    geom::ellipsize(head, budget, "…").into_owned()
}

/// A muted row: the glyph span, then the label. Below three cells only the
/// glyph fits.
fn row(theme: &Theme, width: usize, head: &str, segments: &[String]) -> Line<'static> {
    let style = Style::default().fg(theme.colors.fg_muted);
    if width < 3 {
        return Line::from(Span::styled("◇", style));
    }
    Line::from(vec![
        Span::styled(LEAD, style),
        Span::styled(fit(head, segments, width - 2), style),
    ])
}

/// The committed header row at `width` content cells.
pub(super) fn header_line(
    theme: &Theme,
    width: usize,
    text: &str,
    duration_ms: Option<u64>,
    tokens: Option<u32>,
) -> Line<'static> {
    let mut segments = Vec::with_capacity(2);
    if let Some(ms) = duration_ms {
        segments.push(format_duration(ms));
    }
    segments.push(match tokens {
        Some(count) => token_phrase(count, false),
        None => token_phrase(estimate(text), true),
    });
    row(theme, width, "Thinking", &segments)
}

/// The streaming row: elapsed since the first thinking chunk and an estimate
/// of the buffered text. `started_ms` is `None` when the start is unknown.
pub(super) fn live_line(
    theme: &Theme,
    width: usize,
    started_ms: Option<i64>,
    now_ms: i64,
    buffer: &str,
) -> Line<'static> {
    let mut segments = Vec::with_capacity(2);
    if let Some(started) = started_ms {
        segments.push(format_duration(
            u64::try_from(now_ms.saturating_sub(started)).unwrap_or(0),
        ));
    }
    segments.push(token_phrase(estimate(buffer), true));
    row(theme, width, "Thinking…", &segments)
}

/// Rows of one committed reasoning part: `1` folded, `1 +` the body expanded.
pub(super) fn height(
    display: ReasoningDisplay,
    text: &str,
    duration_ms: Option<u64>,
    tokens: Option<u32>,
    content_width: usize,
    theme: &Theme,
) -> usize {
    if !visible(text, duration_ms, tokens) {
        return 0;
    }
    match display {
        ReasoningDisplay::Folded => 1,
        ReasoningDisplay::Expanded => {
            1 + markdown::compute_height(
                text,
                content_width,
                theme,
                &stage_options(RenderOptions::completed(), false),
            )
        }
    }
}

/// The rows [`height`] counts. `first_line` is the producer index the header
/// lands on, so the body's link runs keep their coordinates.
#[allow(clippy::too_many_arguments)]
pub(super) fn lines(
    display: ReasoningDisplay,
    text: &str,
    duration_ms: Option<u64>,
    tokens: Option<u32>,
    content_width: usize,
    theme: &Theme,
    sink: &mut LinkSink,
    first_line: usize,
) -> Vec<Line<'static>> {
    if !visible(text, duration_ms, tokens) {
        return Vec::new();
    }
    let mut out = vec![header_line(theme, content_width, text, duration_ms, tokens)];
    if display == ReasoningDisplay::Expanded {
        let rendered = markdown::render(
            text,
            content_width,
            theme,
            &sink.options(RenderOptions::completed()),
        );
        for line in sink.take(rendered, first_line + 1) {
            let spans = line
                .spans
                .into_iter()
                .map(|span| {
                    let style = span
                        .style
                        .fg(theme.colors.fg_secondary)
                        .add_modifier(Modifier::ITALIC);
                    Span::styled(span.content.to_string(), style)
                })
                .collect::<Vec<_>>();
            out.push(Line::from(spans));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn story21_9_durations_use_the_d4_formats() {
        for (ms, want) in [
            (0, "<0.1s"),
            (99, "<0.1s"),
            (100, "0.1s"),
            (4_200, "4.2s"),
            (9_999, "9.9s"),
            (10_000, "10s"),
            (42_000, "42s"),
            (59_999, "59s"),
            (60_000, "1m 00s"),
            (72_000, "1m 12s"),
            (3_599_000, "59m 59s"),
            (3_720_000, "1h 02m"),
            (36_000_000, "10h 00m"),
        ] {
            assert_eq!(format_duration(ms), want, "{ms} ms");
        }
    }

    #[test]
    fn story21_9_facts_name_provider_counts_plainly_and_estimates_with_a_tilde() {
        assert_eq!(facts("x", Some(4_200), Some(1_300)), "4.2s · 1.3k tokens");
        assert_eq!(facts("x", Some(4_200), Some(1)), "4.2s · 1 token");
        assert_eq!(facts("x", None, Some(12)), "12 tokens");
        // 400 bytes estimate to 100 tokens.
        assert_eq!(facts(&"a".repeat(400), None, None), "~100 tokens");
        assert_eq!(facts(&"a".repeat(4), Some(0), None), "<0.1s · ~1 token");
    }

    #[test]
    fn story21_9_row_drops_tokens_then_duration_then_ellipsizes_the_label() {
        let theme = Theme::dark();
        let at = |width| {
            text(&header_line(
                &theme,
                width,
                "abcd",
                Some(4_200),
                Some(1_300),
            ))
        };
        assert_eq!(at(40), "◇ Thinking · 4.2s · 1.3k tokens");
        assert_eq!(at(18), "◇ Thinking · 4.2s");
        assert_eq!(at(10), "◇ Thinking");
        assert_eq!(at(8), "◇ Think…");
        assert_eq!(at(3), "◇ T");
        assert_eq!(at(2), "◇");
        for width in 1..=60 {
            let line = header_line(&theme, width, "abcd", Some(4_200), Some(1_300));
            assert!(line.width() <= width, "{width}: {}", text(&line));
        }
    }

    #[test]
    fn story21_9_live_row_shows_elapsed_and_an_estimate() {
        let theme = Theme::dark();
        let line = live_line(&theme, 60, Some(1_000), 4_100, &"a".repeat(480));
        assert_eq!(text(&line), "◇ Thinking… · 3.1s · ~120 tokens");
        let unknown = live_line(&theme, 60, None, 4_100, "abcd");
        assert_eq!(text(&unknown), "◇ Thinking… · ~1 token");
        // A clock behind the start never shows a negative elapsed time.
        let behind = live_line(&theme, 60, Some(9_000), 4_100, "");
        assert_eq!(text(&behind), "◇ Thinking… · <0.1s · ~0 tokens");
    }
}
