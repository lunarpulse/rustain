//! Build the launchpad's facts and the tool-line workspace root (Story 21.9 D14, D17).
//!
//! The only place that reads git or the session index for the welcome screen.
//! A draw calls [`refresh`]; it compares a key `(conversation id, model,
//! provider, locked)` and recomputes only when that changes, so an unchanged
//! draw costs one provider-lock read and a few string compares: never git,
//! never storage.

use std::path::Path;
use std::sync::Arc;

use crate::adapters::cli::auth::providers;
use crate::adapters::provider::ProviderRegistry;
use crate::adapters::tui::keymap::Host;
use crate::adapters::tui::launchpad::{
    AuthState, LaunchpadEnv, LaunchpadFacts, LaunchpadKey, MAX_RECENT, RecentSession,
};
use crate::adapters::tui::state::TuiState;
use crate::domain::clock::Clock;
use crate::domain::models::Conversation;
use crate::domain::services::peer_text::sanitize_display_line;
use crate::domain::services::relative_time::format_relative_time;
use crate::domain::services::session_index::SessionIndex;
use crate::infrastructure::git_head::{self, Head};

/// What the launchpad shows for a session whose title is empty.
const UNTITLED: &str = "Untitled";

/// Give the local host's state what the refresh reads: the workspace and a live
/// provider-lock query (once, at startup).
pub fn install(state: &mut TuiState, workspace: &Path, registry: &Arc<ProviderRegistry>) {
    let registry = Arc::clone(registry);
    state.launchpad_env = Some(LaunchpadEnv::new(workspace.to_path_buf(), move |id| {
        registry.is_locked(id)
    }));
}

/// Refresh the active tab's tool-line root and launchpad for this draw.
/// Returns whether the launchpad facts were recomputed.
pub fn refresh(
    state: &mut TuiState,
    conversation: &Conversation,
    config_model: &str,
    provider_id: &str,
    index: &SessionIndex,
    clock: &dyn Clock,
) -> bool {
    refresh_with(
        state,
        conversation,
        config_model,
        provider_id,
        index,
        clock,
        &mut git_head::read_branch,
    )
}

/// [`refresh`] with the branch reader injected (the counting seam of K7).
pub fn refresh_with(
    state: &mut TuiState,
    conversation: &Conversation,
    config_model: &str,
    provider_id: &str,
    index: &SessionIndex,
    clock: &dyn Clock,
    read_branch: &mut dyn FnMut(&Path) -> Option<Head>,
) -> bool {
    let Some(env) = state.launchpad_env.as_ref() else {
        return false;
    };
    let model = state.selected_model.as_deref().unwrap_or(config_model);
    let width = state.terminal_width;
    let tab = state
        .tab_render_states
        .entry(state.active_tab_id)
        .or_default();
    if tab.workspace_root.is_none() {
        tab.workspace_root = Some(Arc::from(env.workspace.as_path()));
    }
    // Only an empty conversation paints the launchpad.
    if !conversation.messages.is_empty() {
        return false;
    }
    let locked = env.is_locked(provider_id);
    let now = clock.wall_now_ms().div_euclid(1000);
    if let Some(facts) = tab.launchpad.as_mut()
        && facts.key.is(&conversation.id, model, provider_id, locked)
    {
        facts.terminal_width = width;
        // The session index changes on its own (titles arrive, sessions are
        // saved or deleted, ages move): re-derive the list from memory (no
        // storage, no git) and replace it only when it differs.
        let recent = recent_sessions(index, &conversation.id, now);
        if facts.recent != recent {
            facts.recent = recent;
        }
        return false;
    }
    let (provider, auth) = provider_and_auth(provider_id, locked);
    tab.launchpad = Some(LaunchpadFacts {
        host: Host::Local,
        workspace: sanitize_display_line(&env.workspace.to_string_lossy()),
        branch: read_branch(&env.workspace).map(|head| head.label()),
        model: sanitize_display_line(model),
        provider,
        auth,
        recent: recent_sessions(index, &conversation.id, now),
        terminal_width: width,
        key: LaunchpadKey {
            conversation_id: conversation.id.clone(),
            model: model.to_owned(),
            provider_id: provider_id.to_owned(),
            locked,
        },
    });
    true
}

/// The attached host's facts: the workspace and its branch, read once at attach.
pub fn attached_facts(workspace: &Path, terminal_width: u16) -> LaunchpadFacts {
    LaunchpadFacts::attached(
        sanitize_display_line(&workspace.to_string_lossy()),
        git_head::read_branch(workspace).map(|head| head.label()),
        terminal_width,
    )
}

fn provider_and_auth(provider_id: &str, locked: bool) -> (String, AuthState) {
    let meta = providers::lookup(provider_id);
    let display = meta.map_or_else(
        || sanitize_display_line(provider_id),
        |meta| meta.display_name.to_owned(),
    );
    let auth = match (locked, meta) {
        (true, _) => AuthState::NotSignedIn,
        (false, Some(meta)) if meta.requires_key => AuthState::SignedIn,
        (false, Some(_)) => AuthState::NoSignInNeeded,
        (false, None) => AuthState::Unknown,
    };
    (display, auth)
}

/// Up to [`MAX_RECENT`] sessions, newest first: not the one on screen and not
/// an empty one.
fn recent_sessions(index: &SessionIndex, active_id: &str, now: i64) -> Vec<RecentSession> {
    index
        .entries()
        .iter()
        .filter(|entry| entry.conversation_id != active_id && entry.message_count > 0)
        .take(MAX_RECENT)
        .map(|entry| {
            // One line: no control sequence, no bidi override, no newline.
            let title = sanitize_display_line(&entry.title);
            RecentSession {
                title: if title.is_empty() {
                    UNTITLED.to_owned()
                } else {
                    title
                },
                age: format_relative_time(entry.updated_at, now),
            }
        })
        .collect()
}
