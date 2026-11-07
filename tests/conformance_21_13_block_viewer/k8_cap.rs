//! K8 (AC9): the local Stage caps committed assistant code at `code_max_rows`;
//! everything else, the attached host and the block viewer keep every row.
use rustain::adapters::tui::{
    handlers::config::apply_markdown_config,
    markdown::{self, BlockKind, RenderOptions},
    theme::Theme,
};
use rustain::domain::models::{
    AppConfig, ChatMessage, MarkdownConfig, MessageRole, Turn, TurnPart,
};

use super::support::{Fixture, part, screen_rows};

/// A fixture whose single committed prose part carries `text`, painted with the
/// Stage cap `rows` (the production value the draw hands to the render state).
fn capped(rows: u16, text: &str) -> Fixture {
    let mut turn = Turn::new("fixture-model".into(), 0);
    part(&mut turn, text);
    let mut f = Fixture::new(80, 60, turn);
    f.h.state.code_max_rows = rows;
    f.paint();
    f
}

/// Fenced rust code of `lines` short numbered lines (every row fits at 80 cells).
fn code(lines: usize) -> String {
    let body = (1..=lines)
        .map(|n| format!("fn line_{n:02}() {{}}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("```rust\n{body}\n```\n")
}

fn code_rows(f: &Fixture) -> usize {
    screen_rows(f)
        .iter()
        .filter(|row| row.contains("fn line_"))
        .count()
}

fn marker(f: &Fixture) -> Option<String> {
    screen_rows(f)
        .into_iter()
        .find(|row| row.contains("more line"))
        .map(|row| row.trim_matches(|c: char| c == '│' || c == ' ').to_owned())
}

#[test]
fn code_longer_than_the_cap_shows_cap_rows_and_the_exact_remainder() {
    for (lines, shown, remainder) in [
        (29, 29, None),
        (30, 30, None),
        (31, 30, Some("… 1 more line")),
        (45, 30, Some("… 15 more lines")),
    ] {
        let f = capped(30, &code(lines));
        assert_eq!(code_rows(&f), shown, "{lines} lines");
        assert_eq!(marker(&f).as_deref(), remainder, "{lines} lines");
    }
    // The marker sits inside the box: the bottom border follows it directly.
    let f = capped(30, &code(45));
    let rows = screen_rows(&f);
    let at = rows.iter().position(|r| r.contains("more line")).unwrap();
    assert!(
        rows[at].contains('│') && rows[at + 1].contains('└'),
        "{rows:#?}"
    );
    // Cap 1 keeps one code row and counts the rest.
    let f = capped(1, &code(45));
    assert_eq!(code_rows(&f), 1);
    assert_eq!(marker(&f).as_deref(), Some("… 44 more lines"));
}

#[test]
fn the_capped_block_stays_focusable_and_reports_every_line() {
    let f = capped(5, &code(45));
    let state = &f.h.state;
    let render = state
        .tab_render_states
        .get(&f.tabs.active_tab_id())
        .unwrap();
    assert!(
        render
            .item_boundaries
            .iter()
            .any(|(_, id)| id.starts_with("md_")),
        "a capped block is still an Inspect target"
    );
    let rendered = markdown::render(
        &code(45),
        78,
        &Theme::dark(),
        &RenderOptions::completed().with_code_cap(Some(5)),
    );
    assert!(matches!(
        rendered.blocks[0].kind,
        BlockKind::Code { lines: 45, .. }
    ));
    // Rows of the span are the capped rows: header + 5 + marker + border.
    assert_eq!(rendered.blocks[0].rows.len(), 8);
}

/// One turn with a committed `Answer.`, painted at Stage cap 3 after `setup`.
fn painted_at_cap_3(setup: impl FnOnce(&mut Fixture)) -> Fixture {
    let mut turn = Turn::new("fixture-model".into(), 0);
    part(&mut turn, "Answer.");
    let mut f = Fixture::new(80, 140, turn);
    f.h.state.code_max_rows = 3;
    setup(&mut f);
    f.paint();
    f
}

#[test]
fn uncapped_surfaces_keep_every_row_and_the_same_text_committed_is_capped() {
    let fence = code(45);
    // The open streaming prose shows every row (nothing is hidden before it can
    // be inspected) and so does a system message.
    let open = painted_at_cap_3(|f| {
        let tab = f.tabs.active_tab_mut();
        tab.reducer.open_turn = Some(Turn::new("fixture-model".into(), 1));
        tab.reducer.open_prose = Some(fence.clone());
    });
    assert_eq!(code_rows(&open), 45, "open prose");
    assert_eq!(marker(&open), None);
    // A committed part of the still-open turn is capped like any committed part.
    let committed = painted_at_cap_3(|f| {
        let mut open_turn = Turn::new("fixture-model".into(), 1);
        part(&mut open_turn, &fence);
        f.tabs.active_tab_mut().reducer.open_turn = Some(open_turn);
    });
    assert_eq!(code_rows(&committed), 3, "committed part of the open turn");
    assert_eq!(marker(&committed).as_deref(), Some("… 42 more lines"));
    let system = painted_at_cap_3(|f| {
        f.tabs
            .active_tab_mut()
            .conversation
            .messages
            .push(ChatMessage {
                id: "s1".into(),
                role: MessageRole::System,
                content: fence.clone(),
                ..Default::default()
            });
    });
    assert_eq!(code_rows(&system), 45, "system message");
    assert_eq!(marker(&system), None);
    // Reasoning is never capped: its Stage height does not depend on the cap.
    let reasoning = |rows: u16| {
        let mut turn = Turn::new("fixture-model".into(), 0);
        turn.push_part(|id| TurnPart::Reasoning {
            id,
            text: fence.clone(),
        });
        part(&mut turn, "Answer.");
        let mut f = Fixture::new(80, 140, turn);
        f.h.state.code_max_rows = rows;
        f.paint();
        f
    };
    let (three, hundred) = (reasoning(3), reasoning(100));
    assert!(
        screen_rows(&three)
            .iter()
            .any(|row| row.contains("fn line_45"))
    );
    assert_eq!(screen_rows(&three), screen_rows(&hundred));
    assert_eq!(marker(&reasoning(3)), None);
    // Positive control: the same text as a committed prose part is capped.
    let f = capped(3, &fence);
    assert_eq!(code_rows(&f), 3);
    assert_eq!(marker(&f).as_deref(), Some("… 42 more lines"));
}

#[test]
fn nested_code_is_capped_and_the_cap_never_changes_a_height_or_a_row_count() {
    let nested = format!("- item\n\n  {}", code(12).replace('\n', "\n  "));
    let lines = |cap: Option<u16>, width: usize, text: &str| {
        markdown::render(
            text,
            width,
            &Theme::dark(),
            &RenderOptions::completed().with_code_cap(cap),
        )
        .lines
        .len()
    };
    assert!(lines(Some(3), 60, &nested) < lines(None, 60, &nested));
    for text in [code(45), nested, format!("{}\n{}", code(12), code(31))] {
        for cap in [1u16, 3, 30] {
            for width in 0..=130 {
                let options = RenderOptions::completed().with_code_cap(Some(cap));
                let theme = Theme::dark();
                assert_eq!(
                    markdown::compute_height(&text, width, &theme, &options),
                    markdown::render(&text, width, &theme, &options).lines.len(),
                    "cap {cap} width {width}"
                );
            }
        }
    }
}

#[test]
fn changing_the_cap_recomputes_stage_heights_and_an_unchanged_cap_does_not() {
    let mut f = capped(30, &code(45));
    let id = f.tabs.active_tab_id();
    let before = f.cache();
    assert!(
        !before.0.is_empty(),
        "control: the Stage cache is populated"
    );
    // A new cap through the production draw recomputes (the cache equals a
    // fresh fixture drawn at that cap, and the painted rows follow).
    f.h.state.code_max_rows = 10;
    f.paint();
    assert_eq!(code_rows(&f), 10);
    assert_ne!(f.cache(), before);
    assert_eq!(screen_rows(&f), screen_rows(&capped(10, &code(45))));
    // The state-level contract: same value keeps entries, a new value clears them.
    let render = f.h.state.tab_render_states.get_mut(&id).unwrap();
    let populated = render.height_cache.entries.len();
    assert!(populated > 0);
    render.set_code_cap(Some(10));
    assert_eq!(render.height_cache.entries.len(), populated);
    render.set_code_cap(Some(11));
    assert!(render.height_cache.entries.is_empty());
}

#[test]
fn markdown_config_clamps_applies_and_rejects_a_non_integer() {
    let parse = |toml: &str| toml::from_str::<AppConfig>(toml);
    for (toml, rows) in [
        ("", 30),
        ("[markdown]\n", 30),
        ("[markdown]\ncode_max_rows = 12", 12),
        ("[markdown]\ncode_max_rows = -5", 1),
        ("[markdown]\ncode_max_rows = 0", 1),
        ("[markdown]\ncode_max_rows = 5000", 1000),
    ] {
        let config = parse(toml).expect(toml);
        assert_eq!(config.markdown.code_max_rows(), rows, "{toml:?}");
    }
    assert!(parse("[markdown]\ncode_max_rows = \"many\"").is_err());
    assert_eq!(MarkdownConfig::default().code_max_rows(), 30);
    // The adapter fn is the one seam from config to the Stage.
    let mut harness = crate::e2e_harness::TestHarness::with_size(80, 24);
    assert_eq!(harness.state.code_max_rows, 30);
    apply_markdown_config(
        &mut harness.state,
        &parse("[markdown]\ncode_max_rows = 7").unwrap().markdown,
    );
    assert_eq!(harness.state.code_max_rows, 7);
}
