//! `session delete` handler (Story 13.5b).
//!
//! Implements three mutually-exclusive target axes:
//!
//! - `delete <id> [--workspace P]` — single session by exact id or unique prefix.
//! - `delete --all` — all sessions in the current workspace only.
//! - `delete --all-workspaces` — all sessions across every registered, live workspace.
//!
//! All destructive paths are blocked by an in-use guard (`SessionHolderPort`)
//! and require interactive confirmation unless `--force` is passed. `--dry-run`
//! prints the verdict without touching the filesystem.

use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::Serialize;

use super::list::sanitize_title;
use crate::adapters::cli::session::rows::{build_all_workspace_rows, build_session_rows};
use crate::adapters::cli::util::{Confirm, prompt_typed_count, prompt_yes_no, truncate};
use crate::domain::models::ConversationSummary;
use crate::domain::ports::{
    HeldSession, HolderState, SessionHolderPort, StoragePort, WorkspaceRegistryReaderPort,
};
use crate::infrastructure::paths;
use crate::infrastructure::startup::SubcommandExit;
use crate::infrastructure::utils::sanitize_id;

/// Schema version for `rustain session delete --json`.
pub const SESSION_DELETE_SCHEMA_VERSION: &str = "1.0";

// ---------------------------------------------------------------------------
// Exit codes (Story 13.5b — distinct non-zero for scriptable destructive op).
// ---------------------------------------------------------------------------

const EXIT_NOT_FOUND: i32 = 2;
const EXIT_AMBIGUOUS: i32 = 3;
const EXIT_IN_USE: i32 = 4;
const EXIT_UNVERIFIED: i32 = 5;
const EXIT_NEEDS_CONFIRMATION: i32 = 6;
const EXIT_PATH_ESCAPE: i32 = 7;
const EXIT_STORAGE_ERROR: i32 = 8;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Run `rustain session delete`.
///
/// `storage_for` receives the target workspace path and returns a storage
/// adapter rooted at that workspace. `holder` answers whether a session is
/// currently held by a daemon in the workspace. `reader` enumerates registered
/// workspaces for cross-workspace bulk and for the "found elsewhere" hint.
///
/// # Errors
///
/// Returns `Ok(())` on success or on a refusal that has already been rendered.
/// Returns `Err(SubcommandExit(code).into())` for distinct non-zero exits.
#[allow(clippy::too_many_arguments)]
pub async fn run_session_delete(
    id: Option<String>,
    all: bool,
    all_workspaces: bool,
    workspace: Option<PathBuf>,
    force: bool,
    dry_run: bool,
    json: bool,
    current_workspace: &Path,
    storage_for: impl Fn(&Path) -> Arc<dyn StoragePort>,
    holder: &dyn SessionHolderPort,
    reader: &dyn WorkspaceRegistryReaderPort,
    is_tty: bool,
    inp: &mut dyn BufRead,
    out: &mut dyn Write,
) -> Result<()> {
    let modes = [id.is_some(), all, all_workspaces];
    let mode_count = modes.iter().filter(|b| **b).count();
    if mode_count != 1 {
        // Defensive: clap conflicts already enforce exactly-one-mode at the CLI;
        // direct API callers get a clear non-zero instead of a not-found code.
        writeln!(
            out,
            "Usage error: specify exactly one of <id>, --all, or --all-workspaces."
        )?;
        return Err(SubcommandExit(SubcommandExit::GENERIC).into());
    }
    if workspace.is_some() && id.is_none() {
        // Defensive: clap `requires = "id"` already blocks this at the CLI;
        // direct API callers get a clear non-zero instead of a not-found code.
        writeln!(out, "Usage error: --workspace requires a session id.")?;
        return Err(SubcommandExit(SubcommandExit::GENERIC).into());
    }

    // `--json` is non-interactive: never interleave a yes/no prompt with the
    // JSON envelope. Require --force, or --dry-run (which needs no prompt).
    if json && !force && !dry_run {
        writeln!(
            out,
            "`--json` requires `--force` (or `--dry-run` for a non-destructive preview)."
        )?;
        return Err(SubcommandExit(EXIT_NEEDS_CONFIRMATION).into());
    }

    let current_workspace = canonical_workspace_path(current_workspace);

    if all {
        return delete_all_current_workspace(
            dry_run,
            force,
            json,
            &current_workspace,
            storage_for,
            holder,
            is_tty,
            inp,
            out,
        )
        .await;
    }

    if all_workspaces {
        return delete_all_workspaces(
            dry_run,
            force,
            json,
            &current_workspace,
            storage_for,
            holder,
            reader,
            is_tty,
            inp,
            out,
        )
        .await;
    }

    // Single id/workspace path.
    let id_arg = id.expect("mode_count == 1 guarantees id");
    delete_single(
        &id_arg,
        workspace.as_deref(),
        dry_run,
        force,
        json,
        &current_workspace,
        storage_for,
        holder,
        reader,
        is_tty,
        inp,
        out,
    )
    .await
}

// ---------------------------------------------------------------------------
// Single target
// ---------------------------------------------------------------------------

async fn delete_single(
    id_arg: &str,
    workspace_arg: Option<&Path>,
    dry_run: bool,
    force: bool,
    json: bool,
    current_workspace: &Path,
    storage_for: impl Fn(&Path) -> Arc<dyn StoragePort>,
    holder: &dyn SessionHolderPort,
    reader: &dyn WorkspaceRegistryReaderPort,
    is_tty: bool,
    inp: &mut dyn BufRead,
    out: &mut dyn Write,
) -> Result<()> {
    let target_workspace = workspace_arg
        .map(canonical_workspace_path)
        .unwrap_or_else(|| current_workspace.to_path_buf());

    let storage = storage_for(&target_workspace);
    let sessions_dir = paths::sessions_dir(&target_workspace);

    let summaries = storage
        .list_conversations_read_only()
        .await
        .map_err(|e| storage_error(e, "listing sessions"))?;

    let resolved = match resolve_id(id_arg, &summaries) {
        Resolution::Exact(id) | Resolution::UniquePrefix(id) => id,
        Resolution::Ambiguous(candidates) => {
            if json {
                let out_json = SessionDeleteJson {
                    schema_version: SESSION_DELETE_SCHEMA_VERSION,
                    dry_run,
                    deleted: vec![],
                    refused: vec![RefusedRow {
                        id: id_arg.to_string(),
                        title: "".to_string(),
                        workspace: target_workspace.to_string_lossy().to_string(),
                        reason: "ambiguous".to_string(),
                        holder: None,
                    }],
                };
                writeln!(out, "{}", serde_json::to_string_pretty(&out_json)?)?;
            } else {
                writeln!(
                    out,
                    "Ambiguous prefix — {} sessions match '{}'",
                    candidates.len(),
                    id_arg
                )?;
                for c in candidates {
                    writeln!(out, "  {}  \"{}\"", c.id, sanitize_title(&c.title))?;
                }
            }
            return Err(SubcommandExit(EXIT_AMBIGUOUS).into());
        }
        Resolution::NotFound => {
            // Read-only cross-workspace hint.
            let hint =
                find_in_other_workspaces(id_arg, &target_workspace, reader, &storage_for).await;
            if json {
                let refused = vec![RefusedRow {
                    id: id_arg.to_string(),
                    title: "".to_string(),
                    workspace: target_workspace.to_string_lossy().to_string(),
                    reason: "not_found".to_string(),
                    holder: None,
                }];
                let out_json = SessionDeleteJson {
                    schema_version: SESSION_DELETE_SCHEMA_VERSION,
                    dry_run,
                    deleted: vec![],
                    refused,
                };
                writeln!(out, "{}", serde_json::to_string_pretty(&out_json)?)?;
            } else {
                writeln!(
                    out,
                    "No session matching '{}' in {}.",
                    id_arg,
                    abbreviate_home(&target_workspace.to_string_lossy())
                )?;
                if let Some((ws, _title)) = hint {
                    writeln!(
                        out,
                        "Found a match in {} — retry with `--workspace {}`.",
                        abbreviate_home(&ws.to_string_lossy()),
                        ws.display()
                    )?;
                }
            }
            return Err(SubcommandExit(EXIT_NOT_FOUND).into());
        }
    };

    let summary = summaries
        .into_iter()
        .find(|s| s.id == resolved)
        .expect("resolved id came from summaries");

    let target = DeleteTarget {
        workspace: target_workspace.clone(),
        sessions_dir: sessions_dir.clone(),
        id: summary.id.clone(),
        title: summary.title.clone(),
        message_count: summary.message_count,
        updated_at: summary.updated_at,
    };

    // In-use guard.
    match holder.live_holder(&target_workspace).await {
        HolderState::HeldBy(h) if h.conversation_id == target.id => {
            render_refusal(
                &[DeleteResult::Refused(target, RefuseReason::InUse(h))],
                dry_run,
                json,
                out,
            )?;
            return Err(SubcommandExit(EXIT_IN_USE).into());
        }
        HolderState::Unknown if !force => {
            render_refusal(
                &[DeleteResult::Refused(target, RefuseReason::Unverified)],
                dry_run,
                json,
                out,
            )?;
            return Err(SubcommandExit(EXIT_UNVERIFIED).into());
        }
        _ => {}
    }

    if let Err(_e) = assert_path_confined(&sessions_dir, &target.id).await {
        if json {
            let refused = vec![RefusedRow {
                id: target.id.clone(),
                title: sanitize_title(&target.title),
                workspace: target.workspace.to_string_lossy().to_string(),
                reason: "path_escape".to_string(),
                holder: None,
            }];
            let out_json = SessionDeleteJson {
                schema_version: SESSION_DELETE_SCHEMA_VERSION,
                dry_run,
                deleted: vec![],
                refused,
            };
            writeln!(out, "{}", serde_json::to_string_pretty(&out_json)?)?;
        } else {
            writeln!(
                out,
                "Refused — session path escapes the workspace sessions directory."
            )?;
        }
        return Err(SubcommandExit(EXIT_PATH_ESCAPE).into());
    }

    // Confirmation / dry-run.
    if !force && !dry_run {
        if !is_tty {
            if json {
                let refused = vec![RefusedRow {
                    id: target.id.clone(),
                    title: sanitize_title(&target.title),
                    workspace: target.workspace.to_string_lossy().to_string(),
                    reason: "needs_confirmation".to_string(),
                    holder: None,
                }];
                let out_json = SessionDeleteJson {
                    schema_version: SESSION_DELETE_SCHEMA_VERSION,
                    dry_run: false,
                    deleted: vec![],
                    refused,
                };
                writeln!(out, "{}", serde_json::to_string_pretty(&out_json)?)?;
            } else {
                writeln!(
                    out,
                    "This operation needs confirmation; pass --force to proceed without a TTY."
                )?;
            }
            return Err(SubcommandExit(EXIT_NEEDS_CONFIRMATION).into());
        }

        let show_workspace = target.workspace != *current_workspace;
        let prompt = format_single_prompt(
            &target,
            show_workspace,
            &abbreviate_home(&target.workspace.to_string_lossy()),
        );
        writeln!(out, "{}", prompt)?;
        if !prompt_yes_no("Delete this session?", Confirm::No, inp, out)? {
            if json {
                let out_json = SessionDeleteJson {
                    schema_version: SESSION_DELETE_SCHEMA_VERSION,
                    dry_run: false,
                    deleted: vec![],
                    refused: vec![],
                };
                writeln!(out, "{}", serde_json::to_string_pretty(&out_json)?)?;
            } else {
                writeln!(out, "Cancelled. Nothing was deleted.")?;
            }
            return Ok(());
        }
    }

    if dry_run {
        if json {
            let out_json = SessionDeleteJson {
                schema_version: SESSION_DELETE_SCHEMA_VERSION,
                dry_run: true,
                deleted: vec![target.to_deleted_row()],
                refused: vec![],
            };
            writeln!(out, "{}", serde_json::to_string_pretty(&out_json)?)?;
        } else {
            writeln!(
                out,
                "Would delete 1 session — {}  \"{}\"",
                target.id,
                sanitize_title(&target.title)
            )?;
        }
        return Ok(());
    }

    storage
        .delete_conversation(&target.id)
        .await
        .map_err(|e| storage_error(e, "deleting session"))?;

    if json {
        let out_json = SessionDeleteJson {
            schema_version: SESSION_DELETE_SCHEMA_VERSION,
            dry_run: false,
            deleted: vec![target.to_deleted_row()],
            refused: vec![],
        };
        writeln!(out, "{}", serde_json::to_string_pretty(&out_json)?)?;
    } else {
        writeln!(
            out,
            "Deleted {}  \"{}\".",
            target.id,
            sanitize_title(&target.title)
        )?;
    }

    tracing::info!(subcommand = "session-delete", deleted = 1, refused = 0);
    Ok(())
}

// ---------------------------------------------------------------------------
// `--all` current workspace
// ---------------------------------------------------------------------------

async fn delete_all_current_workspace(
    dry_run: bool,
    force: bool,
    json: bool,
    current_workspace: &Path,
    storage_for: impl Fn(&Path) -> Arc<dyn StoragePort>,
    holder: &dyn SessionHolderPort,
    is_tty: bool,
    inp: &mut dyn BufRead,
    out: &mut dyn Write,
) -> Result<()> {
    let storage = storage_for(current_workspace);
    let sessions_dir = paths::sessions_dir(current_workspace);

    let summaries = storage
        .list_conversations_read_only()
        .await
        .map_err(|e| storage_error(e, "listing sessions"))?;
    let rows = build_session_rows(summaries);

    if rows.is_empty() {
        if !json {
            writeln!(out, "No sessions in this workspace. Nothing to delete.")?;
        } else {
            let out_json = SessionDeleteJson {
                schema_version: SESSION_DELETE_SCHEMA_VERSION,
                dry_run,
                deleted: vec![],
                refused: vec![],
            };
            writeln!(out, "{}", serde_json::to_string_pretty(&out_json)?)?;
        }
        return Ok(());
    }

    let targets: Vec<DeleteTarget> = rows
        .into_iter()
        .map(|row| DeleteTarget {
            workspace: current_workspace.to_path_buf(),
            sessions_dir: sessions_dir.clone(),
            id: row.id,
            title: row.title,
            message_count: row.message_count,
            updated_at: row.updated_at,
        })
        .collect();

    if !force && !dry_run {
        if !is_tty {
            if json {
                let out_json = SessionDeleteJson {
                    schema_version: SESSION_DELETE_SCHEMA_VERSION,
                    dry_run: false,
                    deleted: vec![],
                    refused: targets
                        .iter()
                        .map(|t| RefusedRow {
                            id: t.id.clone(),
                            title: sanitize_title(&t.title),
                            workspace: t.workspace.to_string_lossy().to_string(),
                            reason: "needs_confirmation".to_string(),
                            holder: None,
                        })
                        .collect(),
                };
                writeln!(out, "{}", serde_json::to_string_pretty(&out_json)?)?;
            } else {
                writeln!(
                    out,
                    "This operation needs confirmation; pass --force to proceed without a TTY."
                )?;
            }
            return Err(SubcommandExit(EXIT_NEEDS_CONFIRMATION).into());
        }

        let count = targets.len();
        let confirmed = if count == 1 {
            writeln!(out, "{}", format_single_prompt(&targets[0], false, ""))?;
            prompt_yes_no("Delete this session?", Confirm::No, inp, out)?
        } else {
            writeln!(
                out,
                "About to delete ALL {} sessions in THIS workspace — this cannot be undone.",
                count
            )?;
            writeln!(out, "Workspace: {}", current_workspace.display())?;
            writeln!(out)?;
            render_preview_list(out, &targets, 5)?;
            writeln!(
                out,
                "\nOnly this workspace is affected. Other workspaces are untouched (use --all-workspaces)."
            )?;
            writeln!(
                out,
                "Heads up: open TUIs can't be detected — close any session windows you care about first."
            )?;
            writeln!(out)?;
            prompt_typed_count(
                count,
                &format!(
                    "To confirm, type the number of sessions to delete ({}):\n> ",
                    count
                ),
                inp,
                out,
            )?
        };
        if !confirmed {
            if json {
                let out_json = SessionDeleteJson {
                    schema_version: SESSION_DELETE_SCHEMA_VERSION,
                    dry_run: false,
                    deleted: vec![],
                    refused: vec![],
                };
                writeln!(out, "{}", serde_json::to_string_pretty(&out_json)?)?;
            } else {
                writeln!(out, "Cancelled. Nothing was deleted.")?;
            }
            return Ok(());
        }
    }

    execute_bulk(targets, dry_run, force, json, &storage_for, holder, out).await
}

// ---------------------------------------------------------------------------
// `--all-workspaces` cross-workspace bulk
// ---------------------------------------------------------------------------

async fn delete_all_workspaces(
    dry_run: bool,
    force: bool,
    json: bool,
    current_workspace: &Path,
    storage_for: impl Fn(&Path) -> Arc<dyn StoragePort>,
    holder: &dyn SessionHolderPort,
    reader: &dyn WorkspaceRegistryReaderPort,
    is_tty: bool,
    inp: &mut dyn BufRead,
    out: &mut dyn Write,
) -> Result<()> {
    let workspaces = match reader.live_workspaces().await {
        Ok(ws) => ws,
        Err(e) => {
            // Surface the scope reduction: --all-workspaces silently falling
            // back to current-workspace-only on a registry error is dangerous.
            writeln!(
                out,
                "Warning: workspace registry unavailable; --all-workspaces is limited to the current workspace."
            )?;
            tracing::warn!(
                "workspace registry unavailable during session delete --all-workspaces: {e}"
            );
            vec![]
        }
    };

    let mut per_workspace: Vec<(PathBuf, Vec<ConversationSummary>)> = vec![];
    // Always include current workspace first.
    let current_storage = storage_for(current_workspace);
    match current_storage.list_conversations_read_only().await {
        Ok(s) => per_workspace.push((current_workspace.to_path_buf(), s)),
        Err(e) => return Err(storage_error(e, "listing current workspace")),
    }

    for entry in workspaces {
        let normalized = canonical_workspace_path(&entry.path);
        if normalized == *current_workspace {
            continue;
        }
        let storage = storage_for(&normalized);
        match storage.list_conversations_read_only().await {
            Ok(s) => per_workspace.push((normalized, s)),
            Err(e) => {
                tracing::warn!(workspace = %entry.path.display(), "skipping workspace during delete --all-workspaces: {e}");
            }
        }
    }

    let current_ws_str = current_workspace.to_string_lossy().to_string();
    let global_rows = build_all_workspace_rows(
        &current_ws_str,
        per_workspace
            .iter()
            .map(|(p, s)| (p.to_string_lossy().to_string(), s.clone()))
            .collect(),
    );

    if global_rows.is_empty() {
        if !json {
            writeln!(
                out,
                "No sessions found across registered workspaces. Nothing to delete."
            )?;
        } else {
            let out_json = SessionDeleteJson {
                schema_version: SESSION_DELETE_SCHEMA_VERSION,
                dry_run,
                deleted: vec![],
                refused: vec![],
            };
            writeln!(out, "{}", serde_json::to_string_pretty(&out_json)?)?;
        }
        return Ok(());
    }

    let mut targets = Vec::with_capacity(global_rows.len());
    for ws_row in global_rows {
        let sessions_dir = paths::sessions_dir(Path::new(&ws_row.workspace));
        targets.push(DeleteTarget {
            workspace: PathBuf::from(&ws_row.workspace),
            sessions_dir,
            id: ws_row.row.id,
            title: ws_row.row.title,
            message_count: ws_row.row.message_count,
            updated_at: ws_row.row.updated_at,
        });
    }

    if !force && !dry_run {
        if !is_tty {
            if json {
                let out_json = SessionDeleteJson {
                    schema_version: SESSION_DELETE_SCHEMA_VERSION,
                    dry_run: false,
                    deleted: vec![],
                    refused: targets
                        .iter()
                        .map(|t| RefusedRow {
                            id: t.id.clone(),
                            title: sanitize_title(&t.title),
                            workspace: t.workspace.to_string_lossy().to_string(),
                            reason: "needs_confirmation".to_string(),
                            holder: None,
                        })
                        .collect(),
                };
                writeln!(out, "{}", serde_json::to_string_pretty(&out_json)?)?;
            } else {
                writeln!(
                    out,
                    "This operation needs confirmation; pass --force to proceed without a TTY."
                )?;
            }
            return Err(SubcommandExit(EXIT_NEEDS_CONFIRMATION).into());
        }

        let total = targets.len();
        let workspace_count = targets
            .iter()
            .map(|t| &t.workspace)
            .collect::<std::collections::HashSet<_>>()
            .len();
        writeln!(
            out,
            "About to delete ALL {} sessions across {} workspaces — this cannot be undone.",
            total, workspace_count
        )?;
        writeln!(out, "This spans EVERY registered workspace:")?;
        render_workspace_summary(out, &targets)?;
        writeln!(
            out,
            "\nHeads up: open TUIs can't be detected — close any session windows you care about first."
        )?;
        writeln!(out)?;
        let confirmed = prompt_typed_count(
            total,
            &format!(
                "To confirm, type the total number of sessions to delete ({}):\n> ",
                total
            ),
            inp,
            out,
        )?;
        if !confirmed {
            if json {
                let out_json = SessionDeleteJson {
                    schema_version: SESSION_DELETE_SCHEMA_VERSION,
                    dry_run: false,
                    deleted: vec![],
                    refused: vec![],
                };
                writeln!(out, "{}", serde_json::to_string_pretty(&out_json)?)?;
            } else {
                writeln!(out, "Cancelled. Nothing was deleted.")?;
            }
            return Ok(());
        }
    }

    execute_bulk(targets, dry_run, force, json, &storage_for, holder, out).await
}

// ---------------------------------------------------------------------------
// Bulk execution (shared by --all and --all-workspaces)
// ---------------------------------------------------------------------------

async fn execute_bulk(
    targets: Vec<DeleteTarget>,
    dry_run: bool,
    force: bool,
    json: bool,
    storage_for: &impl Fn(&Path) -> Arc<dyn StoragePort>,
    holder: &dyn SessionHolderPort,
    out: &mut dyn Write,
) -> Result<()> {
    let mut results = Vec::with_capacity(targets.len());

    for target in targets {
        match holder.live_holder(&target.workspace).await {
            HolderState::HeldBy(h) if h.conversation_id == target.id => {
                results.push(DeleteResult::Refused(target, RefuseReason::InUse(h)));
                continue;
            }
            HolderState::Unknown if !force => {
                results.push(DeleteResult::Refused(target, RefuseReason::Unverified));
                continue;
            }
            _ => {}
        }

        if let Err(_e) = assert_path_confined(&target.sessions_dir, &target.id).await {
            results.push(DeleteResult::Refused(target, RefuseReason::PathEscape));
            continue;
        }

        if dry_run {
            results.push(DeleteResult::WouldDelete(target));
        } else {
            let storage = storage_for(&target.workspace);
            match storage.delete_conversation(&target.id).await {
                Ok(()) => results.push(DeleteResult::Deleted(target)),
                Err(e) => results.push(DeleteResult::Refused(
                    target,
                    RefuseReason::StorageError(e.to_string()),
                )),
            }
        }
    }

    let deleted: Vec<_> = results
        .iter()
        .filter(|r| matches!(r, DeleteResult::Deleted(_) | DeleteResult::WouldDelete(_)))
        .collect();
    let refused: Vec<_> = results
        .iter()
        .filter(|r| matches!(r, DeleteResult::Refused(_, _)))
        .collect();

    if json {
        let out_json = SessionDeleteJson {
            schema_version: SESSION_DELETE_SCHEMA_VERSION,
            dry_run,
            deleted: deleted
                .iter()
                .map(|r| r.target().to_deleted_row())
                .collect(),
            refused: refused.iter().map(|r| r.to_refused_row()).collect(),
        };
        writeln!(out, "{}", serde_json::to_string_pretty(&out_json)?)?;
    } else {
        if dry_run {
            writeln!(out, "Would delete {} session(s).", deleted.len())?;
            for r in &deleted {
                let t = r.target();
                writeln!(out, "  {}  \"{}\"", t.id, sanitize_title(&t.title))?;
            }
            if !refused.is_empty() {
                writeln!(out, "\nWould SKIP {} session(s):", refused.len())?;
                for r in &refused {
                    let t = r.target();
                    writeln!(
                        out,
                        "  {}  \"{}\" — {}",
                        t.id,
                        sanitize_title(&t.title),
                        r.reason_label()
                    )?;
                }
            }
        } else {
            writeln!(out, "Deleted {} session(s).", deleted.len())?;
            for r in &deleted {
                let t = r.target();
                writeln!(out, "  {}  \"{}\"", t.id, sanitize_title(&t.title))?;
            }
            if !refused.is_empty() {
                writeln!(out, "\nRefused {} session(s):", refused.len())?;
                for r in &refused {
                    let t = r.target();
                    writeln!(
                        out,
                        "  {}  \"{}\" — {}",
                        t.id,
                        sanitize_title(&t.title),
                        r.reason_label()
                    )?;
                }
            }
        }
    }

    tracing::info!(
        subcommand = "session-delete",
        deleted = deleted.len(),
        refused = refused.len()
    );

    if refused.is_empty() {
        Ok(())
    } else {
        // Distinct, stable exit code for the bulk path. The codes are
        // severity-ordered, so the highest numeric reason wins (storage_error >
        // path_escape > unverified > in_use). JSON `refused[].reason` stays the
        // precise per-target source of truth (AC8).
        Err(SubcommandExit(bulk_exit_code(&refused)).into())
    }
}

fn bulk_exit_code(refused: &[&DeleteResult]) -> i32 {
    let mut code = EXIT_IN_USE;
    for r in refused {
        let next = match r {
            DeleteResult::Refused(_, RefuseReason::InUse(_)) => EXIT_IN_USE,
            DeleteResult::Refused(_, RefuseReason::Unverified) => EXIT_UNVERIFIED,
            DeleteResult::Refused(_, RefuseReason::PathEscape) => EXIT_PATH_ESCAPE,
            DeleteResult::Refused(_, RefuseReason::StorageError(_)) => EXIT_STORAGE_ERROR,
            _ => continue,
        };
        if next > code {
            code = next;
        }
    }
    code
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

enum Resolution {
    Exact(String),
    UniquePrefix(String),
    Ambiguous(Vec<ConversationSummary>),
    NotFound,
}

fn resolve_id(input: &str, summaries: &[ConversationSummary]) -> Resolution {
    if sanitize_id(input).is_err() {
        // An invalid prefix cannot match any real id.
        return Resolution::NotFound;
    }

    // Exact match wins.
    if let Some(exact) = summaries.iter().find(|s| s.id == input) {
        return Resolution::Exact(exact.id.clone());
    }

    // Unique prefix.
    let matches: Vec<_> = summaries
        .iter()
        .filter(|s| s.id.starts_with(input))
        .cloned()
        .collect();
    match matches.len() {
        0 => Resolution::NotFound,
        1 => Resolution::UniquePrefix(matches[0].id.clone()),
        _ => Resolution::Ambiguous(matches),
    }
}

async fn find_in_other_workspaces(
    id_arg: &str,
    exclude: &Path,
    reader: &dyn WorkspaceRegistryReaderPort,
    storage_for: &impl Fn(&Path) -> Arc<dyn StoragePort>,
) -> Option<(PathBuf, String)> {
    let workspaces = match reader.live_workspaces().await {
        Ok(ws) => ws,
        Err(_) => return None,
    };

    for entry in workspaces {
        let path = canonical_workspace_path(&entry.path);
        if path == exclude {
            continue;
        }
        let storage = storage_for(&path);
        if let Ok(summaries) = storage.list_conversations_read_only().await {
            if let Resolution::Exact(id) | Resolution::UniquePrefix(id) =
                resolve_id(id_arg, &summaries)
            {
                if let Some(summary) = summaries.iter().find(|s| s.id == id) {
                    return Some((path, summary.title.clone()));
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Guard / path confinement helpers
// ---------------------------------------------------------------------------

async fn assert_path_confined(sessions_dir: &Path, id: &str) -> Result<()> {
    let dir = sessions_dir.join(id);
    let file = sessions_dir.join(format!("{}.session.json", id));

    let dir_meta = tokio::fs::symlink_metadata(&dir).await;
    let file_meta = tokio::fs::symlink_metadata(&file).await;

    let target = match (&dir_meta, &file_meta) {
        (Ok(m), _) if m.is_dir() => &dir,
        (_, Ok(_)) => &file,
        _ => {
            // Race: the resolved session disappeared before we could stat it.
            // delete_conversation is idempotent; skip confinement check.
            return Ok(());
        }
    };

    let canonical_target = match tokio::fs::canonicalize(target).await {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Race: symlink_metadata saw the path but it vanished before
            // canonicalize could resolve it. delete_conversation is idempotent.
            return Ok(());
        }
        // Any other canonicalize failure (permission denied, dangling symlink,
        // I/O error) is fail-closed: refuse the delete rather than skip
        // confinement, which would let a symlink-replacement TOCTOU through.
        Err(_) => anyhow::bail!("session path escapes the workspace sessions directory"),
    };
    let canonical_dir = tokio::fs::canonicalize(sessions_dir)
        .await
        .with_context(|| format!("canonicalizing {}", sessions_dir.display()))?;

    if !canonical_target.starts_with(&canonical_dir) {
        anyhow::bail!("session path escapes the workspace sessions directory");
    }
    Ok(())
}

fn storage_error(_e: crate::domain::errors::StorageError, _context: &str) -> anyhow::Error {
    SubcommandExit(EXIT_STORAGE_ERROR).into()
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn format_single_prompt(
    target: &DeleteTarget,
    show_workspace: bool,
    workspace_label: &str,
) -> String {
    let mut out = String::new();
    out.push_str("About to delete 1 session — this cannot be undone.\n\n");
    out.push_str(&format!(
        "  {}   \"{}\"\n",
        target.id,
        sanitize_title(&target.title)
    ));
    out.push_str(&format!(
        "  last active {} · {} messages",
        format_timestamp(target.updated_at),
        target.message_count
    ));
    if show_workspace {
        out.push_str(&format!(" · {}", workspace_label));
    }
    out.push('\n');
    out
}

fn render_preview_list(
    out: &mut dyn Write,
    targets: &[DeleteTarget],
    max: usize,
) -> io::Result<()> {
    for t in targets.iter().take(max) {
        writeln!(out, "  {}   \"{}\"", t.id, sanitize_title(&t.title))?;
    }
    if targets.len() > max {
        writeln!(
            out,
            "  … and {} more (run with --dry-run to see all)",
            targets.len() - max
        )?;
    }
    Ok(())
}

fn render_workspace_summary(out: &mut dyn Write, targets: &[DeleteTarget]) -> io::Result<()> {
    let mut counts: HashMap<&Path, usize> = HashMap::new();
    for t in targets {
        *counts.entry(&*t.workspace).or_insert(0) += 1;
    }
    let mut pairs: Vec<_> = counts.into_iter().collect();
    pairs.sort_by_key(|(p, _)| p.as_os_str());
    for (ws, count) in pairs.iter().take(5) {
        writeln!(out, "  {}        {} sessions", ws.display(), count)?;
    }
    if pairs.len() > 5 {
        writeln!(
            out,
            "  … and {} more (run with --dry-run to see all)",
            pairs.len() - 5
        )?;
    }
    Ok(())
}

fn render_refusal(
    results: &[DeleteResult],
    dry_run: bool,
    json: bool,
    out: &mut dyn Write,
) -> Result<()> {
    if json {
        let refused: Vec<_> = results.iter().map(|r| r.to_refused_row()).collect();
        let out_json = SessionDeleteJson {
            schema_version: SESSION_DELETE_SCHEMA_VERSION,
            dry_run,
            deleted: vec![],
            refused,
        };
        writeln!(out, "{}", serde_json::to_string_pretty(&out_json)?)?;
        return Ok(());
    }

    for result in results {
        let target = result.target();
        match result {
            DeleteResult::Refused(_, RefuseReason::InUse(h)) => {
                writeln!(out, "Refused — this session is in use.")?;
                writeln!(
                    out,
                    "  {}   \"{}\"   ({})",
                    target.id,
                    sanitize_title(&target.title),
                    target.workspace.display()
                )?;
                let channel = h.channels.first().map(channel_label).unwrap_or("#terminal");
                writeln!(out, "  Held by daemon: pid {}, channel {}", h.pid, channel)?;
                writeln!(out, "Stop the holder, then retry:")?;
                writeln!(
                    out,
                    "  rustain daemon stop          (or detach channel {})",
                    channel
                )?;
            }
            DeleteResult::Refused(_, RefuseReason::Unverified) => {
                writeln!(
                    out,
                    "Refused — can't verify this session is safe to delete."
                )?;
                writeln!(
                    out,
                    "A daemon is running but isn't responding, so rustain can't tell whether it's holding this session."
                )?;
                writeln!(out)?;
                writeln!(out, "Options:")?;
                writeln!(
                    out,
                    "  rustain daemon stop                          stop it, then retry"
                )?;
                writeln!(
                    out,
                    "  rustain session delete <id> --force          delete anyway (you accept the risk)"
                )?;
                writeln!(out)?;
                writeln!(
                    out,
                    "--force skips this check — but it will NEVER delete a session that a responsive daemon reports as in use."
                )?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn channel_label(kind: &crate::domain::models::channel_kind::ChannelKind) -> &'static str {
    use crate::domain::models::channel_kind::ChannelKind;
    match kind {
        ChannelKind::Terminal => "#terminal",
        ChannelKind::Telegram => "#telegram",
        ChannelKind::Cron => "#cron",
    }
}

// ---------------------------------------------------------------------------
// JSON DTOs
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct SessionDeleteJson {
    schema_version: &'static str,
    dry_run: bool,
    deleted: Vec<DeletedRow>,
    refused: Vec<RefusedRow>,
}

#[derive(Serialize)]
struct DeletedRow {
    id: String,
    title: String,
    workspace: String,
}

#[derive(Serialize)]
struct RefusedRow {
    id: String,
    title: String,
    workspace: String,
    reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    holder: Option<HolderJson>,
}

#[derive(Serialize)]
struct HolderJson {
    pid: u32,
    channel: String,
}

// ---------------------------------------------------------------------------
// Internal target / result types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct DeleteTarget {
    workspace: PathBuf,
    sessions_dir: PathBuf,
    id: String,
    title: String,
    message_count: usize,
    updated_at: i64,
}

impl DeleteTarget {
    fn to_deleted_row(&self) -> DeletedRow {
        DeletedRow {
            id: self.id.clone(),
            title: sanitize_title(&self.title),
            workspace: self.workspace.to_string_lossy().to_string(),
        }
    }
}

#[derive(Debug, Clone)]
enum RefuseReason {
    InUse(HeldSession),
    Unverified,
    NotFound,
    Ambiguous,
    PathEscape,
    StorageError(String),
}

#[derive(Debug, Clone)]
enum DeleteResult {
    Deleted(DeleteTarget),
    WouldDelete(DeleteTarget),
    Refused(DeleteTarget, RefuseReason),
}

impl DeleteResult {
    fn target(&self) -> &DeleteTarget {
        match self {
            DeleteResult::Deleted(t)
            | DeleteResult::WouldDelete(t)
            | DeleteResult::Refused(t, _) => t,
        }
    }

    fn reason_label(&self) -> String {
        match self {
            DeleteResult::Deleted(_) | DeleteResult::WouldDelete(_) => "deleted".to_string(),
            DeleteResult::Refused(_, reason) => match reason {
                RefuseReason::InUse(h) => format!(
                    "held by daemon (pid {}, channel {})",
                    h.pid,
                    h.channels.first().map(channel_label).unwrap_or("#terminal")
                ),
                RefuseReason::Unverified => "unverified".to_string(),
                RefuseReason::NotFound => "not_found".to_string(),
                RefuseReason::Ambiguous => "ambiguous".to_string(),
                RefuseReason::PathEscape => "path_escape".to_string(),
                RefuseReason::StorageError(_) => "storage_error".to_string(),
            },
        }
    }

    fn to_refused_row(&self) -> RefusedRow {
        let target = self.target();
        let (reason, holder) = match self {
            DeleteResult::Refused(_, r) => match r {
                RefuseReason::InUse(h) => (
                    "in_use".to_string(),
                    Some(HolderJson {
                        pid: h.pid,
                        channel: h
                            .channels
                            .first()
                            .map(channel_label)
                            .unwrap_or("#terminal")
                            .to_string(),
                    }),
                ),
                RefuseReason::Unverified => ("unverified".to_string(), None),
                RefuseReason::NotFound => ("not_found".to_string(), None),
                RefuseReason::Ambiguous => ("ambiguous".to_string(), None),
                RefuseReason::PathEscape => ("path_escape".to_string(), None),
                RefuseReason::StorageError(_) => ("storage_error".to_string(), None),
            },
            _ => ("deleted".to_string(), None),
        };
        RefusedRow {
            id: target.id.clone(),
            title: sanitize_title(&target.title),
            workspace: target.workspace.to_string_lossy().to_string(),
            reason,
            holder,
        }
    }
}

// ---------------------------------------------------------------------------
// Shared small helpers
// ---------------------------------------------------------------------------

fn format_timestamp(ts: i64) -> String {
    let dt = DateTime::from_timestamp(ts, 0).unwrap_or(DateTime::UNIX_EPOCH);
    dt.with_timezone(&Local)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

fn abbreviate_home(abs: &str) -> String {
    let Some(home) = dirs::home_dir() else {
        return abs.to_string();
    };
    let home = home.to_string_lossy();
    if abs == home.as_ref() {
        return "~".to_string();
    }
    if let Some(stripped) = abs.strip_prefix(home.as_ref()) {
        if stripped.starts_with(std::path::is_separator) {
            return format!("~{}", stripped);
        }
    }
    abs.to_string()
}

fn canonical_workspace_path(workspace: &Path) -> PathBuf {
    let absolute = if workspace.is_absolute() {
        workspace.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(workspace)
    };
    std::fs::canonicalize(&absolute).unwrap_or(absolute)
}
