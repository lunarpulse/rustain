//! Effect shell between the event loop and the transparency read seam
//! (Story 18.2, AC5/AC6).
//!
//! Exists to keep `event_loop.rs` inside its line ratchet
//! (`EVENT_LOOP_HARD_BUDGET`) and to keep `adapters/tui/handlers/` free of
//! `crate::infrastructure::*` imports, which its module contract forbids. The
//! handlers receive data; this module performs the I/O.
//!
//! Deliberately **not** in `handlers/`: it performs journal reads and file
//! writes, and the handler contract is data-in/data-out.

use crate::adapters::tui::handlers::team_command::{TeamLogArgs, TeamLogInput};
use crate::adapters::tui::state::TuiState;
use crate::domain::services::transparency::{
    TransparencyExport, TransparencyFilter, TransparencyReport, TransparencyRow,
};
use crate::infrastructure::runtime::app_state::AppState;

/// Message shown when the workspace has no orchestration journal at all.
const NO_JOURNAL: &str =
    "this session has no orchestration journal (the subagent subsystem is not composed)";

/// Refresh the panel's rows from the durable journal.
///
/// Awaited inline at the dispatch site rather than spawned: the read happens
/// only when the operator opens or refreshes the panel, and a spawn would need
/// a new `AppEvent` round trip that the event-loop line budget cannot pay for.
/// `NodeJournal::load()` is O(whole file) with no tail read — if that ever
/// becomes a stall, it is the journal that needs an index, not this call that
/// needs a thread.
///
/// **Never live.** This is one consistent read under a shared `flock`; the
/// panel renders "as of <time>" and counts rows it has not shown.
pub(crate) async fn refresh_panel(app_state: &AppState, state: &mut TuiState) {
    let Some(service) = app_state.transparency.as_ref() else {
        state.transparency_panel.error = Some(NO_JOURNAL.to_owned());
        return;
    };
    match service.report().await {
        Ok(report) => {
            let now = chrono::Utc::now().timestamp_millis();
            state.transparency_panel.apply_report(report, now);
        }
        Err(error) => state.transparency_panel.error = Some(error.to_string()),
    }
    state.sidebar_entry_count = state.transparency_panel.visible_rows().len();
}

/// Write exactly one already-read transparency report to the regenerable
/// export. Keeping the report argument explicit prevents a panel export from
/// racing a fresh journal read and claiming a different snapshot.
pub(crate) async fn export_report(
    app_state: &AppState,
    report: &TransparencyReport,
) -> Result<TransparencyExport, String> {
    let service = app_state
        .transparency
        .as_ref()
        .ok_or_else(|| NO_JOURNAL.to_owned())?;
    service
        .export_report(report)
        .await
        .map_err(|error| error.to_string())
}

/// Gather everything `/team log` needs: the filtered rows, the divergence
/// report, and the export result when `--export` was asked for.
pub(crate) async fn team_log_input(app_state: &AppState, args: &TeamLogArgs) -> TeamLogInput {
    let Some(service) = app_state.transparency.as_ref() else {
        return TeamLogInput {
            rows: Err(NO_JOURNAL.to_owned()),
            divergence: None,
            export: None,
        };
    };
    let report = match service.report().await {
        Ok(report) => report,
        Err(error) => {
            return TeamLogInput {
                rows: Err(error.to_string()),
                divergence: None,
                export: None,
            };
        }
    };
    let divergence = report.structural_divergence_report();
    let rows = match filter_rows(&report.rows, args.filter.as_deref()) {
        Ok(rows) => rows,
        Err(error) => {
            return TeamLogInput {
                rows: Err(error),
                divergence,
                export: None,
            };
        }
    };
    // The export renders the WHOLE supplied report, never the filtered view:
    // a filtered export would be a different file every time and would stop
    // being byte-identically regenerable.
    let export = if args.export {
        Some(export_report(app_state, &report).await)
    } else {
        None
    };
    TeamLogInput {
        rows: Ok(rows),
        divergence,
        export,
    }
}

fn filter_rows(
    rows: &[TransparencyRow],
    spec: Option<&str>,
) -> Result<Vec<TransparencyRow>, String> {
    let Some(spec) = spec else {
        return Ok(rows.to_vec());
    };
    let filter = TransparencyFilter::parse(spec)?;
    Ok(rows
        .iter()
        .filter(|row| filter.matches(row))
        .cloned()
        .collect())
}

/// One-call shell for the `/team` dispatch arm: parse, read, render.
///
/// The parse and the rendering are pure and live in
/// `handlers::team_command`; this wrapper exists so the event-loop arm is six
/// lines instead of twenty — `EVENT_LOOP_HARD_BUDGET` is a ceiling, not a
/// budget, and Story 18.2 had 24 lines for all of Cluster B.
pub(crate) async fn team_command(
    state: &mut TuiState,
    conversation_id: &str,
    cmd_arg: Option<&str>,
    app_state: &AppState,
) -> Vec<crate::domain::events::AppEvent> {
    use crate::adapters::tui::handlers::team_command as handler;

    let args = match handler::parse_team_command(cmd_arg) {
        Ok(Some(args)) => args,
        // `parse_team_command` only returns `Ok(None)` for a non-`log`
        // invocation, which the grammar currently cannot produce.
        Ok(None) => return Vec::new(),
        Err(message) => {
            state.needs_redraw = true;
            return vec![crate::domain::events::AppEvent::SystemNotice {
                conversation_id: Some(conversation_id.to_owned()),
                level: crate::domain::models::NoticeLevel::Warning,
                message,
            }];
        }
    };
    let input = team_log_input(app_state, &args).await;
    handler::team_command(state, conversation_id, &args, input)
}
