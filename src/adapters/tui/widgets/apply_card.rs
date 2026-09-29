//! Decision-tier confirmation card for `/artifact apply` and
//! `/artifact resolve`.
//!
//! ⛔ **One widget, two modes — and the mode parameterises CONTENT, never
//! DISPATCH** (Story 18.3a-f, rulings A7 + A13). Both modes paint through the
//! same render branch in `event_loop.rs`, resolve through the same
//! `InputAction`s and share one card slot on `TuiState`, which is why the
//! resolution verb costs the event loop nothing. What differs is the labels and
//! the body: a card that paints `[y] Apply` for a verb that applies nothing
//! welds two epistemic classes onto one token, exactly as `apply: applied`
//! would for the row.

use ratatui::prelude::*;

use crate::adapters::tui::state::{ArtifactCardMode, PendingArtifactCard};
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

/// The painted labels and char dispatch for the **apply** card.
///
/// ⚠ Kept under this name and with the label `"Apply"` because
/// `conformance_18_3a_e_apply_surface.rs` requires the literal `"[y] Apply"` in
/// the painted apply card. ⛔ The resolve card must NOT paint it —
/// see [`RESOLVE_CARD_BINDINGS`].
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
        label: "Cancel (Esc)",
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

#[must_use]
pub fn render_apply_card_lines<'a>(
    card: &PendingArtifactCard,
    theme: &Theme,
    width: u16,
) -> Vec<Line<'a>> {
    let title = match card.mode {
        ArtifactCardMode::Apply => format!("  [patch] Apply {}", id_prefix(&card.artifact.id)),
        ArtifactCardMode::Resolve(_) => format!(
            "  [patch] Record apply inspection {}",
            id_prefix(&card.artifact.id)
        ),
    };
    let mut lines = vec![
        Line::from(Span::styled(
            title,
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
    match card.mode {
        ArtifactCardMode::Apply => push_apply_body(&mut lines, card, theme),
        ArtifactCardMode::Resolve(finding) => push_resolve_body(&mut lines, finding, theme, width),
    }
    // ⛔ **Every painted key must dispatch, and every key must be painted.**
    // 18.3c shipped `[✗] Retract` as inert text and it became the epic's
    // most-cited defect; the dual failure is a key that dispatches but is
    // clipped off the pane edge, which is what happens to the resolve card's
    // longer labels at the spec's narrow reference widths. Keep them on one
    // line while it fits, then break — ⛔ never truncate.
    let actions: Vec<String> = bindings_for(card.mode)
        .iter()
        .map(|binding| format!("[{}] {}", binding.key, binding.label))
        .collect();
    let single = format!("  Awaiting your decision.  {}", actions.join("  "));
    if fits(&single, width) {
        lines.push(Line::from(single));
    } else {
        lines.push(Line::from("  Awaiting your decision."));
        for action in actions {
            lines.push(Line::from(format!("  {action}")));
        }
    }
    lines
}

/// Does one composed line survive the card's border and padding at `width`?
///
/// The decision-card chrome spends one column per side on the frame plus one
/// of padding, so a line of `width - 2` columns is the last one that renders
/// whole. Anything longer is silently clipped by the buffer.
fn fits(line: &str, width: u16) -> bool {
    line.chars().count() <= (width as usize).saturating_sub(2)
}

fn push_apply_body<'a>(lines: &mut Vec<Line<'a>>, card: &PendingArtifactCard, theme: &Theme) {
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
        "  Recover with: /artifact resolve <id> present|absent.",
        Style::default().fg(theme.colors.warning),
    )));
}

/// The resolve card's body (Story 18.3a-f, AC4).
///
/// 🔴 **The epistemic sentence is stated before `y`, not after.** Post-
/// Irreversible Confidence applied to the *epistemics*: the operator must know,
/// before confirming, that what is about to be recorded is their report and not
/// the system's observation.
///
/// ⛔ No copy here says *audit trail*, *evidence*, *authenticated*,
/// *tamper-evident*, *verified* or *proof* (`ADR-18-3a-d-01:49`, extended to
/// future surfaces), and ⛔ none calls the record *permanent*, *final* or
/// *durable* (ruling P6) — compaction that drops this record while keeping its
/// `PatchApplyStarted` silently re-wedges the artifact.
fn push_resolve_body<'a>(
    lines: &mut Vec<Line<'a>>,
    finding: OperatorApplyFinding,
    theme: &Theme,
    width: u16,
) {
    if matches!(finding, OperatorApplyFinding::Present) {
        // Ruling A9: name the double-apply consequence. ⛔ Do not repeat the
        // "bounded by `git apply`'s atomicity" claim — demonstrated false on
        // 2026-08-08, when a new-file hunk re-applies with exit=0 onto a tree
        // the operator had deleted the file from.
        push_wrapped(
            lines,
            "Warning: this patch becomes applyable again, and applying a delta \
             that is already in the tree can succeed silently.",
            width,
            Style::default().fg(theme.colors.warning),
        );
    }
    push_wrapped(
        lines,
        "No workspace write is performed by this decision.",
        width,
        Style::default().fg(theme.colors.warning),
    );
    // 🔴 Emitted LAST so the report statement and the epistemic consent sit
    // immediately above the actions. The resolve card renders through
    // `render_bottom_anchored_decision_card`, which tail-scrolls on overflow;
    // a multi-file patch can push the lead off-screen, so the consent must be
    // in the tail region to survive it (AC4 — stated before `y`).
    push_wrapped(
        lines,
        &format!(
            "You are reporting: this patch's changes {} in the working tree.",
            crate::adapters::tui::handlers::artifact_command::finding_clause(finding)
        ),
        width,
        Style::default().add_modifier(Modifier::BOLD),
    );
    push_wrapped(
        lines,
        "This records what YOU report, not what the system observed. It did not \
         look at the working tree, and it will not.",
        width,
        Style::default(),
    );
}

/// Push one sentence, greedy-wrapped to the card width with a two-space indent.
///
/// ⛔ Clipping is not an option here: every line this card paints is a claim the
/// operator is being asked to consent to, and half a claim is worse than none.
fn push_wrapped<'a>(lines: &mut Vec<Line<'a>>, text: &str, width: u16, style: Style) {
    for chunk in wrap_words(text, (width as usize).saturating_sub(4).max(8)) {
        lines.push(Line::from(Span::styled(format!("  {chunk}"), style)));
    }
}

/// Wrap the comma-joined file list to the card width so a multi-file patch
/// names every touched path instead of clipping past the pane edge. The first
/// line sits under `Files: `; continuations align beneath it (9-space indent).
fn wrapped_file_chunks(joined: &str, width: u16) -> Vec<String> {
    const INDENT: usize = 9;
    wrap_words(joined, (width as usize).saturating_sub(INDENT).max(8))
}

/// Greedy word wrap. Shared by the file list and the resolve card's prose so
/// one wrapping rule governs everything this widget paints.
fn wrap_words(text: &str, limit: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        if !cur.is_empty() && cur.chars().count() + 1 + word.chars().count() > limit {
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
