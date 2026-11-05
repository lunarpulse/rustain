//! K9 links and OSC 8, K13 host suffix (Story 21.12, AC7 and AC13, owner
//! ruling OR-21-12-1; slice 12-C).
//!
//! Both hosts (`render_with_search`, `render_attached`), `osc8_links` false and
//! true, painted cells and per-cell OSC 8 symbols read back from a `TestBackend`
//! buffer. No expected row is produced by the formula under test.

use std::collections::{BTreeMap, HashMap};

use ratatui::buffer::{Buffer, CellDiffOption};
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::{Terminal, backend::TestBackend};
use rustain::adapters::tui::state::{TabRenderState, TuiState};
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::chat_pane;
use rustain::domain::clock::MockClock;
use rustain::domain::models::{
    Conversation, MessageRole, StopReason, StreamingState, Turn, ViewState,
};
use unicode_width::UnicodeWidthStr;

use super::support::*;
use crate::render_helpers::{make_conversation, make_msg, make_turn, prose};

const OPEN: &str = "\x1b]8;;";
const CLOSE: &str = "\x1b]8;;\x07";

#[derive(Clone, Copy, Debug)]
enum Host {
    Local,
    Attached,
}

const HOSTS: [Host; 2] = [Host::Local, Host::Attached];

fn paint(host: Host, conversation: &Conversation, osc8: bool, width: u16, height: u16) -> Pane {
    let theme = Theme::dark();
    match host {
        Host::Local => paint_local_osc8(
            conversation,
            None,
            &StreamingState::default(),
            None,
            &theme,
            width,
            height,
            osc8,
        ),
        Host::Attached => paint_attached_osc8(
            conversation,
            &StreamingState::default(),
            &theme,
            width,
            height,
            osc8,
        ),
    }
}

/// `(url, glyph)` of one painted cell symbol; `url` is `None` for a plain cell.
fn split(symbol: &str) -> (Option<&str>, &str) {
    match symbol.strip_prefix(OPEN) {
        Some(rest) => {
            let (url, rest) = rest.split_once('\x07').expect("BEL terminates the target");
            let glyph = rest
                .strip_suffix(CLOSE)
                .expect("balanced one-cell envelope");
            (Some(url), glyph)
        }
        None => (None, symbol),
    }
}

/// Painted rows with every OSC 8 envelope removed and continuation cells of
/// wide glyphs skipped.
fn plain_rows(buffer: &Buffer) -> Vec<String> {
    (buffer.area.top()..buffer.area.bottom())
        .map(|y| {
            let mut out = String::new();
            let mut x = buffer.area.left();
            while x < buffer.area.right() {
                let (_, glyph) = split(buffer[(x, y)].symbol());
                out.push_str(glyph);
                x += u16::try_from(glyph.width().max(1)).unwrap_or(1);
            }
            out.trim_end().to_owned()
        })
        .collect()
}

/// Gutter-stripped message rows with the trailing blank rows dropped.
fn body(pane: &Pane) -> Vec<String> {
    let mut rows: Vec<String> = plain_rows(&pane.buffer)
        .into_iter()
        .filter(|row| row.starts_with('│'))
        .map(|row| {
            row.strip_prefix("│ ")
                .or_else(|| row.strip_prefix('│'))
                .unwrap_or(&row)
                .to_owned()
        })
        .collect();
    while rows.last().is_some_and(String::is_empty) {
        rows.pop();
    }
    rows
}

#[derive(Debug, PartialEq)]
struct Linked {
    x: u16,
    y: u16,
    url: String,
    glyph: String,
    forced: Option<u16>,
}

/// Every cell whose symbol carries an OSC 8 envelope.
fn linked(buffer: &Buffer) -> Vec<Linked> {
    let mut out = Vec::new();
    for y in buffer.area.top()..buffer.area.bottom() {
        for x in buffer.area.left()..buffer.area.right() {
            let cell = &buffer[(x, y)];
            if let (Some(url), glyph) = split(cell.symbol()) {
                out.push(Linked {
                    x,
                    y,
                    url: url.to_owned(),
                    glyph: glyph.to_owned(),
                    forced: match cell.diff_option {
                        CellDiffOption::ForcedWidth(width) => Some(width.get()),
                        _ => None,
                    },
                });
            }
        }
    }
    out
}

/// Linked glyphs per row, in column order.
fn linked_by_row(buffer: &Buffer) -> BTreeMap<u16, String> {
    let mut rows: BTreeMap<u16, String> = BTreeMap::new();
    for cell in linked(buffer) {
        rows.entry(cell.y).or_default().push_str(&cell.glyph);
    }
    rows
}

fn any_escape(buffer: &Buffer) -> bool {
    (buffer.area.top()..buffer.area.bottom()).any(|y| {
        (buffer.area.left()..buffer.area.right()).any(|x| buffer[(x, y)].symbol().contains('\x1b'))
    })
}

fn assistant(markdown: &str) -> Conversation {
    let mut message = make_msg("k", MessageRole::Assistant);
    message.content = markdown.to_owned();
    let turn = make_turn("k", vec![prose(markdown)], Some(StopReason::EndTurn));
    make_conversation(vec![message], vec![turn])
}

/// Join one-line paragraphs with blank lines, as the model would.
fn paragraphs(items: &[&str]) -> String {
    items.join("\n\n")
}

/// The painted rows of `paragraphs(rows)`: one blank row between paragraphs.
fn with_blank_rows(rows: &[&str]) -> Vec<String> {
    paragraphs(rows).split('\n').map(str::to_owned).collect()
}

// ── K13 ──────────────────────────────────────────────────────────────────────

const K13_SOURCE: &[&str] = &[
    "[claude.ai](https://claude.ai@evil.test/)",
    "[docs](https://www.example.com)",
    "[example.com](https://www.example.com/x)",
    "[notevil.test](https://evil.test)",
    "[evil.test.com](https://evil.test)",
    "<https://example.com/a>",
    "[例え](https://例え.test/)",
    "[ip](http://127.0.0.1:8080/p)",
    "[v6](http://[::1]/)",
    "[see https://example.com/a](https://example.com/b)",
    "[evil.test.\u{7}paypal.com](https://evil.test/)",
    "[evil.test\u{200B}.paypal.com](https://evil.test/)",
    "[https://claude.ai](https://evil.test)",
    "[cl\u{430}ude.ai](https://claude.ai)",
    "<https://例え.test/>",
];

// `例え` as IDNA: python3 -c "print('例え.test'.encode('idna'))"
const K13_ROWS: &[&str] = &[
    "claude.ai (evil.test)",
    "docs (example.com)",
    "example.com",
    "notevil.test (evil.test)",
    "evil.test.com (evil.test)",
    "https://example.com/a",
    "例え (xn--r8jz45g.test)",
    "ip (127.0.0.1)",
    "v6 ([::1])",
    "see https://example.com/a",
    "evil.test.paypal.com (evil.test)",
    "evil.test.paypal.com (evil.test)",
    "https://claude.ai (evil.test)",
    "cl\u{430}ude.ai (claude.ai)",
    "https://例え.test/",
];

#[test]
fn k13_both_hosts_paint_label_and_host_suffix_for_both_flag_values() {
    let conversation = assistant(&paragraphs(K13_SOURCE));
    let expected = with_blank_rows(K13_ROWS);
    for host in HOSTS {
        let plain = paint(host, &conversation, false, 80, 60);
        let linked_pane = paint(host, &conversation, true, 80, 60);
        assert_eq!(body(&plain), expected, "{host:?} osc8_links=false");
        assert_eq!(body(&linked_pane), expected, "{host:?} osc8_links=true");
        assert_eq!(
            plain.total_height, linked_pane.total_height,
            "{host:?}: the OSC 8 flag must not change a row height"
        );
        assert!(!any_escape(&plain.buffer), "{host:?}: plain terminal");
        assert!(!linked(&linked_pane.buffer).is_empty(), "{host:?}");
    }
}

#[test]
fn k13_suffix_is_dim_meta_and_outside_the_osc8_run() {
    let theme = Theme::dark();
    let conversation = assistant("[claude.ai](https://evil.test/x)");
    for host in HOSTS {
        let pane = paint(host, &conversation, true, 80, 10);
        let by_row = linked_by_row(&pane.buffer);
        assert_eq!(
            by_row.values().collect::<Vec<_>>(),
            ["claude.ai"],
            "{host:?}: only the label is linked"
        );
        let label_y = *by_row.keys().next().unwrap();
        let rows = plain_rows(&pane.buffer);
        let row = &rows[usize::from(label_y)];
        assert_eq!(row, "│ claude.ai (evil.test)", "{host:?}");
        // Gutter `│ ` then the nine label cells, then the suffix cells.
        let first_suffix = 2 + "claude.ai".len();
        for x in first_suffix..first_suffix + " (evil.test)".len() {
            let cell = &pane.buffer[(u16::try_from(x).unwrap(), label_y)];
            assert!(
                !cell.symbol().contains('\x1b'),
                "{host:?}: suffix cell {x} must not carry OSC 8"
            );
            assert_eq!(cell.style().fg, Some(theme.colors.fg_muted), "{host:?} {x}");
            assert!(
                !cell.style().add_modifier.contains(Modifier::UNDERLINED),
                "{host:?} {x}"
            );
            assert!(
                cell.style()
                    .add_modifier
                    .contains(theme.typography.meta.add_modifier),
                "{host:?} {x}: typography.meta applies"
            );
        }
        for cell in linked(&pane.buffer) {
            assert_eq!(cell.url, "https://evil.test/x");
            assert_eq!(cell.forced, Some(1));
            let style = pane.buffer[(cell.x, cell.y)].style();
            assert_eq!(style.fg, Some(theme.colors.accent), "{host:?} label colour");
            assert!(
                style.add_modifier.contains(Modifier::UNDERLINED),
                "{host:?}"
            );
        }
    }
}

#[test]
fn k13_copy_returns_the_message_source_with_the_full_urls() {
    let source = paragraphs(K13_SOURCE);
    let conversation = assistant(&source);
    let state = TuiState::new(80, 24);
    assert_eq!(state.resolve_copy_content(&conversation), source);
    assert!(source.contains("https://claude.ai@evil.test/"));
}

#[test]
fn k13_unsafe_and_non_http_destinations_keep_the_full_visible_url() {
    let source = paragraphs(&[
        "[x](javascript:alert(1))",
        "[rel](docs/readme.md)",
        "[mail](mailto:a@b.test)",
        "<mailto:a@b.test>",
    ]);
    let expected = with_blank_rows(&[
        "x (javascript:alert(1))",
        "rel (docs/readme.md)",
        "mail (mailto:a@b.test)",
        "mailto:a@b.test",
    ]);
    let conversation = assistant(&source);
    for host in HOSTS {
        for osc8 in [false, true] {
            let pane = paint(host, &conversation, osc8, 80, 30);
            assert_eq!(body(&pane), expected, "{host:?} osc8={osc8}");
            assert!(
                !any_escape(&pane.buffer),
                "{host:?} osc8={osc8}: unsafe destinations are never linked"
            );
        }
    }
}

#[test]
fn k13_legacy_assistant_and_system_messages_follow_the_same_rule() {
    for role in [MessageRole::Assistant, MessageRole::System] {
        let mut message = make_msg("m1", role);
        message.content = "[claude.ai](https://evil.test/x)".to_owned();
        let conversation = make_conversation(vec![message], vec![]);
        for host in HOSTS {
            for osc8 in [false, true] {
                let pane = paint(host, &conversation, osc8, 80, 12);
                let rows = plain_rows(&pane.buffer);
                assert!(
                    rows.iter().any(|row| row == "claude.ai (evil.test)"),
                    "{role:?} {host:?} osc8={osc8}: {rows:?}"
                );
                let linked_text: Vec<String> = linked_by_row(&pane.buffer).into_values().collect();
                if osc8 {
                    assert_eq!(linked_text, ["claude.ai"], "{role:?} {host:?}");
                } else {
                    assert!(linked_text.is_empty() && !any_escape(&pane.buffer));
                }
            }
        }
    }
}

/// The two streaming surfaces: the attached open text buffer and the local
/// open prose that rides under the open turn.
fn stream_panes(markdown: &str, osc8: bool) -> [(&'static str, Pane); 2] {
    let theme = Theme::dark();
    let empty = make_conversation(vec![], vec![]);
    let open_turn: Turn = make_turn("open", vec![], None);
    let attached = streaming_buffer(markdown);
    let local = StreamingState::default();
    [
        (
            "attached",
            paint_attached_osc8(&empty, &attached, &theme, 80, 10, osc8),
        ),
        (
            "local",
            paint_local_osc8(
                &empty,
                Some(&open_turn),
                &local,
                Some(markdown),
                &theme,
                80,
                10,
                osc8,
            ),
        ),
    ]
}

#[test]
fn k13_streaming_surfaces_carry_the_suffix_and_the_link() {
    // (source, painted row, linked glyphs with OSC 8). A half-typed
    // destination stays literal: no run, no escape, no leak.
    let cases = [
        (
            "[claude.ai](https://evil.test/x)",
            "claude.ai (evil.test)",
            "claude.ai",
        ),
        ("[a](https://e.te", "[a](https://e.te", ""),
    ];
    for (markdown, row, label) in cases {
        for osc8 in [false, true] {
            for (surface, pane) in stream_panes(markdown, osc8) {
                let rows = plain_rows(&pane.buffer);
                let found = rows.iter().any(|r| r.trim_start_matches("│ ") == row);
                assert!(found, "{surface} osc8={osc8}: {rows:?}");
                let linked: String = linked_by_row(&pane.buffer).into_values().collect();
                assert_eq!(linked, if osc8 { label } else { "" }, "{surface}");
                let escaped = any_escape(&pane.buffer);
                assert_eq!(escaped, osc8 && !label.is_empty(), "{surface} osc8={osc8}");
            }
        }
    }
}

// ── K9 ───────────────────────────────────────────────────────────────────────

#[test]
fn k9_ascii_label_is_linked_on_every_glyph_and_geometry_ignores_the_flag() {
    let conversation = assistant("see [claude.ai](https://evil.test/x) now");
    for host in HOSTS {
        let plain = paint(host, &conversation, false, 80, 10);
        let on = paint(host, &conversation, true, 80, 10);
        assert_eq!(
            plain_rows(&plain.buffer),
            plain_rows(&on.buffer),
            "{host:?}"
        );
        assert_eq!(plain.total_height, on.total_height, "{host:?}");
        assert!(
            !any_escape(&plain.buffer),
            "{host:?}: no escape without OSC 8"
        );
        let cells = linked(&on.buffer);
        assert_eq!(
            cells.iter().map(|c| c.glyph.as_str()).collect::<String>(),
            "claude.ai",
            "{host:?}"
        );
        for cell in &cells {
            assert_eq!(cell.url, "https://evil.test/x");
            assert_eq!(cell.forced, Some(1));
            assert_eq!(
                on.buffer[(cell.x, cell.y)].symbol(),
                format!("{OPEN}https://evil.test/x\x07{}{CLOSE}", cell.glyph),
                "balanced one-cell envelope"
            );
        }
        // Neighbouring prose cells stay plain.
        assert_eq!(linked_by_row(&on.buffer).len(), 1);
        assert_eq!(
            body(&on)[0],
            "see claude.ai (evil.test) now",
            "{host:?}: suffix sits between the label and the text"
        );
    }
}

#[test]
fn k9_wrapped_ascii_label_is_linked_on_every_row_but_not_across_the_break() {
    let conversation = assistant("[alpha beta gamma delta](https://example.com/z)");
    for host in HOSTS {
        // Pane width 16: a 14-cell content column (the gutter takes two).
        let pane = paint(host, &conversation, true, 16, 12);
        assert_eq!(
            body(&pane),
            ["alpha beta", "gamma delta", "(example.com)"],
            "{host:?}"
        );
        let by_row: Vec<String> = linked_by_row(&pane.buffer).into_values().collect();
        assert_eq!(by_row, ["alpha beta", "gamma delta"], "{host:?}");
    }
}

#[test]
fn k9_cjk_label_wraps_across_rows_with_real_cell_widths() {
    let label = "日本語のテキスト表示確認";
    let conversation = assistant(&format!("[{label}](https://example.com/p)"));
    for host in HOSTS {
        let plain = paint(host, &conversation, false, 12, 20);
        let on = paint(host, &conversation, true, 12, 20);
        assert_eq!(
            plain_rows(&plain.buffer),
            plain_rows(&on.buffer),
            "{host:?}"
        );
        assert_eq!(plain.total_height, on.total_height, "{host:?}");
        let cells = linked(&on.buffer);
        assert_eq!(
            cells.iter().map(|c| c.glyph.as_str()).collect::<String>(),
            label,
            "{host:?}: every glyph is linked"
        );
        let mut rows_with_links: Vec<u16> = cells.iter().map(|c| c.y).collect();
        rows_with_links.dedup();
        assert!(
            rows_with_links.len() >= 2,
            "{host:?}: wrapped label spans rows"
        );
        for cell in &cells {
            assert_eq!(
                cell.forced,
                Some(2),
                "{host:?}: {} keeps its width",
                cell.glyph
            );
            // The continuation cell is left alone (not wrapped, not forced).
            let next = &on.buffer[(cell.x + 1, cell.y)];
            assert!(!next.symbol().contains('\x1b'), "{host:?} {}", cell.glyph);
            assert_eq!(next.diff_option, CellDiffOption::None, "{host:?}");
        }
        for y in on.buffer.area.top()..on.buffer.area.bottom() {
            let mut cells = 0usize;
            let mut x = on.buffer.area.left();
            while x < on.buffer.area.right() {
                let width =
                    u16::try_from(split(on.buffer[(x, y)].symbol()).1.width().max(1)).unwrap_or(1);
                cells += usize::from(width);
                x += width;
            }
            assert!(cells <= 12, "{host:?} row {y} within the pane: {cells}");
        }
    }
}

#[test]
fn k9_autolink_and_image_present_as_specified() {
    let conversation = assistant(&paragraphs(&[
        "<https://example.com/a>",
        "![a chart](https://img.test/c.png)",
        "![](https://img.test/d.png)",
    ]));
    for host in HOSTS {
        let pane = paint(host, &conversation, true, 80, 20);
        assert_eq!(
            body(&pane),
            with_blank_rows(&["https://example.com/a", "[image: a chart]", "[image]"]),
            "{host:?}"
        );
        let by_row: Vec<String> = linked_by_row(&pane.buffer).into_values().collect();
        assert_eq!(
            by_row,
            ["https://example.com/a"],
            "{host:?}: the autolink is linked, images never are"
        );
    }
}

#[test]
fn k9_runs_follow_the_viewport_when_the_pane_is_scrolled_to_the_bottom() {
    let mut items: Vec<String> = (0..12).map(|n| format!("filler paragraph {n}")).collect();
    items.push("[claude.ai](https://evil.test/x)".to_owned());
    let refs: Vec<&str> = items.iter().map(String::as_str).collect();
    let conversation = assistant(&paragraphs(&refs));
    for host in HOSTS {
        let pane = paint(host, &conversation, true, 40, 8);
        let rows = plain_rows(&pane.buffer);
        let by_row = linked_by_row(&pane.buffer);
        assert_eq!(by_row.len(), 1, "{host:?}: {rows:?}");
        let (&y, text) = by_row.iter().next().unwrap();
        assert_eq!(text, "claude.ai");
        assert_eq!(rows[usize::from(y)], "│ claude.ai (evil.test)", "{host:?}");
    }
}

/// `render_with_search` with a caller-chosen view state, scroll position and
/// stream.
#[allow(clippy::too_many_arguments)]
fn paint_view(
    conversation: &Conversation,
    streaming: &StreamingState,
    view: &ViewState,
    auto_scroll: bool,
    scroll_offset: usize,
    width: u16,
    height: u16,
    osc8: bool,
) -> Pane {
    let clock = MockClock::at_wall_ms(1_700_000_000_000);
    clock.set_frame(3);
    let mut total_height = 0usize;
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| {
            let mut tab = TabRenderState {
                osc8_links: osc8,
                ..TabRenderState::default()
            };
            let result = chat_pane::render_with_search(
                frame,
                Rect::new(0, 0, width, height),
                conversation,
                None,
                streaming,
                view,
                &clock,
                scroll_offset,
                auto_scroll,
                &Theme::dark(),
                &mut tab,
                &std::collections::HashMap::new(),
                &std::collections::BTreeMap::new(),
                None,
                None,
                &[],
                &[],
                None,
                None,
                None,
                None,
            );
            total_height = result.total_content_height;
        })
        .unwrap();
    Pane {
        buffer: terminal.backend().buffer().clone(),
        total_height,
    }
}

#[test]
fn k9_focused_turn_arrow_shifts_the_first_row_run_by_two_cells() {
    let conversation = assistant("[claude.ai](https://evil.test/x) tail");
    let view = ViewState {
        focused_turn: Some(conversation.turns[0].id.clone()),
        ..ViewState::default()
    };
    let streaming = StreamingState::default();
    let pane = paint_view(&conversation, &streaming, &view, true, 0, 60, 8, true);
    let buffer = pane.buffer;
    let rows = plain_rows(&buffer);
    assert!(rows[0].starts_with("▶ │ claude.ai (evil.test)"), "{rows:?}");
    let cells = linked(&buffer);
    assert_eq!(
        cells.iter().map(|c| c.glyph.as_str()).collect::<String>(),
        "claude.ai"
    );
    let arrow_and_gutter = "▶ │ ".width();
    assert_eq!(usize::from(cells[0].x), arrow_and_gutter);
    assert!(cells.iter().all(|c| c.y == 0));
}

#[test]
fn k9_the_new_content_indicator_row_drops_the_runs_it_covers() {
    // A link on the last visible row is replaced by the jump indicator.
    let mut items: Vec<String> = (0..12).map(|n| format!("filler {n}")).collect();
    items.push("[claude.ai](https://evil.test/x)".to_owned());
    let refs: Vec<&str> = items.iter().map(String::as_str).collect();
    let conversation = assistant(&paragraphs(&refs));
    let streaming = StreamingState {
        is_streaming: true,
        ..StreamingState::default()
    };
    let at = |auto_scroll, offset, height| {
        let view = ViewState::default();
        paint_view(
            &conversation,
            &streaming,
            &view,
            auto_scroll,
            offset,
            60,
            height,
            true,
        )
    };
    // A tall pane shows every row from the top: find the link row and the total.
    let tall = at(true, 0, 80);
    let link_row = plain_rows(&tall.buffer)
        .iter()
        .position(|row| row == "│ claude.ai (evil.test)")
        .expect("the link row");
    // Scroll so the link is the last visible row of a 6-row viewport.
    let offset = tall.total_height - 1 - link_row;
    let covered = at(false, offset, 6);
    let rows = plain_rows(&covered.buffer);
    assert!(rows[5].contains("New content below"), "{rows:?}");
    assert!(
        linked(&covered.buffer).is_empty() && !any_escape(&covered.buffer),
        "the indicator replaced the link row: {rows:?}"
    );
    // Control: one row lower the same link is visible row 4 and is linked.
    let shifted = at(false, offset - 1, 6);
    let rows = plain_rows(&shifted.buffer);
    assert!(rows[5].contains("New content below"), "{rows:?}");
    assert_eq!(rows[4], "│ claude.ai (evil.test)", "{rows:?}");
    let cells = linked(&shifted.buffer);
    assert_eq!(
        cells.iter().map(|c| c.glyph.as_str()).collect::<String>(),
        "claude.ai",
        "{rows:?}"
    );
    assert!(cells.iter().all(|c| c.y == 4), "{cells:?}");
}

#[test]
fn k9_an_oversized_target_is_never_linked_and_its_url_stays_visible() {
    let url = format!("https://e.test/{}", "a".repeat(5000));
    let conversation = assistant(&format!("[a]({url})"));
    for host in HOSTS {
        let pane = paint(host, &conversation, true, 80, 120);
        assert!(!any_escape(&pane.buffer), "{host:?}");
        let shown = body(&pane).concat().replace(' ', "");
        assert_eq!(shown, format!("a({url})"), "{host:?}");
    }
}

#[test]
fn k9_a_hostile_label_and_url_never_reach_the_terminal() {
    // BEL, ESC and C1 bytes in the label (its `]` is escaped so the link stays
    // whole); a hostile destination is rejected and shown, sanitised.
    let conversation = assistant(&paragraphs(&[
        "[a\u{7}b\u{1b}\\]8;;x\u{9c}c](https://evil.test/)",
        "[a\u{7}b](https://evil.test/\u{1b}]8;;)",
    ]));
    for host in HOSTS {
        for osc8 in [false, true] {
            let pane = paint(host, &conversation, osc8, 80, 10);
            let rows = plain_rows(&pane.buffer);
            let clean = rows.iter().all(|row| !row.chars().any(char::is_control));
            assert!(clean, "{host:?} osc8={osc8}: {rows:?}");
            assert_eq!(body(&pane)[0], "ab]8;;xc (evil.test)", "{host:?} {osc8}");
            let label: String = linked_by_row(&pane.buffer).into_values().collect();
            assert_eq!(label, if osc8 { "ab]8;;xc" } else { "" }, "{host:?}");
            let cells = linked(&pane.buffer);
            assert!(
                cells.iter().all(|c| c.url == "https://evil.test/"),
                "{cells:?}"
            );
        }
    }
}

/// Cross-story (21-1 toast layer x 21-12 link encoding): both hosts paint the
/// chat pane first and the toast layer afterwards, so a toast lands on cells
/// that already carry per-glyph OSC 8 envelopes. `Clear` + the toast text must
/// replace those cells whole: no envelope inside a toast cell, every other
/// envelope still balanced and identical to the toast-free paint.
#[test]
fn k9_a_toast_painted_over_link_runs_leaves_no_envelope_under_or_around_it() {
    use rustain::adapters::tui::layout::compute_layout;
    use rustain::adapters::tui::notification_log::LogFacts;
    use rustain::adapters::tui::notifications::{self, NoticeSeverity, NoticeSource};
    use rustain::adapters::tui::widgets::toast_layer;
    use rustain::domain::models::visual::DensityMode;

    let row = "[docs](https://e.test/a) [more docs](https://e.test/b) [x](https://e.test/c) ";
    let paragraph = row.repeat(12);
    let conversation = assistant(&paragraphs(&[paragraph.trim_end(); 6]));
    // 80x24 paints the one-row form on the pane's last row; 130x40 the bordered
    // three-row form, which lands on full link rows.
    for (width, height, wide) in [(80u16, 24u16, false), (130u16, 40u16, true)] {
        let paint_with_toast = |toast: bool| {
            let mut state = TuiState::new(width, height);
            if toast {
                notifications::notify(
                    &mut state,
                    NoticeSource::System,
                    NoticeSeverity::Error,
                    "the daemon went away",
                );
            }
            let layout = compute_layout(
                Rect::new(0, 0, width, height),
                &state.theme,
                "",
                1,
                false,
                DensityMode::Focus,
            )
            .expect("the terminal is at or above the 60x16 floor");
            notifications::begin_draw(&mut state);
            state
                .notifications
                .set_capacity(toast_layer::capacity_for(layout.chat_pane.height));
            let theme = Theme::dark();
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| {
                    let mut tab = TabRenderState {
                        osc8_links: true,
                        ..TabRenderState::default()
                    };
                    chat_pane::render_attached(
                        frame,
                        layout.chat_pane,
                        &conversation,
                        &StreamingState::default(),
                        // Scrolled off the bottom: the last three rows are
                        // full link rows, not the short last line.
                        &rustain::domain::models::ViewState {
                            scroll_offset: 4,
                            mode: rustain::domain::models::AnchorMode::Reading,
                            ..Default::default()
                        },
                        &theme,
                        &mut tab,
                        &HashMap::new(),
                        &BTreeMap::new(),
                        &HashMap::new(),
                    );
                    toast_layer::render(
                        frame,
                        layout.chat_pane,
                        &state.notifications,
                        &theme,
                        &LogFacts::default(),
                    );
                })
                .unwrap();
            terminal.backend().buffer().clone()
        };

        let bare = paint_with_toast(false);
        let toasted = paint_with_toast(true);

        let covered: Vec<(u16, u16)> = (0..height)
            .flat_map(|y| (0..width).map(move |x| (x, y)))
            .filter(|&(x, y)| bare[(x, y)].symbol() != toasted[(x, y)].symbol())
            .collect();
        assert!(!covered.is_empty(), "the toast painted nothing");
        assert!(
            !wide
                || covered
                    .iter()
                    .any(|&(x, y)| split(bare[(x, y)].symbol()).0.is_some()),
            "the wide toast must sit over at least one linked cell for this test to bite"
        );
        for &(x, y) in &covered {
            assert!(
                !toasted[(x, y)].symbol().contains('\x1b'),
                "envelope under the toast at ({x},{y}): {:?}",
                toasted[(x, y)].symbol()
            );
        }
        // Every surviving envelope is whole (one open, one close, glyph between)
        // and identical to the toast-free paint.
        for y in 0..height {
            for x in 0..width {
                let symbol = toasted[(x, y)].symbol();
                if symbol.contains('\x1b') {
                    assert_eq!(symbol, bare[(x, y)].symbol(), "({x},{y})");
                    assert_eq!(symbol.matches(OPEN).count(), 2, "({x},{y}): {symbol:?}");
                    let _ = split(symbol);
                }
            }
        }
    }
}
