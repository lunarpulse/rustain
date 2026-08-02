//! `rustain team log` handler (Story 18.2, AC6 / AC8).
//!
//! Prints the same rows the `Ctrl+X, L` panel and `/team log` render.
//!
//! # This is the escape-injection sink
//!
//! Every prior consumer of a transparency summary was ratatui, which writes
//! chars into a cell buffer — an escape sequence becomes literal glyphs, ugly
//! and harmless. This function is a `println!` to a real terminal, and the
//! text it prints is partly chosen by a remote peer. That is why
//! `transparency_row` strips C0/C1 on the **read** path as well as on write:
//! records journaled by Story 18.1b are already on disk unsanitized, and a
//! write-only fix would leave them hot.

use std::io::Write;

use anyhow::Result;
use serde::Serialize;

use crate::adapters::cli::team::rows::{TRANSPARENCY_LOG_SCHEMA_VERSION, build_transparency_rows};
use crate::domain::services::transparency::{
    ATTRIBUTION_CAVEAT, STRUCTURAL_REPLAY_CLAIM, TransparencyExport, TransparencyFilter,
    TransparencyReport, TransparencyRow,
};

#[derive(Serialize)]
struct JsonEnvelope<'a> {
    schema_version: u32,
    structural_replay: &'a str,
    attribution: &'a str,
    structurally_consistent: bool,
    divergence: Option<String>,
    export_path: Option<String>,
    export_rows: Option<usize>,
    rows: Vec<JsonRow<'a>>,
}

#[derive(Serialize)]
struct JsonRow<'a> {
    seq: u64,
    recorded_at_ms: Option<i64>,
    direction: &'a str,
    kind: &'a str,
    peer: &'a str,
    task: Option<&'a str>,
    summary: &'a str,
}

/// Render one previously-read transparency report for `rustain team log`.
///
/// The CLI adapter only renders domain data. Startup owns opening the
/// read-only journal, creating the concrete transparency service, and (when
/// requested) writing an export from this exact report snapshot.
///
/// # Errors
///
/// Returns `Err` for filter-grammar, output-write, and JSON serialization
/// failures. It performs no journal or filesystem reads and writes no export.
pub fn render_team_log(
    filter: Option<&str>,
    json: bool,
    report: &TransparencyReport,
    export: Option<&TransparencyExport>,
    out: &mut impl Write,
) -> Result<()> {
    let rows = build_transparency_rows_from_report(&report.rows, filter)?;
    let export_path = export.map(|summary| summary.path.display().to_string());
    let export_rows = export.map(|summary| summary.rows);
    let divergence = report.structural_divergence_report();

    if json {
        let envelope = JsonEnvelope {
            schema_version: TRANSPARENCY_LOG_SCHEMA_VERSION,
            structural_replay: STRUCTURAL_REPLAY_CLAIM,
            attribution: ATTRIBUTION_CAVEAT,
            structurally_consistent: report.is_structurally_consistent(),
            divergence,
            export_path,
            export_rows,
            rows: rows
                .iter()
                .map(|row| JsonRow {
                    seq: row.seq,
                    recorded_at_ms: row.recorded_at_ms,
                    direction: row.direction.label(),
                    kind: row.kind.label(),
                    peer: &row.peer,
                    task: row.task.as_deref(),
                    summary: &row.summary,
                })
                .collect(),
        };
        writeln!(out, "{}", serde_json::to_string_pretty(&envelope)?)?;
        return Ok(());
    }

    if let Some(divergence) = &divergence {
        // Structural divergence is loud before the rows; the report remains
        // useful, but correct code cannot replay the source stream as-is.
        writeln!(out, "WARNING: {divergence}")?;
    }
    if rows.is_empty() {
        writeln!(
            out,
            "· no A2A interactions recorded (or none match this filter)."
        )?;
    } else {
        for row in &rows {
            writeln!(out, "{} {}", row.timestamp_label(), row.one_line())?;
        }
    }
    if let Some(summary) = export {
        writeln!(
            out,
            "export: {} ({} rows)",
            summary.path.display(),
            summary.rows
        )?;
    }
    writeln!(out, "— {STRUCTURAL_REPLAY_CLAIM}")?;
    writeln!(out, "— {ATTRIBUTION_CAVEAT}")?;
    Ok(())
}

fn build_transparency_rows_from_report(
    rows: &[TransparencyRow],
    filter: Option<&str>,
) -> Result<Vec<TransparencyRow>> {
    // Re-uses the shared filter over already-folded rows; `build_transparency_rows`
    // is the entry point when the caller holds entries instead.
    match filter {
        None => Ok(rows.to_vec()),
        Some(spec) => {
            let parsed =
                TransparencyFilter::parse(spec).map_err(|message| anyhow::anyhow!(message))?;
            Ok(rows
                .iter()
                .filter(|row| parsed.matches(row))
                .cloned()
                .collect())
        }
    }
}

/// Kept public so the conformance suite can assert the CLI and the shared
/// fold agree without duplicating the entry-point plumbing.
pub fn rows_from_entries(
    entries: &[crate::domain::models::JournalEntry],
    filter: Option<&str>,
) -> Result<Vec<TransparencyRow>> {
    build_transparency_rows(entries, filter).map_err(|message| anyhow::anyhow!(message))
}
