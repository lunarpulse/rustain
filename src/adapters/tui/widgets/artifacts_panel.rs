//! Durable artifact list (`Ctrl+X, E` / `/artifacts`). Story 18.3a-c, AC3 / AC4.
//!
//! Renders the [`ArtifactRef`]s produced by
//! [`crate::domain::models::OrchestrationRoom::project_for_host`] — the
//! host-honest fold of the one journal. Never `project()`.
//!
//! # This module mints no vocabulary
//!
//! The row grammar extends `/room`'s (UX-DR-ROOM-04 / UX-DR-ROOM-08): a terse
//! ` · <clause>` suffix plus the single `▲` hazard glyph, never a prose
//! sentence in the row. The full explanation belongs in the `/artifact show`
//! drill-down.
//!
//! # Decision, not verdict
//!
//! A patch row renders the **apply decision** that governs it, sourced from
//! [`patch_disposition`] — *the same resolver the apply path uses*, because
//! `may_apply_patch` is now a total projection of it. Rendering the verdict
//! instead is the first of the five mutants UX-DR-ROOM-08 names.
//!
//! ⛔ **The review state is read from the room projection, never from a handle
//! passed around the UI layer.** `ArtifactRef.review` is public and forgeable;
//! `PatchMergeBack::apply` already refuses to trust it and re-derives from
//! `project_room`. The surface inherits the rule.
//!
//! # What is deliberately NOT rendered
//!
//! - **`· applies` is not a string this build emits** (ruling P1). Recording a
//!   verdict does not apply anything in this cut, so a row claiming a
//!   hypothetical workspace write would sit in the same column as
//!   `· auto-applies (policy)`, which describes a write that **already
//!   happened**. Two tenses, one column, adjacent rows. An eligible patch reads
//!   `· applies`; the journaled outcome is rendered separately as
//!   `· apply: <outcome>`.
//! - **The `↓ N newer entries` head-poll chrome is absent.** It is fed by a
//!   1 Hz tick `/room` has and this surface does not; copying the string
//!   without the wiring ships an unreachable line
//!   (`DF-18-3a-c-ARTIFACT-HEAD-POLL`).
//! - **No queue affordances on `InputRequest` rows** (ruling P6). They render,
//!   because hiding one of the three kinds that actually occur would make
//!   `/artifacts` lie about the room's contents — but with no "N open" count,
//!   no urgency glyph and no ordering that implies triage.
//!   `DF-18-3a-b-INBOX-SURFACE` forbids a surface *whose purpose is a queue*
//!   that cannot be cleared; the affordance set is the distinction.
//! - **No action key labels.** 18.3c's highest-frequency review class was
//!   `[✗] Retract` shipped as inert text. The verbs here are typed
//!   (`/artifact review <id> <verdict>`), and the footer says so.

use ratatui::prelude::*;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};

use crate::adapters::tui::state::{ArtifactsPanelState, ArtifactsZeroState};
use crate::adapters::tui::theme::Theme;
use crate::domain::models::{
    ApplyOutcome, ApplyState, ArtifactId, ArtifactKind, ArtifactRef, OperatorApplyFinding,
    OrchestrationRoom, PermissionMode, ReviewStatus, ReviewVerdict,
};
use crate::domain::services::patch_review::{
    MergeBackPolicy, PatchDisposition, operator_patch_decision,
};
use crate::domain::services::transparency::{STRUCTURAL_REPLAY_CLAIM, format_unix_millis};

use super::room_panel::UNKNOWN_RECORD_LABEL;
use super::sidebar::truncate_to_width;

/// Characters of an `ArtifactId` shown in a row.
///
/// ⛔ **This prefix is not "the content hash".** For a `Patch` the id is
/// `patch_artifact_id(body, producer, authority)` — namespaced so two spokes
/// producing an identical diff do not collide — while `content_hash` stays
/// body-only. No row, header or help string may call it the content hash.
pub const ID_PREFIX_LEN: usize = 6;

/// First [`ID_PREFIX_LEN`] characters of an artifact id.
#[must_use]
pub fn id_prefix(id: &ArtifactId) -> String {
    id.as_str().chars().take(ID_PREFIX_LEN).collect()
}

/// Operator-facing label for an artifact kind.
#[must_use]
pub fn kind_label(kind: ArtifactKind) -> &'static str {
    match kind {
        ArtifactKind::Evidence => "evidence",
        ArtifactKind::Patch => "patch",
        ArtifactKind::TestResult => "test-result",
        ArtifactKind::Decision => "decision",
        ArtifactKind::Review => "review",
        ArtifactKind::InputRequest => "input-request",
        // A kind string this build cannot read. Explicit, never silently
        // dropped and never guessed at (UX-DR-ROOM-01).
        _ => "unknown",
    }
}

/// The `<state>` token: what the journal records about this artifact's review.
///
/// ⚠ `unknown` here names an unreadable **field value** inside a recognized
/// record. That is a different fact from [`UNKNOWN_RECORD_LABEL`], which names
/// an unrecognized **record tag** and is rendered in the panel chrome. Two
/// facts, two strings — not two spellings of one.
#[must_use]
pub fn review_state_label(review: Option<&ReviewStatus>) -> String {
    match review {
        None => "unreviewed".to_owned(),
        Some(ReviewStatus::Pending) => "pending".to_owned(),
        Some(ReviewStatus::Reviewed { reviewer, verdict }) => {
            let verdict = match verdict {
                ReviewVerdict::Approved => "approved",
                ReviewVerdict::ChangesRequested => "changes-requested",
                ReviewVerdict::Rejected => "rejected",
                _ => "verdict: unknown",
            };
            // Attribution, never endorsement: this is what the journal
            // records, not a claim that the entry is authenticated
            // (`DF-18-2-AUTHENTICATED-JOURNAL`).
            format!("{verdict}({})", reviewer.as_str())
        }
        // The status tag resolved the whole enum, so serde never read a
        // verdict field at all.
        Some(_) => "review: unknown".to_owned(),
    }
}

/// The decision suffix for a disposition, or `None` where the table has no
/// suffix cell.
///
/// ⛔ [`PatchDisposition::AwaitingReview`] gets **no** suffix: the row's
/// `<state>` field already reads `pending`, and inventing one renders `pending`
/// twice and stops matching the UX line.
#[must_use]
pub fn decision_suffix(disposition: PatchDisposition) -> Option<&'static str> {
    match disposition {
        // Eligible review decisions use the same verb as the confirmed front
        // door. The outcome suffix below remains separate: `applies` is
        // eligibility, `apply: applied` is the journaled result.
        PatchDisposition::Applies => Some("applies"),
        PatchDisposition::AutoApplies => Some("auto-applies (policy)"),
        PatchDisposition::RefusedSelfReview => Some("refused: self-review"),
        PatchDisposition::RefusedPlanMode => Some("refused: plan mode"),
        PatchDisposition::RefusedPeerOwned => Some("refused: peer-owned"),
        PatchDisposition::RefusedNoProvenance => Some("refused: no provenance"),
        PatchDisposition::RefusedChangesRequested => Some("changes requested"),
        PatchDisposition::RefusedRejected => Some("rejected"),
        // Not rendered in a patch row (ruling A4): the condition is
        // unreachable through this front door, and the peer-owned reason it
        // used to share a branch with would be a FALSE reason for a non-patch.
        PatchDisposition::RefusedNotAPatch | PatchDisposition::AwaitingReview => None,
        // ⛔ No `_` arm — the match is exhaustive over `PatchDisposition`, so a
        // future disposition cannot ship without a deliberate suffix decision
        // (AC4 keystone (b)).
    }
}

/// `▲ <word>` when this row needs the operator's attention, else empty.
///
/// Bound to the recorded review state rather than to the disposition, so an
/// artifact carrying a reviewer this build cannot read never claims there is
/// *no* reviewer.
fn hazard(artifact: &ArtifactRef, disposition: Option<PatchDisposition>) -> &'static str {
    match disposition {
        Some(PatchDisposition::AwaitingReview)
            if matches!(artifact.review, None | Some(ReviewStatus::Pending)) =>
        {
            "\u{25b2} no reviewer"
        }
        Some(
            PatchDisposition::RefusedSelfReview
            | PatchDisposition::RefusedPlanMode
            | PatchDisposition::RefusedPeerOwned
            | PatchDisposition::RefusedNoProvenance
            | PatchDisposition::RefusedChangesRequested
            | PatchDisposition::RefusedRejected,
        ) => "\u{25b2}",
        Some(PatchDisposition::AwaitingReview) => "\u{25b2}",
        _ => "",
    }
}

/// Ticket state for an `InputRequest` row, joined from the node views.
///
/// ⛔ **The read model is not widened to carry it.** `TicketAssigned.to` is
/// durable-only by 18.3a-b's deliberate design, and `NodeView.open_tickets`
/// stays `Vec<ArtifactId>`; the fold's own doc calls widening it a mutant. What
/// is available — and what the operator actually needs — is *which node* the
/// ticket is filed against.
#[must_use]
pub fn ticket_state(room: &OrchestrationRoom, id: &ArtifactId) -> Option<String> {
    for view in room.nodes().values() {
        if view.open_tickets.iter().any(|open| open == id) {
            return Some(format!("ticket open on {}", view.id.as_str()));
        }
        if view.resolved_tickets.contains_key(id) {
            return Some(format!("ticket resolved on {}", view.id.as_str()));
        }
    }
    None
}

pub fn apply_state_suffix(room: &OrchestrationRoom, artifact: &ArtifactId) -> String {
    match room.apply_state().get(artifact).copied().unwrap_or_default() {
        ApplyState::NeverAttempted if room.predates_apply_records().contains(artifact) => {
            "apply: state unknown — this journal predates apply records; patch may already be in working tree"
                .to_owned()
        }
        ApplyState::NeverAttempted => "apply: never attempted".to_owned(),
        ApplyState::Indeterminate => {
            "apply: indeterminate — inspect the tree, then /artifact resolve <id> present|absent"
                .to_owned()
        }
        ApplyState::Resolved(ApplyOutcome::Applied) => "apply: applied".to_owned(),
        ApplyState::Resolved(ApplyOutcome::Conflict) => "apply: conflict".to_owned(),
        ApplyState::Resolved(ApplyOutcome::Failed) => "apply: failed".to_owned(),
        ApplyState::Resolved(ApplyOutcome::Unknown) => {
            "apply: unknown outcome (not success)".to_owned()
        }
        // 🔴 Ruling A8: `present`/`absent`, ⛔ never `applied`/`not-applied`.
        // `ApplyOutcome::Applied` already means "`git apply` returned 0";
        // reusing the word would weld two epistemic classes onto one token and
        // both rows would read `apply: applied`.
        //
        // ⚠ Ruling P2: the `present` row reads as a COMPLETION, never an
        // invitation. The gate permits a retry; the row must not advertise one.
        // ⛔ No "retry", "ready to apply", "try again". ⛔ And no string here
        // calls the release permanent, final or durable — compaction that drops
        // the report while keeping its `PatchApplyStarted` silently re-wedges
        // the artifact (`DF-18-2-JOURNAL-GROWTH`).
        ApplyState::OperatorResolved(OperatorApplyFinding::Present) => {
            "apply: resolved by operator — reported present".to_owned()
        }
        ApplyState::OperatorResolved(OperatorApplyFinding::Absent) => {
            "apply: resolved by operator — reported absent".to_owned()
        }
        ApplyState::OperatorResolved(OperatorApplyFinding::Unknown) => {
            "apply: resolved by operator — report unreadable (not resolved)".to_owned()
        }
    }
}

/// The composed pieces of one artifact row.
///
/// 🔴 **One producer, two layouts.** Both [`artifact_row`] and
/// [`visible_artifact_row`] assemble from this and nothing else. The first
/// draft had each of them compose the decision suffix independently, and the
/// consequence was a live false-green: deleting the suffix from the canonical
/// row made the narrow-width fallback *silently repair it*, so the "render the
/// verdict instead of the decision" mutant escaped. Two formatters for one
/// value is the same duplication class this module's decision core exists to
/// prevent — fixed here rather than papered over with a second assertion.
struct RowParts {
    kind: &'static str,
    prefix: String,
    state: String,
    producer: String,
    suffix: Option<&'static str>,
    hazard: &'static str,
    apply_suffix: Option<String>,
}

fn row_parts(
    artifact: &ArtifactRef,
    room: &OrchestrationRoom,
    permission_mode: PermissionMode,
    policy: &MergeBackPolicy,
) -> RowParts {
    let disposition = operator_patch_decision(artifact, permission_mode, policy)
        .map(|decision| decision.disposition);
    RowParts {
        kind: kind_label(artifact.kind),
        prefix: id_prefix(&artifact.id),
        state: if artifact.kind == ArtifactKind::InputRequest {
            ticket_state(room, &artifact.id).unwrap_or_else(|| "unassigned".to_owned())
        } else {
            review_state_label(artifact.review.as_ref())
        },
        producer: artifact.producer.as_str().to_owned(),
        suffix: disposition.and_then(decision_suffix),
        apply_suffix: (artifact.kind == ArtifactKind::Patch)
            .then(|| apply_state_suffix(room, &artifact.id)),
        hazard: hazard(artifact, disposition),
    }
}

/// One artifact row: `<kind> <id-prefix> · <state> · from <producer>[ · <suffix>]  [▲ …]`.
///
/// **Truncation order is a safety property, not cosmetics.** The identifier
/// here *is* a hash, and 18.2's smoke found a panel that "rendered but told you
/// nothing" because a long hash preceded the verdict. [`visible_artifact_row`]
/// therefore reorders at narrow widths so the decision and the hazard survive.
#[must_use]
pub fn artifact_row(
    artifact: &ArtifactRef,
    room: &OrchestrationRoom,
    permission_mode: PermissionMode,
    policy: &MergeBackPolicy,
) -> String {
    let parts = row_parts(artifact, room, permission_mode, policy);
    let mut row = format!(
        "{} {} \u{b7} {} \u{b7} from {}",
        parts.kind, parts.prefix, parts.state, parts.producer
    );
    if let Some(suffix) = parts.suffix {
        row.push_str(&format!(" \u{b7} {suffix}"));
    }
    if let Some(apply_suffix) = &parts.apply_suffix {
        row.push_str(&format!(" \u{b7} {apply_suffix}"));
    }
    if !parts.hazard.is_empty() {
        row.push_str(&format!("  {}", parts.hazard));
    }
    row
}

/// The width-aware row. At narrow widths the decision suffix and hazard precede
/// the unbounded identifier, copying `room_panel::visible_node_row`'s rule.
///
/// ⛔ It reorders [`RowParts`]; it never *derives* a part of its own. A row with
/// no suffix stays a row with no suffix at every width.
#[must_use]
pub fn visible_artifact_row(
    artifact: &ArtifactRef,
    room: &OrchestrationRoom,
    permission_mode: PermissionMode,
    policy: &MergeBackPolicy,
    width: usize,
) -> String {
    let parts = row_parts(artifact, room, permission_mode, policy);
    let canonical = artifact_row(artifact, room, permission_mode, policy);
    let truncated = truncate_to_width(&canonical, width);
    let suffix_lost = parts
        .suffix
        .is_some_and(|suffix| !truncated.contains(suffix));
    let apply_suffix_lost = parts
        .apply_suffix
        .as_ref()
        .is_some_and(|suffix| !truncated.contains(suffix));
    // An `AwaitingReview` row carries NO suffix by design — its `▲ no
    // reviewer` hazard is the only signal, so a truncated hazard must trigger
    // the fallback too, or the canonical pending row loses its warning
    // precisely in the narrow layout this fallback exists to protect.
    let hazard_lost = !parts.hazard.is_empty() && !truncated.contains(parts.hazard);
    if !suffix_lost && !apply_suffix_lost && !hazard_lost {
        return truncated;
    }
    // Safety-first fallback: reason and hazard before identifier.
    let mut reordered = format!("{} \u{b7}", parts.kind);
    if let Some(suffix) = parts.suffix {
        reordered.push_str(&format!(" {suffix}"));
    }
    if !parts.hazard.is_empty() {
        reordered.push_str(&format!(" {}", parts.hazard));
    }
    if let Some(apply_suffix) = &parts.apply_suffix {
        reordered.push_str(&format!(" \u{b7} {apply_suffix}"));
    }
    reordered.push_str(&format!(" {}", parts.prefix));
    truncate_to_width(&reordered, width)
}

/// One-level lineage line per `depends_on` edge.
///
/// ⛔ **One level, and that is the whole graph today.** UX-DR-ROOM-05 says
/// *"every artifact + its `depends_on`"* — direct edges, not a transitive walk
/// — and production populates at most one edge per artifact (both producers
/// collect an `Option` into a `Vec`; `write_input_request` hardcodes empty). The
/// real graph is a forest of two-node trees. No recursive walker, no cycle
/// detector, no memoised traversal. The interactive DAG is BEYOND-18.3a.
#[must_use]
pub fn lineage_line(dependency: &ArtifactId) -> String {
    format!("  \u{2514} depends on {}", id_prefix(dependency))
}

/// Copy for each of the four zero-states. Distinguishable on purpose.
#[must_use]
pub fn zero_state_lines(zero: ArtifactsZeroState) -> [&'static str; 2] {
    match zero {
        ArtifactsZeroState::NotAttached => [
            "· not attached — this session composed no orchestration journal",
            "Nothing has been recorded, and nothing is being hidden.",
        ],
        ArtifactsZeroState::NoArtifacts => [
            "· the room journal is readable and holds no artifacts yet",
            "An artifact appears here the first time one is produced.",
        ],
        ArtifactsZeroState::NoPatches => [
            "· no patches in this room",
            "The rows below are evidence and tickets; there is nothing to review.",
        ],
        ArtifactsZeroState::AllReviewed => [
            "· every patch in this room carries a recorded verdict",
            "Re-reviewing is permitted; the latest verdict governs.",
        ],
    }
}

/// The footer's verb reminder. The typed verbs are the affordance; the panel
/// has no inert action labels.
pub const VERB_HINT: &str = "/artifact show <id> · /artifact review <id> approve|request-changes|reject \
     · /artifact apply <id> (previews impact, then asks for confirmation)";

/// Stated once, in the chrome, so an `InputRequest` row needs no affordance of
/// its own to explain itself.
pub const INPUT_REQUEST_NOTE: &str =
    "input requests are listed as inventory — answering them arrives with the operator inbox";

pub fn render(
    area: Rect,
    buf: &mut Buffer,
    state: &mut ArtifactsPanelState,
    selected: usize,
    focus: &crate::domain::models::FocusState,
    theme: &Theme,
) {
    Clear.render(area, buf);

    let is_focused = matches!(
        focus,
        crate::domain::models::FocusState::Sidebar {
            panel: crate::domain::models::visual::PanelType::Artifacts,
            ..
        }
    );
    let border_style = if is_focused {
        Style::default().fg(theme.colors.accent)
    } else {
        Style::default().fg(theme.colors.fg_secondary)
    };
    // "as of", never "live".
    let title = match state.read_at_ms {
        Some(ms) => format!(" Artifacts · as of {} ", format_unix_millis(ms)),
        None => " Artifacts ".to_owned(),
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(border_style)
        .title_bottom(Span::styled(
            " replay · j/k move · Ctrl+X E close ",
            Style::default().fg(theme.colors.fg_muted),
        ));
    let inner = block.inner(area);
    block.render(area, buf);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let muted = Style::default()
        .fg(theme.colors.fg_muted)
        .add_modifier(Modifier::ITALIC);
    let mut rendered_selected = selected;

    if let Some(error) = state.error.clone() {
        state.set_viewport_rows(inner.height as usize, &mut rendered_selected);
        Paragraph::new(vec![
            Line::from(Span::styled(
                "⚠ could not read the room journal",
                Style::default().fg(theme.colors.error),
            )),
            Line::from(Span::styled(error, Style::default().fg(theme.colors.error))),
            Line::from(Span::styled(
                "This is a read failure, not an empty room.",
                muted,
            )),
        ])
        .wrap(Wrap { trim: true })
        .render(inner, buf);
        return;
    }

    if state.zero_state() == Some(ArtifactsZeroState::NotAttached) {
        state.set_viewport_rows(inner.height as usize, &mut rendered_selected);
        let [headline, detail] = zero_state_lines(ArtifactsZeroState::NotAttached);
        Paragraph::new(vec![
            Line::from(Span::styled(headline, muted)),
            Line::from(Span::styled(detail, muted)),
        ])
        .render(inner, buf);
        return;
    }

    let mut chrome: Vec<Line<'static>> = Vec::new();
    chrome.push(Line::from(Span::styled(
        truncate_to_width(&format!("here: {}", state.host_id), inner.width as usize),
        muted,
    )));
    match state.zero_state() {
        Some(ArtifactsZeroState::NoArtifacts) => {
            let [headline, detail] = zero_state_lines(ArtifactsZeroState::NoArtifacts);
            chrome.push(Line::from(Span::styled(headline, muted)));
            chrome.push(Line::from(Span::styled(detail, muted)));
        }
        Some(zero @ (ArtifactsZeroState::NoPatches | ArtifactsZeroState::AllReviewed)) => {
            let [headline, _] = zero_state_lines(zero);
            chrome.push(Line::from(Span::styled(headline, muted)));
        }
        _ => {}
    }
    if state.unknown_records > 0 {
        chrome.push(Line::from(Span::styled(
            truncate_to_width(
                &format!("? {} × {UNKNOWN_RECORD_LABEL}", state.unknown_records),
                inner.width as usize,
            ),
            Style::default().fg(theme.colors.warning),
        )));
    }
    let has_input_requests = state.room.as_ref().is_some_and(|room| {
        room.artifacts()
            .values()
            .any(|artifact| artifact.kind == ArtifactKind::InputRequest)
    });
    if has_input_requests {
        chrome.push(Line::from(Span::styled(
            truncate_to_width(INPUT_REQUEST_NOTE, inner.width as usize),
            muted,
        )));
    }

    let footer_text = format!("{VERB_HINT}\n{STRUCTURAL_REPLAY_CLAIM}");
    let footer_height = (wrapped_line_count(VERB_HINT, inner.width as usize)
        + wrapped_line_count(STRUCTURAL_REPLAY_CLAIM, inner.width as usize))
    .min(inner.height);
    let list_height = (inner.height as usize).saturating_sub(chrome.len() + footer_height as usize);
    state.set_viewport_rows(list_height.max(1), &mut rendered_selected);

    let permission_mode = state.permission_mode;
    let policy = state.policy;
    let rows: Vec<(ArtifactId, Vec<String>)> = state
        .room
        .as_ref()
        .map(|room| {
            room.artifacts()
                .values()
                .map(|artifact| {
                    let mut lines = vec![visible_artifact_row(
                        artifact,
                        room,
                        permission_mode,
                        &policy,
                        inner.width as usize,
                    )];
                    lines.extend(artifact.depends_on.iter().map(|dependency| {
                        truncate_to_width(&lineage_line(dependency), inner.width as usize)
                    }));
                    (artifact.id.clone(), lines)
                })
                .collect()
        })
        .unwrap_or_default();

    let viewport = list_height.max(1);
    let start = state.scroll_offset.min(rows.len().saturating_sub(1));
    // Whole blocks, measured in rendered LINES: an artifact is its row plus one
    // lineage line per `depends_on` edge. Counting artifacts here lets a
    // lineage-heavy list overflow the area, clipping the row the selection
    // sits on while `Enter` still acts on it. `synchronize_selection` uses the
    // same line math, so the window and the selection never disagree.
    let mut end = start;
    let mut used = 0usize;
    while end < rows.len() {
        let lines = rows[end].1.len();
        if used + lines > viewport && end > start {
            break;
        }
        used += lines;
        end += 1;
    }
    state.record_rendered_viewport(
        rows.get(start).map(|(id, _)| id.clone()),
        rows.get(rendered_selected).map(|(id, _)| id.clone()),
    );
    let items: Vec<ListItem<'static>> = rows[start..end]
        .iter()
        .enumerate()
        .flat_map(|(offset, (_, lines))| {
            let style = if start + offset == rendered_selected {
                Style::default()
                    .fg(theme.colors.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.colors.fg_primary)
            };
            lines
                .iter()
                .map(move |line| ListItem::new(Line::from(Span::styled(line.clone(), style))))
                .collect::<Vec<_>>()
        })
        .collect();

    let chrome_height = chrome.len() as u16;
    let list_area = Rect {
        y: inner.y + chrome_height,
        height: inner.height.saturating_sub(chrome_height + footer_height),
        ..inner
    };
    if list_area.height > 0 {
        ratatui::prelude::Widget::render(List::new(items), list_area, buf);
    }
    if chrome_height > 0 {
        let chrome_area = Rect {
            height: chrome_height.min(inner.height),
            ..inner
        };
        Paragraph::new(chrome).render(chrome_area, buf);
    }
    let footer_area = Rect {
        y: inner.y + inner.height.saturating_sub(footer_height),
        height: footer_height,
        ..inner
    };
    Paragraph::new(footer_text)
        .style(muted)
        .wrap(Wrap { trim: true })
        .render(footer_area, buf);
}

fn wrapped_line_count(text: &str, width: usize) -> u16 {
    let width = width.max(1);
    let mut lines = 1u16;
    let mut used = 0usize;
    for word in text.split_whitespace() {
        let word_width = word.chars().count();
        if used == 0 {
            used = word_width;
        } else if used + 1 + word_width <= width {
            used += 1 + word_width;
        } else {
            lines = lines.saturating_add(1);
            used = word_width;
        }
    }
    lines
}
