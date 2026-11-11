//! The Artifacts tab's rows (Story 21.7 A6, A15): the durable artifact fold as a list tab.
//!
//! No second renderer: a row is the panel's own row (`artifacts_panel::artifact_row`) with its
//! safety-first narrow form (`row_forms`: the decision and the hazard survive truncation), and
//! the wide detail preview is the drill-down's facts (`artifact_command::render_facts`), the
//! lineage lines and the typed-verb reminder. The honest zero states, a read failure and the
//! journal head chrome ("here", "as of", unrecognised records, the input-request note) keep
//! their words: they are the list's empty text and its one muted note row.

use super::McRow;
use crate::adapters::tui::handlers::artifact_command::render_facts;
use crate::adapters::tui::state::{ArtifactsPanelState, ArtifactsZeroState};
use crate::adapters::tui::widgets::artifacts_panel::{
    VERB_HINT, id_prefix, input_request_note, kind_label, lineage_line, row_forms, zero_state_lines,
};
use crate::adapters::tui::widgets::room_panel::UNKNOWN_RECORD_LABEL;
use crate::domain::models::{ArtifactKind, ArtifactRef};
use crate::domain::services::transparency::{STRUCTURAL_REPLAY_CLAIM, format_unix_millis};

/// What the tab shows: the rows, the words for an empty list and the note above the rows.
pub struct ArtifactsList {
    pub rows: Vec<McRow>,
    pub empty: Option<String>,
    pub note: Option<String>,
}

/// The operator-facing name of an artifact: `<kind> <id prefix>` (window titles, notices).
#[must_use]
pub fn name(artifact: &ArtifactRef) -> String {
    format!("{} {}", kind_label(artifact.kind), id_prefix(&artifact.id))
}

/// The artifact window's frame title.
#[must_use]
pub fn window_title(artifact: &ArtifactRef) -> String {
    format!("Artifact · {}", name(artifact))
}

/// Build the tab from the fold the last read produced.
#[must_use]
pub fn list(panel: &ArtifactsPanelState) -> ArtifactsList {
    if let Some(error) = &panel.error {
        return ArtifactsList {
            rows: Vec::new(),
            empty: Some(format!(
                "⚠ could not read the room journal: {error} — this is a read failure, not an \
                 empty room."
            )),
            note: None,
        };
    }
    let zero = panel.zero_state();
    if zero == Some(ArtifactsZeroState::NotAttached) {
        let [headline, detail] = zero_state_lines(ArtifactsZeroState::NotAttached);
        return ArtifactsList {
            rows: Vec::new(),
            empty: Some(format!("{headline} {detail}")),
            note: None,
        };
    }
    let Some(room) = panel.room.as_ref() else {
        return ArtifactsList {
            rows: Vec::new(),
            empty: Some("Nothing read yet.".to_owned()),
            note: None,
        };
    };
    let (mode, policy) = (panel.permission_mode, panel.policy);
    let rows = room
        .artifacts()
        .values()
        .map(|artifact| {
            let (text, compact, keep) = row_forms(artifact, room, mode, &policy);
            let mut detail = render_facts(artifact, room, mode, &policy);
            for dependency in &artifact.depends_on {
                detail.push_str(&lineage_line(dependency));
                detail.push('\n');
            }
            detail.push('\n');
            detail.push_str(VERB_HINT);
            detail.push('\n');
            detail.push_str(STRUCTURAL_REPLAY_CLAIM);
            McRow::new(artifact.id.as_str(), &text, &detail).with_compact(compact, keep)
        })
        .collect::<Vec<_>>();
    if rows.is_empty() {
        let [headline, detail] = zero_state_lines(ArtifactsZeroState::NoArtifacts);
        return ArtifactsList {
            rows,
            empty: Some(format!("{headline} {detail}")),
            note: None,
        };
    }
    // "as of", never "live": the head chrome of the old panel, as one muted row.
    let mut note = format!("here: {}", panel.host_id);
    if let Some(ms) = panel.read_at_ms {
        note.push_str(&format!(" · as of {}", format_unix_millis(ms)));
    }
    if let Some(zero @ (ArtifactsZeroState::NoPatches | ArtifactsZeroState::AllReviewed)) = zero {
        note.push_str(&format!(" · {}", zero_state_lines(zero)[0]));
    }
    if panel.unknown_records > 0 {
        note.push_str(&format!(
            " · ? {} × {UNKNOWN_RECORD_LABEL}",
            panel.unknown_records
        ));
    }
    if room
        .artifacts()
        .values()
        .any(|artifact| artifact.kind == ArtifactKind::InputRequest)
    {
        note.push_str(&format!(" · {}", input_request_note()));
    }
    ArtifactsList {
        rows,
        empty: None,
        note: Some(note),
    }
}
