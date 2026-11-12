//! Export and cross-search handler helpers — extracted from event_loop.rs per
//! Story 8.5 line-budget reduction (D-4 continuation).
//!
//! Contains:
//! - Cross-search query change guard (Story 4-4 AC5)
//! - Cross-search results stale-result guard (Story 4-4 AC5)
//! - Export confirm/cancel/apply (Story 4-4 AC11/AC12)

use crate::adapters::tui::decision_queue::{self, ExportOverwritePayload, FileStamp};
use crate::adapters::tui::notifications::{self, NoticeSource};
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
        // The frame beneath (History, which opened the search) is the top frame again: the
        // focus returns through the stack, nothing is written to the base (K-23).
        reducer::close(state, Surface::CrossSearch);
    });
}

#[doc(hidden)]
pub async fn apply_confirm_export_overwrite(state: &mut TuiState, payload: ExportOverwritePayload) {
    let target_for_msg = payload.target.clone();
    let write_result = tokio::task::spawn_blocking(move || {
        use std::io::Write as _;
        let current = stamp_of(&payload.target);
        if payload.stamp.is_none() || current != payload.stamp {
            return Ok(false);
        }
        let tmp_path = payload.target.with_extension("md.tmp");
        let res: std::io::Result<()> = (|| {
            let mut f = std::fs::File::create(&tmp_path)?;
            f.write_all(payload.content.as_bytes())?;
            f.sync_all()?;
            drop(f);
            std::fs::rename(&tmp_path, &payload.target)?;
            Ok(())
        })();
        if res.is_err() {
            let _ = std::fs::remove_file(&tmp_path);
        }
        res.map(|()| true)
    })
    .await;
    match write_result {
        Ok(Ok(true)) => notifications::success(
            state,
            NoticeSource::Export,
            format!("Overwrote {}", target_for_msg.display()),
        ),
        Ok(Ok(false)) => notifications::warning(
            state,
            NoticeSource::Export,
            "This decision is no longer pending. The export file changed; nothing was written. Run the export again.",
        ),
        Ok(Err(e)) => {
            notifications::failure(
                state,
                NoticeSource::Export,
                "Could not overwrite the export file.",
                "Check the file name and permissions, then run /export again.",
                Some(e.to_string()),
            );
        }
        Err(join_err) => {
            notifications::failure(
                state,
                NoticeSource::Export,
                "Could not finish the export.",
                "Run /export again.",
                Some(join_err.to_string()),
            );
        }
    }
    state.needs_redraw = true;
}

fn stamp_of(path: &std::path::Path) -> Option<FileStamp> {
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file() {
        return None;
    }
    Some(FileStamp {
        len: metadata.len(),
        modified: metadata.modified().ok(),
        #[cfg(unix)]
        dev: metadata.dev(),
        #[cfg(unix)]
        ino: metadata.ino(),
        #[cfg(unix)]
        ctime: metadata.ctime(),
        #[cfg(unix)]
        ctime_nsec: metadata.ctime_nsec(),
    })
}

#[doc(hidden)]
pub fn apply_cancel_export_overwrite(state: &mut TuiState) {
    notifications::info(state, NoticeSource::Export, "Export cancelled");
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
                notifications::failure(
                    state,
                    NoticeSource::Export,
                    "Could not create the exports folder.",
                    "Check the permissions of .rustain/exports/, then run /export again.",
                    Some(e.to_string()),
                );
                state.needs_redraw = true;
                return;
            }
            Err(join_err) => {
                notifications::failure(
                    state,
                    NoticeSource::Export,
                    "Could not finish the export.",
                    "Run /export again.",
                    Some(join_err.to_string()),
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
                notifications::failure(
                    state,
                    NoticeSource::Export,
                    "Could not export to an absolute path.",
                    "Run /export again with a name relative to .rustain/exports/.",
                    None,
                );
                state.needs_redraw = true;
                return;
            }
            if raw
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                notifications::failure(
                    state,
                    NoticeSource::Export,
                    "Could not export to a path containing '..'.",
                    "Run /export again with a name inside .rustain/exports/.",
                    None,
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
                        notifications::failure(
                            state,
                            NoticeSource::Export,
                            "Could not export outside .rustain/exports/.",
                            "Run /export again with a name inside .rustain/exports/.",
                            None,
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

    let (target_exists, stamp) = tokio::task::spawn_blocking({
        let target_path = target_path.clone();
        move || {
            (
                std::fs::symlink_metadata(&target_path).is_ok(),
                stamp_of(&target_path),
            )
        }
    })
    .await
    .unwrap_or((false, None));
    if arg.is_some() && target_exists {
        decision_queue::raise_export_overwrite(
            state,
            ExportOverwritePayload {
                conversation_id: conversation.id.clone(),
                target: target_path,
                content,
                stamp,
            },
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
            notifications::failure(
                state,
                NoticeSource::Export,
                "Could not write the export file.",
                "Check the file name and permissions, then run /export again.",
                Some(e.to_string()),
            );
        }
        Err(join_err) => {
            notifications::failure(
                state,
                NoticeSource::Export,
                "Could not finish the export.",
                "Run /export again.",
                Some(join_err.to_string()),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn story21_6_overwrite_checks_the_captured_target_before_writing() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("export.md");
        std::fs::write(&target, "original").unwrap();
        let payload = ExportOverwritePayload {
            conversation_id: "c1".into(),
            stamp: stamp_of(&target),
            target: target.clone(),
            content: "new export".into(),
        };
        let mut state = TuiState::new(80, 24);
        std::fs::write(&target, "changed externally").unwrap();
        apply_confirm_export_overwrite(&mut state, payload.clone()).await;
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "changed externally"
        );
        assert!(state.notifications.visible().any(|notice|
            notice.message == "This decision is no longer pending. The export file changed; nothing was written. Run the export again."
        ));

        let fresh = ExportOverwritePayload {
            stamp: stamp_of(&target),
            ..payload
        };
        apply_confirm_export_overwrite(&mut state, fresh).await;
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new export");
    }
}
