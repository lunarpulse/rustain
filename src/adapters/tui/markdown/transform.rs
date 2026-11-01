//! Stage 3: map the parsed block tree to theme styles.
//!
//! Nothing here removes source characters: HTML, unmatched delimiters and
//! escapes are shown exactly as CommonMark resolved them (D2, AD-2113).

use std::sync::Arc;

use ratatui::prelude::*;

use super::parse::{Align, Block, Fmt, Inline, List, Table};
use crate::adapters::tui::theme::Theme;

/// Styled block tree produced by Stage 3.
#[derive(Debug)]
pub enum StyledBlock {
    Paragraph(Vec<StyledInline>),
    Heading {
        level: u8,
        inlines: Vec<StyledInline>,
    },
    List(StyledList),
    Quote(Vec<StyledBlock>),
    Code {
        language: Option<String>,
        content: String,
    },
    Html(String),
    Table(StyledTable),
    Rule,
}

#[derive(Debug)]
pub struct StyledList {
    pub start: Option<u64>,
    pub tight: bool,
    pub items: Vec<StyledItem>,
}

#[derive(Debug)]
pub struct StyledItem {
    pub task: Option<bool>,
    pub blocks: Vec<StyledBlock>,
}

/// A styled table: cells are inline runs; the header carries the heading style.
#[derive(Debug)]
pub struct StyledTable {
    pub aligns: Vec<Align>,
    pub head: Vec<Vec<StyledInline>>,
    pub rows: Vec<Vec<Vec<StyledInline>>>,
}

/// A styled inline node. Soft breaks carry the style of the surrounding block
/// so the space they become continues its underline or strike-through.
/// `LinkStart`/`LinkEnd` bracket the label of a safe link and carry no text.
#[derive(Debug, Clone)]
pub enum StyledInline {
    Text(StyledSpan),
    SoftBreak(Style),
    HardBreak,
    LinkStart(Arc<str>),
    LinkEnd,
}

/// A text run with its style. `atomic` marks an inline code span: the wrapper
/// keeps it whole when it fits on a fresh line.
#[derive(Debug, Clone)]
pub struct StyledSpan {
    pub content: String,
    pub style: Style,
    pub atomic: bool,
}

/// Style palette resolved once per transform from the painting theme.
struct Palette {
    normal: Style,
    code: Style,
    link: Style,
    destination: Style,
    host: Style,
    heading: [Style; 3],
    quote: bool,
}

impl Palette {
    fn new(theme: &Theme) -> Self {
        let colors = &theme.colors;
        let base = Style::default().fg(colors.fg_primary);
        let heading = base.patch(theme.typography.heading);
        // Monochrome maps every colour to `Reset`: H2 would equal H3, and
        // hierarchy must never depend on colour alone.
        let mono = colors.accent == Color::Reset;
        let h2 = if mono {
            heading.add_modifier(Modifier::UNDERLINED)
        } else {
            heading.fg(colors.accent)
        };
        Self {
            normal: base,
            code: Style::default()
                .fg(colors.code_span)
                .bg(colors.code_block_bg),
            link: Style::default()
                .fg(colors.accent)
                .add_modifier(Modifier::UNDERLINED),
            destination: Style::default().fg(colors.fg_muted),
            host: theme.typography.meta.fg(colors.fg_muted),
            heading: [heading.fg(colors.accent), h2, heading],
            quote: false,
        }
    }

    fn heading_style(&self, level: u8) -> Style {
        self.heading[usize::from(level.clamp(1, 3)) - 1]
    }

    /// Style of an inline run on top of the block `base`.
    fn inline_style(&self, base: Style, fmt: Fmt) -> Style {
        let mut style = base;
        if fmt.link {
            style = style.patch(self.link);
        }
        if fmt.code {
            style = style.patch(self.code);
        }
        if fmt.destination {
            style = style.patch(self.destination);
        }
        if fmt.host {
            style = style.patch(self.host);
        }
        if fmt.bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        if fmt.italic || self.quote {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if fmt.strike {
            style = style.add_modifier(Modifier::CROSSED_OUT);
        }
        style
    }
}

/// Transform the parsed tree, applying theme styles.
#[must_use]
pub fn transform(blocks: Vec<Block>, theme: &Theme) -> Vec<StyledBlock> {
    let mut palette = Palette::new(theme);
    blocks_to_styled(blocks, &mut palette)
}

fn blocks_to_styled(blocks: Vec<Block>, palette: &mut Palette) -> Vec<StyledBlock> {
    blocks
        .into_iter()
        .map(|block| block_to_styled(block, palette))
        .collect()
}

fn block_to_styled(block: Block, palette: &mut Palette) -> StyledBlock {
    match block {
        Block::Paragraph(inlines) => {
            let base = palette.normal;
            StyledBlock::Paragraph(inlines_to_styled(inlines, base, palette))
        }
        Block::Heading { level, inlines } => {
            let base = palette.heading_style(level);
            StyledBlock::Heading {
                level,
                inlines: inlines_to_styled(inlines, base, palette),
            }
        }
        Block::List(List {
            start,
            tight,
            items,
        }) => StyledBlock::List(StyledList {
            start,
            tight,
            items: items
                .into_iter()
                .map(|item| StyledItem {
                    task: item.task,
                    blocks: blocks_to_styled(item.blocks, palette),
                })
                .collect(),
        }),
        Block::Quote(children) => {
            let outer = palette.quote;
            palette.quote = true;
            let children = blocks_to_styled(children, palette);
            palette.quote = outer;
            StyledBlock::Quote(children)
        }
        Block::Code { language, content } => StyledBlock::Code { language, content },
        Block::Html(source) => StyledBlock::Html(source),
        Block::Table(Table { aligns, head, rows }) => {
            let head_base = palette.heading_style(3);
            let body_base = palette.normal;
            let head = head
                .into_iter()
                .map(|cell| inlines_to_styled(cell, head_base, palette))
                .collect();
            let rows = rows
                .into_iter()
                .map(|row| {
                    row.into_iter()
                        .map(|cell| inlines_to_styled(cell, body_base, palette))
                        .collect()
                })
                .collect();
            StyledBlock::Table(StyledTable { aligns, head, rows })
        }
        Block::Rule => StyledBlock::Rule,
    }
}

fn inlines_to_styled(inlines: Vec<Inline>, base: Style, palette: &Palette) -> Vec<StyledInline> {
    let base = if palette.quote {
        base.add_modifier(Modifier::ITALIC)
    } else {
        base
    };
    inlines
        .into_iter()
        .map(|inline| match inline {
            Inline::Text { text, fmt } => StyledInline::Text(StyledSpan {
                content: text,
                style: palette.inline_style(base, fmt),
                atomic: fmt.code,
            }),
            Inline::SoftBreak => StyledInline::SoftBreak(base),
            Inline::HardBreak => StyledInline::HardBreak,
            Inline::LinkStart(url) => StyledInline::LinkStart(url),
            Inline::LinkEnd => StyledInline::LinkEnd,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::markdown::LinkPresentation;
    use crate::adapters::tui::markdown::parse::parse;

    fn styled(markdown: &str, theme: &Theme) -> Vec<StyledBlock> {
        transform(parse(markdown, LinkPresentation::Explicit), theme)
    }

    fn first_span(blocks: &[StyledBlock], needle: &str) -> StyledSpan {
        fn walk(blocks: &[StyledBlock], needle: &str) -> Option<StyledSpan> {
            for block in blocks {
                let found = match block {
                    StyledBlock::Paragraph(inlines) | StyledBlock::Heading { inlines, .. } => {
                        inlines.iter().find_map(|i| match i {
                            StyledInline::Text(span) if span.content.contains(needle) => {
                                Some(span.clone())
                            }
                            _ => None,
                        })
                    }
                    StyledBlock::Quote(children) => walk(children, needle),
                    StyledBlock::List(list) => list
                        .items
                        .iter()
                        .find_map(|item| walk(&item.blocks, needle)),
                    _ => None,
                };
                if found.is_some() {
                    return found;
                }
            }
            None
        }
        walk(blocks, needle).unwrap_or_else(|| panic!("no span with {needle:?}"))
    }

    #[test]
    fn test_plain_bold_italic_and_code_styles() {
        let theme = Theme::dark();
        let blocks = styled("p **b** *i* `c`", &theme);
        assert_eq!(
            first_span(&blocks, "p").style,
            Style::default().fg(theme.colors.fg_primary)
        );
        assert!(
            first_span(&blocks, "b")
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(
            first_span(&blocks, "i")
                .style
                .add_modifier
                .contains(Modifier::ITALIC)
        );
        let code = first_span(&blocks, "c");
        assert!(code.atomic);
        assert_eq!(code.style.bg, Some(theme.colors.code_block_bg));
        assert_eq!(code.style.fg, Some(theme.colors.code_span));
    }

    #[test]
    fn test_no_html_or_marker_stripping() {
        // D2: literal text is kept exactly (DF-081).
        let theme = Theme::dark();
        let blocks = styled("SELECT * FROM t and Vec<String> and **open", &theme);
        assert_eq!(
            first_span(&blocks, "SELECT").content,
            "SELECT * FROM t and Vec<String> and **open"
        );
    }

    #[test]
    fn test_heading_hierarchy_does_not_depend_on_colour() {
        let theme = Theme::dark();
        let h1 = first_span(&styled("# a", &theme), "a").style;
        let h2 = first_span(&styled("## a", &theme), "a").style;
        let h3 = first_span(&styled("### a", &theme), "a").style;
        assert_eq!(h1.fg, Some(theme.colors.accent));
        assert_eq!(h2.fg, Some(theme.colors.accent));
        assert_ne!(h3.fg, Some(theme.colors.accent));
        assert!(h3.add_modifier.contains(Modifier::BOLD));

        let mut mono = Theme::dark();
        mono.colors.accent = Color::Reset;
        mono.colors.fg_primary = Color::Reset;
        let m2 = first_span(&styled("## a", &mono), "a").style;
        let m3 = first_span(&styled("### a", &mono), "a").style;
        assert_ne!(m2, m3, "monochrome H2 and H3 must differ");
        assert!(m2.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn test_quote_adds_italic_and_keeps_primary_foreground() {
        let theme = Theme::dark();
        let style = first_span(&styled("> q **b**", &theme), "q").style;
        assert!(style.add_modifier.contains(Modifier::ITALIC));
        assert_eq!(style.fg, Some(theme.colors.fg_primary));
        assert!(!style.add_modifier.contains(Modifier::DIM));
        let bold = first_span(&styled("> q **b**", &theme), "b").style;
        assert!(
            bold.add_modifier
                .contains(Modifier::BOLD | Modifier::ITALIC)
        );
    }

    #[test]
    fn test_links_and_strikethrough_are_styled() {
        let theme = Theme::dark();
        let blocks = styled("[lbl](https://e.test/p) ~~x~~", &theme);
        let link = first_span(&blocks, "lbl").style;
        assert_eq!(link.fg, Some(theme.colors.accent));
        assert!(link.add_modifier.contains(Modifier::UNDERLINED));
        let destination = first_span(&blocks, "(https://e.test/p)").style;
        assert_eq!(destination.fg, Some(theme.colors.fg_muted));
        assert!(
            first_span(&blocks, "x")
                .style
                .add_modifier
                .contains(Modifier::CROSSED_OUT)
        );
    }

    #[test]
    fn test_host_suffix_is_dim_muted_and_not_underlined() {
        let theme = Theme::dark();
        let blocks = transform(
            parse(
                "[lbl](https://e.test/p)",
                LinkPresentation::Labeled { osc8: false },
            ),
            &theme,
        );
        let link = first_span(&blocks, "lbl").style;
        assert!(link.add_modifier.contains(Modifier::UNDERLINED));
        assert_eq!(link.fg, Some(theme.colors.accent));
        let host = first_span(&blocks, "(e.test)").style;
        assert_eq!(host.fg, Some(theme.colors.fg_muted));
        assert!(host.add_modifier.contains(Modifier::DIM));
        assert!(!host.add_modifier.contains(Modifier::UNDERLINED));
    }
}
