//! `session` CLI subcommand family (Story 13.5a list; 13.5b delete).
//!
//! The shared pure decision-core lives in `rows.rs` so that `list` and future
//! `delete` both agree on ordering/filtering without depending on render code.

use clap::Subcommand;

pub mod list;
pub mod rows;

/// Session subcommand actions.
#[derive(Subcommand, Debug, Clone)]
pub enum SessionAction {
    /// List persisted sessions in the current workspace (Story 13.5a, FR125).
    /// Read-only, offline-safe, and non-billable.
    List {
        /// Machine-readable JSON output instead of human table.
        #[arg(long)]
        json: bool,
    },
}
