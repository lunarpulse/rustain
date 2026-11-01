//! Stage 1: sanitize raw markdown input.
//!
//! * Normalises `\r\n` and bare `\r` to `\n`.
//! * In every parsed mode, extends a table's header and delimiter rows with
//!   empty cells when a body row has more cells than the header, so GFM's
//!   "excess cells are ignored" never drops a cell (AC3).
//! * In [`RenderMode::Streaming`] only, appends display-only closers for the
//!   formatting still open in the **trailing paragraph** (D3) (inside a table:
//!   its last cell) so a half-typed `**bold te` paints bold instead of
//!   flickering as literal asterisks.
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
    if mode == RenderMode::Literal {
        return normalized;
    }
    let repaired = match repair_tables(&normalized) {
        Some(text) => Cow::Owned(text),
        None => normalized,
    };
    if mode != RenderMode::Streaming {
        return repaired;
    }
    match closers_for_trailing_paragraph(&repaired) {
        Some((at, closers)) => {
            let mut out = String::with_capacity(repaired.len() + closers.len());
            out.push_str(&repaired[..at]);
            out.push_str(&closers);
            out.push_str(&repaired[at..]);
            Cow::Owned(out)
        }
        None => repaired,
    }
}

/// The cells of a table row: split at unescaped pipes, one edge pipe dropped.
fn row_cells(row: &str) -> Vec<&str> {
    let row = row.trim();
    let row = row.strip_prefix('|').unwrap_or(row);
    let bytes = row.as_bytes();
    let mut cells = Vec::new();
    let (mut start, mut i) = (0usize, 0usize);
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'|' => {
                cells.push(&row[start..i]);
                start = i + 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    if start < row.len() {
        cells.push(&row[start..]);
    }
    cells
}

/// The cell count of a delimiter row (`|:--|---:|`), or `None` for any other line.
fn delimiter_cells(line: &str) -> Option<usize> {
    if !line.contains('|') {
        return None;
    }
    let cells = row_cells(line);
    let valid = |cell: &str| {
        let cell = cell.trim().trim_start_matches(':').trim_end_matches(':');
        !cell.is_empty() && cell.bytes().all(|b| b == b'-')
    };
    (!cells.is_empty() && cells.iter().all(|c| valid(c))).then_some(cells.len())
}

/// A line that ends the table body: blank, or the start of another block.
fn ends_table(line: &str) -> bool {
    line.trim().is_empty() || starts_block(line) || opening_fence(line).is_some()
}

/// `line` (without its newline) with `extra` empty cells appended, each as
/// `filler` followed by a pipe; a missing closing pipe is added first.
fn with_cells(line: &str, extra: usize, filler: &str, out: &mut String) {
    let line = line.trim_end();
    out.push_str(line);
    let trailing_backslashes = line
        .bytes()
        .rev()
        .skip(1)
        .take_while(|&b| b == b'\\')
        .count();
    if !(line.ends_with('|') && trailing_backslashes % 2 == 0) {
        out.push_str(" |");
    }
    for _ in 0..extra {
        out.push_str(filler);
        out.push('|');
    }
}

/// Extend the header and delimiter rows of every top-level table whose body
/// has a row wider than the header; `None` when nothing needed repair.
fn repair_tables(text: &str) -> Option<String> {
    // No delimiter row, no table: prose pays one scan and no allocation.
    if !text
        .lines()
        .any(|line| line.contains('|') && line.contains('-') && delimiter_cells(line).is_some())
    {
        return None;
    }
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    // `out` holds the repaired text of `text[..copied]`; unchanged stretches
    // are copied only when a repair needs them.
    let mut out = String::new();
    let mut copied = 0usize;
    let mut offset = 0usize;
    let mut fence: Option<(char, usize)> = None;
    let mut i = 0usize;
    while i < lines.len() {
        let content = lines[i].trim_end_matches('\n');
        if let Some((ch, len)) = fence {
            if closes_fence(content, ch, len) {
                fence = None;
            }
        } else if let Some(open) = opening_fence(content) {
            fence = Some(open);
        } else if let Some(delimiter) = lines.get(i + 1).map(|l| l.trim_end_matches('\n'))
            && content.contains('|')
            && !content.starts_with("    ")
            && !content.starts_with('\t')
            && delimiter_cells(delimiter) == Some(row_cells(content).len())
            && (content.trim_start().starts_with('|') || i == 0 || lines[i - 1].trim().is_empty())
        {
            // pulldown lets only a header with a leading pipe interrupt a
            // paragraph; any other header needs a blank line (or the start)
            // before it, or no table exists and nothing is repaired.
            let columns = row_cells(content).len();
            let mut end = i + 2;
            let mut widest = columns;
            while end < lines.len() && !ends_table(lines[end].trim_end_matches('\n')) {
                widest = widest.max(row_cells(lines[end]).len());
                end += 1;
            }
            let head_len = lines[i].len() + lines[i + 1].len();
            if widest > columns {
                let extra = widest - columns;
                let newline = |line: &str| if line.ends_with('\n') { "\n" } else { "" };
                out.push_str(&text[copied..offset]);
                with_cells(content, extra, "   ", &mut out);
                out.push_str(newline(lines[i]));
                with_cells(delimiter, extra, " --- ", &mut out);
                out.push_str(newline(lines[i + 1]));
                copied = offset + head_len;
            }
            // A body row is never a header candidate: skip the whole table.
            offset += lines[i..end].iter().map(|l| l.len()).sum::<usize>();
            i = end;
            continue;
        }
        offset += lines[i].len();
        i += 1;
    }
    if copied == 0 {
        return None;
    }
    out.push_str(&text[copied..]);
    Some(out)
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

/// The last table cell of `chunk` when it holds a table whose last line is a
/// body row: `(byte offset, the cell's text)`. A chunk that is a table without
/// a body row yields an empty cell (nothing to close); `None` for any other text.
fn table_last_cell(chunk: &str) -> Option<(usize, &str)> {
    let body = chunk.trim_end();
    let mut offsets = Vec::new();
    let mut at = 0usize;
    for line in body.split('\n') {
        offsets.push((at, line));
        at += line.len() + 1;
    }
    let header = offsets.windows(2).position(|pair| {
        pair[0].1.contains('|') && delimiter_cells(pair[1].1) == Some(row_cells(pair[0].1).len())
    })?;
    if header + 2 >= offsets.len() {
        return Some((0, ""));
    }
    let (line_start, line) = *offsets.last()?;
    let trimmed = line.trim_end();
    let mut pipes = Vec::new();
    let bytes = trimmed.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'|' => {
                pipes.push(i);
                i += 1;
            }
            _ => i += 1,
        }
    }
    let closing = pipes.last().is_some_and(|&p| p + 1 == trimmed.len());
    let end = if closing {
        pipes.len() - 1
    } else {
        pipes.len()
    };
    let from = if end == 0 { 0 } else { pipes[end - 1] + 1 };
    let to = if closing {
        trimmed.len() - 1
    } else {
        trimmed.len()
    };
    Some((line_start + from, &trimmed[from..to.max(from)]))
}

/// `(insert byte offset, closers)` for the trailing paragraph, if any.
fn closers_for_trailing_paragraph(text: &str) -> Option<(usize, String)> {
    let start = trailing_paragraph_start(text)?;
    let chunk = &text[start..];
    if !chunk.contains(['*', '_', '~', '`']) {
        return None;
    }
    let (shift, scanned) = table_last_cell(chunk).unwrap_or((0, chunk));
    let (at, closers) = closers_in(scanned)?;
    Some((start + shift + at, closers))
}

/// `(offset after the last visible character, closers)` for one paragraph.
fn closers_in(chunk: &str) -> Option<(usize, String)> {
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
    Some((chunk.trim_end().len(), closers))
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

    #[test]
    fn test_a_wider_body_row_extends_the_header_and_delimiter_rows() {
        assert_eq!(
            completed("| a | b |\n|---|---|\n| 1 | 2 | 3 |"),
            "| a | b |   |\n|---|---| --- |\n| 1 | 2 | 3 |"
        );
        // Without edge pipes a closing pipe is added first; the widest row wins.
        assert_eq!(
            streaming("a | b\n--|--\n1|2\n1|2|3|4\n"),
            "a | b |   |   |\n--|-- | --- | --- |\n1|2\n1|2|3|4\n"
        );
    }

    #[test]
    fn test_tables_that_need_no_repair_are_untouched() {
        for input in [
            "| a | b |\n|---|---|\n| 1 | 2 |",
            "| a | b |\n|---|---|\n| 1 |",
            "| a | b |\n|---|---|\n| 1 \\| x | 2 |",
            "```\n| a |\n|---|\n| 1 | 2 |\n```",
            "    | a |\n    |---|\n    | 1 | 2 |",
            "| a |\n|---|\n| 1 |\n\n| x | y |",
        ] {
            assert_eq!(completed(input), input, "{input:?}");
        }
        // Literal text is never parsed, so never repaired.
        let wide = "| a |\n|---|\n| 1 | 2 |";
        assert_eq!(sanitize(wide, RenderMode::Literal), wide);
    }

    #[test]
    fn test_a_header_that_cannot_interrupt_a_paragraph_is_not_repaired() {
        // pulldown sees no table here (no leading pipe, no blank line before
        // it), so the sanitizer must not rewrite the visible paragraph.
        let no_table = "intro\na |b\n--|--\n1|2|3";
        assert_eq!(completed(no_table), no_table);
        // After a blank line the same text is a table and is repaired.
        assert_eq!(
            completed("intro\n\na |b\n--|--\n1|2|3"),
            "intro\n\na |b |   |\n--|-- | --- |\n1|2|3"
        );
        // A header with a leading pipe interrupts a paragraph.
        assert_eq!(
            completed("intro\n|a |\n|---|\n|1 |2 |"),
            "intro\n|a |   |\n|---| --- |\n|1 |2 |"
        );
    }

    #[test]
    fn test_delimiter_like_body_rows_are_never_new_headers() {
        // The first table is repaired once; the delimiter rows in its body
        // are body rows and are left alone.
        assert_eq!(
            completed("|a|\n|-|\n|1|2|\n|-|\n|-|"),
            "|a|   |\n|-| --- |\n|1|2|\n|-|\n|-|"
        );
    }

    #[test]
    fn test_streaming_closes_formatting_inside_the_last_table_cell_only() {
        let head = "| a | b |\n|---|---|\n";
        assert_eq!(
            streaming(&format!("{head}| *x | **bold te")),
            format!("{head}| *x | **bold te**")
        );
        assert_eq!(
            streaming(&format!("{head}| x | **bold te |")),
            format!("{head}| x | **bold te** |")
        );
        // A table without a body row has nothing to close.
        assert_eq!(streaming("| *a | b |\n|---|---|"), "| *a | b |\n|---|---|");
    }
}
