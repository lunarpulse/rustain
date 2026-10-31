//! Styled, independently balanced OSC 8 cells and default-on output detection.

use std::{io::IsTerminal, num::NonZeroU16, sync::OnceLock};

use ratatui::{
    buffer::{Buffer, CellDiffOption},
    layout::Rect,
};

use crate::infrastructure::utils::env_var_trimmed;

/// OSC 8 is on by default and denied only where it is unsafe: stdout is not a
/// terminal, `TERM` is missing, empty or `dumb`, or the session is GNU screen
/// (`STY` set, or a `screen` `TERM` family outside tmux, whose default `TERM` is
/// also `screen`). The Linux console parser swallows the sequence silently, so it
/// needs no denial. Hints use the environment helper's trimming and empty-value
/// semantics.
pub fn supports_osc8(
    stdout_is_terminal: bool,
    term: Option<&str>,
    tmux: Option<&str>,
    sty: Option<&str>,
) -> bool {
    fn non_empty(value: Option<&str>) -> Option<&str> {
        value.map(str::trim).filter(|value| !value.is_empty())
    }
    let Some(term) = non_empty(term) else {
        return false;
    };
    let family = term.split(['-', '.']).next().unwrap_or_default();
    stdout_is_terminal
        && !term.eq_ignore_ascii_case("dumb")
        && non_empty(sty).is_none()
        && !(family.eq_ignore_ascii_case("screen") && non_empty(tmux).is_none())
}

/// Capture the output's environment/TTY once, never probe during a frame.
pub fn terminal_supports_osc8() -> bool {
    static SUPPORT: OnceLock<bool> = OnceLock::new();
    *SUPPORT.get_or_init(|| {
        supports_osc8(
            std::io::stdout().is_terminal(),
            env_var_trimmed("TERM").as_deref(),
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
    fn capability_denials_cover_only_unsafe_outputs() {
        for term in [
            None,
            Some(""),
            Some("DuMb"),
            Some("screen-256color"),
            Some("screen"),
        ] {
            assert!(!supports_osc8(true, term, None, None), "{term:?}");
        }
        assert!(!supports_osc8(false, Some("xterm-256color"), None, None));
        assert!(!supports_osc8(
            true,
            Some("xterm-256color"),
            None,
            Some("session")
        ));
        assert!(!supports_osc8(
            true,
            Some("screen-256color"),
            Some(""),
            None
        ));
        assert!(!supports_osc8(
            true,
            Some("screen.xterm-256color"),
            None,
            None
        ));
    }

    #[test]
    fn capability_is_default_on_for_every_other_terminal() {
        for (term, tmux) in [
            ("xterm-256color", None),
            ("xterm-kitty", None),
            ("xterm-ghostty", None),
            ("foot", None),
            ("linux", None),
            ("vt100", None),
            ("tmux-256color", Some("/tmp/tmux-1000/default,1,0")),
            ("tmux", None),
            ("screen-256color", Some("/tmp/tmux-1000/default,1,0")),
            ("screen", Some("session")),
        ] {
            assert!(supports_osc8(true, Some(term), tmux, None), "{term}");
        }
    }

    #[test]
    fn capability_hints_follow_trimmed_environment_semantics() {
        let cases = [
            (Some(""), None, None, false),
            (Some(" \t"), None, None, false),
            (Some(" dumb "), None, None, false),
            (Some(" screen-256color "), None, None, false),
            (Some(" screen-256color "), Some(" \t"), None, false),
            (Some(" screen-256color "), Some(" 1 "), None, true),
            (Some(" xterm-256color "), None, None, true),
            (Some("xterm-256color"), Some(""), Some(""), true),
            (Some("xterm-256color"), Some(" \t"), Some(" \n"), true),
            (Some("xterm-256color"), None, Some(" session "), false),
            (
                Some("xterm-256color"),
                Some(" 1 "),
                Some(" session "),
                false,
            ),
        ];
        for (term, tmux, sty, expected) in cases {
            assert_eq!(
                supports_osc8(true, term, tmux, sty),
                expected,
                "{term:?}, {tmux:?}, {sty:?}"
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
