//! `/artifacts` and `/artifact show|review` — Story 18.3a-c, AC3 / AC4 / AC5.
//!
//! Pure parser, resolver and renderers: data in, data out. Every effect (the
//! journal read, the durable append, the store fetch) happens in
//! [`crate::infrastructure::runtime::artifact_bridge`], because the handler
//! contract forbids `crate::infrastructure::*` imports here.
//!
//! Named `artifact_command`, not `handle_artifact_command`: `tests/conformance.rs`
//! pins `EXPECTED_HANDLE_COUNT` with an exact `assert_eq!` over
//! `^\s*pub(\(crate\))?\s+(async\s+)?fn\s+handle_[a-z_]+\(`, and bumping it is a
//! governance decision requiring a `RATCHET-SIGNOFF` trailer. Nothing here needs
//! a `HandlerOutcome`, so the counter stays untouched — the same choice
//! `handlers/room_command.rs` made.

use crate::adapters::tui::state::TuiState;
use crate::adapters::tui::widgets::artifacts_panel::{
    apply_state_suffix, decision_suffix, id_prefix, kind_label, review_state_label,
};
use crate::domain::models::{
    ArtifactId, ArtifactKind, ArtifactRef, OrchestrationRoom, PermissionMode, ReviewVerdict,
};
use crate::domain::ports::PatchApplyPortError;
use crate::domain::services::patch_review::{MergeBackPolicy, operator_patch_decision};

/// The valid sub-verb set, named verbatim in every parser refusal.
pub const USAGE: &str = "/artifacts | /artifact show <id> | /artifact apply <id> | \
                         /artifact review <id> approve|request-changes|reject";

/// Stable id for the in-chat `/artifact` result block. A drill-down is a
/// **view**, not an event stream: re-running it replaces the block rather than
/// stacking another copy under the old one.
pub const ARTIFACT_BLOCK_ID: &str = "artifact-view";

#[derive(Debug, PartialEq, Eq)]
pub enum ArtifactCommandArgs {
    /// Bare `/artifacts` — open the read-only list panel.
    List,
    /// `/artifact show <id>` — the drill-down.
    Show { id: String },
    /// `/artifact apply <id>` — open the confirmed workspace-write front door.
    Apply { id: String },
    /// `/artifact review <id> <verdict>` — the verdict verb.
    Review { id: String, verdict: ReviewVerdict },
}

/// Parse an operator-typed verdict.
///
/// Deliberately **not** `FromStr` over the serde representation: an
/// unrecognised word typed at the prompt is an operator error to report, not a
/// silent fall to [`ReviewVerdict::Unknown`]. Same argument, same shape, as
/// `RoomRole::parse_grantable`.
#[must_use]
pub fn parse_verdict(value: &str) -> Option<ReviewVerdict> {
    match value {
        "approve" | "approved" => Some(ReviewVerdict::Approved),
        "request-changes" | "changes-requested" => Some(ReviewVerdict::ChangesRequested),
        "reject" | "rejected" => Some(ReviewVerdict::Rejected),
        _ => None,
    }
}

/// Parse `/artifacts` (bare list) or one `/artifact` subcommand.
pub fn parse_artifact_command(
    bare_list: bool,
    cmd_arg: Option<&str>,
) -> Result<ArtifactCommandArgs, String> {
    let raw = cmd_arg.map(str::trim).unwrap_or_default();
    if bare_list {
        // `/artifacts` takes no arguments. Silently ignoring them would let a
        // typo'd `/artifacts review …` read as a successful no-op.
        if raw.is_empty() {
            return Ok(ArtifactCommandArgs::List);
        }
        return Err(format!("/artifacts takes no arguments. {USAGE}"));
    }
    let mut parts = raw.split_whitespace();
    match parts.next() {
        None => Err(format!("/artifact needs a subcommand. {USAGE}")),
        Some("show") => {
            let id = parts
                .next()
                .ok_or_else(|| format!("/artifact show needs an artifact id. {USAGE}"))?;
            if parts.next().is_some() {
                return Err(format!("/artifact show takes one id. {USAGE}"));
            }
            Ok(ArtifactCommandArgs::Show { id: id.to_owned() })
        }
        Some("apply") => {
            let id = parts
                .next()
                .ok_or_else(|| format!("/artifact apply needs an artifact id. {USAGE}"))?;
            if parts.next().is_some() {
                return Err(format!("/artifact apply takes one id. {USAGE}"));
            }
            Ok(ArtifactCommandArgs::Apply { id: id.to_owned() })
        }
        Some("review") => {
            let id = parts
                .next()
                .ok_or_else(|| format!("/artifact review needs an artifact id. {USAGE}"))?;
            let verdict = parts
                .next()
                .ok_or_else(|| format!("/artifact review needs a verdict. {USAGE}"))?;
            if parts.next().is_some() {
                return Err(format!(
                    "/artifact review takes one id and one verdict. {USAGE}"
                ));
            }
            let verdict = parse_verdict(verdict)
                .ok_or_else(|| format!("unknown verdict `{verdict}`. {USAGE}"))?;
            Ok(ArtifactCommandArgs::Review {
                id: id.to_owned(),
                verdict,
            })
        }
        Some(other) => Err(format!("unknown /artifact subcommand `{other}`. {USAGE}")),
    }
}

/// Why an operator-typed id could not be turned into a room artifact.
#[derive(Debug, PartialEq, Eq)]
pub enum ResolveError {
    /// No artifact in **this room's fold** carries that prefix.
    Unknown(String),
    /// More than one does. The candidates are named; picking the first would be
    /// a silent, unrecoverable mis-address.
    Ambiguous {
        typed: String,
        candidates: Vec<String>,
    },
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown(typed) => write!(
                formatter,
                "no artifact in this room's fold starts with `{typed}`. \
                 An artifact can exist in the store and still be absent here — \
                 recording a verdict against one would be journaled and then \
                 vanish from the projection. Run /artifacts to see what is folded."
            ),
            Self::Ambiguous { typed, candidates } => write!(
                formatter,
                "`{typed}` matches {} artifacts: {}. Type more characters.",
                candidates.len(),
                candidates.join(", ")
            ),
        }
    }
}

/// Resolve an operator-typed id prefix against **the room projection**.
///
/// 🔴 **The projection is authoritative for addressing, exactly as it is for
/// reading review state** (ruling P3). `PatchMergeBack::review` validates
/// against the artifact *store* only, and `OrchestrationRoom`'s `PatchReviewed`
/// fold is `if let Some(view) = self.artifacts.get_mut(&artifact)` — a **silent
/// no-op** for an artifact it has never seen. A raw-id front door therefore
/// lets an operator approve something that is in the store but not in this
/// room: `Ok(())`, a durable `PatchReviewed` line, and nothing visibly changes.
///
/// ⛔ Never `Ok` on no match. ⛔ Never pick the first of several.
pub fn resolve_artifact<'room>(
    room: &'room OrchestrationRoom,
    typed: &str,
) -> Result<&'room ArtifactRef, ResolveError> {
    let typed = typed.trim();
    // Every id starts with the empty string: without this guard an empty
    // prefix resolves a one-artifact room instead of refusing.
    if typed.is_empty() {
        return Err(ResolveError::Unknown(typed.to_owned()));
    }
    let matches: Vec<&ArtifactRef> = room
        .artifacts()
        .values()
        .filter(|artifact| artifact.id.as_str().starts_with(typed))
        .collect();
    match matches.len() {
        0 => Err(ResolveError::Unknown(typed.to_owned())),
        1 => Ok(matches[0]),
        _ => Err(ResolveError::Ambiguous {
            typed: typed.to_owned(),
            // Full ids, not the 12-char row prefix: two ids sharing >12 chars
            // would otherwise print as indistinguishable candidates, leaving
            // the operator no hint about which characters disambiguate.
            candidates: matches
                .iter()
                .map(|artifact| artifact.id.as_str().to_owned())
                .collect(),
        }),
    }
}

/// Render `/artifact show <id>` — the Drill-Down, not a new overlay.
///
/// ⛔ **Bodies are shown by handle plus a bounded preview, never inlined
/// wholesale.** Apply outcome wording comes only from the room projection; an
/// approved review is eligibility, never evidence of a workspace write.
#[must_use]
pub fn render_show(
    artifact: &ArtifactRef,
    room: &OrchestrationRoom,
    permission_mode: PermissionMode,
    policy: &MergeBackPolicy,
    body: Result<&[u8], String>,
) -> String {
    let mut out = format!(
        "{} {}\n  id        {}\n  producer  {}\n  host      {}\n  state     {}\n",
        kind_label(artifact.kind),
        id_prefix(&artifact.id),
        artifact.id.as_str(),
        artifact.producer.as_str(),
        artifact.host.host_id,
        review_state_label(artifact.review.as_ref()),
    );
    // ⛔ Never "the content hash of the artifact": for a patch the id is
    // namespaced by producer + authority, while `content_hash` is body-only.
    out.push_str(&format!("  body hash {}\n", artifact.content_hash));
    if artifact.depends_on.is_empty() {
        out.push_str("  lineage   none\n");
    } else {
        for dependency in &artifact.depends_on {
            out.push_str(&format!("  lineage   depends on {}\n", dependency.as_str()));
        }
    }
    if artifact.kind == ArtifactKind::InputRequest {
        if let Some(ticket) =
            crate::adapters::tui::widgets::artifacts_panel::ticket_state(room, &artifact.id)
        {
            out.push_str(&format!("  ticket    {ticket}\n"));
        }
        out.push_str(
            "  note      this is an inventory row. Answering an input request \
             arrives with the operator inbox.\n",
        );
    }
    if let Some(disposition) = operator_patch_decision(artifact, permission_mode, policy)
        .map(|decision| decision.disposition)
    {
        // A verdict this build cannot read folds as `Reviewed{Unknown}` and
        // lands fail-closed on `AwaitingReview` — but "no verdict has been
        // recorded" would contradict the state line above, which already
        // renders `verdict: unknown(<reviewer>)` for the same artifact.
        let decision = if disposition
            == crate::domain::services::patch_review::PatchDisposition::AwaitingReview
            && matches!(
                artifact.review,
                Some(crate::domain::models::ReviewStatus::Reviewed { .. })
            ) {
            "awaiting review — a verdict this build cannot read was recorded; \
             it is not an approval"
                .to_owned()
        } else {
            disposition_sentence(disposition)
        };
        out.push_str(&format!("  decision  {decision}\n"));
        out.push_str(&format!(
            "  apply     {}\n",
            apply_state_suffix(room, &artifact.id)
        ));
        // OPEN-DR-4 / `DF-18-3a-MERGEBACK-POLICY-VISIBILITY`: the effective
        // policy, sourced from the value the apply path uses.
        out.push_str(&format!(
            "  policy    auto_approve_user_originated = {}\n",
            policy.auto_approve_user_originated
        ));
        out.push_str(&format!(
            "  mode      permission mode = {permission_mode:?}\n"
        ));
    }
    match body {
        Ok(bytes) => {
            out.push_str(&format!(
                "  body      {} bytes (handle above)\n",
                bytes.len()
            ));
        }
        Err(error) => {
            out.push_str(&format!("  body      unavailable: {error}\n"));
        }
    }
    out
}

/// The full-sentence explanation of a disposition. Belongs in the drill-down;
/// the row carries only the terse ` · <reason>` clause.
#[must_use]
pub fn disposition_sentence(
    disposition: crate::domain::services::patch_review::PatchDisposition,
) -> String {
    use crate::domain::services::patch_review::PatchDisposition as D;
    match disposition {
        D::Applies => "eligible to apply after confirmation with `/artifact apply <id>`".to_owned(),
        D::AutoApplies => "auto-applies under the merge-back policy — a \
                           user-originated patch goes to `git apply` at fan-out \
                           completion without review; a failed apply leaves it \
                           pending, and this row does not change"
            .to_owned(),
        other => match decision_suffix(other) {
            Some(suffix) => format!("will not apply — {suffix}"),
            None => "awaiting review — no verdict has been recorded".to_owned(),
        },
    }
}

/// One-line operator confirmation after a verdict is journaled.
///
/// Approval changes eligibility only; applying remains a separate confirmed
/// command.
#[must_use]
pub fn render_verdict_recorded(artifact: &ArtifactId, verdict: ReviewVerdict) -> String {
    let label = match verdict {
        ReviewVerdict::Approved => "approved",
        ReviewVerdict::ChangesRequested => "changes requested",
        ReviewVerdict::Rejected => "rejected",
        _ => "unknown",
    };
    format!(
        "recorded {label} for {} in the room journal. \
         Nothing was applied yet — run `/artifact apply <id>` to preview and confirm the write.",
        id_prefix(artifact)
    )
}

/// Put one `/artifact` result in the transcript as a replaceable block.
pub(crate) fn show_artifact_message(state: &mut TuiState, message: String) {
    state.feedback_blocks.insert(
        ARTIFACT_BLOCK_ID.to_owned(),
        crate::domain::models::FeedbackBlock {
            id: ARTIFACT_BLOCK_ID.to_owned(),
            level: crate::domain::models::FeedbackLevel::Info,
            message,
            actions: Vec::new(),
        },
    );
    state.active_feedback_id = Some(ARTIFACT_BLOCK_ID.to_owned());
    state.needs_redraw = true;
}

/// Resolve the decision card and restore the focus that owned the command.
///
/// Decline consumes the card and returns `None`; accept returns the captured
/// card for the event-loop effect arm.
pub fn resolve_apply_card(
    state: &mut TuiState,
    accept: bool,
) -> Option<crate::adapters::tui::state::PendingApplyCard> {
    let card = state.pending_apply_card.take()?;
    state.focus = card.prior_focus.clone();
    state.needs_redraw = true;
    accept.then_some(card)
}

#[must_use]
pub fn render_apply_result(
    artifact: &ArtifactId,
    result: Result<(), PatchApplyPortError>,
) -> String {
    let prefix = id_prefix(artifact);
    match result {
        Ok(()) => format!("patch {prefix} applied to the workspace"),
        Err(PatchApplyPortError::WorkspaceBusy) => {
            format!("patch {prefix} was not applied: the workspace is busy")
        }
        Err(PatchApplyPortError::ApplyIndeterminate) => format!(
            "patch {prefix} was not applied: its prior outcome is indeterminate; \
             no resolution verb exists yet (18-3a-f)"
        ),
        Err(PatchApplyPortError::ApplyUnresolved(_)) => format!(
            "patch {prefix} may have been applied, but its outcome could not be \
             recorded — the workspace may have changed and the artifact is now \
             indeterminate. Inspect the working tree; no resolution verb exists \
             yet (18-3a-f)."
        ),
        Err(PatchApplyPortError::Conflict(message)) => {
            format!("patch {prefix} conflicted and did not mutate the workspace: {message}")
        }
        Err(PatchApplyPortError::Failed(message)) => {
            format!("patch {prefix} apply failed: {message}")
        }
    }
}
