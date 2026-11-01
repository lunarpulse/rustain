//! Stage 1: sanitize raw markdown input.
//!
//! * Normalises `\r\n` and bare `\r` to `\n`.
//! * In [`RenderMode::Streaming`] only, appends display-only closers for the
//!   formatting still open in the **trailing paragraph** (D3) so a half-typed
//!   `**bold te` paints bold instead of flickering as literal asterisks.
//!
//! Source characters are never removed or rewritten other than line endings.
//! Fences are left to CommonMark: an unclosed fence already runs to the end of
//! the input, and inline triple backticks never open a block (D4).

use std::borrow::Cow;

use super::RenderMode;

/// Sanitize `input` for `mode`. `Literal` text is never parsed, so it is only
/// line-ending normalised.
#[must_use]
pub fn sanitize(input: &str, mode: RenderMode) -> Cow<'_, str> {
    let normalized = if input.contains('\r') {
        Cow::Owned(input.replace("\r\n", "\n").replace('\r', "\n"))
    } else {
        Cow::Borrowed(input)
    };
    if mode != RenderMode::Streaming {
        return normalized;
    }
    match closers_for_trailing_paragraph(&normalized) {
        Some((at, closers)) => {
            let mut out = String::with_capacity(normalized.len() + closers.len());
            out.push_str(&normalized[..at]);
            out.push_str(&closers);
            out.push_str(&normalized[at..]);
            Cow::Owned(out)
        }
        None => normalized,
    }
}

/// An opening fence found on a line: its character and length.
fn opening_fence(line: &str) -> Option<(char, usize)> {
    let trimmed = line.trim_start();
    let fence = trimmed.chars().next().filter(|c| matches!(c, '`' | '~'))?;
    let run = trimmed.chars().take_while(|c| *c == fence).count();
    if run < 3 {
        return None;
    }
    // A backtick fence's info string cannot contain a backtick.
    if fence == '`' && trimmed[run..].contains('`') {
        return None;
    }
    Some((fence, run))
}

fn closes_fence(line: &str, fence: char, length: usize) -> bool {
    let trimmed = line.trim();
    trimmed.chars().count() >= length && trimmed.chars().all(|c| c == fence)
}

/// A line that starts a new block (heading, quote, list item): the delimiter
/// scan restarts there, because formatting never spans two blocks.
fn starts_block(line: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') || trimmed.starts_with('>') {
        return true;
    }
    let mut chars = trimmed.chars();
    match chars.next() {
        Some('-' | '*' | '+') => chars.next().is_some_and(char::is_whitespace),
        Some(digit) if digit.is_ascii_digit() => {
            let rest = trimmed.trim_start_matches(|c: char| c.is_ascii_digit());
            let mut rest = rest.chars();
            matches!(rest.next(), Some('.' | ')')) && rest.next().is_some_and(char::is_whitespace)
        }
        _ => false,
    }
}

/// Byte offset where the trailing paragraph starts, or `None` when the text
/// ends inside an open fence (code stays literal).
fn trailing_paragraph_start(text: &str) -> Option<usize> {
    let mut fence: Option<(char, usize)> = None;
    let mut start = 0usize;
    let mut offset = 0usize;
    for line in text.split_inclusive('\n') {
        let content = line.trim_end_matches('\n');
        let next = offset + line.len();
        match fence {
            Some((ch, len)) => {
                if closes_fence(content, ch, len) {
                    fence = None;
                    start = next;
                }
            }
            None => {
                if let Some(open) = opening_fence(content) {
                    fence = Some(open);
                    start = next;
                } else if content.trim().is_empty() {
                    start = next;
                } else if starts_block(content) {
                    start = offset;
                }
            }
        }
        offset = next;
    }
    fence.is_none().then_some(start)
}

/// One unmatched opening delimiter run.
struct Open {
    ch: char,
    count: usize,
}

fn is_punct(c: char) -> bool {
    !c.is_alphanumeric() && !c.is_whitespace() && !c.is_control()
}

/// `(insert byte offset, closers)` for the trailing paragraph, if any.
fn closers_for_trailing_paragraph(text: &str) -> Option<(usize, String)> {
    let start = trailing_paragraph_start(text)?;
    let chunk = &text[start..];
    if !chunk.contains(['*', '_', '~', '`']) {
        return None;
    }
    let chars: Vec<char> = chunk.chars().collect();
    let mut stack: Vec<Open> = Vec::new();
    let mut code: Option<usize> = None;
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        if c == '`' {
            let run = chars[i..].iter().take_while(|&&x| x == '`').count();
            let next = chars.get(i + run).copied();
            match code {
                Some(open) if open == run => code = None,
                Some(_) => {}
                None if next.is_some_and(|n| !n.is_whitespace()) => code = Some(run),
                None => {}
            }
            i += run;
            continue;
        }
        if code.is_some() {
            i += 1;
            continue;
        }
        match c {
            '\\' => {
                // A backslash escapes the next ASCII punctuation character.
                i += if chars.get(i + 1).is_some_and(char::is_ascii_punctuation) {
                    2
                } else {
                    1
                };
            }
            '*' | '_' | '~' => {
                let run = chars[i..].iter().take_while(|&&x| x == c).count();
                let prev = if i == 0 { ' ' } else { chars[i - 1] };
                let next = chars.get(i + run).copied().unwrap_or(' ');
                let left = !next.is_whitespace()
                    && (!is_punct(next) || prev.is_whitespace() || is_punct(prev));
                let right = !prev.is_whitespace()
                    && (!is_punct(prev) || next.is_whitespace() || is_punct(next));
                let (can_open, can_close) = match c {
                    '_' => (
                        left && (!right || is_punct(prev)),
                        right && (!left || is_punct(next)),
                    ),
                    _ => (left, right),
                };
                // Strikethrough needs exactly a `~~` pair.
                let delimiter = c != '~' || run == 2;
                if delimiter {
                    apply_run(&mut stack, c, run, can_open, can_close);
                }
                i += run;
            }
            _ => i += 1,
        }
    }
    if code.is_none() && stack.is_empty() {
        return None;
    }
    let mut closers = String::new();
    if let Some(run) = code {
        closers.extend(std::iter::repeat_n('`', run));
    }
    for open in stack.iter().rev() {
        closers.extend(std::iter::repeat_n(open.ch, open.count));
    }
    // Closers must follow the last visible character to close anything.
    Some((start + chunk.trim_end().len(), closers))
}

fn apply_run(stack: &mut Vec<Open>, ch: char, run: usize, can_open: bool, can_close: bool) {
    let mut remaining = run;
    if can_close {
        while remaining > 0 {
            let Some(at) = stack.iter().rposition(|o| o.ch == ch) else {
                break;
            };
            // Openers above the match are abandoned (never closed).
            stack.truncate(at + 1);
            let taken = remaining.min(stack[at].count);
            stack[at].count -= taken;
            remaining -= taken;
            if stack[at].count == 0 {
                stack.pop();
            }
        }
    }
    if remaining > 0 && can_open {
        stack.push(Open {
            ch,
            count: remaining,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn streaming(input: &str) -> String {
        sanitize(input, RenderMode::Streaming).into_owned()
    }

    fn completed(input: &str) -> String {
        sanitize(input, RenderMode::Completed).into_owned()
    }

    #[test]
    fn test_crlf_normalized() {
        assert_eq!(completed("hello\r\nworld\r\n"), "hello\nworld\n");
        assert!(!completed("hello\r\nworld\r\n").contains('\r'));
    }

    #[test]
    fn test_bare_cr_normalized() {
        assert_eq!(completed("hello\rworld"), "hello\nworld");
    }

    #[test]
    fn test_even_fences_unchanged() {
        let input = "```rust\nfn main() {}\n```\n";
        assert_eq!(completed(input), input);
        assert_eq!(streaming(input), input);
    }

    #[test]
    fn test_odd_fence_is_not_repaired_it_runs_to_the_end() {
        // Baseline B10: the count-based append is gone (D4).
        let input = "```rust\nfn main() {}\n";
        assert_eq!(completed(input), input);
        assert_eq!(streaming(input), input);
    }

    #[test]
    fn test_inline_triple_backticks_do_not_fabricate_a_fence() {
        let input = "Type ``` to start a block";
        assert_eq!(completed(input), input);
        assert_eq!(streaming(input), input);
    }

    #[test]
    fn test_completed_text_never_gets_closers() {
        for input in ["Here is **bold te", "a *b `c", "~~x", "snake_case"] {
            assert_eq!(completed(input), input);
        }
    }

    #[test]
    fn test_streaming_closes_open_runs_in_reverse_order() {
        assert_eq!(streaming("Here is **bold te"), "Here is **bold te**");
        assert_eq!(streaming("a *b"), "a *b*");
        assert_eq!(streaming("a __b"), "a __b__");
        assert_eq!(streaming("a ~~b"), "a ~~b~~");
        assert_eq!(streaming("a `b"), "a `b`");
        assert_eq!(streaming("a ``b"), "a ``b``");
        assert_eq!(streaming("a **b *c `d"), "a **b *c `d`***");
        assert_eq!(streaming("***x"), "***x***");
    }

    #[test]
    fn test_streaming_inserts_closers_before_trailing_whitespace() {
        assert_eq!(streaming("a **b \n"), "a **b** \n");
    }

    #[test]
    fn test_streaming_leaves_literal_markers_alone() {
        for input in [
            "2 * 3",
            "\\*x",
            "snake_case_name",
            "a ** b",
            "SELECT * FROM t",
            "tilde ~ alone",
            "- item",
            "* item",
            "trailing **",
            "`closed` and **closed**",
        ] {
            assert_eq!(streaming(input), input, "{input}");
        }
    }

    #[test]
    fn test_streaming_ignores_markers_inside_code_spans_and_fences() {
        assert_eq!(streaming("`a **b`"), "`a **b`");
        let input = "```\nlet x = **y;";
        assert_eq!(streaming(input), input);
        let tilde = "~~~\n**y";
        assert_eq!(streaming(tilde), tilde);
        // After the fence closes the next paragraph is a normal one again.
        assert_eq!(streaming("```\nx\n```\n**y"), "```\nx\n```\n**y**");
    }

    #[test]
    fn test_streaming_only_touches_the_last_paragraph() {
        assert_eq!(streaming("a **b\n\nc **d"), "a **b\n\nc **d**");
        assert_eq!(streaming("a **b\n\nc"), "a **b\n\nc");
    }

    #[test]
    fn test_streaming_list_items_do_not_share_openers() {
        assert_eq!(streaming("- a **b\n- c"), "- a **b\n- c");
        assert_eq!(streaming("- a **b\n- c **d"), "- a **b\n- c **d**");
    }

    #[test]
    fn test_source_characters_are_never_removed() {
        for input in ["a **b *c `d\n\ne _f ~~g", "```\n**\n", "x\r\ny **z"] {
            let out = streaming(input);
            let source = completed(input);
            let mut it = out.chars();
            for c in source.chars() {
                assert!(it.any(|o| o == c), "{input:?} lost {c:?}");
            }
        }
    }
}
