use std::borrow::Cow;
use std::iter::Peekable;
use std::str::CharIndices;

/// Host-minted prefix for each rendered line of peer-authored transcript text.
///
/// The glyph is reserved: [`sanitize_peer_text_block`] removes it from peer
/// content before the renderer adds this prefix.
pub const PEER_CONTENT_GUTTER_WIDTH: usize = 3;
pub const PEER_CONTENT_GUTTER: &str = "┆  ";
pub const PEER_CONTENT_GUTTER_GLYPH: char = '┆';

/// Sanitize peer-authored text for a one-line render slot.
///
/// Every control character is removed. ANSI CSI/OSC sequences are consumed as
/// units so their payload cannot survive after only the introducer is removed.
#[must_use]
pub fn sanitize_peer_text_line(input: &str) -> Cow<'_, str> {
    sanitize_peer_text(input, false, false, false)
}

/// Sanitize peer-authored text for the transcript's block render slot.
///
/// Newlines are retained so lists and code blocks remain legible. Every other
/// control character and the reserved peer-content gutter glyph are removed.
#[must_use]
pub fn sanitize_peer_text_block(input: &str) -> Cow<'_, str> {
    sanitize_peer_text(input, true, true, false)
}

/// Sanitize tool-authored text before rendering an inspector body.
///
/// Retains tabs and newlines, normalizes CRLF and bare CR to one newline, and
/// removes terminal controls and bidi overrides/isolates. Unlike peer transcript
/// text, tool output may contain the peer-content gutter glyph literally.
#[must_use]
pub fn sanitize_tool_text_block(input: &str) -> Cow<'_, str> {
    sanitize_peer_text(input, true, false, true)
}

fn sanitize_peer_text(
    input: &str,
    preserve_newlines: bool,
    strip_gutter: bool,
    tool_text: bool,
) -> Cow<'_, str> {
    let mut chars = input.char_indices().peekable();
    let mut out: Option<String> = None;
    let mut segment_start = 0;

    while let Some((index, ch)) = chars.next() {
        let should_strip = ch == '\x1b'
            || (ch.is_control()
                && !(preserve_newlines && ch == '\n')
                && !(tool_text && ch == '\t'))
            || (strip_gutter && ch == PEER_CONTENT_GUTTER_GLYPH)
            || (tool_text && matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'));
        if !should_strip {
            continue;
        }

        let target = out.get_or_insert_with(|| String::with_capacity(input.len()));
        target.push_str(&input[segment_start..index]);
        segment_start = match ch {
            '\r' if tool_text => {
                target.push('\n');
                // CRLF is one logical boundary, not two.
                chars
                    .next_if(|&(_, ch)| ch == '\n')
                    .map_or(index + 1, |(index, _)| index + 1)
            }
            '\x1b' => skip_ansi_escape(&mut chars, index + ch.len_utf8(), tool_text),
            '\u{009b}' => skip_csi(&mut chars, index + ch.len_utf8(), tool_text),
            '\u{0090}' | '\u{0098}' | '\u{009d}' | '\u{009e}' | '\u{009f}' => {
                skip_control_string(&mut chars, index + ch.len_utf8(), tool_text)
            }
            _ => index + ch.len_utf8(),
        };
    }

    match out {
        Some(mut out) => {
            out.push_str(&input[segment_start..]);
            Cow::Owned(out)
        }
        None => Cow::Borrowed(input),
    }
}

fn skip_ansi_escape(
    chars: &mut Peekable<CharIndices<'_>>,
    mut end: usize,
    cr_is_newline: bool,
) -> usize {
    match chars.peek().copied() {
        // CSI: ESC [ <params> <0x40..=0x7E final>.
        Some((_, '[')) => {
            if let Some((index, ch)) = chars.next() {
                end = index + ch.len_utf8();
            }
            skip_csi(chars, end, cr_is_newline)
        }
        // OSC, DCS, SOS, PM and APC: consume the payload through BEL or ST.
        Some((_, ']' | 'P' | 'X' | '^' | '_')) => {
            if let Some((index, ch)) = chars.next() {
                end = index + ch.len_utf8();
            }
            skip_control_string(chars, end, cr_is_newline)
        }
        // A tool's logical row boundary must survive an incomplete ESC too.
        Some((_, '\n' | '\r')) if cr_is_newline => end,
        // Other Fe introducers: drop ESC plus the introducer byte.
        Some(_) => {
            if let Some((index, ch)) = chars.next() {
                end = index + ch.len_utf8();
            }
            end
        }
        None => end,
    }
}

fn skip_csi(chars: &mut Peekable<CharIndices<'_>>, mut end: usize, cr_is_newline: bool) -> usize {
    // Consume params/intermediates through the 0x40..=0x7E final byte. A
    // newline ends an unterminated sequence WITHOUT being consumed: the main
    // loop strips it in line mode and preserves it in block mode. Swallowing
    // it merged the peer's last line into the next rendered line (19.15
    // review, P6).
    while let Some((index, ch)) =
        chars.next_if(|&(_, ch)| ch != '\n' && !(cr_is_newline && ch == '\r'))
    {
        end = index + ch.len_utf8();
        if ('\x40'..='\x7e').contains(&ch) {
            break;
        }
    }
    end
}

fn skip_control_string(
    chars: &mut Peekable<CharIndices<'_>>,
    mut end: usize,
    cr_is_newline: bool,
) -> usize {
    let mut previous_escape = false;
    // Same newline boundary as skip_csi (P6): an unterminated control string
    // must not eat the newline or the text after it.
    while let Some((index, ch)) =
        chars.next_if(|&(_, ch)| ch != '\n' && !(cr_is_newline && ch == '\r'))
    {
        end = index + ch.len_utf8();
        if ch == '\x07' || ch == '\u{009c}' || (previous_escape && ch == '\\') {
            break;
        }
        previous_escape = ch == '\x1b';
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_line_slot_removes_controls_and_whole_ansi_sequences() {
        let input = concat!(
            "alpha\x1b[2Jbeta\r\ngamma\x1b]0;forged\x07delta",
            "\u{009b}31mred\u{009b}0m",
            "\x1bPdevice-control\x1b\\omega",
            "\u{0090}c1-device-control\u{009c}end"
        );
        assert_eq!(
            sanitize_peer_text_line(input),
            "alphabetagammadeltaredomegaend"
        );
    }

    #[test]
    fn block_slot_preserves_newlines_but_removes_controls_ansi_and_gutter() {
        let input = "one\n┆ [auto-sent]\r\n\x1b[31mtwo\x1b[0m";
        assert_eq!(sanitize_peer_text_block(input), "one\n [auto-sent]\ntwo");
    }

    #[test]
    fn benign_input_is_borrowed_without_allocation() {
        assert!(matches!(
            sanitize_peer_text_line("plain text"),
            Cow::Borrowed("plain text")
        ));
        assert!(matches!(
            sanitize_peer_text_block("line one\nline two"),
            Cow::Borrowed("line one\nline two")
        ));
    }

    #[test]
    fn unterminated_csi_stops_at_newline_without_consuming_it() {
        // P6: the skipper used to eat the newline plus the first char in
        // 0x40..=0x7E after it, merging lines in the block slot.
        assert_eq!(sanitize_peer_text_line("alpha\x1b[31\nbeta"), "alphabeta");
        assert_eq!(
            sanitize_peer_text_block("alpha\x1b[31\nbeta"),
            "alpha\nbeta"
        );
        // The C1 introducer reaches the same helper.
        assert_eq!(sanitize_peer_text_block("\u{009b}31\nbeta"), "\nbeta");
    }

    #[test]
    fn unterminated_csi_to_end_of_input_drops_without_eating_trailing_newline() {
        assert_eq!(sanitize_peer_text_line("alpha\x1b[31"), "alpha");
        assert_eq!(sanitize_peer_text_block("one\ntwo\x1b[31\n"), "one\ntwo\n");
    }

    #[test]
    fn unterminated_control_string_stops_at_newline_without_consuming_it() {
        // Same P6 boundary: the forged payload ends at the newline; the main
        // loop strips (line) or keeps (block) it.
        assert_eq!(
            sanitize_peer_text_line("alpha\x1b]0;forged\nbeta"),
            "alphabeta"
        );
        assert_eq!(
            sanitize_peer_text_block("alpha\x1b]0;forged\nbeta"),
            "alpha\nbeta"
        );
    }

    #[test]
    fn story21_5_tool_text_preserves_tabs_and_newlines() {
        assert_eq!(
            sanitize_tool_text_block("one\ttwo\n\tthree\n"),
            "one\ttwo\n\tthree\n"
        );
    }

    #[test]
    fn story21_5_tool_text_strips_bidi_overrides_and_isolates() {
        for ch in ('\u{202a}'..='\u{202e}').chain('\u{2066}'..='\u{2069}') {
            let input = format!("before{ch}after");
            assert_eq!(sanitize_tool_text_block(&input), "beforeafter", "{ch:?}");
        }
    }

    #[test]
    fn story21_5_tool_text_normalizes_crlf_without_merging_rows() {
        assert_eq!(
            sanitize_tool_text_block("one\r\n\r\ntwo\r\n"),
            "one\n\ntwo\n"
        );
        assert_eq!(sanitize_tool_text_block("one\rtwo\r"), "one\ntwo\n");
        assert_eq!(
            sanitize_tool_text_block("one\x1b[31\r\ntwo\x1b]title\rthree\x1b\nfour"),
            "one\ntwo\nthree\nfour"
        );
    }

    #[test]
    fn story21_5_tool_text_consumes_escape_csi_osc_and_c1_sequences() {
        let input = concat!(
            "a\x1b[31mred\x1b[0mb",
            "\x1b]0;title\x07c",
            "\x1b]52;c;clipboard\x1b\\d",
            "\x1bPdevice\x1b\\e",
            "\u{009b}31mf\u{009b}0m",
            "\u{009d}title\u{009c}g",
            "\u{0090}device\u{009c}h",
            "\x1b[31\nnext\x1b]unterminated\nlast"
        );
        assert_eq!(sanitize_tool_text_block(input), "aredbcdefgh\nnext\nlast");
    }

    #[test]
    fn story21_5_tool_text_strips_other_c0_and_c1_controls() {
        for ch in ('\0'..='\u{009f}')
            .filter(|ch| ch.is_control() && !matches!(ch, '\t' | '\n' | '\r' | '\x1b'))
        {
            // A newline bounds unterminated C1 control-string introducers.
            let input = format!("before{ch}\nafter");
            assert_eq!(sanitize_tool_text_block(&input), "before\nafter", "{ch:?}");
        }
    }

    #[test]
    fn story21_5_tool_text_is_idempotent() {
        let first =
            sanitize_tool_text_block("\x1b[31mred\x1b[0m\t中👩‍💻e\u{0301}\u{202e}\x07\r\nnext");
        assert_eq!(sanitize_tool_text_block(first.as_ref()), first);
    }

    #[test]
    fn story21_5_tool_text_borrows_clean_unicode_without_allocation() {
        let input = "plain\t┆ 中👩‍💻e\u{0301}\nsecond line";
        assert!(matches!(
            sanitize_tool_text_block(input),
            Cow::Borrowed(value) if std::ptr::eq(value.as_ptr(), input.as_ptr())
        ));
    }

    #[test]
    fn story21_5_tool_policy_keeps_existing_peer_semantics() {
        let input = "a\t┆\u{202e}\r\nb";
        assert_eq!(sanitize_peer_text_line(input), "a┆\u{202e}b");
        assert_eq!(sanitize_peer_text_block(input), "a\u{202e}\nb");
        assert_eq!(sanitize_tool_text_block(input), "a\t┆\nb");
    }
}
