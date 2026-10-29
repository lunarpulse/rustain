//! Styled, independently balanced OSC 8 cells and conservative output detection.

use std::{io::IsTerminal, num::NonZeroU16, sync::OnceLock};

use ratatui::{
    buffer::{Buffer, CellDiffOption},
    layout::Rect,
};

use crate::infrastructure::utils::env_var_trimmed;

/// Conservative terminal hints; denials take precedence over known programs.
/// Hints use the same trimming and empty-value semantics as the environment helper.
pub fn supports_osc8(
    stdout_is_terminal: bool,
    term: Option<&str>,
    term_program: Option<&str>,
    tmux: Option<&str>,
    sty: Option<&str>,
) -> bool {
    let term = term.map(str::trim);
    let term_program = term_program.map(str::trim);
    let tmux = tmux.map(str::trim);
    let sty = sty.map(str::trim);
    let Some(term) = term.filter(|term| !term.is_empty()) else {
        return false;
    };
    let family = term.split(['-', '.']).next().unwrap_or_default();
    if !stdout_is_terminal
        || term.eq_ignore_ascii_case("dumb")
        || family.eq_ignore_ascii_case("screen")
        || family.eq_ignore_ascii_case("tmux")
        || tmux.is_some_and(|value| !value.is_empty())
        || sty.is_some_and(|value| !value.is_empty())
    {
        return false;
    }
    term_program.is_some_and(|program| {
        ["wezterm", "kitty", "iTerm.app", "iTerm2", "foot"]
            .iter()
            .any(|known| program.eq_ignore_ascii_case(known))
    }) || matches!(term, "xterm-kitty" | "foot" | "foot-extra")
}

/// Capture the output's environment/TTY once, never probe during a frame.
pub fn terminal_supports_osc8() -> bool {
    static SUPPORT: OnceLock<bool> = OnceLock::new();
    *SUPPORT.get_or_init(|| {
        supports_osc8(
            std::io::stdout().is_terminal(),
            env_var_trimmed("TERM").as_deref(),
            env_var_trimmed("TERM_PROGRAM").as_deref(),
            env_var_trimmed("TMUX").as_deref(),
            env_var_trimmed("STY").as_deref(),
        )
    })
}

/// Decorate already-laid-out one-column URL cells, preserving their styles.
/// `url` must be the result of `login_ui::safe_url`; the renderer validates it
/// once before layout. No escapes participate in Paragraph's width calculation.
pub(super) fn encode(buffer: &mut Buffer, visible: Rect, url: &str) {
    let visible = visible.intersection(buffer.area);
    if visible.is_empty() {
        return;
    }
    let prefix = format!("\x1b]8;;{url}\x07");
    let mut symbol = String::with_capacity(prefix.len() + 4 + "\x1b]8;;\x07".len());
    for y in visible.y..visible.bottom() {
        for x in visible.x..visible.right() {
            let cell = &mut buffer[(x, y)];
            symbol.clear();
            symbol.push_str(&prefix);
            symbol.push_str(cell.symbol());
            symbol.push_str("\x1b]8;;\x07");
            cell.set_symbol(&symbol)
                .set_diff_option(CellDiffOption::ForcedWidth(NonZeroU16::new(1).unwrap()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_denials_override_known_terminal_hints() {
        for term in [
            None,
            Some(""),
            Some("DuMb"),
            Some("screen-256color"),
            Some("tmux"),
        ] {
            assert!(!supports_osc8(true, term, Some("wezterm"), None, None));
        }
        assert!(!supports_osc8(false, Some("foot"), None, None, None));
        assert!(!supports_osc8(
            true,
            Some("foot"),
            None,
            Some("session"),
            None
        ));
        assert!(!supports_osc8(
            true,
            Some("foot"),
            None,
            None,
            Some("session")
        ));
        assert!(supports_osc8(
            true,
            Some("xterm-256color"),
            Some("WezTerm"),
            None,
            None
        ));
        assert!(supports_osc8(true, Some("xterm-kitty"), None, None, None));
        assert!(!supports_osc8(
            true,
            Some("xterm-256color"),
            None,
            None,
            None
        ));
    }

    #[test]
    fn capability_hints_follow_trimmed_environment_semantics() {
        let cases = [
            (Some(""), Some("wezterm"), None, None, false),
            (Some(" \t"), Some("wezterm"), None, None, false),
            (Some(" dumb "), Some("wezterm"), None, None, false),
            (
                Some(" screen-256color "),
                Some("wezterm"),
                None,
                None,
                false,
            ),
            (Some(" tmux "), Some("wezterm"), None, None, false),
            (
                Some(" xterm-256color "),
                Some(" WezTerm "),
                None,
                None,
                true,
            ),
            (Some(" foot "), None, None, None, true),
            (Some("xterm-256color"), Some(" \t"), None, None, false),
            (Some("foot"), None, Some(""), Some(""), true),
            (Some("foot"), None, Some(" \t"), Some(" \n"), true),
            (Some("foot"), None, Some(" session "), None, false),
            (Some("foot"), None, None, Some(" session "), false),
        ];
        for (term, program, tmux, sty, expected) in cases {
            assert_eq!(
                supports_osc8(true, term, program, tmux, sty),
                expected,
                "{term:?}, {program:?}, {tmux:?}, {sty:?}"
            );
        }
    }

    #[test]
    fn encoded_cells_keep_style_and_are_clipped_to_visible_rectangle() {
        use ratatui::{
            buffer::Buffer,
            layout::Rect,
            style::{Color, Modifier, Style},
        };
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 2));
        let style = Style::default()
            .fg(Color::Cyan)
            .bg(Color::Blue)
            .add_modifier(Modifier::BOLD);
        buffer.set_string(0, 0, "abc!rest", style);
        encode(&mut buffer, Rect::new(0, 0, 3, 1), "https://a.test/full");
        for x in 0..3 {
            let cell = &buffer[(x, 0)];
            assert_eq!(cell.style(), buffer[(3, 0)].style());
            assert_eq!(
                cell.diff_option,
                ratatui::buffer::CellDiffOption::ForcedWidth(std::num::NonZeroU16::new(1).unwrap())
            );
            assert!(cell.symbol().starts_with("\x1b]8;;https://a.test/full\x07"));
            assert!(cell.symbol().ends_with("\x1b]8;;\x07"));
        }
        assert_eq!(buffer[(3, 0)].symbol(), "!");
        let before = buffer.clone();
        encode(&mut buffer, Rect::new(0, 1, 0, 1), "https://a.test/");
        assert_eq!(buffer, before);
    }
}
