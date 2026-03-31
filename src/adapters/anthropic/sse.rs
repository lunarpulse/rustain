//! Lightweight SSE line buffer (~40 lines of core logic).
//! Buffers raw bytes from an HTTP response and emits complete SSE frames.

/// A complete SSE frame parsed from the byte stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseFrame {
    pub event: String,
    pub data: String,
}

/// Buffers raw bytes and emits complete SSE frames.
///
/// Handles partial line buffering across chunk boundaries, multiple `data:` lines
/// per event (concatenated with `\n`), `event:` lines, comment lines (`:` prefix),
/// and `id:` lines (ignored).
pub struct SseLineBuffer {
    line_buf: String,
    current_event: String,
    current_data: Vec<String>,
}

impl Default for SseLineBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl SseLineBuffer {
    pub fn new() -> Self {
        Self {
            line_buf: String::new(),
            current_event: String::new(),
            current_data: Vec::new(),
        }
    }

    /// Feed raw bytes into the buffer. Returns any complete SSE frames.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<SseFrame> {
        let mut frames = Vec::new();
        let text = String::from_utf8_lossy(bytes);

        for ch in text.chars() {
            if ch == '\n' {
                let line = std::mem::take(&mut self.line_buf);
                self.process_line(&line, &mut frames);
            } else if ch == '\r' {
                // Skip \r; we handle \n as the delimiter
            } else {
                self.line_buf.push(ch);
            }
        }

        frames
    }

    fn process_line(&mut self, line: &str, frames: &mut Vec<SseFrame>) {
        if line.is_empty() {
            // Blank line = end of event
            if !self.current_data.is_empty() {
                frames.push(SseFrame {
                    event: if self.current_event.is_empty() {
                        "message".to_string()
                    } else {
                        std::mem::take(&mut self.current_event)
                    },
                    data: self.current_data.join("\n"),
                });
                self.current_data.clear();
            }
            // Always clear event name on blank line, even if no data was accumulated.
            // Prevents event name leaking into the next event.
            self.current_event.clear();
        } else if let Some(value) = line.strip_prefix("event:") {
            self.current_event = value.trim_start().to_string();
        } else if let Some(value) = line.strip_prefix("data:") {
            self.current_data.push(value.trim_start().to_string());
        } else if line.starts_with("id:") || line.starts_with(':') {
            // id: lines and comments are ignored per SSE spec
        }
        // Unknown field names are also ignored per spec
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sse_buffer_complete_event() {
        let mut buf = SseLineBuffer::new();
        let frames = buf.feed(b"event: message_start\ndata: {\"type\":\"message_start\"}\n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event, "message_start");
        assert_eq!(frames[0].data, "{\"type\":\"message_start\"}");
    }

    #[test]
    fn test_sse_buffer_partial_lines() {
        let mut buf = SseLineBuffer::new();
        let f1 = buf.feed(b"event: content_block");
        assert!(f1.is_empty());
        let f2 = buf.feed(b"_delta\ndata: {\"ty");
        assert!(f2.is_empty());
        let f3 = buf.feed(b"pe\":\"text\"}\n\n");
        assert_eq!(f3.len(), 1);
        assert_eq!(f3[0].event, "content_block_delta");
        assert_eq!(f3[0].data, "{\"type\":\"text\"}");
    }

    #[test]
    fn test_sse_buffer_multi_data_lines() {
        let mut buf = SseLineBuffer::new();
        let frames = buf.feed(b"event: test\ndata: line1\ndata: line2\ndata: line3\n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "line1\nline2\nline3");
    }

    #[test]
    fn test_sse_buffer_comment_lines_ignored() {
        let mut buf = SseLineBuffer::new();
        let frames = buf.feed(b": this is a comment\nevent: ping\ndata: {}\n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event, "ping");
    }

    #[test]
    fn test_sse_buffer_id_lines_ignored() {
        let mut buf = SseLineBuffer::new();
        let frames = buf.feed(b"id: 123\nevent: test\ndata: hello\n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event, "test");
        assert_eq!(frames[0].data, "hello");
    }

    #[test]
    fn test_sse_buffer_multiple_events() {
        let mut buf = SseLineBuffer::new();
        let frames = buf.feed(
            b"event: ping\ndata: {}\n\nevent: content_block_delta\ndata: {\"text\":\"hi\"}\n\n",
        );
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].event, "ping");
        assert_eq!(frames[1].event, "content_block_delta");
    }

    #[test]
    fn test_sse_buffer_crlf_handling() {
        let mut buf = SseLineBuffer::new();
        let frames = buf.feed(b"event: test\r\ndata: hello\r\n\r\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "hello");
    }

    #[test]
    fn test_sse_buffer_empty_data() {
        let mut buf = SseLineBuffer::new();
        let frames = buf.feed(b"event: test\ndata: \n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "");
    }

    #[test]
    fn test_sse_buffer_no_event_field_defaults_to_message() {
        let mut buf = SseLineBuffer::new();
        let frames = buf.feed(b"data: hello\n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event, "message");
    }

    #[test]
    fn test_sse_buffer_blank_line_without_data_no_frame() {
        let mut buf = SseLineBuffer::new();
        let frames = buf.feed(b"\n\n\n");
        assert!(frames.is_empty());
    }

    #[test]
    fn test_sse_buffer_event_name_cleared_on_empty_data() {
        // Regression: event name from a data-less event must not leak into the next event.
        // Sequence: event:ping, blank line (no data), then data:hello, blank line.
        let mut buf = SseLineBuffer::new();
        let frames = buf.feed(b"event: ping\n\ndata: hello\n\n");
        // First blank line has no data -> no frame emitted, but event name must be cleared.
        // Second event has data but no event: line -> should default to "message", not "ping".
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event, "message");
        assert_eq!(frames[0].data, "hello");
    }

    #[test]
    fn test_sse_buffer_malformed_utf8() {
        let mut buf = SseLineBuffer::new();
        // Invalid UTF-8 bytes get replaced with U+FFFD
        let frames = buf.feed(&[
            b'e', b'v', b'e', b'n', b't', b':', b' ', b't', b'e', b's', b't', b'\n', b'd', b'a',
            b't', b'a', b':', b' ', 0xFF, 0xFE, b'\n', b'\n',
        ]);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event, "test");
        // Lossy conversion replaces invalid bytes
        assert!(frames[0].data.contains('\u{FFFD}'));
    }
}
