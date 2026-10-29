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
        #[arg(allow_hyphen_values = true)]
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

#[cfg(test)]
mod tests {
    use super::SessionAction;
    use crate::adapters::cli::commands::{Cli, Command};
    use clap::{Parser, error::ErrorKind};

    #[test]
    fn hyphen_session_ids_parse_without_losing_flags() {
        for args in [
            vec!["rustain", "--session", "-b123", "ask", "hello"],
            vec!["rustain", "--session=-b123", "ask", "hello"],
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert_eq!(cli.session.as_deref(), Some("-b123"));
            assert!(matches!(cli.command, Some(Command::Ask { .. })));
        }
        for args in [
            vec!["rustain", "session", "delete", "-b123", "--force"],
            vec!["rustain", "session", "delete", "--force", "-b123"],
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert!(matches!(
                cli.command,
                Some(Command::Session {
                    action: SessionAction::Delete { id: Some(id), force: true, .. }
                }) if id == "-b123"
            ));
        }
        let cli =
            Cli::try_parse_from(["rustain", "session", "delete", "--all", "--force"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Session {
                action: SessionAction::Delete {
                    id: None,
                    all: true,
                    force: true,
                    ..
                }
            })
        ));
        for args in [
            vec!["rustain", "--session", "-b123", "--unrelated"],
            vec!["rustain", "session", "delete", "-b123", "--unrelated"],
        ] {
            assert_eq!(
                Cli::try_parse_from(args).unwrap_err().kind(),
                ErrorKind::UnknownArgument
            );
        }
    }
}
