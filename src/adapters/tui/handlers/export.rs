//! Export and cross-search handler helpers — extracted from event_loop.rs per
//! Story 8.5 line-budget reduction (D-4 continuation).
//!
//! Contains:
//! - Cross-search query change guard (Story 4-4 AC5)
//! - Cross-search results stale-result guard (Story 4-4 AC5)
//! - Export confirm/cancel/apply (Story 4-4 AC11/AC12)

use crate::adapters::tui::notifications::{self, NoticeSource};
use crate::adapters::tui::presentation::reducer;
use crate::adapters::tui::state::TuiState;
use crate::domain::models::{Conversation, FocusState};

#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrossSearchScanAction {
    Spawn {
        query: String,
        frame_id: u64,
        revision: u64,
    },
    Cleared,
}

#[doc(hidden)]
pub fn apply_cross_search_query_change(state: &mut TuiState) -> CrossSearchScanAction {
    let Some(frame_id) = state
        .modal_stack
        .top()
        .filter(|frame| frame.surface == crate::adapters::tui::presentation::Surface::CrossSearch)
        .map(|frame| frame.id)
    else {
        return CrossSearchScanAction::Cleared;
    };
    state.cross_search.load_failed = false;
    state.cross_search.request_revision = state
        .cross_search
        .request_revision
        .checked_add(1)
        .expect("cross-search request revision overflow");
    if state.cross_search.query.as_str().chars().count() >= 2 {
        state.cross_search.running = true;
        CrossSearchScanAction::Spawn {
            query: state.cross_search.query.as_str().to_owned(),
            frame_id,
            revision: state.cross_search.request_revision,
        }
    } else {
        state.cross_search.results.clear();
        state.cross_search.selected = 0;
        state.cross_search.truncated_by_count = false;
        state.cross_search.truncated_by_time = false;
        state.cross_search.running = false;
        CrossSearchScanAction::Cleared
    }
}

#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrossSearchResultsOutcome {
    Applied,
    DiscardedStale,
}

#[doc(hidden)]
pub fn apply_cross_search_results(
    state: &mut TuiState,
    frame_id: u64,
    revision: u64,
    query: String,
    results: Vec<crate::domain::services::cross_search::CrossSearchResult>,
    truncated_by_count: bool,
    truncated_by_time: bool,
) -> CrossSearchResultsOutcome {
    if state.cross_search.request_revision != revision
        || !state.modal_stack.all_frames().any(|frame| {
            frame.surface == crate::adapters::tui::presentation::Surface::CrossSearch
                && frame.id == frame_id
        })
        || state.cross_search.query.as_str() != query
    {
        return CrossSearchResultsOutcome::DiscardedStale;
    }
    state.cross_search.results = results;
    state.cross_search.truncated_by_count = truncated_by_count;
    state.cross_search.truncated_by_time = truncated_by_time;
    state.cross_search.running = false;
    if state.cross_search.results.is_empty() {
        state.cross_search.selected = 0;
    } else {
        state.cross_search.selected = state
            .cross_search
            .selected
            .min(state.cross_search.results.len() - 1);
    }
    CrossSearchResultsOutcome::Applied
}

/// Apply the runtime's explicit CrossSearch close effect.
#[doc(hidden)]
pub fn close_cross_search(state: &mut TuiState) {
    crate::adapters::tui::app::with_card_focus_reconcile(state, |state| {
        use crate::adapters::tui::presentation::{Surface, reducer};
        if !state
            .modal_stack
            .top()
            .is_some_and(|frame| frame.surface == Surface::CrossSearch)
        {
            return;
        }
        reducer::close(state, Surface::CrossSearch);
        reducer::settle_stage_base(
            state,
            FocusState::Sidebar {
                panel: crate::domain::models::visual::PanelType::History,
                selected: state.sidebar_selected,
            },
        );
    });
}

#[doc(hidden)]
pub async fn apply_confirm_export_overwrite(state: &mut TuiState) {
    if let Some((target_path, content)) = state.pending_export.take() {
        let target_for_msg = target_path.clone();
        let write_result = tokio::task::spawn_blocking(move || {
            use std::io::Write as _;
            let tmp_path = target_path.with_extension("md.tmp");
            let res: std::io::Result<()> = (|| {
                let mut f = std::fs::File::create(&tmp_path)?;
                f.write_all(content.as_bytes())?;
                f.sync_all()?;
                drop(f);
                std::fs::rename(&tmp_path, &target_path)?;
                Ok(())
            })();
            if res.is_err() {
                let _ = std::fs::remove_file(&tmp_path);
            }
            res
        })
        .await;
        match write_result {
            Ok(Ok(())) => {
                notifications::success(
                    state,
                    NoticeSource::Export,
                    format!("Overwrote {}", target_for_msg.display()),
                );
            }
            Ok(Err(e)) => {
                notifications::error(state, NoticeSource::Export, format!("Export failed: {}", e));
            }
            Err(join_err) => {
                notifications::error(
                    state,
                    NoticeSource::Export,
                    format!("Export failed: {}", join_err),
                );
            }
        }
    }
    reducer::set_base(state, FocusState::Chat);
    state.needs_redraw = true;
}

#[doc(hidden)]
pub fn apply_cancel_export_overwrite(state: &mut TuiState) {
    state.pending_export = None;
    notifications::info(state, NoticeSource::Export, "Export cancelled");
    reducer::set_base(state, FocusState::Chat);
    state.needs_redraw = true;
}

#[doc(hidden)]
pub async fn apply_export_command(
    arg: Option<&str>,
    conversation: &Conversation,
    meta: &crate::domain::models::SessionMeta,
    workspace_path: &std::path::Path,
    state: &mut TuiState,
) {
    use crate::domain::services::export::{render_conversation_markdown, slugify};

    let exports_dir = workspace_path.join(".rustain").join("exports");
    {
        let exports_dir = exports_dir.clone();
        let create_result =
            tokio::task::spawn_blocking(move || std::fs::create_dir_all(&exports_dir)).await;
        match create_result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                notifications::error(
                    state,
                    NoticeSource::Export,
                    format!("Export failed: cannot create exports dir: {}", e),
                );
                state.needs_redraw = true;
                return;
            }
            Err(join_err) => {
                notifications::error(
                    state,
                    NoticeSource::Export,
                    format!("Export failed: {}", join_err),
                );
                state.needs_redraw = true;
                return;
            }
        }
    }

    let canonical_exports = match tokio::task::spawn_blocking({
        let exports_dir = exports_dir.clone();
        move || std::fs::canonicalize(&exports_dir)
    })
    .await
    {
        Ok(Ok(p)) => p,
        _ => exports_dir.clone(),
    };

    let target_path: std::path::PathBuf = match arg {
        None => {
            let base_slug = if conversation.title.is_empty() {
                format!(
                    "conversation-{}",
                    &conversation.id[..8.min(conversation.id.len())]
                )
            } else {
                slugify(&conversation.title)
            };
            let candidate = exports_dir.join(format!("{}.md", base_slug));
            find_available_numbered_path(candidate, &exports_dir, &base_slug).await
        }
        Some(name) => {
            let raw = std::path::PathBuf::from(name);
            if raw.is_absolute() {
                notifications::error(
                    state,
                    NoticeSource::Export,
                    "Export failed: absolute paths are not allowed (use a name relative to .rustain/exports/)",
                );
                state.needs_redraw = true;
                return;
            }
            if raw
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                notifications::error(
                    state,
                    NoticeSource::Export,
                    "Export failed: path contains '..' — not allowed",
                );
                state.needs_redraw = true;
                return;
            }
            let candidate = exports_dir.join(&raw);
            if let Some(parent) = candidate.parent() {
                if let Ok(Ok(canonical_parent)) = tokio::task::spawn_blocking({
                    let parent = parent.to_path_buf();
                    move || std::fs::canonicalize(&parent)
                })
                .await
                {
                    if !canonical_parent.starts_with(&canonical_exports) {
                        notifications::error(
                            state,
                            NoticeSource::Export,
                            "Export failed: target escapes .rustain/exports/",
                        );
                        state.needs_redraw = true;
                        return;
                    }
                }
            }
            candidate
        }
    };

    let now = crate::domain::models::session_meta::now_unix();
    let content = render_conversation_markdown(conversation, meta, now);

    let target_exists = tokio::task::spawn_blocking({
        let target_path = target_path.clone();
        move || target_path.exists()
    })
    .await
    .unwrap_or(false);
    if arg.is_some() && target_exists {
        state.pending_export = Some((target_path.clone(), content));
        reducer::raise_confirmation(
            state,
            crate::domain::models::visual::ConfirmationType::ExportOverwrite(target_path),
        );
        state.needs_redraw = true;
        return;
    }

    let workspace_path_owned = workspace_path.to_path_buf();
    let target_path_owned = target_path.clone();
    let write_result = tokio::task::spawn_blocking(move || {
        use std::io::Write as _;
        let tmp_path = target_path_owned.with_extension("md.tmp");
        let write: std::io::Result<()> = (|| {
            let mut f = std::fs::File::create(&tmp_path)?;
            f.write_all(content.as_bytes())?;
            f.sync_all()?;
            drop(f);
            std::fs::rename(&tmp_path, &target_path_owned)?;
            Ok(())
        })();
        if write.is_err() {
            let _ = std::fs::remove_file(&tmp_path);
        }
        write
    })
    .await;

    match write_result {
        Ok(Ok(())) => {
            let display_path = target_path
                .strip_prefix(&workspace_path_owned)
                .unwrap_or(&target_path);
            notifications::success(
                state,
                NoticeSource::Export,
                format!("Exported to {}", display_path.display()),
            );
        }
        Ok(Err(e)) => {
            notifications::error(state, NoticeSource::Export, format!("Export failed: {}", e));
        }
        Err(join_err) => {
            notifications::error(
                state,
                NoticeSource::Export,
                format!("Export failed: {}", join_err),
            );
        }
    }
    state.needs_redraw = true;
}

async fn find_available_numbered_path(
    initial: std::path::PathBuf,
    exports_dir: &std::path::Path,
    base_slug: &str,
) -> std::path::PathBuf {
    let exports_dir_owned = exports_dir.to_path_buf();
    let exports_dir_fallback = exports_dir_owned.clone();
    let base_slug_owned = base_slug.to_string();
    let base_slug_fallback = base_slug_owned.clone();
    tokio::task::spawn_blocking(move || {
        let mut path = initial;
        let mut n: u64 = 2;
        while path.exists() {
            path = exports_dir_owned.join(format!("{}-{}.md", base_slug_owned, n));
            n = n.saturating_add(1);
        }
        path
    })
    .await
    .unwrap_or_else(|_| exports_dir_fallback.join(format!("{}.md", base_slug_fallback)))
}
