//! K1 (AC1, AC5): committed reasoning folds to one honest row on the local
//! Stage, in its part position, with the D4 formats and width fit; the setting
//! changes geometry only through `set_reasoning`; turn folds do not move.
use rustain::adapters::tui::{
    handlers::config::apply_markdown_config,
    markdown::{self, RenderOptions},
    theme::Theme,
    widgets::chat_pane,
};
use rustain::domain::models::{AppConfig, InvocationStatus, ReasoningDisplay, StopReason, Turn};

use super::support::{
    Fixture, cached_height, call, part, reasoning, row_starting, rows, thinking_turn, unfocused,
};

const BODY: &str = "first thought\n\nsecond thought\n\nthird thought";

#[test]
fn folded_reasoning_is_one_row_in_height_and_paint() {
    let (turn, _) = thinking_turn(BODY, Some(4_200), Some(1_300));
    let f = unfocused(turn);
    let rows = rows(&f);
    let at = row_starting(&rows, "│ ◇ Thinking").unwrap_or_else(|| panic!("fold row: {rows:?}"));
    assert_eq!(rows[at], "│ ◇ Thinking · 4.2s · 1.3k tokens");
    // Part position: the answer follows the fold row (one spacing row between).
    assert_eq!(rows[at + 1], "│");
    assert_eq!(rows[at + 2], "│ Answer.");
    // The rail row ends the turn (after the answer's own rows).
    assert!(rows[at + 3..].iter().any(|row| row == "│"));
    assert!(
        rows.iter().all(|row| !row.contains("thought")),
        "folded text is not painted: {rows:?}"
    );
    // Height == paint: fold row + spacing + answer rows + rail.
    assert_eq!(
        cached_height(&f),
        1 + 1 + body_rows(inner(&f), "Answer.") + 1
    );
}

#[test]
fn facts_use_the_d4_formats_and_omit_what_is_unknown() {
    // The estimate is bytes / 4, the one estimator the row reuses.
    for (duration, tokens, text, want) in [
        (
            Some(4_200),
            Some(1_300),
            BODY.to_owned(),
            "4.2s · 1.3k tokens".to_owned(),
        ),
        (
            Some(4_200),
            None,
            "a".repeat(4_000),
            "4.2s · ~1.0k tokens".to_owned(),
        ),
        (None, None, "a".repeat(400), "~100 tokens".to_owned()),
        (None, Some(1), BODY.to_owned(), "1 token".to_owned()),
        (None, Some(12), BODY.to_owned(), "12 tokens".to_owned()),
        (
            Some(0),
            Some(5),
            BODY.to_owned(),
            "<0.1s · 5 tokens".to_owned(),
        ),
        (
            Some(9_900),
            Some(5),
            BODY.to_owned(),
            "9.9s · 5 tokens".to_owned(),
        ),
        (
            Some(42_000),
            Some(5),
            BODY.to_owned(),
            "42s · 5 tokens".to_owned(),
        ),
        (
            Some(72_000),
            Some(5),
            BODY.to_owned(),
            "1m 12s · 5 tokens".to_owned(),
        ),
        (
            Some(3_720_000),
            Some(5),
            BODY.to_owned(),
            "1h 02m · 5 tokens".to_owned(),
        ),
    ] {
        let (turn, _) = thinking_turn(&text, duration, tokens);
        let f = unfocused(turn);
        let rows = rows(&f);
        let at = row_starting(&rows, "│ ◇ Thinking").expect("fold row");
        assert_eq!(
            rows[at],
            format!("│ ◇ Thinking · {want}"),
            "{duration:?} {tokens:?}"
        );
        let _ = &want;
    }
}
#[test]
fn the_row_never_wraps_and_drops_tokens_then_duration() {
    use ratatui::{Terminal, backend::TestBackend};
    let (turn, _) = thinking_turn(BODY, Some(4_200), Some(1_300));
    let (conversation, streaming) = {
        let f = Fixture::new(80, 24, turn);
        (
            f.conversation().clone(),
            f.tabs.active_tab().streaming.clone(),
        )
    };
    for width in [12u16, 20, 40, 60, 80, 130] {
        let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
        let mut render = rustain::adapters::tui::state::TabRenderState::default();
        render.set_reasoning(ReasoningDisplay::Folded);
        terminal
            .draw(|frame| {
                let area = frame.area();
                chat_pane::render(
                    frame,
                    area,
                    &conversation,
                    &streaming,
                    0,
                    true,
                    &Theme::dark(),
                    &mut render,
                    &std::collections::BTreeMap::new(),
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let rows: Vec<String> = (0..24)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect();
        let at = row_starting(&rows, "│ ◇").expect("fold row");
        // Content cells after the gutter and the glyph.
        let budget = usize::from(width).saturating_sub(4);
        let label = [
            "Thinking · 4.2s · 1.3k tokens",
            "Thinking · 4.2s",
            "Thinking",
        ]
        .into_iter()
        .find(|candidate| candidate.chars().count() <= budget)
        .expect("a candidate fits");
        assert_eq!(rows[at], format!("│ ◇ {label}"), "width {width}");
        assert!(
            rows[at].chars().count() <= usize::from(width),
            "width {width}"
        );
    }
}

/// Content cells of the Stage: the chat pane minus the two-cell gutter.
fn inner(f: &Fixture) -> usize {
    usize::from(f.layout().chat_pane.width) - 2
}

fn body_rows(width: usize, text: &str) -> usize {
    markdown::compute_height(text, width, &Theme::dark(), &RenderOptions::completed())
}

#[test]
fn expanded_reasoning_keeps_the_header_and_paints_the_body() {
    let (turn, _) = thinking_turn(BODY, Some(4_200), Some(1_300));
    let mut f = unfocused(turn);
    f.h.state.reasoning_display = ReasoningDisplay::Expanded;
    f.paint();
    let rows = rows(&f);
    let at = row_starting(&rows, "│ ◇ Thinking").expect("header kept");
    assert_eq!(rows[at], "│ ◇ Thinking · 4.2s · 1.3k tokens");
    assert_eq!(rows[at + 1], "│ first thought");
    assert!(rows.iter().any(|row| row == "│ second thought"));
    assert!(rows.iter().any(|row| row == "│ third thought"));
    // Header + body + spacing + answer rows + rail.
    assert_eq!(
        cached_height(&f),
        1 + body_rows(inner(&f), BODY) + 1 + body_rows(inner(&f), "Answer.") + 1
    );
}

#[test]
fn switching_display_mode_recomputes_stage_heights() {
    let (turn, _) = thinking_turn(BODY, Some(4_200), Some(1_300));
    let mut f = Fixture::new(80, 24, turn);
    let folded = f.h.state.total_content_height;
    f.h.state.reasoning_display = ReasoningDisplay::Expanded;
    f.paint();
    assert_eq!(
        f.h.state.total_content_height,
        folded + body_rows(inner(&f), BODY),
        "the very next draw measures the new mode, not a stale cache entry"
    );
    // Back again: the original geometry returns.
    f.h.state.reasoning_display = ReasoningDisplay::Folded;
    f.paint();
    assert_eq!(f.h.state.total_content_height, folded);
}

#[test]
fn adopting_the_same_display_keeps_the_cache() {
    let (turn, _) = thinking_turn(BODY, Some(4_200), Some(1_300));
    let mut f = Fixture::new(80, 24, turn);
    let id = f.tabs.active_tab_id();
    let render = f.h.state.tab_render_states.get_mut(&id).unwrap();
    assert!(!render.height_cache.entries.is_empty());
    let before = render.height_cache.entries.len();
    render.set_reasoning(ReasoningDisplay::Folded);
    assert_eq!(
        render.height_cache.entries.len(),
        before,
        "unchanged: a hit"
    );
    render.set_reasoning(ReasoningDisplay::Expanded);
    assert!(
        render.height_cache.entries.is_empty(),
        "a change drops heights"
    );
}

#[test]
fn tab_render_state_defaults_to_expanded() {
    let render = rustain::adapters::tui::state::TabRenderState::default();
    assert_eq!(render.reasoning, ReasoningDisplay::Expanded);
}

fn tool_turn(reasoning_text: &str) -> Turn {
    let mut turn = Turn::new("fixture-model".into(), 0);
    reasoning(&mut turn, reasoning_text, Some(1_000), Some(10));
    for name in ["Read", "Read", "Read"] {
        call(
            &mut turn,
            name,
            serde_json::json!({"file_path": "a.rs"}),
            InvocationStatus::Success,
            None,
        );
    }
    part(&mut turn, "Done.");
    turn.stop_reason = Some(StopReason::EndTurn);
    turn
}

#[test]
fn the_turn_fold_decision_counts_reasoning_on_its_full_body_in_either_display() {
    // Three tools without results measure 3 rows against the prose newlines:
    // reasoning with 5 newlines keeps the turn expanded, with none it folds.
    let long = tool_turn("a\nb\nc\nd\ne\nf");
    let short = tool_turn("a");
    let view = rustain::domain::models::view_state::ViewState::default();
    assert!(!chat_pane::effective_is_collapsed(&long, &view));
    assert!(chat_pane::effective_is_collapsed(&short, &view));
    // The collapsed turn paints the same rows whatever the display setting.
    let mut painted = Vec::new();
    for display in [ReasoningDisplay::Folded, ReasoningDisplay::Expanded] {
        let mut f = Fixture::new(80, 24, short.clone());
        f.h.state.reasoning_display = display;
        f.paint();
        painted.push(rows(&f));
    }
    assert_eq!(painted[0], painted[1]);
    assert_eq!(painted.len(), 2);
}

#[test]
fn the_markdown_setting_defaults_to_folded_and_an_unknown_value_folds_without_losing_other_keys() {
    let parse = |toml: &str| toml::from_str::<AppConfig>(toml);
    for (toml, want) in [
        ("", ReasoningDisplay::Folded),
        ("[markdown]\n", ReasoningDisplay::Folded),
        (
            "[markdown]\nreasoning = \"folded\"",
            ReasoningDisplay::Folded,
        ),
        (
            "[markdown]\nreasoning = \"expanded\"",
            ReasoningDisplay::Expanded,
        ),
        (
            "[markdown]\nreasoning = \"Expanded\"",
            ReasoningDisplay::Folded,
        ),
        (
            "[markdown]\nreasoning = \"bogus\"",
            ReasoningDisplay::Folded,
        ),
    ] {
        let config = parse(toml).expect(toml);
        assert_eq!(config.markdown.reasoning(), want, "{toml:?}");
    }
    // An unknown value never invalidates the config: its other keys stay.
    let config = parse("[markdown]\ncode_max_rows = 12\nreasoning = \"bogus\"").unwrap();
    assert_eq!(
        (config.markdown.code_max_rows(), config.markdown.reasoning()),
        (12, ReasoningDisplay::Folded)
    );
    // A non-string value is a type error like any other mistyped key.
    assert!(parse("[markdown]\nreasoning = 3").is_err());
}

#[test]
fn a_config_reload_switches_the_display_and_the_next_draw_measures_it() {
    let (turn, _) = thinking_turn(BODY, Some(4_200), Some(1_300));
    let mut f = Fixture::new(80, 24, turn);
    let folded = f.h.state.total_content_height;
    let parse = |toml: &str| toml::from_str::<AppConfig>(toml).unwrap().markdown;
    // Startup and `/config reload` share the one adapter seam.
    apply_markdown_config(
        &mut f.h.state,
        &parse("[markdown]\nreasoning = \"expanded\""),
    );
    f.paint();
    assert_eq!(
        f.h.state.total_content_height,
        folded + body_rows(inner(&f), BODY),
        "no stale cache entry survives the reload"
    );
    assert!(rows(&f).iter().any(|row| row == "│ second thought"));
    apply_markdown_config(&mut f.h.state, &parse("[markdown]\nreasoning = \"folded\""));
    f.paint();
    assert_eq!(f.h.state.total_content_height, folded);
    assert!(rows(&f).iter().all(|row| !row.contains("thought")));
}
