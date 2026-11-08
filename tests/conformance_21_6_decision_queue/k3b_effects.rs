//! K3b (slice 6-C): every interim effect arm still runs through the frame, once and
//! by value, after the inert window. The item is raised by its producer, the key goes
//! through `presentation::route` (the router the event loop calls first), the effect
//! the pane returns is compared by value, and then the effect's real body is driven:
//! the synchronous parts of the event-loop arms live in
//! `infrastructure::runtime::decision_effects` (the arm calls the same fns and keeps
//! only the I/O: persisting the new session / the budget pause and spawning the
//! compaction), and the large-image tails through `apply_resolved_action`. Not driven
//! here: the async persistence of the fresh conversation / budget pause, which touch
//! the operator's data dir, and the Recovery producer, which is inline in the
//! event loop (the draft is raised the way it does).

use std::sync::Arc;

use rustain::adapters::noop::NoOpProvider;
use rustain::adapters::tui::app::InputAction;
use rustain::adapters::tui::decision_queue::{
    self, Arrival, DecisionDraft, DecisionEffect, DecisionKind, DecisionSubject,
};
use rustain::adapters::tui::handlers::CompactionPayload;
use rustain::adapters::tui::handlers::HandlerOutcome;
use rustain::adapters::tui::handlers::budget::handle_upsert_daily_budget_warning;
use rustain::adapters::tui::handlers::context_warning::handle_upsert_context_warning;
use rustain::adapters::tui::presentation::{self, RouteEffect, Surface};
use rustain::adapters::tui::state::{DailyBudgetState, TuiState};
use rustain::domain::clock::MockClock;
use rustain::domain::errors::ProviderError;
use rustain::domain::events::{CompactionPurpose, DomainKey};
use rustain::domain::models::provider::{ModelDescriptor, ProviderDescriptor};
use rustain::domain::models::tab::TabManager;
use rustain::domain::models::{
    AppConfig, Authorship, ChannelKind, ChatMessage, Conversation, ConversationSummary, FocusState,
    ImageAttachment, MessageRole, SendAuthorization, StatusState, StreamingState, UsageInfo,
    generate_conversation_id,
};
use rustain::domain::ports::{ProviderInfoPort, StreamingProvider};
use rustain::domain::services::session_index::SessionIndex;
use rustain::domain::services::turn_queue::TurnQueue;
use rustain::infrastructure::clock_util::next_midnight_unix;
use rustain::infrastructure::runtime::decision_effects::{
    apply_budget_pause, budget_continue, budget_switch_cheaper, open_fresh_tab, request_compaction,
    start_new_session,
};
use rustain::infrastructure::runtime::tui_turn_lifecycle::TuiTurnLifecycle;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::support::{clocked, facts, key, paint, ready, route, special};

/// `(now_unix, today_start_unix_ms)` and a one-provider catalog: the pure seam the
/// daily-budget handlers and the model selector read.
struct FixedNow(i64, i64);

impl FixedNow {
    fn model(id: &str) -> ModelDescriptor {
        ModelDescriptor {
            model_id: id.into(),
            display_name: format!("Model {id}"),
            provider_id: "fixture".into(),
            context_window: 32_000,
            capabilities: Default::default(),
            pricing_tier: None,
            stale: false,
            wire_model_id: None,
        }
    }
}

impl ProviderInfoPort for FixedNow {
    fn active_delegate_id(&self) -> String {
        "fixture".into()
    }
    fn get_model(&self, p: &str, m: &str) -> Option<ModelDescriptor> {
        (p == "fixture").then(|| Self::model(m))
    }
    fn get_model_provider(&self, _: &str, _: Option<&str>) -> Option<String> {
        Some("fixture".into())
    }
    fn list_providers(&self) -> Vec<ProviderDescriptor> {
        vec![ProviderDescriptor {
            provider_id: "fixture".into(),
            healthy: true,
            model_count: 2,
            display_name: "Fixture".into(),
        }]
    }
    fn list_models_by_provider(&self, _: &str) -> Vec<ModelDescriptor> {
        vec![Self::model("cheap"), Self::model("dear")]
    }
    fn get_provider(&self, _: &str) -> Option<Arc<dyn StreamingProvider>> {
        None
    }
    fn set_active_provider(&self, _: &str) -> Result<(), ProviderError> {
        Ok(())
    }
    fn now_unix(&self) -> i64 {
        self.0
    }
    fn today_start_unix_ms(&self) -> i64 {
        self.1
    }
}

fn over(spent: f64, dismissed_until_unix: i64) -> DailyBudgetState {
    DailyBudgetState {
        spent_today_usd: spent,
        limit_usd: 10.0,
        computed_at_ms: 0,
        dismissed_until_unix,
    }
}

/// Open the item of `kind` for `conversation` again (a tab switch healed its frame closed).
fn reopen(state: &mut TuiState, kind: DecisionKind, conversation: &str) {
    let key = state
        .decisions
        .items()
        .find(|item| item.kind == kind && item.conversation_id.as_deref() == Some(conversation))
        .map(|item| decision_queue::DecisionKey {
            kind: item.kind,
            source: item.source.clone(),
            conversation_id: item.conversation_id.clone(),
        })
        .expect("the item is still queued");
    assert!(decision_queue::open(state, &key));
}

/// Press `c` (painted first, inert window passed): the effect the pane returned.
fn decide(state: &mut TuiState, clock: &MockClock, c: char) -> Option<RouteEffect> {
    paint(state, &facts("c1"), 80, 24);
    ready(state, clock);
    route(state, &key(c), &facts("c1"))
}

fn effect_of(route_effect: Option<RouteEffect>) -> DecisionEffect {
    match route_effect {
        Some(RouteEffect::Action(InputAction::Decision(effect))) => effect,
        other => panic!("expected a decision effect, got {other:?}"),
    }
}

/// Exactly once: the item left with its effect, and a second press finds nothing.
fn assert_once(state: &mut TuiState, c: char) {
    assert_eq!(
        decision_queue::waiting(state),
        0,
        "the item resolved with its effect"
    );
    let again = route(state, &key(c), &facts("c1"));
    assert!(
        !matches!(again, Some(RouteEffect::Action(InputAction::Decision(_)))),
        "a second press ran the effect again: {again:?}"
    );
}

fn msg(role: MessageRole, content: &str) -> ChatMessage {
    ChatMessage {
        id: generate_conversation_id(),
        role,
        content: content.into(),
        content_blocks: vec![],
        tool_calls: vec![],
        created_at: 1_700_000_000,
        token_count: None,
        stop_reason: None,
        synthetic: false,
        images: vec![],
        origin: ChannelKind::Terminal,
        authorship: Authorship::HumanWritten,
        send_authorization: SendAuthorization::Unknown,
        retracted_at_ms: None,
        history_source: Default::default(),
    }
}

/// Two exchanges (so an inline compaction has a boundary to keep).
fn chat(id: &str) -> Conversation {
    Conversation {
        id: id.into(),
        title: "Plan".into(),
        messages: vec![
            msg(MessageRole::User, "one"),
            msg(MessageRole::Assistant, "ok"),
            msg(MessageRole::User, "two"),
            msg(MessageRole::Assistant, "done"),
        ],
        session_id: Some("s1".into()),
        ..Default::default()
    }
}

/// The compaction request the effect fn issues for `effect` on `conversation`.
fn compaction_of(
    effect: DecisionEffect,
    state: &mut TuiState,
    conversation: &Conversation,
) -> Option<CompactionPayload> {
    let provider: Arc<dyn StreamingProvider> = Arc::new(NoOpProvider);
    let (tx, _rx) = mpsc::unbounded_channel();
    match request_compaction(
        effect,
        state,
        conversation,
        &StreamingState::default(),
        &provider,
        &AppConfig::default(),
        &tx,
        &FixedNow(0, 0),
    ) {
        HandlerOutcome::RequestCompaction(payload) => Some(payload),
        _ => None,
    }
}

#[test]
fn k3b_recovery_new_session_and_continue_each_decide_once() {
    for (c, expected) in [
        ('n', DecisionEffect::RecoveryNewSession),
        ('y', DecisionEffect::RecoveryContinue),
    ] {
        let (mut state, clock) = clocked(80, 24);
        // The Recovery producer is inline in the event loop (`recovery_prompt` is
        // crate-private); it raises exactly this draft, Automatic.
        decision_queue::raise(
            &mut state,
            DecisionDraft::new(DecisionKind::Recovery, "Recovered \"Plan\" (1,200 tokens)."),
            Arrival::Automatic,
        );
        // Control: a key on first sight is dropped, only the ready frame decides.
        paint(&mut state, &facts("c1"), 80, 24);
        assert_ne!(
            route(&mut state, &key(c), &facts("c1")),
            Some(RouteEffect::Action(InputAction::Decision(expected)))
        );
        let got = if c == 'y' {
            ready(&mut state, &clock);
            effect_of(route(&mut state, &special(DomainKey::Enter), &facts("c1")))
        } else {
            effect_of(decide(&mut state, &clock, c))
        };
        assert_eq!(got, expected, "{c}");
        if got == DecisionEffect::RecoveryNewSession {
            // The arm's synchronous body: a fresh conversation identity, nothing carried.
            let mut conversation = chat("c1");
            conversation.usage = Some(UsageInfo {
                input_tokens: 10,
                output_tokens: 5,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
                reasoning_tokens: None,
            });
            conversation.last_response_at = Some(5);
            start_new_session(&mut conversation, &mut state);
            assert_ne!(conversation.id, "c1", "a fresh identity");
            assert_ne!(conversation.session_id.as_deref(), Some("s1"));
            assert!(conversation.session_id.is_some());
            assert!(conversation.messages.is_empty());
            assert!(conversation.title.is_empty());
            assert!(conversation.usage.is_none() && conversation.last_response_at.is_none());
            assert!(conversation.created_at > 1_700_000_000);
            assert_eq!(state.focus, FocusState::Input);
        }
        assert_once(&mut state, c);
    }
}

#[test]
fn k3b_carryover_accept_and_decline_decide_once_for_their_conversation() {
    for (c, expected) in [
        ('y', DecisionEffect::CarryoverAccept),
        ('n', DecisionEffect::CarryoverDecline),
    ] {
        let (mut state, clock) = clocked(80, 24);
        decision_queue::request_carryover(&mut state, Some("c1".into()));
        // Another tab's key never decides this one's carryover: it waits, counted.
        paint(&mut state, &facts("c2"), 80, 24);
        ready(&mut state, &clock);
        assert_ne!(
            route(&mut state, &key(c), &facts("c2")),
            Some(RouteEffect::Action(InputAction::Decision(expected))),
            "the item waits in its own tab"
        );
        assert_eq!(decision_queue::waiting(&state), 1);
        reopen(&mut state, DecisionKind::Carryover, "c1");
        let got = effect_of(decide(&mut state, &clock, c));
        assert_eq!(got, expected, "{c}");
        assert_once(&mut state, c);

        if got == DecisionEffect::CarryoverAccept {
            // A Carryover compaction request for the active conversation, nothing kept.
            let conversation = chat("c1");
            let payload = compaction_of(got, &mut state, &conversation)
                .expect("accept issues a compaction request");
            assert!(matches!(payload.purpose, CompactionPurpose::Carryover));
            assert_eq!(payload.conversation_id, "c1");
            assert_eq!(payload.first_kept_message_id, None);
            assert!(!payload.history_text.is_empty());
            assert!(state.compacting);
            assert!(
                compaction_of(got, &mut state, &conversation).is_none(),
                "one compaction at a time"
            );
        } else {
            // Decline: a fresh tab, no carryover; the old tab keeps its conversation.
            let mut tab_manager =
                TabManager::with_conversation(chat("c1"), CancellationToken::new());
            let mut conversation = tab_manager.active_tab().conversation.clone();
            let mut streaming = StreamingState::default();
            let mut session_manager = tab_manager.active_tab().session.clone();
            let mut turn_queue = TurnQueue::default();
            let mut index = SessionIndex::build(vec![ConversationSummary {
                id: "c1".into(),
                title: "Plan".into(),
                created_at: 0,
                updated_at: 0,
                message_count: 4,
                has_fork_source: false,
            }]);
            index.set_active(Some("c1"));
            state.input_buffer = "draft".into();
            state.cursor_position = 5;
            open_fresh_tab(
                &mut TuiTurnLifecycle::default(),
                &mut tab_manager,
                &mut conversation,
                &mut streaming,
                &mut session_manager,
                &mut state,
                &mut turn_queue,
                &mut index,
            );
            assert_eq!(tab_manager.tab_count(), 2);
            assert_eq!(tab_manager.active_tab_index(), 1);
            assert_ne!(conversation.id, "c1");
            assert!(conversation.messages.is_empty(), "no carryover");
            assert_eq!(tab_manager.tabs()[1].conversation.id, conversation.id);
            assert_eq!(tab_manager.tabs()[0].conversation.id, "c1");
            assert_eq!(tab_manager.tabs()[0].conversation.messages.len(), 4);
            assert!(state.input_buffer.is_empty() && state.cursor_position == 0);
            assert_eq!(state.status, StatusState::Idle);
            assert_eq!(state.focus, FocusState::Input);
            assert!(
                !index.get("c1").unwrap().is_active,
                "the old tab is no longer active"
            );
        }
    }
}

#[test]
fn k3b_budget_continue_holds_for_the_day_and_switch_opens_the_model_selector_only() {
    let day_one = FixedNow(1_000, 86_400_000);
    // Continue: the effect is by value and its fn suppresses a re-raise for that day.
    let (mut state, clock) = clocked(80, 24);
    handle_upsert_daily_budget_warning(&mut state, &over(11.0, 0), &day_one);
    let got = effect_of(decide(&mut state, &clock, 'y'));
    assert_eq!(got, DecisionEffect::BudgetContinue);
    assert_once(&mut state, 'y');
    state.daily_budget = Some(over(11.0, 0));
    budget_continue(&mut state, &day_one);
    assert_eq!(
        state.budget_acknowledged,
        Some((86_400_000, 10.0f64.to_bits()))
    );
    handle_upsert_daily_budget_warning(&mut state, &over(25.0, 0), &day_one);
    assert!(
        state.decisions.is_empty(),
        "same day and limit: not raised again"
    );
    let day_two = FixedNow(90_000, 172_800_000);
    handle_upsert_daily_budget_warning(&mut state, &over(25.0, 0), &day_two);
    assert!(
        state.decisions.holds(DecisionKind::Budget),
        "next day re-arms"
    );

    // Switch: the arm only opens the model selector (it chooses no model).
    let (mut state, clock) = clocked(80, 24);
    handle_upsert_daily_budget_warning(&mut state, &over(11.0, 0), &day_one);
    let got = effect_of(decide(&mut state, &clock, 's'));
    assert_eq!(got, DecisionEffect::BudgetSwitchCheaper);
    assert_once(&mut state, 's');
    assert!(!presentation::is_open(&state, Surface::ModelSelector));
    budget_switch_cheaper(&mut state, &day_one, &AppConfig::default());
    assert!(presentation::is_open(&state, Surface::ModelSelector));
    assert!(presentation::consistent(&state));
    assert_eq!(state.model_selector.columns.len(), 1);
    assert_eq!(state.model_selector.columns[0].provider_id, "fixture");
    assert!(
        state.selected_model.is_none(),
        "no model was picked for the user"
    );
}

#[test]
fn k3b_budget_pause_decides_once_and_the_midnight_dismissal_suppresses_until_it_passes() {
    let (mut state, clock) = clocked(80, 24);
    let midnight = next_midnight_unix();
    let now = FixedNow(midnight - 3_600, 0);
    state.daily_budget = Some(over(11.0, 0));
    handle_upsert_daily_budget_warning(&mut state, &over(11.0, 0), &now);
    let got = effect_of(decide(&mut state, &clock, 'p'));
    assert_eq!(got, DecisionEffect::BudgetPause);
    assert_once(&mut state, 'p');
    // The persistence tail's state half (a still-queued warning is withdrawn too): the
    // dismissal is the given midnight, on the state's own budget.
    handle_upsert_daily_budget_warning(&mut state, &over(11.0, 0), &now);
    assert!(state.decisions.holds(DecisionKind::Budget));
    apply_budget_pause(&mut state, midnight);
    assert_eq!(
        state.daily_budget.as_ref().unwrap().dismissed_until_unix,
        midnight
    );
    assert!(state.decisions.is_empty(), "the warning is withdrawn");
    // Fed back with its own dismissal: paused through local midnight itself, and one
    // second past it the condition raises again.
    let paused = state.daily_budget.clone().unwrap();
    handle_upsert_daily_budget_warning(&mut state, &paused, &FixedNow(midnight, 0));
    assert!(state.decisions.is_empty(), "paused until local midnight");
    handle_upsert_daily_budget_warning(&mut state, &paused, &FixedNow(midnight + 1, 0));
    assert!(
        state.decisions.holds(DecisionKind::Budget),
        "past midnight the warning re-arms"
    );
}

#[test]
fn k3b_context_compact_decides_once_and_only_in_the_warned_conversation() {
    let (mut state, clock) = clocked(80, 24);
    handle_upsert_context_warning(&mut state, "c1", 85);
    // The other tab never decides c1's warning (the arm compacts the ACTIVE conversation).
    paint(&mut state, &facts("c2"), 80, 24);
    ready(&mut state, &clock);
    assert_ne!(
        route(&mut state, &key('k'), &facts("c2")),
        Some(RouteEffect::Action(InputAction::Decision(
            DecisionEffect::ContextCompact
        )))
    );
    assert_eq!(decision_queue::waiting(&state), 1);
    reopen(&mut state, DecisionKind::ContextWarning, "c1");
    let got = effect_of(decide(&mut state, &clock, 'k'));
    assert_eq!(got, DecisionEffect::ContextCompact);
    assert_once(&mut state, 'k');
    // An Inline compaction request for the warned conversation, keeping its last exchange.
    let conversation = chat("c1");
    let payload =
        compaction_of(got, &mut state, &conversation).expect("compact issues a compaction request");
    assert!(matches!(payload.purpose, CompactionPurpose::Inline));
    assert_eq!(payload.conversation_id, "c1");
    assert_eq!(
        payload.first_kept_message_id.as_deref(),
        Some(conversation.messages[2].id.as_str())
    );
    assert!(state.compacting);
}

#[test]
fn k3b_large_image_attach_and_cancel_decide_once_and_leave_their_marks() {
    let image = || ImageAttachment {
        media_type: "image/png".into(),
        data: "AAAA".into(),
    };
    // Attach.
    let (mut state, clock) = clocked(80, 24);
    decision_queue::raise_large_image(&mut state, image(), "Image is large (6 MB).");
    let action = match decide(&mut state, &clock, 'y') {
        Some(RouteEffect::Action(action)) => action,
        other => panic!("{other:?}"),
    };
    assert_eq!(action, InputAction::ImageConfirmAttach);
    decision_queue::apply_resolved_action(&mut state, action);
    assert_eq!(state.pending_images.len(), 1, "attached exactly once");
    assert!(state.image_indicator.is_some());
    assert_once(&mut state, 'y');
    assert_eq!(state.pending_images.len(), 1);
    // Cancel.
    let (mut state, clock) = clocked(80, 24);
    decision_queue::raise_large_image(&mut state, image(), "Image is large (6 MB).");
    let action = match decide(&mut state, &clock, 'n') {
        Some(RouteEffect::Action(action)) => action,
        other => panic!("{other:?}"),
    };
    assert_eq!(action, InputAction::ImageConfirmCancel);
    decision_queue::apply_resolved_action(&mut state, action);
    assert!(state.pending_images.is_empty(), "nothing attached");
    assert!(
        state
            .notifications
            .records()
            .any(|r| r.message.contains("Image attachment cancelled"))
    );
    assert_once(&mut state, 'n');
}

#[test]
fn k3b_a_resolved_item_hands_its_payload_over_exactly_once() {
    let (mut state, clock) = clocked(80, 24);
    decision_queue::raise_large_image(
        &mut state,
        ImageAttachment {
            media_type: "image/png".into(),
            data: "PAYLOAD".into(),
        },
        "Image is large (6 MB).",
    );
    let item = state.decisions.items().next().unwrap();
    assert!(matches!(&item.subject, DecisionSubject::LargeImage(a) if a.data == "PAYLOAD"));
    let action = match decide(&mut state, &clock, 'y') {
        Some(RouteEffect::Action(action)) => action,
        other => panic!("{other:?}"),
    };
    assert_eq!(action, InputAction::ImageConfirmAttach);
    decision_queue::apply_resolved_action(&mut state, action);
    assert_eq!(state.pending_images.len(), 1);
    assert_eq!(state.pending_images[0].data, "PAYLOAD");
    // Applying the same action again finds no resolved payload: no second attachment.
    decision_queue::apply_resolved_action(&mut state, InputAction::ImageConfirmAttach);
    assert_eq!(
        state.pending_images.len(),
        1,
        "the payload was handed over once"
    );
}
