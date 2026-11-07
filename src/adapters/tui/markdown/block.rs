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
            let rows: Vec<Line<'static>> = source
                .split('\n')
                .map(|line| Line::from(geom::expand_tabs(line, 0).into_owned()))
                .collect();
            let mapping = (1..=rows.len()).map(Some).collect();
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
