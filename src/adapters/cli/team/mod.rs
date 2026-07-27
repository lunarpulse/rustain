//! `team` CLI subcommand family (Story 18.2, AC6 / FR95).
//!
//! `rustain team log` is the headless face of the `Ctrl+X, L` panel and
//! `/team log`. All three render the output of **one** fold
//! ([`crate::domain::services::transparency::fold_transparency`]); the rows
//! are constructed in `rows.rs`, which delegates to the same domain core the
//! TUI uses. Divergence between the faces is the defect this shape prevents,
//! and `tests/conformance_transparency.rs` asserts they agree.

use clap::Subcommand;

pub mod log;
pub mod rows;

/// `team` subcommand actions. Declared beside its handlers, mirroring
/// `cli/session/mod.rs`.
#[derive(Subcommand, Debug, Clone)]
pub enum TeamAction {
    /// Show the A2A transparency log for this workspace (FR95, NFR67).
    ///
    /// Read-only, offline-safe, non-billable: it folds the durable room
    /// journal and prints the result. It never reads
    /// `.rustain/transparency.jsonl` — that file is a regenerable export of
    /// this same fold, not a source of truth.
    Log {
        /// Filter rows. Grammar: `direction=inbound|outbound|unknown`,
        /// `kind=accepted|refused|awaiting-approval|status-query|unknown`,
        /// `peer=<substring>`, or a bare substring. Terms are ANDed.
        #[arg(long)]
        filter: Option<String>,
        /// Machine-readable JSON output — one object per line.
        #[arg(long)]
        json: bool,
        /// Also (re)generate `{workspace}/.rustain/transparency.jsonl`.
        /// Rendered whole from the journal, never appended.
        #[arg(long)]
        export: bool,
    },
}
