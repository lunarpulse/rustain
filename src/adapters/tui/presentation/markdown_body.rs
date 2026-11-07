//! Shared Markdown body window (Story 21.13 D15, MW-5/MW-6): one layout for a
//! surface's *sanitized display projection* in either presentation. The frame
//! keeps its own `Viewer`; only rows inside `wide` ever move horizontally.

use std::ops::Range;

use ratatui::text::Line;

use crate::adapters::tui::{
    markdown::{self, BlockPresentation, RenderOptions},
    theme::Theme,
};

/// Largest source (bytes) a body renders as markdown; larger stays raw with a
/// notice (author decision, story 21.13: one MiB, the order of the tool-output
/// display caps, so one `v` can never stall a draw on a huge payload).
pub const RENDERED_BODY_MAX_BYTES: usize = 1 << 20;

/// The one row shown above a body that is over [`RENDERED_BODY_MAX_BYTES`].
pub const RENDERED_BODY_NOTICE: &str = "Rendered view is limited to 1 MiB; showing the raw text.";

/// How a body shows its source. Raw is the default of every body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum BodyMode {
    /// The surface's literal rendering of the source.
    #[default]
    Source,
    /// Markdown rendered at the inner width, links explicit (MW-7).
    Rendered,
}

impl BodyMode {
    pub fn toggled(self) -> Self {
        match self {
            Self::Source => Self::Rendered,
            Self::Rendered => Self::Source,
        }
    }
}

/// Rows of a body, the rows that may scroll sideways and their widest extent.
#[derive(Debug, Clone)]
pub struct BodyLayout {
    pub lines: Vec<Line<'static>>,
    /// Ascending, disjoint row ranges that `h_off` applies to.
    pub wide: Vec<Range<usize>>,
    /// Cells of the widest scrollable row (0 when nothing scrolls).
    pub widest: usize,
}

/// Lay `display_source` out for `mode`. `display_source` is always the sanitized
/// projection, never raw bytes (copy alone reads the raw source).
///
/// - Source: every row is scrollable when `wrap` is off (unwrapped literal
///   rows); none when it is on.
/// - Rendered: prose wraps at `inner_width` whatever `wrap` says; only the rows
///   of code blocks and tables wider than `inner_width` scroll.
#[must_use]
pub fn layout(
    display_source: &str,
    inner_width: usize,
    theme: &Theme,
    mode: BodyMode,
    wrap: bool,
) -> BodyLayout {
    match mode {
        BodyMode::Source => {
            let width = if wrap { inner_width } else { usize::MAX };
            let lines =
                markdown::render(display_source, width, theme, &RenderOptions::literal()).lines;
            let widest = if wrap {
                0
            } else {
                lines.iter().map(Line::width).max().unwrap_or(0)
            };
            let wide = if wrap || lines.is_empty() {
                Vec::new()
            } else {
                std::iter::once(0..lines.len()).collect()
            };
            BodyLayout {
                lines,
                wide,
                widest,
            }
        }
        BodyMode::Rendered => {
            let rendered = markdown::render(
                display_source,
                inner_width,
                theme,
                &RenderOptions::completed().with_blocks(BlockPresentation::Window),
            );
            let mut wide = Vec::new();
            let mut widest = 0;
            for span in &rendered.blocks {
                if span.cells > inner_width && !span.rows.is_empty() {
                    widest = widest.max(span.cells);
                    wide.push(span.rows.clone());
                }
            }
            BodyLayout {
                lines: rendered.lines,
                wide,
                widest,
            }
        }
    }
}

/// Whether `row` belongs to a scrollable range (`wide` is ascending).
#[must_use]
pub fn is_wide_row(wide: &[Range<usize>], row: usize) -> bool {
    let at = wide.partition_point(|range| range.end <= row);
    wide.get(at).is_some_and(|range| range.start <= row)
}
