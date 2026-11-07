//! K1: block identity, standalone rendering and original source.

use ratatui::text::Line;
use rustain::adapters::tui::markdown::{
    BlockKind, BlockMode, BlockPresentation, BlockRender, RenderOptions, compute_height, render,
    render_block,
};
use rustain::adapters::tui::theme::Theme;
use unicode_width::UnicodeWidthStr;

fn text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

fn cells(line: &Line<'_>) -> usize {
    text(line).width()
}

fn view(
    source: &str,
    ordinal: usize,
    mode: BlockMode,
    wrap: Option<usize>,
) -> rustain::adapters::tui::markdown::BlockView {
    render_block(source, ordinal, &Theme::dark(), &BlockRender { mode, wrap }).unwrap()
}

#[test]
fn nested_blocks_have_spans_on_their_prefixed_rows() {
    let source = "intro\n\n```rust\none\ntwo\n```\n\n- item\n\n  ```\n  list\n  ```\n\n> ```\n> quote\n> ```";
    for width in [40, 80, 120] {
        let rendered = render(source, width, &Theme::dark(), &RenderOptions::completed());
        assert_eq!(rendered.blocks.len(), 3);
        let kinds = [
            BlockKind::Code {
                language: Some("rust".into()),
                lines: 2,
            },
            BlockKind::Code {
                language: None,
                lines: 1,
            },
            BlockKind::Code {
                language: None,
                lines: 1,
            },
        ];
        for (index, span) in rendered.blocks.iter().enumerate() {
            assert_eq!(span.ordinal, index);
            assert_eq!(span.kind, kinds[index]);
            let lines = &rendered.lines[span.rows.clone()];
            assert_eq!(lines.len(), if index == 0 { 4 } else { 3 });
            let first = text(&lines[0]);
            let last = text(lines.last().unwrap());
            let prefix = match index {
                0 => "",
                1 => "  ",
                _ => "│ ",
            };
            assert!(first.starts_with(&format!("{prefix}┌")), "{first:?}");
            assert!(last.starts_with(&format!("{prefix}└")), "{last:?}");
            assert_eq!(span.cells, lines.iter().map(cells).max().unwrap());
            assert_eq!(span.cells, width);
            if index > 0 {
                assert!(span.rows.start > rendered.blocks[index - 1].rows.end);
            }
        }
    }
    for (source, prefix) in [
        ("```\nx\n```", ""),
        ("- ```\n  x\n  ```", "  "),
        ("> ```\n> x\n> ```", "│ "),
    ] {
        let alone = view(source, 0, BlockMode::Rendered, None);
        let width = alone.cells + if prefix.is_empty() { 0 } else { 2 };
        let stage = render(source, width, &Theme::dark(), &RenderOptions::completed());
        let span = &stage.blocks[0];
        for (index, (painted, standalone)) in stage.lines[span.rows.clone()]
            .iter()
            .zip(&alone.rows)
            .enumerate()
        {
            let painted = text(painted);
            let lead = if source.starts_with('-') && index == 0 {
                "• "
            } else {
                prefix
            };
            assert_eq!(painted.strip_prefix(lead).unwrap(), text(standalone));
        }
    }
}

#[test]
fn source_is_byte_exact_for_code_and_tables() {
    let source = "```txt\r\n\talpha  \r\nend\r\n\r\n```\r\n\r\n> | a | b |\r\n> |---|---|\r\n> | 1 | 2 |\r\n";
    let code = view(source, 0, BlockMode::Source, None);
    assert_eq!(code.source, "\talpha  \nend\n");
    assert_eq!(
        code.rows.iter().map(text).collect::<Vec<_>>(),
        ["        alpha  ", "end", ""]
    );
    assert_eq!(code.source_line, [Some(1), Some(2), Some(3)]);
    let table = view(source, 1, BlockMode::Source, None);
    assert_eq!(table.source, "| a | b |\n|---|---|\n| 1 | 2 |");
    assert_eq!(
        table.rows.iter().map(text).collect::<Vec<_>>(),
        ["| a | b |", "|---|---|", "| 1 | 2 |"]
    );
    assert_eq!(table.source_line, [Some(1), Some(2), Some(3)]);
    for (tail, expected) in [("x\n", "x"), ("x\n\n", "x\n"), ("x\n\n\n", "x\n\n")] {
        let fixture = format!("```\n{tail}```");
        assert_eq!(
            view(&fixture, 0, BlockMode::Rendered, None).source,
            expected
        );
    }
}

#[test]
fn ordinals_do_not_drift_after_a_repaired_table() {
    let source = "| a |\n|---|\n| 1 | 2 |\n\n```rust\nx\n```";
    let rendered = render(source, 40, &Theme::dark(), &RenderOptions::completed());
    assert_eq!(rendered.blocks.len(), 2);
    assert_eq!(
        rendered.blocks[0].kind,
        BlockKind::Table { cols: 2, rows: 1 }
    );
    assert_eq!(
        rendered.blocks[1].kind,
        BlockKind::Code {
            language: Some("rust".into()),
            lines: 1
        }
    );
    assert_eq!(
        view(source, 0, BlockMode::Source, None).source,
        "| a |\n|---|\n| 1 | 2 |"
    );
    assert_eq!(view(source, 1, BlockMode::Source, None).source, "x");
    assert!(
        render_block(
            source,
            2,
            &Theme::dark(),
            &BlockRender {
                mode: BlockMode::Rendered,
                wrap: None
            }
        )
        .is_none()
    );
}

#[test]
fn window_retains_wide_blocks_and_wrap_maps_continuations() {
    let long = "x".repeat(120);
    let source = format!("```rust\n{long}\nnext\n```");
    for width in [40, 80, 120] {
        let window = render(
            &source,
            width,
            &Theme::dark(),
            &RenderOptions::completed().with_blocks(BlockPresentation::Window),
        );
        let span = &window.blocks[0];
        assert_eq!(span.rows.len(), 4);
        assert_eq!(span.cells, 122);
        assert!(text(&window.lines[span.rows.start + 1]).contains(&long));
        assert!(!text(&window.lines[span.rows.start + 1]).contains('→'));
    }
    let wrapped = view(&source, 0, BlockMode::Rendered, Some(20));
    assert_eq!(
        wrapped.source_line,
        [None, Some(1), None, None, None, None, None, Some(2), None]
    );
    assert!(wrapped.rows.iter().all(|line| cells(line) <= 22));
    assert_eq!(wrapped.cells, 22);
}

#[test]
fn tables_report_natural_shrunk_and_collapsed_geometry() {
    let source = "| one | two | three | four |\n|---|---|---|---|\n| aaaaaaaaaaaaaaaaaaaa | bbbbbbbbbbbbbbbbbbbb | cccccccccccccccccccc | dddddddddddddddddddd |";
    for width in [10, 40, 80, 120] {
        let stage = render(source, width, &Theme::dark(), &RenderOptions::completed());
        let span = &stage.blocks[0];
        assert_eq!(span.ordinal, 0);
        assert_eq!(span.kind, BlockKind::Table { cols: 4, rows: 1 });
        assert_eq!(
            span.cells,
            stage.lines[span.rows.clone()]
                .iter()
                .map(cells)
                .max()
                .unwrap()
        );
        if width == 10 {
            assert_eq!(span.rows.len(), 1);
        } else {
            assert_eq!(span.rows.len(), 5);
            assert!(text(&stage.lines[span.rows.start]).starts_with('┌'));
            assert!(text(&stage.lines[span.rows.end - 1]).starts_with('└'));
        }
        let window = render(
            source,
            width,
            &Theme::dark(),
            &RenderOptions::completed().with_blocks(BlockPresentation::Window),
        );
        assert_eq!(window.blocks[0].rows.len(), 5);
        assert!(window.blocks[0].cells > 80);
        if width == 120 {
            let alone = view(source, 0, BlockMode::Rendered, None);
            assert_eq!(&stage.lines[span.rows.clone()], alone.rows.as_slice());
            assert_eq!(span.cells, alone.cells);
        }
    }
    let collapse = "| a | b | c | d | e | f | g | h |\n|---|---|---|---|---|---|---|---|\n| 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 |";
    let stage = render(collapse, 30, &Theme::dark(), &RenderOptions::completed());
    assert_eq!(
        text(&stage.lines[stage.blocks[0].rows.start]),
        "table · 8 cols × 1 row"
    );
    let linked = view(
        "| link |\n|---|\n| [label](https://x) |",
        0,
        BlockMode::Rendered,
        None,
    );
    assert!(
        linked
            .rows
            .iter()
            .map(text)
            .any(|line| line.contains("label (https://x)"))
    );
    assert_eq!(linked.source_line, [None, Some(1), None, Some(3), None]);
}

#[test]
fn indented_code_is_inspectable() {
    let source = "    alpha\n    beta\n";
    let rendered = render(source, 40, &Theme::dark(), &RenderOptions::completed());
    assert_eq!(rendered.blocks.len(), 1);
    assert_eq!(
        rendered.blocks[0].kind,
        BlockKind::Code {
            language: None,
            lines: 2,
        }
    );
    assert_eq!(
        view(source, 0, BlockMode::Source, None).source,
        "alpha\nbeta"
    );
}

#[test]
fn height_truth_covers_both_presentations_and_literal_has_no_spans() {
    let fixtures = [
        "```rust\na\nb\n```",
        "- ```\n  x\n  ```",
        "> ```\n> x\n> ```",
        "| a | b |\n|---|---|\n| long long long | 2 |",
        "| a |\n|---|\n| 1 | 2 |\n\n```\nx\n```",
        "~~~\nunclosed",
    ];
    let theme = Theme::dark();
    for source in fixtures {
        for width in [0, 1, 2, 3, 5, 10, 40, 80, 120] {
            for presentation in [BlockPresentation::Stage, BlockPresentation::Window] {
                let opts = RenderOptions::completed().with_blocks(presentation);
                assert_eq!(
                    compute_height(source, width, &theme, &opts),
                    render(source, width, &theme, &opts).lines.len()
                );
            }
            assert!(
                render(source, width, &theme, &RenderOptions::literal())
                    .blocks
                    .is_empty()
            );
        }
    }
    assert_eq!(
        view("~~~\nunclosed", 0, BlockMode::Source, None).source,
        "unclosed"
    );
}

/// D3's claim "repair never changes block classification" is only as good as the
/// line scanner in `repair_tables`: every adversarial placement of a table-looking
/// run must still give the rendered pass and the unrepaired source pass the same
/// blocks, in the same order and kind.
#[test]
fn repair_never_changes_which_blocks_exist_even_for_adversarial_placements() {
    let wide_body = "| a | b |\n|---|---|\n| 1 | 2 | 3 |\n";
    let cases = [
        format!("<div>\n{wide_body}</div>\n\n```\ncode\n```\n"),
        format!("    {wide_body}\n```\ncode\n```\n"),
        format!("> {wide_body}\n```\ncode\n```\n"),
        format!("- item\n\n  {wide_body}\n```\ncode\n```\n"),
        format!("text\n{wide_body}\n```\ncode\n```\n"),
        format!("```\n{wide_body}```\n\n{wide_body}"),
        format!("{wide_body}\n{wide_body}\n```\nlast\n```\n"),
        "| a | b |\n|---|\n| 1 | 2 | 3 |\n\n```\nx\n```\n".to_owned(),
    ];
    for source in cases {
        let rendered = render(&source, 80, &Theme::dark(), &RenderOptions::completed());
        for span in &rendered.blocks {
            let view = view(&source, span.ordinal, BlockMode::Rendered, None);
            assert_eq!(view.kind, span.kind, "{source:?} #{}", span.ordinal);
        }
        let past = render_block(
            &source,
            rendered.blocks.len(),
            &Theme::dark(),
            &BlockRender {
                mode: BlockMode::Rendered,
                wrap: None,
            },
        );
        assert!(
            past.is_none(),
            "{source:?}: a block the Stage does not show"
        );
    }
}

/// F2: a fence opened inside a list item or quote is still a fence: its interior
/// is shown (and copied) exactly as written, never repaired like a table.
#[test]
fn a_fence_inside_a_list_or_quote_is_never_repaired_like_a_table() {
    for (opener, indent, closer) in [
        ("- ```md", "  ", "  ```"),
        ("1. ```md", "   ", "   ```"),
        ("> ```md", "> ", "> ```"),
        ("* ~~~md", "  ", "  ~~~"),
    ] {
        let source = format!("{opener}\n{indent}|a|\n{indent}|---|\n{indent}|1|2|\n{closer}\n");
        let rendered = render(&source, 80, &Theme::dark(), &RenderOptions::completed());
        assert_eq!(rendered.blocks.len(), 1, "{source:?}");
        assert!(
            matches!(rendered.blocks[0].kind, BlockKind::Code { lines: 3, .. }),
            "{source:?}"
        );
        let rows: Vec<String> = rendered.lines.iter().map(text).collect();
        assert!(rows.iter().any(|row| row.contains("│|a|  ")), "{rows:?}");
        assert!(
            rows.iter().all(|row| !row.contains("|a| |")),
            "no widened header: {rows:?}"
        );
        assert_eq!(
            view(&source, 0, BlockMode::Rendered, None).source,
            "|a|\n|---|\n|1|2|",
            "{source:?}"
        );
    }
}
