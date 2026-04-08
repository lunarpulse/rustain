use super::transform::StyledBlock;
use crate::adapters::tui::theme::Theme;

/// Stage 4: Syntax highlighting (no-op passthrough for Story 3-6).
///
/// Syntect highlighting is deferred to Epic 15.
pub fn highlight(blocks: Vec<StyledBlock>, _theme: &Theme) -> Vec<StyledBlock> {
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::tui::theme::Theme;

    #[test]
    fn test_highlight_is_identity() {
        let theme = Theme::dark();
        // Build a minimal block list
        let input: Vec<StyledBlock> = vec![
            StyledBlock::Paragraph(vec![]),
            StyledBlock::ThematicBreak,
        ];
        // Measure length before and after (can't compare by value without PartialEq,
        // but we can verify the same number of blocks come back)
        let input_len = input.len();
        let output = highlight(input, &theme);
        assert_eq!(output.len(), input_len);
    }
}
