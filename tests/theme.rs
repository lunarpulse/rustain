use ratatui::prelude::*;

use rustain::adapters::tui::color_detect::ColorCapability;
use rustain::adapters::tui::theme::Theme;

// ── Task 5.1: Theme::dark() returns valid theme with all fields populated ──

// Covers: UX-DR5 (color detection and degradation)
#[test]
fn test_dark_theme_name() {
    let theme = Theme::dark();
    assert_eq!(theme.name, "dark");
}

// Covers: UX-DR5 (color detection and degradation)
#[test]
fn test_dark_theme_colors_populated() {
    let theme = Theme::dark();
    // Verify key color tokens are non-default (not all Reset)
    assert_ne!(theme.colors.fg_primary, Color::Reset);
    assert_ne!(theme.colors.accent, Color::Reset);
    assert_ne!(theme.colors.success, Color::Reset);
    assert_ne!(theme.colors.error, Color::Reset);
    assert_ne!(theme.colors.warning, Color::Reset);
    assert_ne!(theme.colors.info, Color::Reset);
}

// Covers: UX-DR5 (color detection and degradation)
#[test]
fn test_dark_theme_spacing_populated() {
    let theme = Theme::dark();
    assert_eq!(theme.spacing.none, 0);
    assert_eq!(theme.spacing.tight, 1);
    assert_eq!(theme.spacing.normal, 2);
    assert_eq!(theme.spacing.loose, 3);
    assert_eq!(theme.spacing.section, 4);
    assert_eq!(theme.spacing.content_padding, 2);
    assert_eq!(theme.spacing.block_gap, 2);
    assert_eq!(theme.spacing.section_gap, 4);
    assert_eq!(theme.spacing.indent_width, 2);
    assert_eq!(theme.spacing.indent_block, 2);
    assert_eq!(theme.spacing.indent_list, 2);
    assert_eq!(theme.spacing.indent_quote, 2);
    assert_eq!(theme.spacing.sidebar_width_min, 30);
    assert_eq!(theme.spacing.sidebar_width_max, 50);
    assert!((theme.spacing.sidebar_width_ratio - 0.3).abs() < f32::EPSILON);
}

// Covers: UX-DR5 (color detection and degradation)
#[test]
fn test_dark_theme_timing_populated() {
    let theme = Theme::dark();
    assert_eq!(theme.timing.tick_interval_ms, 250);
    assert_eq!(theme.timing.cursor_blink_ms, 530);
    assert_eq!(theme.timing.status_hint_fade_sessions, 5);
    assert_eq!(theme.timing.notification_hold_ms, 3000);
    assert_eq!(theme.timing.status_flash_ms, 1000);
    assert_eq!(theme.timing.typing_pulse_ms, 500);
    assert_eq!(theme.timing.which_key_timeout_ms, 2000);
    assert_eq!(theme.timing.auto_scroll_resume_ms, 200);
}

// Covers: UX-DR5 (color detection and degradation)
#[test]
fn test_dark_theme_typography_levels() {
    let theme = Theme::dark();
    // display: BOLD | UNDERLINED
    let display_debug = format!("{:?}", theme.typography.display).to_lowercase();
    assert!(
        display_debug.contains("bold"),
        "display should have bold: {display_debug}"
    );
    assert!(
        display_debug.contains("underlined"),
        "display should have underlined: {display_debug}"
    );
    // body: no modifiers
    assert_eq!(theme.typography.body, Style::default());
    // heading: bold only
    let heading_debug = format!("{:?}", theme.typography.heading).to_lowercase();
    assert!(
        heading_debug.contains("bold"),
        "heading should have bold: {heading_debug}"
    );
    // hint: dim | italic
    let hint_debug = format!("{:?}", theme.typography.hint).to_lowercase();
    assert!(
        hint_debug.contains("dim"),
        "hint should have dim: {hint_debug}"
    );
    assert!(
        hint_debug.contains("italic"),
        "hint should have italic: {hint_debug}"
    );
}

// ── Task 5.3: Monochrome degradation ──

// Covers: UX-DR5 (color detection and degradation)
#[test]
fn test_monochrome_all_colors_reset() {
    let theme = Theme::for_capability(ColorCapability::Monochrome);

    // All color tokens should be Color::Reset
    assert_eq!(theme.colors.bg_primary, Color::Reset);
    assert_eq!(theme.colors.bg_secondary, Color::Reset);
    assert_eq!(theme.colors.bg_surface, Color::Reset);
    assert_eq!(theme.colors.fg_primary, Color::Reset);
    assert_eq!(theme.colors.fg_secondary, Color::Reset);
    assert_eq!(theme.colors.fg_muted, Color::Reset);
    assert_eq!(theme.colors.accent, Color::Reset);
    assert_eq!(theme.colors.success, Color::Reset);
    assert_eq!(theme.colors.warning, Color::Reset);
    assert_eq!(theme.colors.error, Color::Reset);
    assert_eq!(theme.colors.info, Color::Reset);
}

// ── Task 5.4: Color16 degradation ──
// Covers: UX-DR5 (color detection and degradation)

fn is_named_ansi(color: Color) -> bool {
    matches!(
        color,
        Color::Black
            | Color::Red
            | Color::Green
            | Color::Yellow
            | Color::Blue
            | Color::Magenta
            | Color::Cyan
            | Color::Gray
            | Color::DarkGray
            | Color::LightRed
            | Color::LightGreen
            | Color::LightYellow
            | Color::LightBlue
            | Color::LightMagenta
            | Color::LightCyan
            | Color::White
            | Color::Reset
    )
}

#[test]
fn test_color16_uses_named_colors() {
    let theme = Theme::for_capability(ColorCapability::Color16);

    assert!(
        is_named_ansi(theme.colors.fg_primary),
        "fg_primary should be named ANSI: {:?}",
        theme.colors.fg_primary
    );
    assert!(
        is_named_ansi(theme.colors.fg_secondary),
        "fg_secondary should be named ANSI: {:?}",
        theme.colors.fg_secondary
    );
    assert!(
        is_named_ansi(theme.colors.bg_secondary),
        "bg_secondary should be named ANSI: {:?}",
        theme.colors.bg_secondary
    );
    assert!(
        is_named_ansi(theme.colors.bg_surface),
        "bg_surface should be named ANSI: {:?}",
        theme.colors.bg_surface
    );
    assert!(
        is_named_ansi(theme.colors.accent),
        "accent should be named ANSI: {:?}",
        theme.colors.accent
    );
    assert!(
        is_named_ansi(theme.colors.success),
        "success should be named ANSI: {:?}",
        theme.colors.success
    );
    assert!(
        is_named_ansi(theme.colors.error),
        "error should be named ANSI: {:?}",
        theme.colors.error
    );
    assert!(
        is_named_ansi(theme.colors.warning),
        "warning should be named ANSI: {:?}",
        theme.colors.warning
    );
}

// ── Task 5.5: WCAG AA contrast validation ──
// Covers: UX-DR5 (color detection and degradation), NFR5 (accessibility)

fn relative_luminance(r: u8, g: u8, b: u8) -> f64 {
    let linearize = |c: u8| -> f64 {
        let s = c as f64 / 255.0;
        if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linearize(r) + 0.7152 * linearize(g) + 0.0722 * linearize(b)
}

fn contrast_ratio(fg: (u8, u8, u8), bg: (u8, u8, u8)) -> f64 {
    let l1 = relative_luminance(fg.0, fg.1, fg.2);
    let l2 = relative_luminance(bg.0, bg.1, bg.2);
    let (lighter, darker) = if l1 > l2 { (l1, l2) } else { (l2, l1) };
    (lighter + 0.05) / (darker + 0.05)
}

/// Extract RGB tuple from a ratatui Color, returning None for non-RGB colors.
fn color_to_rgb(color: Color) -> Option<(u8, u8, u8)> {
    match color {
        Color::Rgb(r, g, b) => Some((r, g, b)),
        Color::Reset => Some((0, 0, 0)), // Assume black terminal background
        Color::White => Some((255, 255, 255)),
        Color::Green => Some((0, 128, 0)),
        Color::Red => Some((128, 0, 0)),
        Color::Blue => Some((0, 0, 128)),
        Color::Yellow => Some((128, 128, 0)),
        Color::Cyan => Some((0, 128, 128)),
        Color::Magenta => Some((128, 0, 128)),
        Color::DarkGray => Some((128, 128, 128)),
        Color::Gray => Some((192, 192, 192)),
        Color::LightRed => Some((255, 0, 0)),
        Color::LightGreen => Some((0, 255, 0)),
        Color::LightYellow => Some((255, 255, 0)),
        Color::LightBlue => Some((0, 0, 255)),
        Color::LightMagenta => Some((255, 0, 255)),
        Color::LightCyan => Some((0, 255, 255)),
        Color::Black => Some((0, 0, 0)),
        _ => None,
    }
}

#[test]
fn test_wcag_aa_contrast_truecolor() {
    let theme = Theme::dark();

    // All foreground text tokens that should be readable against bg_primary and bg_secondary
    let fg_tokens: Vec<(&str, Color)> = vec![
        ("fg_primary", theme.colors.fg_primary),
        ("fg_secondary", theme.colors.fg_secondary),
        // fg_muted (DarkGray) excluded: named ANSI color with terminal-defined RGB,
        // intentionally low-contrast for de-emphasized text
        ("accent", theme.colors.accent),
        ("success", theme.colors.success),
        ("warning", theme.colors.warning),
        ("error", theme.colors.error),
        ("info", theme.colors.info),
    ];

    let bg_tokens: Vec<(&str, Color)> = vec![
        ("bg_primary", theme.colors.bg_primary),
        ("bg_secondary", theme.colors.bg_secondary),
        ("bg_surface", theme.colors.bg_surface),
    ];

    for (fg_name, fg_color) in &fg_tokens {
        if let Some(fg_rgb) = color_to_rgb(*fg_color) {
            for (bg_name, bg_color) in &bg_tokens {
                if let Some(bg_rgb) = color_to_rgb(*bg_color) {
                    let ratio = contrast_ratio(fg_rgb, bg_rgb);
                    assert!(
                        ratio >= 4.5,
                        "WCAG AA failed: {fg_name} ({fg_color:?}) vs {bg_name} ({bg_color:?}) = {ratio:.2}:1 (need 4.5:1)"
                    );
                }
            }
        }
    }
}

// ── Task 5.6: SemanticSymbol constants ──

// Covers: UX-DR5 (color detection and degradation)
#[test]
fn test_semantic_symbols() {
    use rustain::domain::models::visual::symbols;
    assert_eq!(symbols::SUCCESS, '✓');
    assert_eq!(symbols::WORKING, '●');
    assert_eq!(symbols::ERROR, '✗');
    assert_eq!(symbols::WARNING, '⚠');
    assert_eq!(symbols::OWNED, '♦');
    assert_eq!(symbols::PEER, '◇');
}

// ── Task 5.7: BlockBorder and DensityMode enum variants ──

// Covers: UX-DR5 (color detection and degradation)
#[test]
fn test_block_border_variants() {
    use rustain::domain::models::BlockBorder;

    // Verify all 6 variants exist and are constructible
    let variants = [
        BlockBorder::None,
        BlockBorder::DottedThin,
        BlockBorder::SolidThin,
        BlockBorder::BoldThick,
        BlockBorder::Double,
        BlockBorder::AgentAuto,
    ];
    assert_eq!(variants.len(), 6);

    // Verify Serialize/Deserialize (round-trip)
    let json = serde_json::to_string(&BlockBorder::SolidThin).unwrap();
    let deserialized: BlockBorder = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized, BlockBorder::SolidThin);
}

// Covers: UX-DR5 (color detection and degradation)
#[test]
fn test_density_mode_variants() {
    use rustain::domain::models::DensityMode;

    let variants = [
        DensityMode::Focus,
        DensityMode::Monitor,
        DensityMode::Dashboard,
    ];
    assert_eq!(variants.len(), 3);

    let json = serde_json::to_string(&DensityMode::Focus).unwrap();
    let deserialized: DensityMode = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized, DensityMode::Focus);
}

// ── Task 5.9: FocusState variant count and structure ──

// Covers: FR22 (vim keybindings)
#[test]
fn test_focus_state_variants() {
    use rustain::domain::models::FocusState;
    use rustain::domain::models::visual::{OverlayType, PanelType};

    // 4 variants: Input, Chat, Sidebar, Overlay
    let input = FocusState::Input;
    let chat = FocusState::Chat;
    let sidebar = FocusState::Sidebar {
        panel: PanelType::History,
        selected: 0,
    };
    let overlay = FocusState::Overlay(OverlayType::CommandPalette);

    // Chat is a unit variant (scroll state owned by TuiState)
    assert_eq!(chat, FocusState::Chat);

    // All variants are distinct
    assert_ne!(input, chat);
    assert_ne!(input, sidebar);
    assert_ne!(input, overlay);
}

// Covers: FR22 (vim keybindings)
#[test]
fn test_overlay_type_variants() {
    use rustain::domain::models::visual::OverlayType;

    // 5 variants (no Confirmation yet)
    let variants = [
        OverlayType::CommandPalette,
        OverlayType::ModelSelector,
        OverlayType::ProfileSwitcher,
        OverlayType::WhichKey,
        OverlayType::Help,
    ];
    assert_eq!(variants.len(), 5);
}

// ── Task 5.8: Integration test -- TUI renders with Theme applied ──

// Covers: UX-DR5 (color detection and degradation)
#[test]
fn test_tui_renders_with_theme_colors() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use rustain::adapters::tui::layout;
    use rustain::adapters::tui::state::TuiState;
    use rustain::adapters::tui::widgets::status_bar;

    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    let state = TuiState::new(80, 24);

    terminal
        .draw(|frame| {
            let area = frame.area();
            if let Some(app_layout) = layout::compute_layout(area, &state.theme, &state.input_buffer) {
                status_bar::render(
                    frame,
                    app_layout.status_bar,
                    "test-model",
                    &state.status,
                    &state.theme,
                    0,
                    &[],
                    0,
                    app_layout.chat_pane.height,
                    rustain::domain::models::PermissionMode::Normal,
                    state.token_usage.as_ref(),
                    state.has_project_context,
                    None,
                    state.multiline_mode,
                );
            }
        })
        .unwrap();

    let buffer = terminal.backend().buffer().clone();
    // The status bar is at row height-4 (1 row, between chat and input)
    // In 80x24: chat=0..20, status_bar=20, input=21..23
    let status_row = 20;
    let cell = &buffer[(1, status_row)];
    // The status bar should have theme-styled foreground (status_fg = Rgb(136,136,136))
    // and background (status_bg = Rgb(30,30,30))
    assert!(
        cell.fg != Color::Reset || cell.bg != Color::Reset,
        "Status bar cell at (1,{status_row}) should use theme colors. fg={:?}, bg={:?}",
        cell.fg,
        cell.bg
    );
}

// ── Review patch: Color256 degradation boundary test ──

// Covers: UX-DR5 (color detection and degradation)
#[test]
fn test_color256_degradation_boundary_values() {
    let theme = Theme::for_capability(ColorCapability::Color256);

    // All Rgb colors should now be Color::Indexed
    let check_indexed = |name: &str, color: Color| {
        assert!(
            matches!(
                color,
                Color::Indexed(_) | Color::Reset | Color::White | Color::DarkGray
            ),
            "{name} should be Indexed after 256 degradation, got {:?}",
            color
        );
    };

    check_indexed("bg_secondary", theme.colors.bg_secondary);
    check_indexed("bg_surface", theme.colors.bg_surface);
    check_indexed("fg_secondary", theme.colors.fg_secondary);
    check_indexed("accent", theme.colors.accent);
    check_indexed("success", theme.colors.success);
    check_indexed("error", theme.colors.error);
    check_indexed("info", theme.colors.info);
}
