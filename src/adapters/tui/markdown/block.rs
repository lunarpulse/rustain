//! Inspectable block selection and source retention.

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use ratatui::prelude::*;

use super::transform::StyledBlock;
use super::{BlockKind, BlockMode, BlockRender, BlockView, LinkPresentation, RenderMode};
use crate::adapters::tui::text_geometry as geom;
use crate::adapters::tui::theme::Theme;

/// Capture the unrepaired source in the parser's block order.
fn sources(input: &str) -> Vec<String> {
    let input = super::sanitize::sanitize(input, RenderMode::Literal);
    let options =
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS | Options::ENABLE_TABLES;
    let mut result = Vec::new();
    let mut code: Option<String> = None;
    for (event, range) in Parser::new_ext(&input, options).into_offset_iter() {
        match event {
            Event::Start(Tag::CodeBlock(_)) => code = Some(String::new()),
            Event::Text(text) if code.is_some() => {
                if let Some(code) = &mut code {
                    code.push_str(&text);
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some(mut code) = code.take() {
                    if code.ends_with('\n') {
                        code.pop();
                    }
                    result.push(code);
                }
            }
            Event::Start(Tag::Table(_)) => {
                let line_start = input[..range.start].rfind('\n').map_or(0, |i| i + 1);
                let prefix = &input[line_start..range.start];
                let table = &input[range];
                let table = table.strip_suffix('\n').unwrap_or(table);
                if !prefix.is_empty() && prefix.bytes().all(|b| b == b' ' || b == b'>') {
                    let mut stripped = String::with_capacity(table.len());
                    for (index, line) in table.split_inclusive('\n').enumerate() {
                        if index == 0 {
                            stripped.push_str(line);
                        } else {
                            stripped.push_str(line.strip_prefix(prefix).unwrap_or(line));
                        }
                    }
                    result.push(stripped);
                } else {
                    result.push(table.to_owned());
                }
            }
            _ => {}
        }
    }
    result
}

/// How many code blocks and tables `content` holds, in the parser's block order
/// (the same order and count `render_block` selects from). One parse, no
/// source text is retained.
#[must_use]
pub fn block_count(content: &str) -> usize {
    let input = super::sanitize::sanitize(content, RenderMode::Literal);
    let options =
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS | Options::ENABLE_TABLES;
    Parser::new_ext(&input, options)
        .filter(|event| {
            matches!(
                event,
                Event::Start(Tag::CodeBlock(_)) | Event::Start(Tag::Table(_))
            )
        })
        .count()
}

/// The byte length `render_block(content, ordinal, ..).source` would have, from
/// one parser pass that retains nothing (no source text, styled blocks or rows).
/// Exact for code; for a table the raw table range, an upper bound of its source
/// (stripping a container prefix only shortens it). `None`: no such block.
#[must_use]
pub fn block_source_len(content: &str, ordinal: usize) -> Option<usize> {
    let input = super::sanitize::sanitize(content, RenderMode::Literal);
    let options =
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS | Options::ENABLE_TABLES;
    let (mut seen, mut code) = (0usize, None::<(usize, bool)>);
    for (event, range) in Parser::new_ext(&input, options).into_offset_iter() {
        match event {
            Event::Start(Tag::CodeBlock(_)) => code = Some((0, false)),
            Event::Text(text) if code.is_some() => {
                if let Some((len, ends_newline)) = &mut code {
                    *len += text.len();
                    *ends_newline = text.ends_with('\n');
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                let (len, ends_newline) = code.take()?;
                if seen == ordinal {
                    return Some(len - usize::from(ends_newline));
                }
                seen += 1;
            }
            Event::Start(Tag::Table(_)) => {
                if seen == ordinal {
                    let table = &input[range];
                    return Some(table.strip_suffix('\n').unwrap_or(table).len());
                }
                seen += 1;
            }
            _ => {}
        }
    }
    None
}

fn find_block<'a>(blocks: &'a [StyledBlock], ordinal: &mut usize) -> Option<&'a StyledBlock> {
    for block in blocks {
        match block {
            StyledBlock::Code { .. } | StyledBlock::Table(_) => {
                if *ordinal == 0 {
                    return Some(block);
                }
                *ordinal -= 1;
            }
            StyledBlock::Quote(children) => {
                if let Some(found) = find_block(children, ordinal) {
                    return Some(found);
                }
            }
            StyledBlock::List(list) => {
                for item in &list.items {
                    if let Some(found) = find_block(&item.blocks, ordinal) {
                        return Some(found);
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// Render a single inspectable block without its surrounding container.
#[must_use]
pub fn render_block(
    content: &str,
    ordinal: usize,
    theme: &Theme,
    render: &BlockRender,
) -> Option<BlockView> {
    #[cfg(any(test, debug_assertions))]
    super::metrics::record_block_render();
    let source = sources(content).into_iter().nth(ordinal)?;
    let sanitized = super::sanitize::sanitize(content, RenderMode::Completed);
    let parsed = super::parse::parse(&sanitized, LinkPresentation::Explicit);
    let styled = super::highlight::highlight(super::transform::transform(parsed, theme), theme);
    let mut remaining = ordinal;
    let block = find_block(&styled, &mut remaining)?;
    let kind = match block {
        StyledBlock::Code { language, content } => BlockKind::Code {
            language: language.clone(),
            lines: if content.is_empty() {
                0
            } else {
                content.split('\n').count()
            },
        },
        StyledBlock::Table(table) => BlockKind::Table {
            cols: table.aligns.len(),
            rows: table.rows.len(),
        },
        _ => return None,
    };
    let (rows, source_line) = match render.mode {
        BlockMode::Rendered => super::layout::inspect(block, theme, render.wrap)?,
        BlockMode::Source => {
            // Raw lines, tabs expanded and control characters omitted for display
            // only; only code wraps (`wrap` is the content width in cells).
            let wrap = render
                .wrap
                .filter(|_| matches!(kind, BlockKind::Code { .. }));
            if wrap == Some(0) {
                return None;
            }
            let mut rows: Vec<Line<'static>> = Vec::new();
            let mut mapping: Vec<Option<usize>> = Vec::new();
            for (index, line) in source.split('\n').enumerate() {
                let expanded = geom::expand_tabs(line, 0);
                match wrap {
                    Some(width) => {
                        for (part, chunk) in
                            geom::wrap_hard(&expanded, width).into_iter().enumerate()
                        {
                            rows.push(Line::from(chunk.to_owned()));
                            mapping.push((part == 0).then_some(index + 1));
                        }
                    }
                    None => {
                        rows.push(Line::from(expanded.into_owned()));
                        mapping.push(Some(index + 1));
                    }
                }
            }
            (rows, mapping)
        }
    };
    if rows.is_empty() {
        return None;
    }
    let cells = rows
        .iter()
        .map(super::layout::line_cells)
        .max()
        .unwrap_or(0);
    Some(BlockView {
        kind,
        rows,
        source_line,
        cells,
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::sources;

    #[test]
    fn story21_13_unclosed_and_tilde_fences() {
        assert_eq!(sources("~~~rust\na\n~~~\n\n```\nb\n"), ["a", "b"]);
    }

    #[test]
    fn story21_13_info_attributes_and_empty_block() {
        assert_eq!(sources("```rust {ignore}\nx\n```\n\n```\n```"), ["x", ""]);
    }

    #[test]
    fn story21_13_quote_in_list() {
        assert_eq!(sources("- item\n\n  > ```\n  > x\n  > ```"), ["x"]);
    }
}
