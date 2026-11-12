//! K8 copy pins (AC16, AC17, AC18): the user-visible replacement for every rewritten producer
//! that no other test quotes, pinned by value. Each expected text is written out in full here,
//! never rebuilt from the producer's own constant, so a deleted or reworded message fails. The
//! producers that are only reachable inside the crate (restart reason, uncached card, delegation,
//! SSE refusal, reconnect log, consent queue, footer, init menu, editor error, write access,
//! reorder and edit-title warnings, retry cap) carry `story21_14_` tests next to them.
//!
//! The Windows daemon refusal (`windows_not_supported`, compiled only off Unix) cannot run in this
//! Unix test binary and has no witness here.

use rustain::adapters::tui::handlers::model_switch;
use rustain::adapters::tui::keymap::table::{CLOSE_WINDOW_FIRST, attached_panel_remedy};
use rustain::adapters::tui::notifications::NoticeSeverity;
use rustain::adapters::tui::state::TuiState;
use rustain::domain::errors::ProviderError;
use rustain::domain::models::conversation::Conversation;
use rustain::domain::models::provider::{
    LockedProvider, ModelCapability, ModelDescriptor, ProviderDescriptor,
};
use rustain::domain::models::{StatusState, StreamingState};
use rustain::domain::ports::{ProviderInfoPort, StreamingProvider};
use rustain::infrastructure::startup::{
    preview_profile_notice, validate_assembler_strategy, validate_sandbox_adapter,
    validate_skill_exposure, validate_tools_exposure,
};
use std::sync::Arc;

fn invalid(field: &str, value: &str) -> String {
    format!("Invalid configuration value: {field} = {value}")
}

#[test]
fn skill_exposure_errors_say_what_is_accepted() {
    assert_eq!(
        validate_skill_exposure("")
            .expect_err("empty is refused")
            .to_string(),
        invalid(
            "skill_exposure.kind",
            r#"empty exposure strategy value is invalid. Accepts `"l1-metadata"` (the default) or `"static-full"` (codex-parity opt-in)."#
        )
    );
    assert_eq!(
        validate_skill_exposure("bogus")
            .expect_err("unknown is refused")
            .to_string(),
        invalid(
            "skill_exposure.kind",
            r#"unknown skill exposure strategy `bogus`. Accepts only `"l1-metadata"` (default) and `"static-full"` (codex-parity opt-in fallback). Reserved value: `"meta-search"` (requires `--features meta-search`)."#
        )
    );
    #[cfg(not(feature = "meta-search"))]
    assert_eq!(
        validate_skill_exposure("meta-search")
            .expect_err("needs the feature")
            .to_string(),
        invalid(
            "skill_exposure.kind",
            r#"`meta-search` skill exposure strategy requires the `meta-search` cargo feature. Compile with `--features meta-search` or set `[skill_exposure].kind = "l1-metadata"` (the default) or remove the key entirely."#
        )
    );
}

#[test]
fn assembler_errors_say_what_is_accepted() {
    assert_eq!(
        validate_assembler_strategy("")
            .expect_err("empty is refused")
            .to_string(),
        invalid(
            "assembler.strategy",
            r#"empty assembler strategy is invalid. Accepts `"passthrough"` (the default) or `"windowing"` (within-session windowing). Remove the `[assembler]` block to use the default."#
        )
    );
    assert_eq!(
        validate_assembler_strategy("bogus")
            .expect_err("unknown is refused")
            .to_string(),
        invalid(
            "assembler.strategy",
            r#"unknown assembler strategy `bogus`. Accepts only `"passthrough"` (default, behaviour-preserving) and `"windowing"` (within-session grouped windowing)."#
        )
    );
}

#[test]
fn tools_exposure_errors_say_what_is_accepted() {
    assert_eq!(
        validate_tools_exposure("")
            .expect_err("empty is refused")
            .to_string(),
        invalid(
            "tools.exposure",
            r#"empty exposure strategy value is invalid. Accepts only `"static-full"` (the default). Remove `[tools]` block or set `[tools].exposure = "static-full"`."#
        )
    );
    assert_eq!(
        validate_tools_exposure("bogus")
            .expect_err("unknown is refused")
            .to_string(),
        invalid(
            "tools.exposure",
            r#"unknown exposure strategy `bogus`. Accepts only `"static-full"`. Reserved value: `"meta-search"` (requires `--features meta-search`)."#
        )
    );
    #[cfg(not(feature = "meta-search"))]
    assert_eq!(
        validate_tools_exposure("meta-search")
            .expect_err("needs the feature")
            .to_string(),
        invalid(
            "tools.exposure",
            r#"`meta-search` exposure strategy requires the `meta-search` cargo feature. Compile with `--features meta-search` or set `[tools].exposure = "static-full"` (the default) or remove the key entirely."#
        )
    );
}

#[test]
fn sandbox_adapter_errors_say_what_is_accepted() {
    assert_eq!(
        validate_sandbox_adapter("")
            .expect_err("empty is refused")
            .to_string(),
        invalid(
            "sandbox.adapter",
            r#"empty sandbox adapter value is invalid. Accepts `"noop"` (the default on all platforms) or `"landlock"` (Linux only, requires `sandbox` cargo feature)."#
        )
    );
    assert_eq!(
        validate_sandbox_adapter("bogus")
            .expect_err("unknown is refused")
            .to_string(),
        invalid(
            "sandbox.adapter",
            r#"unknown sandbox adapter `bogus`. Accepts only `"noop"` (the default) and `"landlock"` (Linux + `sandbox` feature only)."#
        )
    );
    #[cfg(not(all(target_os = "linux", feature = "sandbox")))]
    assert_eq!(
        validate_sandbox_adapter("landlock")
            .expect_err("needs the feature")
            .to_string(),
        invalid(
            "sandbox.adapter",
            r#"`landlock` sandbox adapter requires the `sandbox` cargo feature AND a Linux build. Rebuild with `--features sandbox` or set `[sandbox].adapter = "noop"` (the default)."#
        )
    );
}

#[test]
fn the_preview_profile_notice_names_what_did_not_load() {
    assert_eq!(
        preview_profile_notice("personal"),
        "Profile 'personal (preview)' partially loaded: the telegram and cron adapters are not available yet"
    );
}

#[test]
fn the_keymap_remedies_say_what_to_do_next() {
    assert_eq!(
        CLOSE_WINDOW_FIRST,
        "Close the open window first, then try again."
    );
    assert_eq!(
        attached_panel_remedy(),
        "This panel is local to a standalone session and is not available in an attached session. Detach (Ctrl+X, D) and run `rustain` for the panels, or use `/team status`, `/team board` and `/team log` here."
    );
}

/// A provider catalogue with one provider and one model whose capabilities the case sets.
#[derive(Default)]
struct Info {
    tool_use: bool,
    unknown_model: bool,
    refuse_switch: bool,
}

impl Info {
    fn model(&self, id: &str) -> ModelDescriptor {
        ModelDescriptor {
            model_id: id.into(),
            display_name: format!("Model {id}"),
            provider_id: "fixture".into(),
            context_window: 32_000,
            capabilities: if self.tool_use {
                std::collections::HashSet::from([ModelCapability::ToolUse])
            } else {
                Default::default()
            },
            pricing_tier: None,
            stale: false,
            wire_model_id: None,
        }
    }
}

impl ProviderInfoPort for Info {
    fn active_delegate_id(&self) -> String {
        "fixture".into()
    }
    fn get_model(&self, provider_id: &str, model_id: &str) -> Option<ModelDescriptor> {
        (provider_id == "fixture").then(|| self.model(model_id))
    }
    fn get_model_provider(&self, _: &str, _: Option<&str>) -> Option<String> {
        (!self.unknown_model).then(|| "fixture".into())
    }
    fn list_providers(&self) -> Vec<ProviderDescriptor> {
        vec![ProviderDescriptor {
            provider_id: "fixture".into(),
            healthy: true,
            model_count: 1,
            display_name: "Fixture".into(),
        }]
    }
    fn list_models_by_provider(&self, _: &str) -> Vec<ModelDescriptor> {
        vec![self.model("one")]
    }
    fn list_locked_providers(&self) -> Vec<LockedProvider> {
        vec![]
    }
    fn get_provider(&self, _: &str) -> Option<Arc<dyn StreamingProvider>> {
        None
    }
    fn set_active_provider(&self, _: &str) -> Result<(), ProviderError> {
        if self.refuse_switch {
            return Err(ProviderError::ConnectionFailed("refused".into()));
        }
        Ok(())
    }
    fn now_unix(&self) -> i64 {
        0
    }
    fn today_start_unix_ms(&self) -> i64 {
        0
    }
}

fn idle_state() -> TuiState {
    let mut state = TuiState::new(80, 24);
    state.status = StatusState::Idle;
    state
}

fn warnings(state: &TuiState) -> Vec<String> {
    state
        .notifications
        .records()
        .filter(|record| record.severity == NoticeSeverity::Warning)
        .map(|record| record.message.clone())
        .collect()
}

async fn switch(state: &mut TuiState, info: &Info, provider: Option<&str>, model: &str) {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    model_switch::handle_apply_model_switch(
        state,
        info,
        &StreamingState::default(),
        &tx,
        &Conversation::default(),
        provider.map(str::to_owned),
        model.to_owned(),
    )
    .await;
}

#[tokio::test]
async fn switching_to_an_unknown_provider_says_where_to_choose_one() {
    let mut state = idle_state();
    let info = Info {
        tool_use: true,
        ..Info::default()
    };
    switch(&mut state, &info, Some("ghost"), "one").await;
    assert_eq!(
        warnings(&state),
        vec!["Provider 'ghost' not found. Choose a provider from the model list."]
    );
}

#[tokio::test]
async fn switching_to_a_model_without_tool_use_says_tools_will_not_run() {
    let mut state = idle_state();
    switch(
        &mut state,
        &Info {
            tool_use: false,
            ..Info::default()
        },
        Some("fixture"),
        "one",
    )
    .await;
    assert_eq!(
        warnings(&state),
        vec![
            "Model one does not support tool use, so tools will not run. Choose a model with tool use to run tools."
        ]
    );
    let mut with_tools = idle_state();
    switch(
        &mut with_tools,
        &Info {
            tool_use: true,
            ..Info::default()
        },
        Some("fixture"),
        "one",
    )
    .await;
    assert!(warnings(&with_tools).is_empty());
}

#[tokio::test]
async fn an_unknown_model_names_it_and_says_where_to_choose_one() {
    use rustain::domain::events::AppEvent;
    use rustain::domain::models::NoticeLevel;
    let mut state = idle_state();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let info = Info {
        tool_use: true,
        unknown_model: true,
        ..Info::default()
    };
    model_switch::handle_apply_model_switch(
        &mut state,
        &info,
        &StreamingState::default(),
        &tx,
        &Conversation::default(),
        None,
        "no-such-model".to_owned(),
    )
    .await;
    let Some(AppEvent::SystemNotice {
        conversation_id,
        level,
        message,
    }) = rx.recv().await
    else {
        panic!("expected a notice");
    };
    assert_eq!(conversation_id, None);
    assert_eq!(level, NoticeLevel::Warning);
    assert_eq!(
        message,
        "Unknown model: no-such-model. Choose a model from the model list."
    );
    assert!(rx.try_recv().is_err(), "one notice only");
}

#[tokio::test]
async fn a_refused_provider_switch_says_why_and_what_to_do_next() {
    let mut state = idle_state();
    let info = Info {
        tool_use: true,
        refuse_switch: true,
        ..Info::default()
    };
    model_switch::handle_complete_model_switch(&mut state, &info, "other", "one").await;
    assert_eq!(
        warnings(&state),
        vec![
            "Could not switch providers (Connection failed: refused). Choose a model again, or pick another provider."
        ]
    );
    assert_eq!(state.selected_model, None, "the model stays as it was");
}

/// The long help of the command at `path`, with every run of whitespace folded to one space so
/// the wrapping never decides a pin.
fn help_of(path: &[&str]) -> String {
    use clap::CommandFactory;
    let mut root = rustain::adapters::cli::commands::Cli::command();
    root.build();
    let mut command = &mut root;
    for name in path {
        command = command
            .find_subcommand_mut(name)
            .unwrap_or_else(|| panic!("no subcommand {name}"));
    }
    command
        .render_long_help()
        .to_string()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn the_rewritten_help_lines_read_as_instructions() {
    let root = help_of(&[]);
    for expected in [
        "Override the default model. This has the highest priority of all config layers",
        "Choose how tools are exposed to the model each turn.",
        "Choose how skills are exposed to the model each turn:",
        "Choose the sandbox adapter:",
    ] {
        assert!(root.contains(expected), "{expected:?} missing from {root}");
    }
    let daemon = help_of(&["daemon"]);
    assert!(
        daemon.contains(
            "Unix only: Linux and macOS are supported; Windows daemon support (named pipes) is not available yet"
        ),
        "{daemon}"
    );
    let revoke = help_of(&["peer", "revoke"]);
    assert!(
        revoke.contains(
            "Accepted for compatibility; changes nothing, because every frame is checked on arrival"
        ),
        "{revoke}"
    );
    let send = help_of(&["team", "send"]);
    assert!(
        send.contains("(requires the write lock)"),
        "the team send help names the write lock: {send}"
    );
}

#[test]
fn the_peer_command_texts_say_what_happened_in_plain_words() {
    use rustain::adapters::cli::peer::{revoke, rows};
    use rustain::domain::models::{FrameOutcome, PeerId, peer_fingerprint};

    assert_eq!(
        revoke::NOW_IS_A_NO_OP,
        "note: --now is accepted for compatibility and changes nothing. Access is already checked per frame, so every revocation takes effect on the next frame."
    );
    assert_eq!(
        revoke::nothing_recorded_text("ghost"),
        "No peer entry recorded for \"ghost\"; nothing changed.\n`rustain peer list` shows what is configured. A well-formed peer id that\nno configured entry derives is not a target — nothing is written for it."
    );
    let id = PeerId::from_public_key(&[8u8; 32]).expect("peer id");
    assert_eq!(
        rows::ping_single_text("b", &id, FrameOutcome::Unanswered),
        format!(
            "Sent 1 frame to b ({}) — outcome unknown; the peer did not answer. Check the peer's transport log.",
            peer_fingerprint(&id)
        )
    );
}

#[test]
fn the_unreadable_auth_store_warning_says_how_to_recover() {
    assert_eq!(
        rustain::infrastructure::startup::auth_store_warning("expected value at line 1"),
        "auth.json unreadable; stored credentials ignored: expected value at line 1. Fix or remove auth.json, then sign in again with /login."
    );
}
