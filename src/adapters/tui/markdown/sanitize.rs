/// Stage 1: Sanitize raw markdown input.
///
/// - Normalizes `\r\n` line endings to `\n`
/// - Auto-closes unclosed fences (odd number of triple-backtick markers)
/// - Does NOT strip HTML tags here — that is done in Stage 3 (transform)
///   on non-code inline spans only, to preserve Rust generics / operators inside
///   code blocks.
pub fn sanitize(input: &str) -> String {
    // Normalize CRLF → LF
    let normalized = if input.contains('\r') {
        input.replace("\r\n", "\n").replace('\r', "\n")
    } else {
        input.to_owned()
    };

    // Count triple-backtick fence markers
    let fence_count = count_fences(&normalized);

    // Odd number of fences → append a closing fence
    if !fence_count.is_multiple_of(2) {
        let mut out = normalized;
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("```\n");
        out
    } else {
        normalized
    }
}

/// Count the number of triple-backtick fence markers in `text`.
fn count_fences(text: &str) -> usize {
    let mut count = 0;
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    while i + 2 < len {
        if bytes[i] == b'`' && bytes[i + 1] == b'`' && bytes[i + 2] == b'`' {
            count += 1;
            i += 3; // skip past the triple-backtick
        } else {
            i += 1;
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_crlf_normalized() {
        let input = "hello\r\nworld\r\n";
        let out = sanitize(input);
        assert_eq!(out, "hello\nworld\n");
        assert!(!out.contains('\r'));
    }

    #[test]
    fn test_bare_cr_normalized() {
        let input = "hello\rworld";
        let out = sanitize(input);
        assert_eq!(out, "hello\nworld");
    }

    #[test]
    fn test_even_fences_unchanged() {
        let input = "```rust\nfn main() {}\n```\n";
        let out = sanitize(input);
        assert_eq!(out, input);
    }

    #[test]
    fn test_odd_fence_auto_closed() {
        let input = "```rust\nfn main() {";
        let out = sanitize(input);
        assert!(out.ends_with("```\n"));
        // Should have an even number of fence markers now
        assert_eq!(count_fences(&out) % 2, 0);
    }

    #[test]
    fn test_unclosed_fence_with_trailing_newline() {
        let input = "```rust\nfn main() {\n";
        let out = sanitize(input);
        assert!(out.ends_with("```\n"));
        assert_eq!(count_fences(&out) % 2, 0);
    }

    #[test]
    fn test_no_markdown_plain_text() {
        let input = "Hello world";
        let out = sanitize(input);
        assert_eq!(out, "Hello world");
    }

    #[test]
    fn test_broken_bold_degrades_to_plain() {
        // Broken bold formatting — sanitize leaves it as-is (parse stage handles it)
        let input = "**unclosed bold";
        let out = sanitize(input);
        assert_eq!(out, "**unclosed bold");
    }

    #[test]
    fn test_empty_input() {
        let out = sanitize("");
        assert_eq!(out, "");
    }

    #[test]
    fn test_multiple_code_blocks_even() {
        let input = "```\nblock1\n```\n\n```\nblock2\n```\n";
        let out = sanitize(input);
        assert_eq!(out, input);
    }

    #[test]
    fn test_multiple_code_blocks_odd() {
        let input = "```\nblock1\n```\n\n```\nblock2\n";
        let out = sanitize(input);
        assert_eq!(count_fences(&out) % 2, 0);
    }
}
