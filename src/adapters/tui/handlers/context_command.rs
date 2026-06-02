//! Story 11.4 — Content-tier context injection seam + `/context show|off|on`
//! command handling, extracted from `event_loop.rs` (handlers-extraction pattern,
//! Story 8.0a) to respect the AC-4 line budget. Domain-only deps (no `&AppState`):
//! the command handler returns the `AppEvent`s to emit and the caller pumps them
//! through the event bus.

use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::adapters::tui::state::TuiState;
use crate::domain::events::AppEvent;
use crate::domain::models::{ContextBudget, Message, MessageRole, NoticeLevel};
use crate::domain::ports::ContextPort;

/// Call-site ceiling for memory/context injection tokens. A safety upper bound;
/// the real cap is the `MemoryContextAdapter`'s `ContextAssemblyConfig.max_tokens`
/// (default ~2k, Q4), applied as `min(this, config.max_tokens)`.
const CONTEXT_INJECTION_BUDGET_TOKENS: usize = 4096;

/// Turn-start memory/context injection (AC1, AC3). Assemble the bundle for `text`
/// and attach it to the CURRENT-TURN (last) user message's `context_prefix` — NOT
/// `messages[0]` (Amendment 1): `messages[0]` is the turn-1 message replayed from
/// history; mutating it stacks prefixes on a stale message and busts the prefix
/// cache on every historical message. Mirrors the image-attachment / history-
/// rebuild precedents. Fully short-circuits when injection is toggled OFF (AC7) —
/// zero `MemoryPort` calls. The assembled bundle is cached on `state` for
/// `/context show` (Task 5) and the status-bar token cost (Task 7).
pub async fn inject_assembled_context(
    state: &mut TuiState,
    context: &Arc<ArcSwap<Arc<dyn ContextPort>>>,
    text: &str,
    messages: &mut [Message],
) {
    if !state.context_injection_on {
        return;
    }
    let ctx_port = context.load_full();
    let budget = ContextBudget::new(CONTEXT_INJECTION_BUDGET_TOKENS);
    match ctx_port.assemble(text, budget).await {
        Ok(bundle) => {
            if let Some(prefix) = bundle.to_prefix() {
                // Attach to the LAST user message (current turn), not by content
                // match — upstream normalization could mismatch (Patch 9).
                if let Some(target) = messages
                    .iter_mut()
                    .rev()
                    .find(|m| m.role == MessageRole::User)
                {
                    target.context_prefix =
                        Some(crate::domain::services::compaction::compose_context_prefix(
                            target.context_prefix.take(),
                            prefix,
                        ));
                } else {
                    tracing::warn!("context prefix target not found — no user message in batch");
                }
            }
            state.last_context_bundle = Some(bundle);
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "context assemble failed — turn proceeds without injected memory"
            );
            // Clear stale bundle so /context show reflects the failure, not
            // a previous turn's successful assembly (Patch 10).
            state.last_context_bundle = None;
        }
    }
}

/// Handle `/context show | off | on` (AC6/AC7). Returns the `AppEvent`s the caller
/// should emit; sets `state.needs_redraw`. Reserved sub-verbs only — the bare
/// `/context <adapter>` override is intentionally dropped (Q7: the `[context]`
/// profile key is the real override surface).
pub fn handle_context_command(
    state: &mut TuiState,
    conversation_id: &str,
    cmd_arg: Option<&str>,
) -> Vec<AppEvent> {
    state.needs_redraw = true;
    let notice = |message: String, level: NoticeLevel| AppEvent::SystemNotice {
        conversation_id: Some(conversation_id.to_string()),
        level,
        message,
    };
    match cmd_arg.map(str::trim) {
        Some("off") => {
            state.context_injection_on = false;
            vec![notice(
                "Memory context injection disabled for this session. Project context \
                 (CLAUDE.md) still applies. Re-enable with /context on."
                    .to_string(),
                NoticeLevel::Info,
            )]
        }
        Some("on") => {
            state.context_injection_on = true;
            vec![notice(
                "Memory context injection enabled for this session.".to_string(),
                NoticeLevel::Info,
            )]
        }
        Some("show") | None | Some("") => {
            let message = match state.last_context_bundle {
                Some(ref bundle) => crate::adapters::tui::widgets::context_card::context_card_text(
                    bundle,
                    state.context_injection_on,
                ),
                None if state.context_injection_on => {
                    "No context assembled yet — it is built at the start of your next message."
                        .to_string()
                }
                None => "Memory context injection is OFF (/context on to enable). No memory \
                         context is being injected."
                    .to_string(),
            };
            vec![notice(message, NoticeLevel::Info)]
        }
        Some(other) => vec![notice(
            format!("Unknown /context subcommand '{other}'. Use: /context show | off | on."),
            NoticeLevel::Warning,
        )],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{
        AssembleDiagnostics, ContextBundle, ContextSource, ProvenancedEntry, Relevance,
        RetrievalMethod,
    };
    use crate::domain::ports::ContextPort;
    use async_trait::async_trait;
    use std::sync::Arc;

    fn state() -> TuiState {
        TuiState::new(80, 24)
    }

    /// Mock ContextPort that returns a fixed bundle (deterministic, no I/O).
    struct MockContextPort {
        bundle: ContextBundle,
    }

    #[async_trait]
    impl ContextPort for MockContextPort {
        async fn assemble(
            &self,
            _query: &str,
            _budget: crate::domain::models::ContextBudget,
        ) -> Result<ContextBundle, crate::domain::errors::ContextError> {
            Ok(self.bundle.clone())
        }
    }

    fn mock_slot(bundle: ContextBundle) -> Arc<ArcSwap<Arc<dyn ContextPort>>> {
        Arc::new(ArcSwap::from_pointee(
            Arc::new(MockContextPort { bundle }) as Arc<dyn ContextPort>
        ))
    }

    #[test]
    fn off_then_on_toggles_session_flag_with_notice() {
        let mut s = state();
        assert!(s.context_injection_on, "default on");
        let evs = handle_context_command(&mut s, "c1", Some("off"));
        assert!(!s.context_injection_on, "off disables");
        assert_eq!(evs.len(), 1);
        let evs = handle_context_command(&mut s, "c1", Some("on"));
        assert!(s.context_injection_on, "on re-enables");
        assert_eq!(evs.len(), 1);
    }

    #[test]
    fn show_with_no_bundle_explains_state() {
        let mut s = state();
        let evs = handle_context_command(&mut s, "c1", Some("show"));
        match &evs[0] {
            AppEvent::SystemNotice { message, .. } => {
                assert!(message.contains("No context assembled yet"))
            }
            other => panic!("expected SystemNotice, got {other:?}"),
        }
    }

    #[test]
    fn bare_context_defaults_to_show() {
        let mut s = state();
        let evs = handle_context_command(&mut s, "c1", None);
        assert_eq!(evs.len(), 1);
    }

    #[test]
    fn unknown_subcommand_warns() {
        let mut s = state();
        let evs = handle_context_command(&mut s, "c1", Some("bogus"));
        match &evs[0] {
            AppEvent::SystemNotice { level, message, .. } => {
                assert_eq!(*level, NoticeLevel::Warning);
                assert!(message.contains("Unknown /context subcommand"));
            }
            other => panic!("expected Warning SystemNotice, got {other:?}"),
        }
    }

    // AC3 / Amendment 1: replayed-history `context_prefix` is byte-unchanged
    // turn-over-turn. The historical message from turn 1 keeps its prefix; the
    // current-turn (turn 2) message gets the new injected prefix.
    #[tokio::test]
    async fn replayed_history_prefix_unchanged() {
        let mut s = state();
        let historical_prefix = "[memory] historical fact".to_string();

        // Simulate turn-1 message (already in history) with an existing prefix.
        let mut messages = vec![
            Message {
                role: MessageRole::User,
                content: "turn 1 query".to_string(),
                images: vec![],
                tool_results: vec![],
                tool_uses: vec![],
                context_prefix: Some(historical_prefix.clone()),
                reasoning_content: None,
            },
            Message {
                role: MessageRole::Assistant,
                content: "turn 1 response".to_string(),
                images: vec![],
                tool_results: vec![],
                tool_uses: vec![],
                context_prefix: None,
                reasoning_content: None,
            },
        ];

        // Current-turn (turn 2) user message — will get the injection.
        let turn2_text = "turn 2 query";
        messages.push(Message {
            role: MessageRole::User,
            content: turn2_text.to_string(),
            images: vec![],
            tool_results: vec![],
            tool_uses: vec![],
            context_prefix: None,
            reasoning_content: None,
        });

        let bundle = ContextBundle {
            entries: vec![ProvenancedEntry {
                source: ContextSource::MemoryMd,
                content: Arc::from("new fact for turn 2"),
                timestamp: 0,
                retrieval_method: RetrievalMethod::MemoryMd,
                relevance: Relevance::Unscored,
            }],
            diagnostics: AssembleDiagnostics {
                per_source_tokens: vec![(ContextSource::MemoryMd, 5)],
                total_tokens: 5,
                truncated: false,
                deduped_count: 0,
            },
        };

        inject_assembled_context(&mut s, &mock_slot(bundle), turn2_text, &mut messages).await;

        // Historical message's prefix MUST be unchanged.
        assert_eq!(
            messages[0].context_prefix,
            Some(historical_prefix),
            "historical message prefix must be byte-unchanged"
        );

        // Current-turn message MUST have received the new prefix.
        let turn2_prefix = messages[2].context_prefix.as_ref().unwrap();
        assert!(
            turn2_prefix.contains("new fact for turn 2"),
            "current-turn message should get injected prefix: {turn2_prefix}"
        );
    }
}
