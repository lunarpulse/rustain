//! OSC 8 hyperlinks over already-drawn text.
//!
//! ratatui 0.30.0 sizes a cell by the display width of its whole symbol, so an
//! escape-wrapped symbol (whose URL counts as printable width) makes the buffer
//! diff skip the cells after it. The link is therefore written after the frame,
//! over the text the frame already drew: same cells, same characters, plus the
//! OSC 8 open/close sequences (terminator `BEL`). The cursor is saved and
//! restored around it, and the URL is always a re-serialized `http(s)` URL
//! (`login_ui::safe_url`), so it carries no control byte.

use crossterm::{cursor, queue, style::Print};
use std::io::Write;

/// A link drawn over `text` at (`x`, `y`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkSpan {
    pub x: u16,
    pub y: u16,
    pub text: String,
    pub url: String,
}

/// Writes every link over its drawn text and flushes.
pub fn write_links<W: Write>(out: &mut W, links: &[LinkSpan]) -> std::io::Result<()> {
    if links.is_empty() {
        return Ok(());
    }
    queue!(out, cursor::SavePosition)?;
    for link in links {
        queue!(
            out,
            cursor::MoveTo(link.x, link.y),
            Print(format!("\x1b]8;;{}\x07{}\x1b]8;;\x07", link.url, link.text))
        )?;
    }
    queue!(out, cursor::RestorePosition)?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_bytes_wrap_the_visible_text_with_osc8_bel() {
        let mut out = Vec::new();
        write_links(
            &mut out,
            &[LinkSpan {
                x: 3,
                y: 4,
                text: "https://a.test/…/z".into(),
                url: "https://a.test/long/path/z".into(),
            }],
        )
        .unwrap();
        let bytes = String::from_utf8(out).unwrap();
        assert!(
            bytes.contains("\x1b]8;;https://a.test/long/path/z\x07https://a.test/…/z\x1b]8;;\x07"),
            "{bytes:?}"
        );
        let mut empty = Vec::new();
        write_links(&mut empty, &[]).unwrap();
        assert!(empty.is_empty());
    }
}
