//! `session` CLI subcommand family (Story 13.5a list; 13.5b delete).
//!
//! The shared pure decision-core lives in `rows.rs` so that `list` and future
//! `delete` both agree on ordering/filtering without depending on render code.

use std::path::PathBuf;

use clap::Subcommand;

pub mod delete;
pub mod list;
pub mod rows;

/// Session subcommand actions.
#[derive(Subcommand, Debug, Clone)]
pub enum SessionAction {
    /// List persisted sessions in the current workspace, or across all
    /// registered workspaces with `--all` (Stories 13.5a / 13.5a-1, FR125).
    /// Read-only, offline-safe, and non-billable.
    List {
        /// Machine-readable JSON output instead of human table.
        #[arg(long)]
        json: bool,
        /// Include sessions from every registered workspace.
        #[arg(long)]
        all: bool,
    },
    /// Delete persisted sessions (Story 13.5b, FR125).
    ///
    /// This is an irreversible, scriptable destructive operation. The guard can
    /// only detect sessions held by a running daemon; open TUIs/foreground
    /// clients are not detectable. Close any session windows you care about
    /// before deleting.
    Delete {
        /// Session id or unique prefix to delete.
        id: Option<String>,
        /// Delete all sessions in the current workspace only.
        #[arg(long, conflicts_with_all = ["all_workspaces", "id"])]
        all: bool,
        /// Delete all sessions across every registered workspace.
        #[arg(long, conflicts_with_all = ["all", "id"])]
        all_workspaces: bool,
        /// Target a specific workspace (only valid with an explicit id).
        #[arg(long, requires = "id")]
        workspace: Option<PathBuf>,
        /// Skip interactive confirmation. Does NOT bypass the in-use guard for
        /// a session a responsive daemon reports as held, nor ambiguous/not-found
        /// resolution.
        #[arg(long)]
        force: bool,
        /// Show what would be deleted without changing any file.
        #[arg(long)]
        dry_run: bool,
        /// Machine-readable JSON output. Requires `--force` (or `--dry-run`) so it
        /// never interleaves an interactive prompt with the JSON envelope.
        #[arg(long)]
        json: bool,
    },
}
