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

/// Open the Transparency Log panel: read the journal, park the cursor on the
/// newest row, and size the sidebar.
///
/// Story 18.3 Task 0.5 pulled this out of the `InputAction::OpenPanel` arm.
/// Same reason as the rest of this module: `EVENT_LOOP_HARD_BUDGET` is a
/// ceiling, and Story 18.2 spent the last of its headroom (11_354 > 11_321).
pub(crate) async fn open_panel(app_state: &AppState, state: &mut TuiState) {
    refresh_panel(app_state, state).await;
    state
        .transparency_panel
        .open_at_tail(&mut state.sidebar_selected);
    state.sidebar_entry_count = state.transparency_panel.visible_len();
}

/// Export the snapshot the panel is currently showing, then report the outcome
/// as an operator notice.
///
/// Reads the report out of the panel rather than re-reading the journal, so the
/// exported bytes are exactly the snapshot on screen — the same anti-race
/// reasoning as `export_report`'s explicit report argument.
///
/// Story 18.3 Task 0.5 pulled this out of the `InputAction::ExportTransparency`
/// arm, which was 31 inline lines of the event loop's line budget.
pub(crate) async fn export_command(
    app_state: &AppState,
    state: &mut TuiState,
    conversation_id: &str,
) {
    let export = match state.transparency_panel.report.as_ref() {
        Some(report) => export_report(app_state, report).await,
        None => Err("open the Transparency Log before exporting its snapshot".to_owned()),
    };
    let (level, message) = match export {
        Ok(export) => (
            crate::domain::models::NoticeLevel::Warning,
            format!(
                "Transparency export written to {} ({} unfiltered rows).",
                export.path.display(),
                export.rows
            ),
        ),
        Err(error) => (
            crate::domain::models::NoticeLevel::Error,
            format!("Transparency export failed: {error}"),
        ),
    };
    let _ = app_state
        .event_bus
        .emit_domain(crate::domain::events::AppEvent::SystemNotice {
            conversation_id: Some(conversation_id.to_owned()),
            level,
            message,
        });
    state.needs_redraw = true;
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

/// One-call shell for the `/team` dispatch arm: parse, read, render, emit.
///
/// The parse and the rendering are pure and live in
/// `handlers::team_command`; this wrapper exists so the event-loop arm is two
/// lines instead of twenty — `EVENT_LOOP_HARD_BUDGET` is a ceiling, not a
/// budget, and Story 18.2 had 24 lines for all of Cluster B.
///
/// Emits its own notices rather than returning them (Story 18.3 Task 0.5): the
/// caller's `for … emit_domain` loop cost three lines of a budget that Story
/// 18.2 had already overrun. The pure `handlers::team_command` still returns
/// its events, so the behavioural tests are unaffected.
pub(crate) async fn team_command(
    state: &mut TuiState,
    conversation_id: &str,
    cmd_arg: Option<&str>,
    app_state: &AppState,
) {
    use crate::adapters::tui::handlers::team_command as handler;

    let args = match handler::parse_team_command(cmd_arg) {
        Ok(Some(args)) => args,
        // `parse_team_command` only returns `Ok(None)` for a non-`log`
        // invocation, which the grammar currently cannot produce.
        Ok(None) => return,
        Err(message) => {
            state.needs_redraw = true;
            let _ =
                app_state
                    .event_bus
                    .emit_domain(crate::domain::events::AppEvent::SystemNotice {
                        conversation_id: Some(conversation_id.to_owned()),
                        level: crate::domain::models::NoticeLevel::Warning,
                        message,
                    });
            return;
        }
    };
    let input = team_log_input(app_state, &args).await;
    for event in handler::team_command(state, conversation_id, &args, input) {
        let _ = app_state.event_bus.emit_domain(event);
    }
}
