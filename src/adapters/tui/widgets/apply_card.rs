//! Decision-tier confirmation card for `/artifact apply`.

use ratatui::prelude::*;

use crate::adapters::tui::state::PendingApplyCard;
use crate::adapters::tui::theme::Theme;
use crate::adapters::tui::widgets::artifacts_panel::id_prefix;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyCardChoice {
    Accept,
    Decline,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApplyCardBinding {
    pub key: char,
    pub label: &'static str,
    pub choice: ApplyCardChoice,
}

/// The painted labels and char dispatch both derive from this table.
pub const APPLY_CARD_BINDINGS: [ApplyCardBinding; 2] = [
    ApplyCardBinding {
        key: 'y',
        label: "Apply",
        choice: ApplyCardChoice::Accept,
    },
    ApplyCardBinding {
        key: 'n',
        label: "Cancel (Esc)",
        choice: ApplyCardChoice::Decline,
    },
];

#[must_use]
pub fn choice_for_key(key: char) -> Option<ApplyCardChoice> {
    APPLY_CARD_BINDINGS
        .iter()
        .find(|binding| binding.key == key)
        .map(|binding| binding.choice)
}

#[must_use]
pub fn render_apply_card_lines<'a>(
    card: &PendingApplyCard,
    theme: &Theme,
    width: u16,
) -> Vec<Line<'a>> {
    let mut lines = vec![
        Line::from(Span::styled(
            format!("  [patch] Apply {}", id_prefix(&card.artifact.id)),
            Style::default()
                .fg(theme.colors.accent)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(format!("  Producer: {}", card.artifact.producer.as_str())),
    ];
    for (index, chunk) in wrapped_file_chunks(&card.files.join(", "), width)
        .into_iter()
        .enumerate()
    {
        let prefix = if index == 0 { "  Files: " } else { "         " };
        lines.push(Line::from(format!("{prefix}{chunk}")));
    }
    lines.push(Line::from(format!(
        "  Workspace: {}",
        card.workspace.display()
    )));
    if card.predates_apply_records {
        lines.push(Line::from(
            "  Apply state unknown — this journal predates apply records;",
        ));
        lines.push(Line::from("  patch may already be in the working tree."));
    }
    lines.push(Line::from(Span::styled(
        "  Warning: interruption can leave this patch indeterminate.",
        Style::default().fg(theme.colors.warning),
    )));
    lines.push(Line::from(Span::styled(
        "  No resolution verb exists yet (18-3a-f).",
        Style::default().fg(theme.colors.warning),
    )));
    let actions = APPLY_CARD_BINDINGS
        .iter()
        .map(|binding| format!("[{}] {}", binding.key, binding.label))
        .collect::<Vec<_>>()
        .join("  ");
    lines.push(Line::from(format!("  Awaiting your decision.  {actions}")));
    lines
}

/// Wrap the comma-joined file list to the card width so a multi-file patch
/// names every touched path instead of clipping past the pane edge. The first
/// line sits under `Files: `; continuations align beneath it (9-space indent).
fn wrapped_file_chunks(joined: &str, width: u16) -> Vec<String> {
    const INDENT: usize = 9;
    let limit = (width as usize).saturating_sub(INDENT).max(8);
    let mut out = Vec::new();
    let mut cur = String::new();
    for word in joined.split(' ') {
        if !cur.is_empty() && cur.len() + 1 + word.len() > limit {
            out.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    out
}
