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

use crate::adapters::tui::state::ArtifactsZeroState;
use crate::domain::models::{
    ApplyOutcome, ApplyState, ArtifactId, ArtifactKind, ArtifactRef, OperatorApplyFinding,
    OrchestrationRoom, PermissionMode, ReviewStatus, ReviewVerdict,
};
use crate::domain::services::patch_review::{
    MergeBackPolicy, PatchDisposition, operator_patch_decision,
};

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
    if !parts_lost(&parts, &truncated) {
        return truncated;
    }
    truncate_to_width(&reordered(&parts), width)
}

/// Whether cutting the canonical row to `shown` lost a part that must survive.
///
/// An `AwaitingReview` row carries NO suffix by design — its `▲ no reviewer` hazard is the
/// only signal, so a truncated hazard must trigger the fallback too, or the canonical pending
/// row loses its warning precisely in the narrow layout this fallback exists to protect.
fn parts_lost(parts: &RowParts, shown: &str) -> bool {
    parts.suffix.is_some_and(|suffix| !shown.contains(suffix))
        || parts
            .apply_suffix
            .as_ref()
            .is_some_and(|suffix| !shown.contains(suffix))
        || (!parts.hazard.is_empty() && !shown.contains(parts.hazard))
}

/// Safety-first fallback: reason and hazard before identifier.
fn reordered(parts: &RowParts) -> String {
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
    reordered
}

/// Everything the Mission Control Artifacts tab needs to paint one row at any width: the
/// canonical row, its safety-first reordering and the parts the reordering protects (the
/// painter reorders exactly when cutting the canonical row would lose one).
#[must_use]
pub fn row_forms(
    artifact: &ArtifactRef,
    room: &OrchestrationRoom,
    permission_mode: PermissionMode,
    policy: &MergeBackPolicy,
) -> (String, String, Vec<String>) {
    let parts = row_parts(artifact, room, permission_mode, policy);
    let canonical = artifact_row(artifact, room, permission_mode, policy);
    let keep = parts
        .suffix
        .map(str::to_owned)
        .into_iter()
        .chain(parts.apply_suffix.clone())
        .chain((!parts.hazard.is_empty()).then(|| parts.hazard.to_owned()))
        .collect();
    (canonical, reordered(&parts), keep)
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
pub fn input_request_note() -> String {
    format!(
        "answer it from the Needs-you queue ({})",
        crate::adapters::tui::decision_queue::queue_key_hint()
    )
}
