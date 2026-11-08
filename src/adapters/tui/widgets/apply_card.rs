//! Decision frame body for `/artifact apply` and `/artifact resolve`.

use ratatui::prelude::*;

use crate::adapters::tui::decision_queue::{ArtifactCardMode, PendingArtifactCard};
use crate::adapters::tui::keymap::table_card::CardCmd;
use crate::adapters::tui::theme::Theme;
use crate::adapters::tui::widgets::artifacts_panel::id_prefix;
use crate::domain::models::OperatorApplyFinding;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
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

/// The labels and char dispatch for the apply decision.
pub const APPLY_CARD_BINDINGS: [ApplyCardBinding; 2] = [
    ApplyCardBinding {
        key: 'y',
        label: "Apply",
        choice: ApplyCardChoice::Accept,
    },
    ApplyCardBinding {
        key: 'n',
        label: "Cancel",
        choice: ApplyCardChoice::Decline,
    },
];

/// The painted labels for the **resolve** card (Story 18.3a-f, ruling A13).
///
/// ⛔ No label says "Apply": this verb records a report and writes nothing to
/// the workspace. ⛔ No third key — the finding travels in the command, not in
/// the card's keys, which is what keeps the card a two-key confirmation and the
/// event loop untouched.
pub const RESOLVE_CARD_BINDINGS: [ApplyCardBinding; 2] = [
    ApplyCardBinding {
        key: 'y',
        label: "Record report",
        choice: ApplyCardChoice::Accept,
    },
    ApplyCardBinding {
        key: 'n',
        label: "Cancel",
        choice: ApplyCardChoice::Decline,
    },
];

/// 🔴 **Compile-time proof that the mode parameterises labels ONLY.**
///
/// [`choice_for_key`] is single-sourced from [`APPLY_CARD_BINDINGS`], and
/// `app.rs`'s char dispatch calls it without knowing the mode. That is only
/// sound while every mode agrees on the key → choice mapping — so a future
/// author who gives the resolve card a different key gets a build failure here
/// instead of a card whose painted keys and real dispatch silently disagree.
/// ⛔ Do not relax this to a runtime test; a `const` assertion cannot be
/// skipped, ignored or made flaky.
const _: () = {
    assert!(APPLY_CARD_BINDINGS.len() == RESOLVE_CARD_BINDINGS.len());
    let mut index = 0;
    while index < APPLY_CARD_BINDINGS.len() {
        assert!(APPLY_CARD_BINDINGS[index].key == RESOLVE_CARD_BINDINGS[index].key);
        assert!(
            APPLY_CARD_BINDINGS[index].choice as u8 == RESOLVE_CARD_BINDINGS[index].choice as u8
        );
        index += 1;
    }
};

/// The label table for one card mode. Dispatch never consults it.
#[must_use]
pub fn bindings_for(mode: ArtifactCardMode) -> &'static [ApplyCardBinding; 2] {
    match mode {
        ArtifactCardMode::Apply => &APPLY_CARD_BINDINGS,
        ArtifactCardMode::Resolve(_) => &RESOLVE_CARD_BINDINGS,
    }
}

#[must_use]
pub fn choice_for_key(key: char) -> Option<ApplyCardChoice> {
    APPLY_CARD_BINDINGS
        .iter()
        .find(|binding| binding.key == key)
        .map(|binding| binding.choice)
}

/// Map an artifact choice to its decision-frame command.
#[must_use]
pub fn choice_command(choice: ApplyCardChoice) -> CardCmd {
    match choice {
        ApplyCardChoice::Accept => CardCmd::Yes,
        ApplyCardChoice::Decline => CardCmd::No,
    }
}

/// Build the artifact decision body; the frame wraps its rows and supplies the rail.
#[must_use]
pub fn lines(card: &PendingArtifactCard, theme: &Theme) -> Vec<Line<'static>> {
    lines_with_file_limit(card, theme, 200)
}

/// Copy uses the same raw body but retains every file.
pub(crate) fn lines_with_file_limit(
    card: &PendingArtifactCard,
    theme: &Theme,
    limit: usize,
) -> Vec<Line<'static>> {
    let title = match card.mode {
        ArtifactCardMode::Apply => format!("  [patch] Apply {}", id_prefix(&card.artifact.id)),
        ArtifactCardMode::Resolve(_) => format!(
            "  [patch] Record apply inspection {}",
            id_prefix(&card.artifact.id)
        ),
    };
    let head = Line::from(Span::styled(
        title,
        Style::default()
            .fg(theme.colors.accent)
            .add_modifier(Modifier::BOLD),
    ));
    // The consequence and consent come first: the frame body scrolls from its top, so a
    // long file list can never push the epistemic consent out of the first screen.
    let mut lines: Vec<Line<'static>> = Vec::new();
    let warning = Style::default().fg(theme.colors.warning);
    let mut push = |text: String, style: Style| {
        lines.push(Line::from(Span::styled(format!("  {text}"), style)));
    };
    match card.mode {
        ArtifactCardMode::Apply => {
            if card.predates_apply_records {
                push(
                    "Apply state unknown — this journal predates apply records;".into(),
                    Style::default(),
                );
                push(
                    "patch may already be in the working tree.".into(),
                    Style::default(),
                );
            }
            push(
                "Warning: interruption can leave this patch indeterminate.".into(),
                warning,
            );
            push(
                "Recover with: /artifact resolve <id> present|absent.".into(),
                warning,
            );
        }
        ArtifactCardMode::Resolve(finding) => {
            if matches!(finding, OperatorApplyFinding::Present) {
                push("Warning: this patch becomes applyable again, and applying a delta that is already in the tree can succeed silently.".into(), warning);
            }
            push(
                "No workspace write is performed by this decision.".into(),
                warning,
            );
            push(
                format!(
                    "You are reporting: this patch's changes {} in the working tree.",
                    crate::adapters::tui::handlers::artifact_command::finding_clause(finding)
                ),
                Style::default().add_modifier(Modifier::BOLD),
            );
            push("This records what YOU report, not what the system observed. It did not look at the working tree, and it will not.".into(), Style::default());
        }
    }
    lines.push(Line::from("  Awaiting your decision."));
    lines.insert(0, head);
    lines.push(Line::from(format!(
        "  Producer: {}",
        card.artifact.producer.as_str()
    )));
    lines.push(Line::from("  Files:"));
    for file in card.files.iter().take(limit) {
        lines.push(Line::from(format!("  {file}")));
    }
    let omitted = card.files.len().saturating_sub(limit);
    if omitted > 0 {
        lines.push(Line::from(format!("  … and {omitted} more files")));
    }
    lines.push(Line::from(format!(
        "  Workspace: {}",
        card.workspace.display()
    )));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_and_resolve_bindings_share_dispatch() {
        for (apply, resolve) in APPLY_CARD_BINDINGS.iter().zip(RESOLVE_CARD_BINDINGS.iter()) {
            assert_eq!(apply.key, resolve.key);
            assert_eq!(choice_for_key(apply.key), Some(apply.choice));
            assert_eq!(apply.choice, resolve.choice);
            assert_eq!(choice_command(apply.choice), choice_command(resolve.choice));
        }
        assert_eq!(choice_for_key('x'), None);
    }
}
