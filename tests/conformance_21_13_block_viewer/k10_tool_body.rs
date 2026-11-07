//! K10 (AC12): S-TOOL's Output tab is a Markdown body: raw by default, `v`
//! renders the sanitized text, only wide blocks scroll sideways while rendered,
//! and copy is always the original bytes.
use std::cell::RefCell;

use ratatui::buffer::Buffer;
use rustain::adapters::tui::{
    app::InputAction,
    clipboard::ClipboardResult,
    keymap::Key,
    tool_inspector::{self, ToolTab, layout},
};
use rustain::domain::models::{InvocationStatus as S, WriteDiffState as D};
use serde_json::json;

use super::support::Fixture;

/// Heading, prose with a link, an 8-column table, 120-cell code, plus an escape
/// sequence, a C1 control and a bidi override that must never reach a cell.
fn markdown() -> String {
    let wide = |c: char| c.to_string().repeat(10);
    let header = (1..=8)
        .map(|n| format!("c{n}"))
        .collect::<Vec<_>>()
        .join(" | ");
    let rule = ["---"; 8].join(" | ");
    let cells = "abcdefgh".chars().map(wide).collect::<Vec<_>>().join(" | ");
    format!(
        "# Heading\n\nProse with a [link](https://example.com/page) that is long enough to wrap \
         at the inner width of the frame because prose always wraps.\n\n\
         | {header} |\n| {rule} |\n| {cells} |\n\n\
         ```rust\nlet wide = \"{}TAIL120\";\n```\n\n\x1b[31mred\x1b[0m C1\u{85}x bidi\u{202E}end\n",
        "x".repeat(102)
    )
}

fn open(width: u16, height: u16) -> Fixture {
    let mut f = Fixture::single(
        width,
        height,
        "Bash",
        json!({"command":"inspect"}),
        S::Success,
        Some((&markdown(), D::NotAWrite)),
    )
    .0;
    f.open();
    f
}

fn rows(f: &Fixture) -> Vec<String> {
    (0..f.body_rect().height).map(|y| f.body_row(y)).collect()
}

fn status(f: &Fixture) -> String {
    let status = layout::parts(f.frame_rect()).status.expect("status row");
    let buffer: &Buffer = f.buffer();
    (status.x..status.right())
        .map(|x| buffer[(x, status.y)].symbol())
        .collect()
}

fn rail(f: &Fixture) -> String {
    let rail = layout::parts(f.frame_rect()).rail;
    let buffer: &Buffer = f.buffer();
    (rail.x..rail.right())
        .map(|x| buffer[(x, rail.y)].symbol())
        .collect::<String>()
        .trim()
        .to_owned()
}

fn keys(f: &mut Fixture, keys: &[Key]) {
    for key in keys {
        f.press(*key);
    }
    f.paint();
}

/// No escape, C1 or bidi control character in any painted cell.
fn assert_clean(f: &Fixture) {
    for cell in f.buffer().content() {
        for c in cell.symbol().chars() {
            assert!(
                !c.is_control()
                    && !('\u{202a}'..='\u{202e}').contains(&c)
                    && !('\u{2066}'..='\u{2069}').contains(&c)
                    && !matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}'),
                "{c:?} reached a cell"
            );
        }
    }
}

fn row_with<'a>(rows: &'a [String], needle: &str) -> &'a str {
    rows.iter()
        .find(|row| row.contains(needle))
        .unwrap_or_else(|| panic!("no row has {needle:?}: {rows:#?}"))
}

#[test]
fn output_opens_raw_and_v_renders_the_sanitized_text_as_markdown() {
    let mut f = open(80, 40);
    let raw = rows(&f);
    assert!(
        raw.iter().any(|row| row.starts_with("# Heading")),
        "raw by default"
    );
    assert!(rail(&f).contains("[v] Rendered"), "{}", rail(&f));
    assert!(status(&f).contains("Wrap on"), "{}", status(&f));
    assert_clean(&f);
    keys(&mut f, &[Key::Char('v')]);
    let rendered = rows(&f);
    assert!(
        !rendered.iter().any(|row| row.contains("# Heading")),
        "{rendered:#?}"
    );
    assert!(
        rendered.iter().any(|row| row.contains("Heading")),
        "{rendered:#?}"
    );
    // Links stay explicit (MW-7): the destination is readable.
    assert!(
        rendered
            .iter()
            .any(|row| row.contains("link (https://example.com/page)")),
        "{rendered:#?}"
    );
    assert!(rail(&f).contains("[v] Source"), "{}", rail(&f));
    assert!(
        !status(&f).contains("Wrap") && status(&f).contains("Rendered"),
        "w is not offered while rendered: {}",
        status(&f)
    );
    assert!(!rail(&f).contains("Wrap"), "{}", rail(&f));
    assert_clean(&f);
    // A rendered table and code box exist; the control characters are gone.
    assert!(
        rendered.iter().any(|row| row.starts_with('┌'))
            && rendered
                .iter()
                .any(|row| row.starts_with("│ c1") && row.contains("c2")),
        "{rendered:#?}"
    );
    assert!(
        rendered.iter().any(|row| row.contains("red C1")),
        "{rendered:#?}"
    );
    keys(&mut f, &[Key::Char('v')]);
    assert_eq!(rows(&f), raw, "back to the raw rows");
    assert_clean(&f);
}

#[test]
fn horizontal_scroll_moves_only_wide_block_rows() {
    let mut f = open(80, 40);
    keys(&mut f, &[Key::Char('v')]);
    let before = rows(&f);
    assert!(f.tool().wrap, "21-5's wrap stays on");
    keys(&mut f, &[Key::Char('l'), Key::Char('l')]);
    let after = rows(&f);
    assert!(
        f.tool().wrap,
        "rendering never consumes 21-5's wrap setting"
    );
    let moved: Vec<usize> = (0..before.len())
        .filter(|&i| before[i] != after[i])
        .collect();
    assert!(!moved.is_empty(), "control: some rows moved");
    for needle in ["c1", "let wide"] {
        assert!(
            moved.iter().any(|&i| before[i].contains(needle)),
            "the {needle:?} row must move: {moved:?}"
        );
    }
    for &i in &moved {
        let text = &before[i];
        assert!(
            text.contains('│') || text.contains('┌') || text.contains('└') || text.contains('├'),
            "only table/code rows move, not {text:?}"
        );
    }
    // Prose rows are cell-identical before and after.
    for needle in ["Heading", "Prose with a", "inner width"] {
        assert_eq!(row_with(&before, needle), row_with(&after, needle));
    }
    keys(&mut f, &[Key::Char('$')]);
    assert!(
        rows(&f).iter().any(|row| row.contains("TAIL120")),
        "{:#?}",
        rows(&f)
    );
    keys(&mut f, &[Key::Char('0')]);
    assert_eq!(rows(&f), before);
}

#[test]
fn raw_scrolls_every_row_only_with_wrap_off() {
    let mut f = open(80, 40);
    let raw = rows(&f);
    keys(&mut f, &[Key::Char('l')]);
    assert_eq!(rows(&f), raw, "wrap on: h/l are inapplicable");
    keys(&mut f, &[Key::Char('w')]);
    let unwrapped = rows(&f);
    assert!(!f.tool().wrap);
    keys(&mut f, &[Key::Char('l')]);
    let shifted = rows(&f);
    for (index, row) in unwrapped.iter().enumerate() {
        if row.trim().is_empty() {
            continue;
        }
        assert_ne!(&shifted[index], row, "row {index} {row:?} must shift");
    }
    keys(&mut f, &[Key::Char('w')]);
    assert!(f.tool().wrap);
    let wrapped = rows(&f);
    keys(&mut f, &[Key::Char('l')]);
    assert_eq!(
        rows(&f),
        wrapped,
        "wrap on again: inapplicable and the offset is gone"
    );
}

#[test]
fn copy_is_the_original_bytes_in_both_presentations() {
    let mut f = open(80, 40);
    let copied = RefCell::new(Vec::new());
    let copier = |text: &str| {
        copied.borrow_mut().push(text.to_owned());
        ClipboardResult::Osc52Success
    };
    for _ in 0..2 {
        let InputAction::CopyToolSource = f.press(Key::Char('c')) else {
            panic!("c must request the tool source");
        };
        let tab = f.tabs.active_tab();
        assert!(tool_inspector::copy::copy_source(
            &mut f.h.state,
            &tab.conversation,
            Some(&tab.reducer),
            &copier,
        ));
        keys(&mut f, &[Key::Char('v')]);
    }
    let copied = copied.into_inner();
    assert_eq!(copied, [markdown(), markdown()]);
    assert!(
        copied[0].contains('\x1b') && copied[0].contains('\u{202E}'),
        "control: original bytes"
    );
}

#[test]
fn other_tabs_are_not_markdown_bodies() {
    let mut f = open(80, 40);
    keys(&mut f, &[Key::Right]);
    assert_ne!(f.tool().tab, ToolTab::Output);
    let before = (rows(&f), rail(&f));
    assert!(
        !before.1.contains("[v]") && !before.1.contains("Columns"),
        "{}",
        before.1
    );
    keys(&mut f, &[Key::Char('v'), Key::Char('l'), Key::Char('$')]);
    assert_eq!((rows(&f), rail(&f)), before);
}

#[test]
fn toggles_never_touch_the_stage_and_search_follows_the_displayed_rows() {
    let mut f = open(80, 40);
    let (cache, view) = (f.cache(), f.view());
    // `link (https` exists only in the rendered text; raw has `link](https`.
    keys(&mut f, &[Key::Char('/')]);
    for c in "link (https".chars() {
        f.press(Key::Char(c));
    }
    keys(&mut f, &[Key::Enter]);
    assert!(status(&f).contains("no match"), "{}", status(&f));
    keys(&mut f, &[Key::Char('v')]);
    let rendered = rows(&f);
    assert!(status(&f).contains("Search 1/1"), "{}", status(&f));
    assert!(rendered.iter().any(|row| row.contains("link (https")));
    keys(
        &mut f,
        &[Key::Char('l'), Key::Char('w'), Key::Char('v'), Key::Esc],
    );
    f.paint();
    assert_eq!(
        (f.cache(), f.view()),
        (cache, view),
        "Stage caches and ViewState untouched"
    );
}

#[test]
fn keys_before_the_first_paint_apply_in_order() {
    // The reference: the same keys after the frame has been painted.
    let mut painted = open(80, 40);
    keys(
        &mut painted,
        &[Key::Char('v'), Key::Char('l'), Key::Char('l')],
    );
    let expected = rows(&painted);
    // The keys arrive between the opening effect and the first paint.
    let mut early = Fixture::single(
        80,
        40,
        "Bash",
        json!({"command":"inspect"}),
        S::Success,
        Some((&markdown(), D::NotAWrite)),
    )
    .0;
    assert_eq!(early.press(Key::Enter), InputAction::InspectTool);
    assert!(early.inspect());
    for key in [Key::Char('v'), Key::Char('l'), Key::Char('l')] {
        early.press(key);
    }
    early.paint();
    assert_eq!(rows(&early), expected);
    assert_ne!(
        expected,
        {
            let mut plain = open(80, 40);
            keys(&mut plain, &[Key::Char('v')]);
            rows(&plain)
        },
        "control: the horizontal keys changed rows"
    );
}

#[test]
fn a_toggle_resets_the_view_after_the_scrolls_issued_before_it() {
    // The rendered body is taller than the 80x24 viewport; `v` alone shows its top.
    let mut reference = open(80, 24);
    keys(&mut reference, &[Key::Char('v')]);
    let top = rows(&reference);
    assert_eq!(reference.tool().viewers[0].top, 0);
    // The same `v` after two scrolls issued in the same batch (no paint between):
    // the scrolls act on the raw rows, then the toggle starts the new body at the top.
    let mut batched = open(80, 24);
    for key in [Key::Char('j'), Key::Char('j'), Key::Char('v')] {
        batched.press(key);
    }
    batched.paint();
    assert_eq!(batched.tool().viewers[0].top, 0);
    assert_eq!(rows(&batched), top);
    // Control: without the toggle the same scrolls do move the view.
    let mut scrolled = open(80, 24);
    keys(&mut scrolled, &[Key::Char('j'), Key::Char('j')]);
    assert_eq!(scrolled.tool().viewers[0].top, 2);
}

fn open_text(text: &str) -> Fixture {
    let mut f = Fixture::single(
        80,
        40,
        "Bash",
        json!({"command":"inspect"}),
        S::Success,
        Some((text, D::NotAWrite)),
    )
    .0;
    f.open();
    f
}

#[test]
fn entity_forms_of_bidi_and_format_characters_never_reach_a_rendered_cell() {
    // Entities decode after sanitizing, so the literal-only strip cannot see them.
    let text = "Prose ev&#x202E;il &#x2066;iso&#x2069; &#x200E;l&#x200F;r &#x61C;m.\n\n\
                | h&#x202E;1 | h2 |\n|---|---|\n| a | b&#x2067;c |\n\n\
                [la&#x200E;bel](https://ex.test/p&#x61C;q)\n";
    let mut f = open_text(text);
    keys(&mut f, &[Key::Char('v')]);
    assert_clean(&f);
    let rendered = rows(&f);
    // Control: the text around the stripped characters is really painted.
    assert!(
        rendered.iter().any(|row| row.contains("evil")),
        "{rendered:#?}"
    );
    assert!(
        rendered.iter().any(|row| row.contains("h1")),
        "{rendered:#?}"
    );
    assert!(
        rendered.iter().any(|row| row.contains("label")),
        "{rendered:#?}"
    );
}

/// A markdown source of exactly `bytes` bytes: a heading and filler paragraphs.
fn sized(bytes: usize) -> String {
    let mut text = String::from("# Big\n\n");
    while text.len() + 6 <= bytes {
        text.push_str("aaaa\n\n");
    }
    while text.len() < bytes {
        text.push('a');
    }
    assert_eq!(text.len(), bytes);
    text
}

#[test]
fn rendering_is_bounded_and_a_larger_body_stays_raw_with_a_notice() {
    let bound = rustain::adapters::tui::presentation::markdown_body::RENDERED_BODY_MAX_BYTES;
    let mut at = open_text(&sized(bound));
    keys(&mut at, &[Key::Char('v')]);
    assert!(
        rows(&at).iter().any(|row| row.trim() == "Big"),
        "at the bound the markdown renders: {:#?}",
        rows(&at)
    );
    let mut over = open_text(&sized(bound + 1));
    keys(&mut over, &[Key::Char('v')]);
    let shown = rows(&over);
    assert!(
        shown
            .iter()
            .any(|row| row.contains("Rendered view is limited to 1 MiB")),
        "{shown:#?}"
    );
    assert!(
        shown.iter().any(|row| row.starts_with("# Big")),
        "still raw: {shown:#?}"
    );
}

#[test]
fn v_remembers_each_presentations_position() {
    let mut f = open(80, 24);
    keys(&mut f, &[Key::Char('j'), Key::Char('j')]);
    assert_eq!(f.tool().viewers[0].top, 2);
    keys(&mut f, &[Key::Char('v'), Key::Char('j')]);
    assert_eq!(f.tool().viewers[0].top, 1, "rendered starts at its top");
    keys(&mut f, &[Key::Char('v')]);
    assert_eq!(f.tool().viewers[0].top, 2, "source is where it was left");
    keys(&mut f, &[Key::Char('v')]);
    assert_eq!(f.tool().viewers[0].top, 1, "and so is rendered");
}

#[test]
fn no_other_tab_is_a_markdown_body() {
    use rustain::domain::models::{DiffKind, DiffLine};
    let diff = D::Diff {
        lines: vec![
            DiffLine {
                kind: DiffKind::Removed,
                content: "old".into(),
            },
            DiffLine {
                kind: DiffKind::Added,
                content: "new".into(),
            },
        ],
        more: 0,
    };
    let mut f = Fixture::single(
        80,
        24,
        "Write",
        json!({"file_path":"notes.txt","content":"new\nnext"}),
        S::Success,
        Some(("written", diff)),
    )
    .0;
    f.open();
    let mut seen = Vec::new();
    for _ in 0..4 {
        keys(&mut f, &[Key::Right]);
        let tab = f.tool().tab;
        if tab == ToolTab::Output || seen.contains(&tab) {
            continue;
        }
        seen.push(tab);
        let before = (rows(&f), rail(&f), f.tool().mode);
        assert!(
            !before.1.contains("[v]") && !before.1.contains("Columns"),
            "{tab:?}: {}",
            before.1
        );
        keys(&mut f, &[Key::Char('v'), Key::Char('l'), Key::Char('$')]);
        assert_eq!((rows(&f), rail(&f), f.tool().mode), before, "{tab:?}");
    }
    assert!(
        seen.contains(&ToolTab::Input)
            && seen.contains(&ToolTab::Raw)
            && seen.contains(&ToolTab::Diff),
        "every non-Output tab was visited: {seen:?}"
    );
}
