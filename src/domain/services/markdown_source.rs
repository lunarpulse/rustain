//! Producer-side markdown safety (Story 21.12, D14).
//!
//! Domain code that interpolates text into markdown it later shows to the user
//! (the plan summary, `/export`) must neither let that text change the
//! document's structure nor let an unclosed block swallow what follows. Pure
//! functions, no I/O. `pulldown_cmark` is used only to locate source ranges (it
//! is a pure parser; no rendering type crosses this module).

use std::ops::Range;

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag};
use unicode_segmentation::UnicodeSegmentation;

/// `text` made safe to show literally inside one table cell or one line:
/// line breaks become one space each (a CRLF pair counts once) and every
/// character CommonMark could read as syntax is backslash-escaped.
#[must_use]
pub fn escape_inline_literal(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push(' ');
            }
            '\n' => out.push(' '),
            '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '|' | '~' | '&' => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out
}

/// The longest prefix of `text` that is at most `max_bytes` long and ends on a
/// grapheme cluster boundary (never splits a ZWJ sequence or a combining mark).
#[must_use]
pub fn truncate_at_grapheme(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = 0usize;
    for (at, grapheme) in text.grapheme_indices(true) {
        let next = at + grapheme.len();
        if next > max_bytes {
            break;
        }
        end = next;
    }
    &text[..end]
}

/// A backtick fence that `text` cannot close early: one longer than the longest
/// backtick run in it, at least three.
#[must_use]
pub fn fence_for(text: &str) -> String {
    let mut longest = 0usize;
    let mut run = 0usize;
    for byte in text.bytes() {
        if byte == b'`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    "`".repeat((longest + 1).max(3))
}

/// The line that closes the fenced code block or the HTML block (types 1–5,
/// the ones that run until an explicit end marker) that `text` ends inside, or
/// `None` when it ends outside any such block. The closer carries the same
/// quote and list prefix as the block's opening line, so it stays inside the
/// container that holds the block.
#[must_use]
pub fn closer_for_open_block(text: &str) -> Option<String> {
    let end = text.trim_end().len();
    if end == 0 {
        return None;
    }
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS | Options::ENABLE_STRIKETHROUGH;
    let mut last: Option<(bool, Range<usize>)> = None;
    for (event, range) in Parser::new_ext(text, options).into_offset_iter() {
        match event {
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(_))) => last = Some((true, range)),
            Event::Start(Tag::HtmlBlock) => last = Some((false, range)),
            _ => {}
        }
    }
    let (fenced, range) = last?;
    if range.end < end {
        return None;
    }
    let line_start = text[..range.start].rfind('\n').map_or(0, |i| i + 1);
    let line = text[line_start..].split('\n').next().unwrap_or("");
    if fenced {
        open_fence_closer(text, line_start, line, range.end.min(text.len()))
    } else {
        open_html_closer(text, line_start, line, range.end.min(text.len()))
    }
}

/// The container prefix of `line[..at]` re-used on a continuation line: quote
/// markers stay, everything else (list markers, indentation) becomes spaces.
fn continuation_prefix(prefix: &str) -> String {
    prefix
        .chars()
        .map(|c| if c == '>' { '>' } else { ' ' })
        .collect()
}

fn open_fence_closer(text: &str, line_start: usize, line: &str, end: usize) -> Option<String> {
    let at = line.find(['`', '~'])?;
    let fence = line[at..].chars().next()?;
    let length = line[at..].chars().take_while(|&c| c == fence).count();
    if length < 3 {
        return None;
    }
    let block = text[line_start..end].trim_end();
    // A block that is only its opening line is open; otherwise the last line
    // closes it when it is a bare run of at least the opening length.
    if let Some((_, last)) = block.rsplit_once('\n') {
        let bare = last.trim_start_matches(['>', ' ', '\t']).trim_end();
        if bare.len() >= length && bare.chars().all(|c| c == fence) {
            return None;
        }
    }
    Some(format!(
        "{}{}",
        continuation_prefix(&line[..at]),
        fence.to_string().repeat(length)
    ))
}

/// `(start tag, end marker, the marker's search offset)` of an HTML block that
/// needs an explicit end, from the text starting at its `<`.
fn html_end_marker(rest: &str) -> Option<(String, usize)> {
    let lower: String = rest
        .chars()
        .take(12)
        .collect::<String>()
        .to_ascii_lowercase();
    if lower.starts_with("<!--") {
        return Some(("-->".to_owned(), 4));
    }
    if lower.starts_with("<?") {
        return Some(("?>".to_owned(), 2));
    }
    if lower.starts_with("<![cdata[") {
        return Some(("]]>".to_owned(), 9));
    }
    if lower.starts_with("<!") && lower[2..].starts_with(|c: char| c.is_ascii_alphabetic()) {
        return Some((">".to_owned(), 2));
    }
    for name in ["script", "pre", "style", "textarea"] {
        let tag = format!("<{name}");
        if let Some(after) = lower.strip_prefix(&tag)
            && after
                .chars()
                .next()
                .is_none_or(|c| c.is_whitespace() || c == '>')
        {
            return Some((format!("</{name}>"), tag.len()));
        }
    }
    None
}

fn open_html_closer(text: &str, line_start: usize, line: &str, end: usize) -> Option<String> {
    let at = line.find('<')?;
    let (closer, skip) = html_end_marker(&line[at..])?;
    let block = &text[line_start + at..end];
    let ends = |haystack: &str| match closer.as_str() {
        // Any of the four raw-text end tags closes a type 1 block.
        tag if tag.starts_with("</") => ["</script>", "</pre>", "</style>", "</textarea>"]
            .iter()
            .any(|end_tag| haystack.to_ascii_lowercase().contains(end_tag)),
        marker => haystack.contains(marker),
    };
    if ends(&block[skip.min(block.len())..]) {
        return None;
    }
    Some(format!("{}{closer}", continuation_prefix(&line[..at])))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_turns_syntax_into_literal_text_on_one_line() {
        assert_eq!(escape_inline_literal("a | b"), "a \\| b");
        assert_eq!(escape_inline_literal("x\\|y"), "x\\\\\\|y");
        assert_eq!(
            escape_inline_literal("`t` *a* _b_ [l] <T> ~s~ &amp;"),
            "\\`t\\` \\*a\\* \\_b\\_ \\[l\\] \\<T\\> \\~s\\~ \\&amp;"
        );
        assert_eq!(
            escape_inline_literal("one\ntwo\r\nthree\rfour"),
            "one two three four"
        );
        assert_eq!(escape_inline_literal("plain 界 text"), "plain 界 text");
    }

    #[test]
    fn escaped_text_parses_back_to_itself_inside_a_table_cell() {
        let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
        for title in [
            "a | b",
            "x\\|y",
            "`t`",
            "*a*",
            "[l](u)",
            "<T>",
            "~~s~~",
            "&amp; _u_",
        ] {
            let source = format!("| h |\n|---|\n| {} |\n", escape_inline_literal(title));
            let mut cell = String::new();
            let mut body = false;
            for event in Parser::new_ext(&source, options) {
                match event {
                    Event::End(pulldown_cmark::TagEnd::TableHead) => body = true,
                    Event::Text(t) | Event::Code(t) | Event::InlineHtml(t) if body => {
                        cell.push_str(&t);
                    }
                    _ => {}
                }
            }
            assert_eq!(cell, title, "{title:?}");
        }
    }

    #[test]
    fn truncation_never_splits_a_grapheme_cluster() {
        let family = "👨\u{200d}👩\u{200d}👧";
        let text = format!("{}{family}tail", "a".repeat(490));
        assert_eq!(truncate_at_grapheme(&text, 500), "a".repeat(490));
        assert_eq!(
            truncate_at_grapheme(&text, 508),
            format!("{}{family}", "a".repeat(490))
        );
        assert_eq!(truncate_at_grapheme("abc", 3), "abc");
        assert_eq!(truncate_at_grapheme("abc", 2), "ab");
        assert_eq!(truncate_at_grapheme("e\u{301}x", 2), "");
        assert_eq!(truncate_at_grapheme("e\u{301}x", 3), "e\u{301}");
        assert_eq!(truncate_at_grapheme("anything", 0), "");
    }

    #[test]
    fn fence_is_longer_than_any_backtick_run() {
        assert_eq!(fence_for("plain"), "```");
        assert_eq!(fence_for("a ``` b"), "````");
        assert_eq!(fence_for("```\n````\n`"), "`````");
        assert_eq!(fence_for(""), "```");
    }

    #[test]
    fn closer_names_the_block_the_text_ends_inside() {
        let some = |text: &str| closer_for_open_block(text);
        assert_eq!(some("a\n```rust\nlet x;"), Some("```".to_owned()));
        assert_eq!(some("a\n```rust\n"), Some("```".to_owned()));
        assert_eq!(some("~~~\nx"), Some("~~~".to_owned()));
        assert_eq!(some("````md\n```\ninner\n```"), Some("````".to_owned()));
        assert_eq!(some("x\n<!-- note"), Some("-->".to_owned()));
        assert_eq!(some("<script>\nvar a;"), Some("</script>".to_owned()));
        assert_eq!(some("<?php echo"), Some("?>".to_owned()));
        assert_eq!(some("<![CDATA[ raw"), Some("]]>".to_owned()));
        assert_eq!(some("<!DOCTYPE html"), Some(">".to_owned()));
    }

    #[test]
    fn closer_keeps_the_container_of_the_open_block() {
        let nested = closer_for_open_block;
        assert_eq!(nested("- ```rust\n  let x;"), Some("  ```".to_owned()));
        assert_eq!(nested("> ```\n> code"), Some("> ```".to_owned()));
        assert_eq!(
            nested("1. - <!-- c\n     more"),
            Some("     -->".to_owned())
        );
    }

    #[test]
    fn closed_or_absent_blocks_need_no_closer() {
        for text in [
            "",
            "plain",
            "```\nx\n```",
            "```\nx\n```\n",
            "````\nx\n```\n````",
            "<!-- done -->",
            "<!-- done -->\ntext",
            "<pre>x</pre>",
            "<div>\nstill type six",
            "```\nx\n```\n\nafter",
            "- ```\n  x\ndedent",
            "<script>x</script>\n",
        ] {
            assert_eq!(closer_for_open_block(text), None, "{text:?}");
        }
    }

    #[test]
    fn the_closer_really_ends_the_block() {
        for text in [
            "a\n```rust\nlet x;",
            "<!-- c",
            "- ```\n  x",
            "> ~~~\n> y",
            "<style>a{}",
        ] {
            let closer = closer_for_open_block(text).unwrap();
            let closed = format!("{text}\n{closer}\n");
            assert_eq!(closer_for_open_block(&closed), None, "{text:?}");
            // And what follows is ordinary prose again.
            let after = format!("{closed}\nafter");
            assert!(
                Parser::new(&after).any(|e| matches!(e, Event::Text(t) if &*t == "after")),
                "{text:?}"
            );
        }
    }
}
