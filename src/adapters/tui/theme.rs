use ratatui::prelude::*;
use ratatui::symbols::line;
use ratatui::widgets::BorderType;

use crate::domain::models::BlockBorder;

use super::color_detect::ColorCapability;

/// Complete design token system for consistent visual grammar.
pub struct Theme {
    pub name: String,
    pub colors: ColorTokens,
    pub spacing: SpacingTokens,
    pub timing: TimingTokens,
    pub borders: BorderTokens,
    pub typography: TypographyTokens,
}

/// Semantic color tokens for all UI elements.
pub struct ColorTokens {
    pub bg_primary: Color,
    pub bg_secondary: Color,
    pub bg_surface: Color,
    pub fg_primary: Color,
    pub fg_secondary: Color,
    pub fg_muted: Color,
    pub accent: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    pub info: Color,
    pub tool_name: Color,
    pub tool_border_collapsed: Color,
    pub tool_border_expanded: Color,
    pub code_span: Color,
    pub code_block_bg: Color,
    pub code_block_border: Color,
    pub permission_border: Color,
    pub decision_border: Color,
    pub error_border: Color,
    pub warning_border: Color,
    pub info_border: Color,
    pub auto_sent_border: Color,
    pub status_bg: Color,
    pub status_fg: Color,
    pub status_streaming: Color,
    pub status_approval: Color,
    pub status_yolo_warning: Color,
    pub profile_coding: Color,
    pub profile_personal: Color,
    pub profile_devops: Color,
    pub profile_custom: Color,
    pub profile_base: Color,
    /// Hint text color (dim, italic) for token estimates and secondary info.
    pub text_hint: Color,
}

/// Spacing tokens for layout calculations.
pub struct SpacingTokens {
    // Vertical spacing
    pub none: u16,
    pub tight: u16,
    pub normal: u16,
    pub loose: u16,
    pub section: u16,
    // Horizontal spacing
    pub content_padding: u16,
    pub block_gap: u16,
    pub section_gap: u16,
    pub indent_width: u16,
    pub indent_block: u16,
    pub indent_list: u16,
    pub indent_quote: u16,
    // Sidebar dimensions (populated but unused until sidebar story)
    pub sidebar_width_min: u16,
    pub sidebar_width_max: u16,
    pub sidebar_width_ratio: f32,
}

/// Timing tokens for temporal behavior.
pub struct TimingTokens {
    pub tick_interval_ms: u64,
    pub cursor_blink_ms: u64,
    pub status_hint_fade_sessions: u32,
    pub notification_hold_ms: u64,
    pub status_flash_ms: u64,
    pub typing_pulse_ms: u64,
    pub which_key_timeout_ms: u64,
    pub auto_scroll_resume_ms: u64,
}

/// Border token for a single BlockBorder variant.
pub struct BorderToken {
    pub border_type: BorderType,
    pub line_set: Option<line::Set<'static>>,
    pub style: Style,
}

/// Border tokens mapping each `BlockBorder` variant to rendering info.
pub struct BorderTokens {
    pub none: BorderToken,
    pub dotted_thin: BorderToken,
    pub solid_thin: BorderToken,
    pub bold_thick: BorderToken,
    pub double: BorderToken,
    pub agent_auto: BorderToken,
}

/// Typography levels using ratatui Style modifiers.
pub struct TypographyTokens {
    pub display: Style,
    pub heading: Style,
    pub subheading: Style,
    pub body: Style,
    pub meta: Style,
    pub hint: Style,
}

/// Custom dotted line set for DottedThin and AgentAuto borders.
pub fn dotted_line_set() -> line::Set<'static> {
    line::Set {
        horizontal: "┄",
        vertical: "┊",
        top_left: "┌",
        top_right: "┐",
        bottom_left: "└",
        bottom_right: "┘",
        vertical_left: "┤",
        vertical_right: "├",
        horizontal_down: "┬",
        horizontal_up: "┴",
        cross: "┼",
    }
}

impl Theme {
    /// Create the default dark theme with all hardcoded values from the UX spec.
    pub fn dark() -> Self {
        Self {
            name: "dark".to_string(),
            colors: ColorTokens {
                bg_primary: Color::Reset,
                bg_secondary: Color::Rgb(30, 30, 30),
                bg_surface: Color::Rgb(40, 40, 40),
                fg_primary: Color::White,
                fg_secondary: Color::Rgb(155, 155, 155),
                fg_muted: Color::DarkGray,
                accent: Color::Rgb(0, 200, 200),
                success: Color::Rgb(0, 200, 0),
                warning: Color::Rgb(230, 200, 0),
                error: Color::Rgb(255, 100, 100),
                info: Color::Rgb(100, 150, 255),
                tool_name: Color::Rgb(0, 200, 200),
                tool_border_collapsed: Color::DarkGray,
                tool_border_expanded: Color::Rgb(155, 155, 155),
                code_span: Color::Rgb(200, 200, 200),
                code_block_bg: Color::Rgb(30, 30, 30),
                code_block_border: Color::DarkGray,
                permission_border: Color::White,
                decision_border: Color::White,
                error_border: Color::Rgb(255, 100, 100),
                warning_border: Color::Rgb(230, 200, 0),
                info_border: Color::Rgb(100, 150, 255),
                auto_sent_border: Color::Rgb(180, 80, 200),
                status_bg: Color::Rgb(30, 30, 30),
                status_fg: Color::Rgb(155, 155, 155),
                status_streaming: Color::Rgb(0, 200, 200),
                status_approval: Color::Rgb(230, 200, 0),
                status_yolo_warning: Color::Rgb(255, 100, 100),
                profile_coding: Color::Rgb(100, 150, 255),
                profile_personal: Color::Rgb(0, 200, 0),
                profile_devops: Color::Rgb(230, 200, 0),
                profile_custom: Color::Rgb(180, 80, 200),
                profile_base: Color::Rgb(128, 128, 128),
                text_hint: Color::DarkGray,
            },
            spacing: SpacingTokens {
                none: 0,
                tight: 1,
                normal: 2,
                loose: 3,
                section: 4,
                content_padding: 2,
                block_gap: 2,
                section_gap: 4,
                indent_width: 2,
                indent_block: 2,
                indent_list: 2,
                indent_quote: 2,
                sidebar_width_min: 30,
                sidebar_width_max: 50,
                sidebar_width_ratio: 0.3,
            },
            timing: TimingTokens {
                tick_interval_ms: 250,
                cursor_blink_ms: 530,
                status_hint_fade_sessions: 5,
                notification_hold_ms: 3000,
                status_flash_ms: 1000,
                typing_pulse_ms: 500,
                which_key_timeout_ms: 2000,
                auto_scroll_resume_ms: 200,
            },
            borders: Self::dark_borders(),
            typography: TypographyTokens {
                display: Style::default()
                    .add_modifier(Modifier::BOLD)
                    .add_modifier(Modifier::UNDERLINED),
                heading: Style::default().add_modifier(Modifier::BOLD),
                subheading: Style::default()
                    .add_modifier(Modifier::BOLD)
                    .add_modifier(Modifier::DIM),
                body: Style::default(),
                meta: Style::default().add_modifier(Modifier::DIM),
                hint: Style::default()
                    .add_modifier(Modifier::DIM)
                    .add_modifier(Modifier::ITALIC),
            },
        }
    }

    fn dark_borders() -> BorderTokens {
        BorderTokens {
            none: BorderToken {
                border_type: BorderType::Plain,
                line_set: None,
                style: Style::default(),
            },
            dotted_thin: BorderToken {
                border_type: BorderType::Plain,
                line_set: Some(dotted_line_set()),
                style: Style::default().fg(Color::DarkGray),
            },
            solid_thin: BorderToken {
                border_type: BorderType::Plain,
                line_set: None,
                style: Style::default().fg(Color::DarkGray),
            },
            bold_thick: BorderToken {
                border_type: BorderType::Thick,
                line_set: None,
                style: Style::default().fg(Color::White),
            },
            double: BorderToken {
                border_type: BorderType::Double,
                line_set: None,
                style: Style::default().fg(Color::White),
            },
            agent_auto: BorderToken {
                border_type: BorderType::Plain,
                line_set: Some(dotted_line_set()),
                style: Style::default().fg(Color::Rgb(180, 80, 200)),
            },
        }
    }

    /// Create a theme adapted to the detected terminal color capability.
    pub fn for_capability(capability: ColorCapability) -> Self {
        let mut theme = Self::dark();
        match capability {
            ColorCapability::TrueColor => {} // Full RGB, no changes
            ColorCapability::Color256 => theme.degrade_to_256(),
            ColorCapability::Color16 => theme.degrade_to_16(),
            ColorCapability::Monochrome => theme.degrade_to_monochrome(),
        }
        theme
    }

    /// Look up the border token for a given `BlockBorder` variant.
    pub fn border_for(&self, border: BlockBorder) -> &BorderToken {
        match border {
            BlockBorder::None => &self.borders.none,
            BlockBorder::DottedThin => &self.borders.dotted_thin,
            BlockBorder::SolidThin => &self.borders.solid_thin,
            BlockBorder::BoldThick => &self.borders.bold_thick,
            BlockBorder::Double => &self.borders.double,
            BlockBorder::AgentAuto => &self.borders.agent_auto,
        }
    }

    fn degrade_to_256(&mut self) {
        self.colors.apply_all(rgb_to_color256);
        self.degrade_borders(rgb_to_color256);
    }

    fn degrade_to_16(&mut self) {
        self.colors.apply_all(rgb_to_color16);
        self.degrade_borders(rgb_to_color16);
    }

    fn degrade_to_monochrome(&mut self) {
        self.colors.apply_all(|_| Color::Reset);
        self.degrade_borders(|_| Color::Reset);
        // Typography: keep modifiers only (they already don't use color)
    }

    fn degrade_borders(&mut self, mapper: fn(Color) -> Color) {
        for token in [
            &mut self.borders.none,
            &mut self.borders.dotted_thin,
            &mut self.borders.solid_thin,
            &mut self.borders.bold_thick,
            &mut self.borders.double,
            &mut self.borders.agent_auto,
        ] {
            if let Some(fg) = token.style.fg {
                token.style = token.style.fg(mapper(fg));
            }
        }
    }
}

impl ColorTokens {
    fn apply_all(&mut self, mapper: fn(Color) -> Color) {
        self.bg_primary = mapper(self.bg_primary);
        self.bg_secondary = mapper(self.bg_secondary);
        self.bg_surface = mapper(self.bg_surface);
        self.fg_primary = mapper(self.fg_primary);
        self.fg_secondary = mapper(self.fg_secondary);
        self.fg_muted = mapper(self.fg_muted);
        self.accent = mapper(self.accent);
        self.success = mapper(self.success);
        self.warning = mapper(self.warning);
        self.error = mapper(self.error);
        self.info = mapper(self.info);
        self.tool_name = mapper(self.tool_name);
        self.tool_border_collapsed = mapper(self.tool_border_collapsed);
        self.tool_border_expanded = mapper(self.tool_border_expanded);
        self.code_span = mapper(self.code_span);
        self.code_block_bg = mapper(self.code_block_bg);
        self.code_block_border = mapper(self.code_block_border);
        self.permission_border = mapper(self.permission_border);
        self.decision_border = mapper(self.decision_border);
        self.error_border = mapper(self.error_border);
        self.warning_border = mapper(self.warning_border);
        self.info_border = mapper(self.info_border);
        self.auto_sent_border = mapper(self.auto_sent_border);
        self.status_bg = mapper(self.status_bg);
        self.status_fg = mapper(self.status_fg);
        self.status_streaming = mapper(self.status_streaming);
        self.status_approval = mapper(self.status_approval);
        self.status_yolo_warning = mapper(self.status_yolo_warning);
        self.profile_coding = mapper(self.profile_coding);
        self.profile_personal = mapper(self.profile_personal);
        self.profile_devops = mapper(self.profile_devops);
        self.profile_custom = mapper(self.profile_custom);
        self.profile_base = mapper(self.profile_base);
        self.text_hint = mapper(self.text_hint);
    }
}

/// Map an RGB color to the nearest 256-color palette index.
fn rgb_to_color256(color: Color) -> Color {
    match color {
        Color::Rgb(r, g, b) => Color::Indexed(rgb_to_256_index(r, g, b)),
        Color::Reset => Color::Reset,
        // Named ANSI colors are already valid in 256-color mode
        other => other,
    }
}

/// Map an RGB color to the nearest base-16 ANSI color.
fn rgb_to_color16(color: Color) -> Color {
    match color {
        Color::Rgb(r, g, b) => rgb_to_ansi16(r, g, b),
        Color::Reset => Color::Reset,
        Color::Indexed(n) if n < 16 => indexed_to_ansi16(n),
        Color::Indexed(n) => {
            let (r, g, b) = index256_to_rgb(n);
            rgb_to_ansi16(r, g, b)
        }
        other => other,
    }
}

/// Convert RGB to nearest 256-color index.
fn rgb_to_256_index(r: u8, g: u8, b: u8) -> u8 {
    // Check if close to grayscale
    let avg = (r as u16 + g as u16 + b as u16) / 3;
    let spread = (r as i16 - avg as i16).unsigned_abs()
        + (g as i16 - avg as i16).unsigned_abs()
        + (b as i16 - avg as i16).unsigned_abs();

    if spread < 20 {
        // Map to 24-shade grayscale ramp (indices 232–255)
        // Gray levels: 8, 18, 28, ..., 238
        if avg < 8 {
            return 16; // black (covers 0-7 where subtraction would underflow)
        }
        if avg > 243 {
            return 231; // white
        }
        return (232 + ((avg - 8) * 24 / 236)) as u8;
    }

    // Map to 6x6x6 cube (indices 16–231)
    let ri = (r as u16 * 5 / 255) as u8;
    let gi = (g as u16 * 5 / 255) as u8;
    let bi = (b as u16 * 5 / 255) as u8;
    16 + 36 * ri + 6 * gi + bi
}

/// Convert 256-palette index to approximate RGB.
fn index256_to_rgb(n: u8) -> (u8, u8, u8) {
    match n {
        0..=15 => {
            // Standard 16 ANSI colors — approximate
            match n {
                0 => (0, 0, 0),
                1 => (128, 0, 0),
                2 => (0, 128, 0),
                3 => (128, 128, 0),
                4 => (0, 0, 128),
                5 => (128, 0, 128),
                6 => (0, 128, 128),
                7 => (192, 192, 192),
                8 => (128, 128, 128),
                9 => (255, 0, 0),
                10 => (0, 255, 0),
                11 => (255, 255, 0),
                12 => (0, 0, 255),
                13 => (255, 0, 255),
                14 => (0, 255, 255),
                15 => (255, 255, 255),
                _ => unreachable!(),
            }
        }
        16..=231 => {
            let idx = n - 16;
            let b = (idx % 6) * 51;
            let g = ((idx / 6) % 6) * 51;
            let r = (idx / 36) * 51;
            (r, g, b)
        }
        232..=255 => {
            let gray = 8 + (n - 232) * 10;
            (gray, gray, gray)
        }
    }
}

/// Map RGB to nearest named ANSI 16-color.
fn rgb_to_ansi16(r: u8, g: u8, b: u8) -> Color {
    let luminance = 0.299 * r as f64 + 0.587 * g as f64 + 0.114 * b as f64;
    let is_bright = luminance > 128.0;

    // Find dominant channel(s)
    let max = r.max(g).max(b);
    if max < 40 {
        return Color::Black;
    }

    let threshold = max / 2;
    let has_r = r > threshold;
    let has_g = g > threshold;
    let has_b = b > threshold;

    match (has_r, has_g, has_b, is_bright) {
        (true, false, false, false) => Color::Red,
        (true, false, false, true) => Color::LightRed,
        (false, true, false, false) => Color::Green,
        (false, true, false, true) => Color::LightGreen,
        (false, false, true, false) => Color::Blue,
        (false, false, true, true) => Color::LightBlue,
        (true, true, false, false) => Color::Yellow,
        (true, true, false, true) => Color::LightYellow,
        (true, false, true, false) => Color::Magenta,
        (true, false, true, true) => Color::LightMagenta,
        (false, true, true, false) => Color::Cyan,
        (false, true, true, true) => Color::LightCyan,
        (true, true, true, false) => Color::Gray,
        (true, true, true, true) => Color::White,
        _ => Color::DarkGray,
    }
}

/// Convert a 256-color index (0–15) to a named ANSI Color.
fn indexed_to_ansi16(n: u8) -> Color {
    match n {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        7 => Color::Gray,
        8 => Color::DarkGray,
        9 => Color::LightRed,
        10 => Color::LightGreen,
        11 => Color::LightYellow,
        12 => Color::LightBlue,
        13 => Color::LightMagenta,
        14 => Color::LightCyan,
        15 => Color::White,
        _ => Color::Gray,
    }
}
