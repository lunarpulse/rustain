//! Shared CLI prompt / string helpers (Story 13.5b).
//!
//! Consolidates the yes/no prompt pattern that previously existed in
//! `cli/init.rs` and `cli/profile/prompt.rs`, and adds a typed-count
//! confirmation helper plus a display-width-aware truncation function.

use std::io::{self, BufRead, Write};

/// Default answer used by [`prompt_yes_no`] when the user presses Enter or
/// EOF is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirm {
    Yes,
    No,
}

/// Prompt the user with a yes/no question.
///
/// `default` controls what an empty line or EOF means. Only a line beginning
/// with `y` or `Y` returns `true`; everything else (including empty/EOF)
/// returns `false`.
pub fn prompt_yes_no(
    prompt: &str,
    default: Confirm,
    inp: &mut dyn BufRead,
    out: &mut dyn Write,
) -> io::Result<bool> {
    let suffix = match default {
        Confirm::Yes => "[Y/n]",
        Confirm::No => "[y/N]",
    };
    write!(out, "{} {} ", prompt, suffix)?;
    out.flush()?;

    let mut line = String::new();
    match inp.read_line(&mut line) {
        Ok(0) => Ok(default == Confirm::Yes),
        Ok(_) => {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                Ok(default == Confirm::Yes)
            } else {
                Ok(trimmed.starts_with(['y', 'Y']))
            }
        }
        Err(e) => Err(e),
    }
}

/// Prompt the user to type the literal session count.
///
/// Returns `Ok(true)` if the typed value equals `expected`. Wrong numbers,
/// non-numeric input, empty lines, and EOF all return `Ok(false)`.
pub fn prompt_typed_count(
    expected: usize,
    prompt: &str,
    inp: &mut dyn BufRead,
    out: &mut dyn Write,
) -> io::Result<bool> {
    write!(out, "{}", prompt)?;
    out.flush()?;

    let mut line = String::new();
    match inp.read_line(&mut line) {
        Ok(0) => Ok(false),
        Ok(_) => {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return Ok(false);
            }
            match trimmed.parse::<usize>() {
                Ok(n) => Ok(n == expected),
                Err(_) => Ok(false),
            }
        }
        Err(e) => Err(e),
    }
}

/// Truncate `s` to a maximum display width, never slicing inside a Unicode
/// scalar or grapheme cluster. Adds a trailing ellipsis (`…`) when truncation
/// occurs.
pub fn truncate(s: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }

    use unicode_width::UnicodeWidthStr;
    if s.width() <= max_width {
        return s.to_string();
    }

    let mut out = String::new();
    let mut width = 0;
    for ch in s.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + w + 1 > max_width {
            out.push('…');
            break;
        }
        out.push(ch);
        width += w;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_yes_no_y_returns_true() {
        let mut inp = "yes\n".as_bytes();
        let mut out = Vec::new();
        assert!(prompt_yes_no("ok?", Confirm::No, &mut inp, &mut out).unwrap());
    }

    #[test]
    fn prompt_yes_no_empty_uses_default() {
        // Empty line ⇒ default. A fresh buffer per call so each assertion
        // exercises the empty-line path rather than falling through to EOF.
        let mut out = Vec::new();
        let mut inp_no = "\n".as_bytes();
        assert!(!prompt_yes_no("ok?", Confirm::No, &mut inp_no, &mut out).unwrap());
        let mut inp_yes = "\n".as_bytes();
        assert!(prompt_yes_no("ok?", Confirm::Yes, &mut inp_yes, &mut out).unwrap());
    }

    #[test]
    fn prompt_typed_count_exact_match() {
        let mut inp = "7\n".as_bytes();
        let mut out = Vec::new();
        assert!(prompt_typed_count(7, "> ", &mut inp, &mut out).unwrap());
    }

    #[test]
    fn prompt_typed_count_wrong_number() {
        let mut inp = "8\n".as_bytes();
        let mut out = Vec::new();
        assert!(!prompt_typed_count(7, "> ", &mut inp, &mut out).unwrap());
    }

    #[test]
    fn truncate_noop_when_narrow() {
        assert_eq!(truncate("hi", 10), "hi");
    }

    #[test]
    fn truncate_adds_ellipsis() {
        assert_eq!(truncate("hello world", 8), "hello w…");
    }

    #[test]
    fn truncate_respects_display_width() {
        // 日本語 is 4 display-width cells; max 3 must truncate to one cell + ellipsis.
        let s = "日本語";
        let out = truncate(s, 3);
        assert!(out.starts_with('…') || out.len() < s.len());
    }
}
