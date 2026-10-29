//! K10 (AC9; closes DF-20-1C-PICKER-SMALL-TERMINAL): at 60×16, the smallest
//! rendered size, the model picker's locked and available panes keep intact
//! borders, the header, at least one row and the footer. The login waiting
//! view is checked at 60×16 inside K03.

use super::common::buffer_text;
use ratatui::{Terminal, backend::TestBackend};
use rustain::adapters::tui::state::{ModelSelectorState, ProviderColumn};
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::model_selector;
use rustain::domain::models::provider::{DiscoveryStatus, ModelDescriptor};

/// One bordered modal whose rows lie inside the border, holding `needles`
/// and ending with a line that contains `footer`.
pub(crate) fn assert_fitted(frame: &str, needles: &[&str], footer: &str) {
    let lines: Vec<&str> = frame.lines().collect();
    let top = lines
        .iter()
        .position(|line| line.contains('┌'))
        .unwrap_or_else(|| panic!("top border\n{frame}"));
    let bottom = lines
        .iter()
        .rposition(|line| line.contains('└'))
        .unwrap_or_else(|| panic!("bottom border\n{frame}"));
    let left = lines[top].find('┌').unwrap();
    assert!(lines[top].contains('┐'), "top-right corner\n{frame}");
    assert!(lines[bottom].contains('┘'), "bottom-right corner\n{frame}");
    let inner: Vec<&str> = lines[top + 1..bottom].to_vec();
    for line in &inner {
        assert!(line[left..].starts_with('│'), "left border intact\n{frame}");
    }
    for needle in needles {
        assert!(
            inner.iter().any(|line| line.contains(needle)),
            "{needle}\n{frame}"
        );
    }
    // The footer is the last drawn inner line: a modal padded to its minimum
    // height may have rows below it, and by this search each of them is blank.
    let blank = |line: &&str| line.chars().all(|c| c == '│' || c == ' ');
    let last = inner
        .iter()
        .rposition(|line| !blank(line))
        .unwrap_or_else(|| panic!("an empty modal\n{frame}"));
    assert!(
        inner[last].contains(footer),
        "footer on the last drawn inner line\n{frame}"
    );
}

fn model(id: &str, stale: bool) -> ModelDescriptor {
    ModelDescriptor {
        model_id: id.into(),
        display_name: id.into(),
        provider_id: "p".into(),
        context_window: 128_000,
        capabilities: Default::default(),
        pricing_tier: None,
        stale,
        wire_model_id: None,
    }
}

fn render(state: &ModelSelectorState) -> String {
    let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
    terminal
        .draw(|frame| model_selector::render(frame, frame.area(), state, &Theme::dark()))
        .unwrap();
    buffer_text(terminal.backend().buffer())
}

/// K10 (M10): a locked pane with its long hint and catalogue, and an
/// available pane with an Unauthenticated notice, a stale selection and a
/// refresh line, both fit 60×16.
#[test]
fn k10_model_picker_panes_fit_60x16() {
    let mut state = ModelSelectorState::new();
    state.active = true;
    state.columns = vec![
        ProviderColumn {
            provider_id: "anthropic".into(),
            display_name: "Anthropic".into(),
            healthy: false,
            models: (0..12).map(|i| model(&format!("catalog-model-{i}"), false)).collect(),
            locked_hint: Some(
                "Set ANTHROPIC_AUTH_TOKEN or ANTHROPIC_API_KEY, or run `rustain auth login anthropic`."
                    .into(),
            ),
            discovery: None,
            sign_in_hint: None,
        },
        ProviderColumn {
            provider_id: "deepseek".into(),
            display_name: "DeepSeek".into(),
            healthy: true,
            models: (0..14).map(|i| model(&format!("ds-model-{i}"), i == 3)).collect(),
            locked_hint: None,
            discovery: Some(DiscoveryStatus::Unauthenticated),
            sign_in_hint: Some("Run `rustain auth login deepseek` to sign in again.".into()),
        },
    ];
    state.sign_in.insert("anthropic".into(), "anthropic".into());
    let frame = render(&state);
    assert_fitted(
        &frame,
        &["◄ Anthropic · not signed in", "catalog-model-0"],
        "close",
    );

    state.selected_provider = 1;
    state.selected_model = 3;
    let _guard = state.refreshing.insert("deepseek".into());
    let frame = render(&state);
    assert_fitted(
        &frame,
        &["◄ DeepSeek ⚠ sign in again", "ds-model-3"],
        "search",
    );
}
