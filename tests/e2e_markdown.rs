//! E2E tests for Story 3.6: Basic Markdown Rendering
//!
//! Validates the 5-stage markdown pipeline end-to-end through chat_pane rendering.
//! Uses TestHarness to simulate streaming responses and verify visual output.

mod e2e_harness;

use e2e_harness::TestHarness;
use ratatui::style::Modifier;

use rustain::domain::models::{StopReason, StreamChunk};

// ═══════════════════════════════════════════════════════════════════════════
// AC1: Inline Formatting (headings, bold, italic, code spans)
// ═══════════════════════════════════════════════════════════════════════════

/// AC1: Headings render with bold formatting.
/// Verifies that # Heading appears in the chat pane with heading text.
#[test]
fn test_e2e_markdown_heading_renders() {
    let mut h = TestHarness::new();
    h.complete_turn(
        "Show me a heading",
        vec![
            StreamChunk::Text {
                content: "# My Heading\n\nSome paragraph text.".to_string(),
                parent_tool_use_id: None,
            },
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
        ],
    );
    h.render();

    h.assert_screen_contains("My Heading", "Heading text should be visible");
    h.assert_screen_contains("Some paragraph", "Paragraph text should follow heading");
}

/// AC1: Bold text renders with BOLD modifier.
/// Verifies that **bold** text appears with proper styling.
#[test]
fn test_e2e_markdown_bold_renders() {
    let mut h = TestHarness::new();
    h.complete_turn(
        "Show bold text",
        vec![
            StreamChunk::Text {
                content: "This has **bold text** in it.".to_string(),
                parent_tool_use_id: None,
            },
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
        ],
    );
    h.render();

    h.assert_screen_contains("bold text", "Bold text content should be visible");

    // Verify bold text has BOLD modifier applied
    let buffer = h.terminal.backend().buffer().clone();
    let found_bold = e2e_harness::buffer_contains_styled_text(
        &buffer,
        "bold text",
        |style| style.add_modifier.contains(Modifier::BOLD),
    );
    assert!(
        found_bold,
        "Expected 'bold text' to have BOLD modifier applied"
    );
}

/// AC1: Code spans render with background color.
/// Verifies that `code` text has distinct styling from plain text.
#[test]
fn test_e2e_markdown_code_span_renders() {
    let mut h = TestHarness::new();
    h.complete_turn(
        "Show code span",
        vec![
            StreamChunk::Text {
                content: "Use `println!(\"Hello\")` to print.".to_string(),
                parent_tool_use_id: None,
            },
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
        ],
    );
    h.render();

    h.assert_screen_contains("println!", "Code span content should be visible");

    // Verify code span has background color (distinct from plain text)
    let buffer = h.terminal.backend().buffer().clone();
    let found_code = e2e_harness::buffer_contains_styled_text(
        &buffer,
        "println!",
        |style| style.bg.is_some(), // Code spans have background color
    );
    assert!(
        found_code,
        "Expected code span 'println!' to have background color"
    );
}

/// AC1: Mixed inline formatting (bold + italic + code).
#[test]
fn test_e2e_markdown_mixed_inline_formatting() {
    let mut h = TestHarness::new();
    h.complete_turn(
        "Show all formatting",
        vec![
            StreamChunk::Text {
                content: "**bold**, *italic*, and `code` together.".to_string(),
                parent_tool_use_id: None,
            },
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
        ],
    );
    h.render();

    h.assert_screen_contains("bold", "Bold text should be visible");
    h.assert_screen_contains("italic", "Italic text should be visible");
    h.assert_screen_contains("code", "Code text should be visible");
}

// ═══════════════════════════════════════════════════════════════════════════
// AC2: List Rendering (bullet and numbered)
// ═══════════════════════════════════════════════════════════════════════════

/// AC2: Bullet list items render with bullet prefix (•).
#[test]
fn test_e2e_markdown_bullet_list_renders() {
    let mut h = TestHarness::new();
    h.complete_turn(
        "Show bullet list",
        vec![
            StreamChunk::Text {
                content: "- First item\n- Second item\n- Third item".to_string(),
                parent_tool_use_id: None,
            },
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
        ],
    );
    h.render();

    h.assert_screen_contains("First item", "First bullet item visible");
    h.assert_screen_contains("Second item", "Second bullet item visible");
    h.assert_screen_contains("Third item", "Third bullet item visible");

    // Verify bullet character appears
    let text = h.screen_text();
    assert!(
        text.contains('•'),
        "Expected bullet character (•) in rendered output"
    );
}

/// AC2: Ordered list items render with number prefix (1., 2., etc.).
#[test]
fn test_e2e_markdown_ordered_list_renders() {
    let mut h = TestHarness::new();
    h.complete_turn(
        "Show numbered list",
        vec![
            StreamChunk::Text {
                content: "1. First step\n2. Second step\n3. Third step".to_string(),
                parent_tool_use_id: None,
            },
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
        ],
    );
    h.render();

    h.assert_screen_contains("First step", "First numbered item visible");
    h.assert_screen_contains("Second step", "Second numbered item visible");
    h.assert_screen_contains("Third step", "Third numbered item visible");

    // Verify number prefixes appear
    let text = h.screen_text();
    assert!(
        text.contains("1."),
        "Expected '1.' prefix in rendered output"
    );
    assert!(
        text.contains("2."),
        "Expected '2.' prefix in rendered output"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// AC3: Code Block Rendering
// ═══════════════════════════════════════════════════════════════════════════

/// AC3: Code blocks render in bordered container with language tag.
#[test]
fn test_e2e_markdown_code_block_renders() {
    let mut h = TestHarness::new();
    h.complete_turn(
        "Show code block",
        vec![
            StreamChunk::Text {
                content: "```rust\nfn main() {\n    println!(\"Hello\");\n}\n```".to_string(),
                parent_tool_use_id: None,
            },
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
        ],
    );
    h.render();

    // Verify bordered container characters
    h.assert_screen_contains("┌", "Code block header border (┌)");
    h.assert_screen_contains("└", "Code block footer border (└)");
    h.assert_screen_contains("│", "Code block side border (│)");

    // Verify language tag and code content
    h.assert_screen_contains("rust", "Language tag in header");
    h.assert_screen_contains("fn main()", "Code content visible");
    h.assert_screen_contains("println!", "Code content with special chars visible");
}

/// AC3: Code block with no language tag still renders bordered.
#[test]
fn test_e2e_markdown_code_block_no_language() {
    let mut h = TestHarness::new();
    h.complete_turn(
        "Show plain code block",
        vec![
            StreamChunk::Text {
                content: "```\nsome plain code\n```".to_string(),
                parent_tool_use_id: None,
            },
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
        ],
    );
    h.render();

    h.assert_screen_contains("┌", "Code block header without lang");
    h.assert_screen_contains("└", "Code block footer without lang");
    h.assert_screen_contains("some plain code", "Plain code content visible");
}

// ═══════════════════════════════════════════════════════════════════════════
// AC4/AC5: Streaming & Malformed Markdown Tolerance
// ═══════════════════════════════════════════════════════════════════════════

/// AC4/AC5: Unclosed code fence renders as bordered container (streaming).
/// Simulates receiving partial code block during streaming.
#[test]
fn test_e2e_streaming_unclosed_code_block_renders() {
    let mut h = TestHarness::new();

    // Start streaming with incomplete code block
    h.send_message("Code example");
    h.process_chunk(StreamChunk::Text {
        content: "```rust\nfn main() {".to_string(),
        parent_tool_use_id: None,
    });
    h.render();

    // Even with unclosed fence, should render as bordered block
    h.assert_screen_contains("┌", "Unclosed code block has header border");
    h.assert_screen_contains("rust", "Language tag visible in streaming");
    h.assert_screen_contains("fn main() {", "Partial code content visible");
}

/// AC5: Malformed markdown with unclosed inline formatting degrades gracefully.
#[test]
fn test_e2e_malformed_markdown_no_crash() {
    let mut h = TestHarness::new();

    // This should not panic
    h.complete_turn(
        "Send broken markdown",
        vec![
            StreamChunk::Text {
                content: "**unclosed bold and *nested italic".to_string(),
                parent_tool_use_id: None,
            },
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
        ],
    );
    h.render();

    // Content should be visible even if malformed
    let text = h.screen_text();
    assert!(
        text.contains("unclosed bold") || text.contains("nested italic"),
        "Malformed content should still be visible in some form"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// AC7: Backward Compatibility (plain text)
// ═══════════════════════════════════════════════════════════════════════════

/// AC7: Plain text renders without artifacts.
#[test]
fn test_e2e_plain_text_backward_compat() {
    let mut h = TestHarness::new();
    h.complete_turn(
        "Plain text message",
        vec![
            StreamChunk::Text {
                content: "This is plain text with no markdown formatting at all.".to_string(),
                parent_tool_use_id: None,
            },
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
        ],
    );
    h.render();

    h.assert_screen_contains("plain text", "Plain text content visible");
    h.assert_screen_contains("no markdown", "Plain text without formatting");

    // Should NOT contain markdown artifacts
    let text = h.screen_text();
    assert!(
        !text.contains("**"),
        "Plain text should not have bold markers visible"
    );
    assert!(
        !text.contains("```"),
        "Plain text should not have fence markers"
    );
}

/// AC7: Paragraph separation (blank lines between paragraphs).
#[test]
fn test_e2e_paragraph_spacing() {
    let mut h = TestHarness::new();
    h.complete_turn(
        "Multiple paragraphs",
        vec![
            StreamChunk::Text {
                content: "First paragraph.\n\nSecond paragraph.".to_string(),
                parent_tool_use_id: None,
            },
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
        ],
    );
    h.render();

    h.assert_screen_contains("First paragraph", "First paragraph visible");
    h.assert_screen_contains("Second paragraph", "Second paragraph visible");
}

// ═══════════════════════════════════════════════════════════════════════════
// Full Pipeline Integration Test (Task 9.3 from story)
// ═══════════════════════════════════════════════════════════════════════════

/// Full markdown pipeline test as specified in Task 9.3:
/// "use TestHarness to send a streaming response with markdown
/// (`# Title\n\n**bold** and `code``), call `h.render()`,
/// assert screen contains 'Title', 'bold', 'code'"
#[test]
fn test_e2e_markdown_full_pipeline() {
    let mut h = TestHarness::new();
    h.complete_turn(
        "Show markdown",
        vec![
            StreamChunk::Text {
                content: "# Title\n\n**bold** and `code`".to_string(),
                parent_tool_use_id: None,
            },
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
        ],
    );
    h.render();

    // Assertions from Task 9.3
    h.assert_screen_contains("Title", "Heading text from Task 9.3");
    h.assert_screen_contains("bold", "Bold text from Task 9.3");
    h.assert_screen_contains("code", "Code text from Task 9.3");

    // Additional style verification
    let buffer = h.terminal.backend().buffer().clone();

    // Bold should have BOLD modifier
    let bold_styled = e2e_harness::buffer_contains_styled_text(
        &buffer,
        "bold",
        |style| style.add_modifier.contains(Modifier::BOLD),
    );
    assert!(bold_styled, "'bold' should have BOLD modifier");

    // Code should have background color
    let code_styled = e2e_harness::buffer_contains_styled_text(&buffer, "code", |style| {
        style.bg.is_some()
    });
    assert!(code_styled, "'code' should have background color");
}
