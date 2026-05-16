//! E2E tests for Story 7.2: Model & Provider Switcher
//!
//! Uses TestHarness to verify end-to-end behavior of:
//! - Ctrl+X then M opens the model selector overlay (AC1)
//! - Left/Right/h/l navigate providers (AC1)
//! - Up/Down/j/k navigate models within a provider (AC1)
//! - Enter selects a model and returns SwitchModelProvider (AC3)
//! - Esc dismisses and restores focus (AC1)
//! - Single-provider Left/Right is a no-op (AC7)
//! - Ctrl+C dismisses like Esc (AC1)
//! - Ctrl+X,M blocked when command palette is open

use std::collections::HashSet;

use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::state::ProviderColumn;
use rustain::adapters::tui::widgets::model_selector;
use rustain::domain::events::DomainKey;
use rustain::domain::models::FocusState;
use rustain::domain::models::provider::ModelDescriptor;
use rustain::domain::models::visual::OverlayType;

mod e2e_harness;
use e2e_harness::TestHarness;

fn model(id: &str, display: &str, provider: &str, ctx: u32) -> ModelDescriptor {
    ModelDescriptor {
        model_id: id.to_string(),
        display_name: display.to_string(),
        provider_id: provider.to_string(),
        context_window: ctx,
        capabilities: HashSet::new(),
        pricing_tier: None,
    stale: false,
    }
}

fn two_provider_columns() -> Vec<ProviderColumn> {
    vec![
        ProviderColumn {
            provider_id: "anthropic".to_string(),
            display_name: "Anthropic".to_string(),
            healthy: true,
            models: vec![
                model("claude-opus-4", "Claude Opus 4", "anthropic", 200_000),
                model("claude-sonnet-4", "Claude Sonnet 4", "anthropic", 200_000),
                model("claude-haiku-3", "Claude Haiku 3", "anthropic", 200_000),
            ],
        },
        ProviderColumn {
            provider_id: "openai".to_string(),
            display_name: "OpenAI".to_string(),
            healthy: true,
            models: vec![
                model("gpt-4o", "GPT-4o", "openai", 128_000),
                model("gpt-4o-mini", "GPT-4o Mini", "openai", 128_000),
            ],
        },
    ]
}

fn single_provider_columns() -> Vec<ProviderColumn> {
    vec![ProviderColumn {
        provider_id: "anthropic".to_string(),
        display_name: "Anthropic".to_string(),
        healthy: true,
        models: vec![
            model("claude-opus-4", "Claude Opus 4", "anthropic", 200_000),
            model("claude-sonnet-4", "Claude Sonnet 4", "anthropic", 200_000),
        ],
    }]
}

fn open_selector(h: &mut TestHarness, columns: Vec<ProviderColumn>) {
    h.state.model_selector.open(
        h.state.focus.clone(),
        columns,
        "anthropic",
        "claude-sonnet-4",
    );
    h.state.focus = FocusState::Overlay(OverlayType::ModelSelector);
}

// ═══════════════════════════════════════════════════════════════════════════
// AC1: Ctrl+X, M chord opens overlay
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_ctrl_x_m_returns_open_model_selector() {
    let mut h = TestHarness::new();

    h.press_key(DomainKey::CtrlX);
    assert!(h.state.which_key.active);

    let action = h.type_char('m');
    assert!(matches!(action, InputAction::OpenModelSelector));
    assert!(!h.state.which_key.active);
}

// ═══════════════════════════════════════════════════════════════════════════
// AC1: Arrow key navigation — providers (Left/Right)
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_right_navigates_to_next_provider() {
    let mut h = TestHarness::new();
    open_selector(&mut h, two_provider_columns());

    assert_eq!(h.state.model_selector.selected_provider, 0);

    let action = h.press_key(DomainKey::Right);
    assert!(matches!(action, InputAction::Consumed));
    assert_eq!(h.state.model_selector.selected_provider, 1);
}

#[test]
fn test_left_navigates_to_previous_provider() {
    let mut h = TestHarness::new();
    open_selector(&mut h, two_provider_columns());

    h.state.model_selector.selected_provider = 1;

    let action = h.press_key(DomainKey::Left);
    assert!(matches!(action, InputAction::Consumed));
    assert_eq!(h.state.model_selector.selected_provider, 0);
}

#[test]
fn test_provider_navigation_wraps() {
    let mut h = TestHarness::new();
    open_selector(&mut h, two_provider_columns());

    assert_eq!(h.state.model_selector.selected_provider, 0);
    h.press_key(DomainKey::Left);
    assert_eq!(h.state.model_selector.selected_provider, 1);

    h.press_key(DomainKey::Right);
    assert_eq!(h.state.model_selector.selected_provider, 0);
}

// ═══════════════════════════════════════════════════════════════════════════
// AC1: Arrow key navigation — models (Up/Down)
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_down_navigates_to_next_model() {
    let mut h = TestHarness::new();
    open_selector(&mut h, two_provider_columns());

    assert_eq!(h.state.model_selector.selected_model, 1);

    let action = h.press_key(DomainKey::Down);
    assert!(matches!(action, InputAction::Consumed));
    assert_eq!(h.state.model_selector.selected_model, 2);
}

#[test]
fn test_up_navigates_to_previous_model() {
    let mut h = TestHarness::new();
    open_selector(&mut h, two_provider_columns());

    h.state.model_selector.selected_model = 2;

    let action = h.press_key(DomainKey::Up);
    assert!(matches!(action, InputAction::Consumed));
    assert_eq!(h.state.model_selector.selected_model, 1);
}

#[test]
fn test_model_navigation_wraps() {
    let mut h = TestHarness::new();
    open_selector(&mut h, two_provider_columns());

    h.state.model_selector.selected_model = 0;
    h.press_key(DomainKey::Up);
    assert_eq!(h.state.model_selector.selected_model, 2);

    h.press_key(DomainKey::Down);
    assert_eq!(h.state.model_selector.selected_model, 0);
}

// ═══════════════════════════════════════════════════════════════════════════
// AC1: Vim-style h/l/j/k navigation
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_h_navigates_provider_left() {
    let mut h = TestHarness::new();
    open_selector(&mut h, two_provider_columns());

    h.state.model_selector.selected_provider = 1;
    let action = h.type_char('h');
    assert!(matches!(action, InputAction::Consumed));
    assert_eq!(h.state.model_selector.selected_provider, 0);
}

#[test]
fn test_l_navigates_provider_right() {
    let mut h = TestHarness::new();
    open_selector(&mut h, two_provider_columns());

    assert_eq!(h.state.model_selector.selected_provider, 0);
    let action = h.type_char('l');
    assert!(matches!(action, InputAction::Consumed));
    assert_eq!(h.state.model_selector.selected_provider, 1);
}

#[test]
fn test_j_navigates_model_down() {
    let mut h = TestHarness::new();
    open_selector(&mut h, two_provider_columns());

    let start = h.state.model_selector.selected_model;
    let action = h.type_char('j');
    assert!(matches!(action, InputAction::Consumed));
    assert_eq!(h.state.model_selector.selected_model, start + 1);
}

#[test]
fn test_k_navigates_model_up() {
    let mut h = TestHarness::new();
    open_selector(&mut h, two_provider_columns());

    h.state.model_selector.selected_model = 2;
    let action = h.type_char('k');
    assert!(matches!(action, InputAction::Consumed));
    assert_eq!(h.state.model_selector.selected_model, 1);
}

// ═══════════════════════════════════════════════════════════════════════════
// AC3: Enter selects a model and returns SwitchModelProvider
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_enter_returns_switch_model_provider() {
    let mut h = TestHarness::new();
    open_selector(&mut h, two_provider_columns());

    h.state.model_selector.selected_provider = 0;
    h.state.model_selector.selected_model = 0;

    let action = h.press_key(DomainKey::Enter);
    match action {
        InputAction::SwitchModelProvider {
            provider_id,
            model_id,
        } => {
            assert_eq!(provider_id.as_deref(), Some("anthropic"));
            assert_eq!(model_id, "claude-opus-4");
        }
        other => panic!("Expected SwitchModelProvider, got {:?}", other),
    }
}

#[test]
fn test_enter_returns_switch_for_second_provider() {
    let mut h = TestHarness::new();
    open_selector(&mut h, two_provider_columns());

    h.state.model_selector.selected_provider = 1;
    h.state.model_selector.selected_model = 1;

    let action = h.press_key(DomainKey::Enter);
    match action {
        InputAction::SwitchModelProvider {
            provider_id,
            model_id,
        } => {
            assert_eq!(provider_id.as_deref(), Some("openai"));
            assert_eq!(model_id, "gpt-4o-mini");
        }
        other => panic!("Expected SwitchModelProvider, got {:?}", other),
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// AC1: Esc dismisses and restores focus
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_esc_dismisses_selector() {
    let mut h = TestHarness::new();
    h.state.focus = FocusState::Chat;
    open_selector(&mut h, two_provider_columns());
    assert!(h.state.model_selector.active);

    let action = h.press_key(DomainKey::Esc);
    assert!(matches!(action, InputAction::Consumed));
    assert!(!h.state.model_selector.active);
    assert!(matches!(h.state.focus, FocusState::Chat));
}

#[test]
fn test_ctrl_c_dismisses_selector() {
    let mut h = TestHarness::new();
    h.state.focus = FocusState::Input;
    open_selector(&mut h, two_provider_columns());
    assert!(h.state.model_selector.active);

    let action = h.press_key(DomainKey::CtrlC);
    assert!(matches!(action, InputAction::Consumed));
    assert!(!h.state.model_selector.active);
    assert!(matches!(h.state.focus, FocusState::Input));
}

// ═══════════════════════════════════════════════════════════════════════════
// AC7: Single-provider Left/Right no-op
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_single_provider_left_right_noop() {
    let mut h = TestHarness::new();
    open_selector(&mut h, single_provider_columns());

    assert_eq!(h.state.model_selector.selected_provider, 0);

    h.press_key(DomainKey::Left);
    assert_eq!(h.state.model_selector.selected_provider, 0);

    h.press_key(DomainKey::Right);
    assert_eq!(h.state.model_selector.selected_provider, 0);

    h.type_char('h');
    assert_eq!(h.state.model_selector.selected_provider, 0);

    h.type_char('l');
    assert_eq!(h.state.model_selector.selected_provider, 0);
}

#[test]
fn test_single_provider_model_navigation_still_works() {
    let mut h = TestHarness::new();
    open_selector(&mut h, single_provider_columns());

    assert_eq!(h.state.model_selector.selected_model, 1);

    h.press_key(DomainKey::Up);
    assert_eq!(h.state.model_selector.selected_model, 0);

    h.press_key(DomainKey::Down);
    assert_eq!(h.state.model_selector.selected_model, 1);
}

// ═══════════════════════════════════════════════════════════════════════════
// AC5: Context warning y/n confirmation
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_enter_blocked_during_context_warning() {
    let mut h = TestHarness::new();
    open_selector(&mut h, two_provider_columns());

    h.state.model_selector.pending_context_warning =
        Some(rustain::adapters::tui::state::ContextWarning {
            provider_id: "anthropic".to_string(),
            model_id: "claude-opus-4".to_string(),
            model_display_name: "Claude Opus 4".to_string(),
            current_tokens: 250_000,
            context_window: 200_000,
        });

    let action = h.press_key(DomainKey::Enter);
    assert!(matches!(action, InputAction::Consumed));
    assert!(h.state.model_selector.pending_context_warning.is_some());
}

#[test]
fn test_y_confirms_context_warning() {
    let mut h = TestHarness::new();
    open_selector(&mut h, two_provider_columns());

    h.state.model_selector.pending_context_warning =
        Some(rustain::adapters::tui::state::ContextWarning {
            provider_id: "anthropic".to_string(),
            model_id: "claude-opus-4".to_string(),
            model_display_name: "Claude Opus 4".to_string(),
            current_tokens: 250_000,
            context_window: 200_000,
        });

    let action = h.type_char('y');
    match action {
        InputAction::CompactThenSwitchModel {
            provider_id,
            model_id,
        } => {
            assert_eq!(provider_id, "anthropic");
            assert_eq!(model_id, "claude-opus-4");
        }
        other => panic!("Expected CompactThenSwitchModel after y, got {:?}", other),
    }
    assert!(h.state.model_selector.pending_context_warning.is_none());
}

#[test]
fn test_n_cancels_context_warning() {
    let mut h = TestHarness::new();
    open_selector(&mut h, two_provider_columns());

    h.state.model_selector.pending_context_warning =
        Some(rustain::adapters::tui::state::ContextWarning {
            provider_id: "anthropic".to_string(),
            model_id: "claude-opus-4".to_string(),
            model_display_name: "Claude Opus 4".to_string(),
            current_tokens: 250_000,
            context_window: 200_000,
        });

    let action = h.type_char('n');
    assert!(matches!(action, InputAction::Consumed));
    assert!(h.state.model_selector.pending_context_warning.is_none());
    assert!(h.state.model_selector.active);
}

// ═══════════════════════════════════════════════════════════════════════════
// Widget rendering — no crash at 80x24 and 120x40
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_model_selector_renders_at_80x24() {
    let mut h = TestHarness::new();
    open_selector(&mut h, two_provider_columns());

    h.terminal
        .draw(|frame| {
            model_selector::render(frame, frame.area(), &h.state.model_selector, &h.theme);
        })
        .unwrap();
}

#[test]
fn test_model_selector_renders_at_120x40() {
    let mut h = TestHarness::with_size(120, 40);
    open_selector(&mut h, two_provider_columns());

    h.terminal
        .draw(|frame| {
            model_selector::render(frame, frame.area(), &h.state.model_selector, &h.theme);
        })
        .unwrap();
}

#[test]
fn test_model_selector_renders_with_single_provider() {
    let mut h = TestHarness::new();
    open_selector(&mut h, single_provider_columns());

    h.terminal
        .draw(|frame| {
            model_selector::render(frame, frame.area(), &h.state.model_selector, &h.theme);
        })
        .unwrap();
}

#[test]
fn test_model_selector_inactive_render_no_crash() {
    let h = TestHarness::new();
    assert!(!h.state.model_selector.active);

    let mut terminal = h.terminal;
    let state = &h.state.model_selector;
    let theme = &h.theme;

    terminal
        .draw(|frame| {
            model_selector::render(frame, frame.area(), state, theme);
        })
        .unwrap();
}

// ═══════════════════════════════════════════════════════════════════════════
// Regression: Ctrl+X,M blocked when command palette is open
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_model_selector_blocked_when_command_palette_open() {
    let mut h = TestHarness::new();

    h.press_key(DomainKey::CtrlP);
    assert!(h.state.command_palette.active);

    let _action = h.press_key(DomainKey::CtrlX);
    assert!(h.state.command_palette.active);
    assert!(!h.state.which_key.active);
}

// ═══════════════════════════════════════════════════════════════════════════
// Open seeds selection to active provider/model
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_open_seeds_active_provider_and_model() {
    let mut h = TestHarness::new();

    h.state.model_selector.open(
        FocusState::Input,
        two_provider_columns(),
        "openai",
        "gpt-4o-mini",
    );

    assert!(h.state.model_selector.active);
    assert_eq!(h.state.model_selector.selected_provider, 1);
    assert_eq!(h.state.model_selector.selected_model, 1);
}

#[test]
fn test_open_seeds_default_on_unknown_model() {
    let mut h = TestHarness::new();

    h.state.model_selector.open(
        FocusState::Chat,
        two_provider_columns(),
        "unknown-provider",
        "unknown-model",
    );

    assert!(h.state.model_selector.active);
    assert_eq!(h.state.model_selector.selected_provider, 0);
    assert_eq!(h.state.model_selector.selected_model, 0);
}

// ═══════════════════════════════════════════════════════════════════════════
// Focus state is Overlay(ModelSelector) when selector is active
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_focus_is_model_selector_overlay_when_open() {
    let mut h = TestHarness::new();
    open_selector(&mut h, two_provider_columns());

    assert!(matches!(
        h.state.focus,
        FocusState::Overlay(OverlayType::ModelSelector)
    ));
}
