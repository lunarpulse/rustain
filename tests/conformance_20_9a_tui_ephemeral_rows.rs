//! Production ingress and ephemeral display projection regressions for story 20.9a.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use ratatui::{Terminal, backend::TestBackend};
use rustain::adapters::filesystem::FileSystemStorage;
use rustain::adapters::tui::handlers::plan_card::handle_plan_proposed;
use rustain::adapters::tui::state::{TabRenderState, TuiState};
use rustain::adapters::tui::theme::Theme;
use rustain::adapters::tui::widgets::chat_pane;
use rustain::domain::clock::{Clock, MockClock};
use rustain::domain::events::AppEvent;
use rustain::domain::models::plan::{Plan, PlanStatus};
use rustain::domain::models::{
    ChatMessage, ContentBlockType, Conversation, MessageRole, PermissionMode, ProviderReplayItem,
    StopReason, StreamChunk, StreamingState, ViewState,
};
use rustain::domain::services::reducer::{ReducerState, reduce};
use rustain::infrastructure::runtime::tui_turn_lifecycle::TuiTurnLifecycle;
use tokio_util::sync::CancellationToken;

fn text(value: &str) -> StreamChunk {
    StreamChunk::Text {
        content: value.into(),
        parent_tool_use_id: None,
    }
}

fn replay(value: &str) -> StreamChunk {
    StreamChunk::ProviderReplay {
        item: ProviderReplayItem {
            provider: "fixture".into(),
            model: "model".into(),
            payload: Arc::new(serde_json::json!({"canary": value})),
            scope: None,
        },
    }
}

fn send(tx: &tokio::sync::mpsc::UnboundedSender<AppEvent>, id: &str, chunk: StreamChunk) {
    tx.send(AppEvent::ProviderChunk {
        conversation_id: id.into(),
        chunk,
    })
    .unwrap();
}

fn apply(
    lifecycle: &mut TuiTurnLifecycle,
    event: AppEvent,
    reducer: &mut ReducerState,
    clock: &MockClock,
) {
    if let Some((AppEvent::ProviderChunk { chunk, .. }, _)) = lifecycle.admit(event) {
        reduce(reducer, chunk, clock);
    }
}

#[tokio::test]
async fn retirement_drains_ordered_forwarding_tail_without_dropping_unrelated_events() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut lifecycle = TuiTurnLifecycle::default();
        let (bus, mut events) = tokio::sync::mpsc::unbounded_channel();
        let (_, sender) = lifecycle.register(
            "retired".into(), CancellationToken::new(), "model".into(), bus.clone(),
        );
        send(&sender, "retired", text("stale queued chunk"));
        sender.send(AppEvent::SystemNotice {
            conversation_id: None, level: rustain::domain::models::NoticeLevel::Info,
            message: "unrelated queued notice".into(),
        }).unwrap();
        // No await has polled the forwarding task: both events are in its
        // private queue when admission is retired.
        lifecycle.retire("retired");
        assert!(lifecycle.admit(events.recv().await.unwrap()).is_none());
        let (event, _) = lifecycle.admit(events.recv().await.unwrap()).unwrap();
        assert!(matches!(event, AppEvent::SystemNotice { message, .. } if message == "unrelated queued notice"));
        sender.closed().await;
    }).await.expect("retirement must close and drain the bounded forwarding tail");
}

#[tokio::test]
async fn ingress_fences_enqueued_old_chunks_and_preserves_other_owners() {
    let clock = MockClock::at_wall_ms(1000);
    let mut reducer = ReducerState::new(clock.wall_now_ms(), clock.now());
    let mut lifecycle = TuiTurnLifecycle::default();
    let (bus, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (old_id, old) = lifecycle.register(
        "a".into(),
        CancellationToken::new(),
        "old-model".into(),
        bus.clone(),
    );
    let (_, other) = lifecycle.register(
        "other".into(),
        CancellationToken::new(),
        "other-model".into(),
        bus.clone(),
    );
    let mut conversation = Conversation {
        id: "a".into(),
        ..Default::default()
    };
    reduce(&mut reducer, text("prior control"), &clock);
    reduce(&mut reducer, replay("prior replay"), &clock);
    reduce(
        &mut reducer,
        StreamChunk::TurnComplete {
            stop_reason: StopReason::EndTurn,
        },
        &clock,
    );
    conversation
        .turns
        .push(reducer.committed_turn.take().unwrap());
    conversation.rebuild_messages_mirror();
    let prior = serde_json::to_value(&conversation.turns).unwrap();
    for chunk in [
        text("partial"),
        StreamChunk::ToolUse {
            id: "finished".into(),
            name: "tool".into(),
            input: serde_json::json!({}),
        },
        StreamChunk::TurnComplete {
            stop_reason: StopReason::ToolUse,
        },
        StreamChunk::ToolResult {
            id: "finished".into(),
            content: "finished result".into(),
            is_error: false,
            operator_private: false,
            diff: Default::default(),
            images: vec![],
        },
        text("second response"),
        StreamChunk::Thinking {
            content: "partial reasoning".into(),
            parent_tool_use_id: None,
        },
        replay("partial replay"),
        StreamChunk::ToolUse {
            id: "unfinished".into(),
            name: "tool".into(),
            input: serde_json::json!({}),
        },
    ] {
        send(&old, "a", chunk);
    }
    for _ in 0..8 {
        apply(
            &mut lifecycle,
            rx.recv().await.unwrap(),
            &mut reducer,
            &clock,
        );
    }
    let mut streaming = StreamingState::default();
    let mut pending_model = Some("old-model".into());
    for chunk in [
        text("stale text"),
        StreamChunk::Thinking {
            content: "stale thinking".into(),
            parent_tool_use_id: None,
        },
        StreamChunk::ToolUse {
            id: "old-call".into(),
            name: "old-tool".into(),
            input: serde_json::json!({}),
        },
        StreamChunk::ToolResult {
            id: "old-call".into(),
            content: "stale result".into(),
            is_error: false,
            operator_private: false,
            diff: Default::default(),
            images: vec![],
        },
        replay("stale replay"),
        StreamChunk::Usage {
            usage: rustain::domain::models::UsageInfo {
                input_tokens: 11,
                output_tokens: 7,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
                reasoning_tokens: None,
            },
            session_id: None,
        },
        StreamChunk::TurnComplete {
            stop_reason: StopReason::EndTurn,
        },
        StreamChunk::TurnComplete {
            stop_reason: StopReason::MaxTokens,
        },
    ] {
        send(&old, "a", chunk);
    }
    drop(old);
    let mut queued = Vec::new();
    for _ in 0..7 {
        queued.push(rx.recv().await.unwrap());
    }
    assert!(queued.iter().all(
        |e| matches!(e, AppEvent::LocalProviderChunk { producer_id, .. } if *producer_id == old_id)
    ));
    assert!(matches!(
        queued.last(),
        Some(AppEvent::LocalProviderChunk {
            chunk: StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn
            },
            ..
        })
    ));
    lifecycle.interrupt(
        &mut streaming,
        &mut conversation,
        &mut reducer,
        &clock,
        &mut pending_model,
    );
    assert!(!lifecycle.is_live("a"));
    assert!(lifecycle.is_live("other"));
    assert!(pending_model.is_none());
    assert!(reducer.open_turn.is_none() && reducer.committed_turn.is_none());
    assert!(reducer.pending_usage.is_none() && reducer.pending_invocations.is_empty());
    assert_eq!(serde_json::to_value(&conversation.turns).unwrap(), prior);
    let partial = conversation
        .messages
        .iter()
        .find(|m| m.is_ephemeral_display())
        .unwrap();
    assert!(partial.content.contains("partial") && partial.content.contains("second response"));
    assert!(partial.tool_calls.iter().any(|call| {
        call.result
            .as_ref()
            .is_some_and(|result| result.content == "[aborted]")
    }));
    assert!(
        !serde_json::to_string(partial)
            .unwrap()
            .contains("partial replay")
    );
    let count = conversation.messages.len();
    lifecycle.interrupt(
        &mut streaming,
        &mut conversation,
        &mut reducer,
        &clock,
        &mut pending_model,
    );
    assert_eq!(conversation.messages.len(), count);
    for event in queued.drain(..4) {
        assert!(lifecycle.admit(event).is_none());
    }
    let (_, replacement) = lifecycle.register(
        "a".into(),
        CancellationToken::new(),
        "new-model".into(),
        bus,
    );
    send(&replacement, "a", text("replacement"));
    apply(
        &mut lifecycle,
        rx.recv().await.unwrap(),
        &mut reducer,
        &clock,
    );
    for event in queued {
        assert!(lifecycle.admit(event).is_none());
    }
    assert!(lifecycle.is_live("a"));
    send(
        &other,
        "other",
        StreamChunk::TurnComplete {
            stop_reason: StopReason::EndTurn,
        },
    );
    drop(other);
    let (_, model) = lifecycle
        .admit(rx.recv().await.unwrap())
        .expect("other remains admitted");
    assert_eq!(model.as_deref(), Some("other-model"));
    send(
        &replacement,
        "a",
        StreamChunk::TurnComplete {
            stop_reason: StopReason::EndTurn,
        },
    );
    drop(replacement);
    let (event, model) = lifecycle
        .admit(rx.recv().await.unwrap())
        .expect("replacement terminal admitted");
    assert_eq!(model.as_deref(), Some("new-model"));
    if let AppEvent::ProviderChunk { chunk, .. } = event {
        reduce(&mut reducer, chunk, &clock);
    }
    let completed = reducer.committed_turn.take().unwrap();
    let encoded = serde_json::to_string(&completed).unwrap();
    assert!(encoded.contains("replacement"));
    assert!(!encoded.contains("stale"));
}

#[tokio::test]
async fn new_tab_background_rejects_all_old_chunk_kinds_before_side_effects() {
    use rustain::domain::models::tab::TabManager;
    let mut tabs = TabManager::new(CancellationToken::new());
    let old_owner = tabs.active_tab().conversation.id.clone();
    let mut conversation = tabs.active_tab().conversation.clone();
    let mut streaming = StreamingState::default();
    let mut lifecycle = TuiTurnLifecycle::default();
    let (bus, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let (_, sender) = lifecycle.register(
        old_owner.clone(),
        CancellationToken::new(),
        "old".into(),
        bus.clone(),
    );
    send(&sender, &old_owner, text("visible before NewTab"));
    let (AppEvent::ProviderChunk { chunk, .. }, _) =
        lifecycle.admit(receiver.recv().await.unwrap()).unwrap()
    else {
        panic!("provider chunk")
    };
    let tab = tabs.active_tab_mut();
    reduce(&mut tab.reducer, chunk, &*tab.clock);
    let stale = [
        text("background stale text"),
        StreamChunk::Thinking {
            content: "background stale thinking".into(),
            parent_tool_use_id: None,
        },
        StreamChunk::ToolUse {
            id: "old".into(),
            name: "echo".into(),
            input: serde_json::json!({}),
        },
        StreamChunk::ToolResult {
            id: "old".into(),
            content: "background stale result".into(),
            is_error: false,
            operator_private: false,
            diff: Default::default(),
            images: vec![],
        },
        replay("background stale replay"),
        StreamChunk::Usage {
            usage: rustain::domain::models::UsageInfo {
                input_tokens: 999,
                output_tokens: 0,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
                reasoning_tokens: None,
            },
            session_id: None,
        },
        StreamChunk::TurnComplete {
            stop_reason: StopReason::MaxTokens,
        },
        StreamChunk::TurnComplete {
            stop_reason: StopReason::EndTurn,
        },
    ];
    for chunk in stale {
        send(&sender, &old_owner, chunk);
    }
    drop(sender);
    let mut enqueued = Vec::new();
    for _ in 0..7 {
        enqueued.push(receiver.recv().await.unwrap());
    }
    assert!(matches!(
        enqueued.last(),
        Some(AppEvent::LocalProviderChunk {
            chunk: StreamChunk::TurnComplete {
                stop_reason: StopReason::MaxTokens
            },
            ..
        })
    ));
    lifecycle.interrupt_active(
        &mut tabs,
        &mut conversation,
        &mut streaming,
        &mut Some("old".into()),
    );
    tabs.active_tab_mut().conversation = conversation;
    tabs.create_tab();
    let new_owner = tabs.active_tab().conversation.id.clone();
    let (_, current) = lifecycle.register(
        new_owner.clone(),
        CancellationToken::new(),
        "current".into(),
        bus,
    );
    send(&current, &new_owner, text("accepted current"));
    for old in enqueued {
        assert!(lifecycle.admit(old).is_none());
    }
    let (AppEvent::ProviderChunk { chunk, .. }, _) =
        lifecycle.admit(receiver.recv().await.unwrap()).unwrap()
    else {
        panic!("current text")
    };
    let tab = tabs.active_tab_mut();
    reduce(&mut tab.reducer, chunk, &*tab.clock);
    send(
        &current,
        &new_owner,
        StreamChunk::TurnComplete {
            stop_reason: StopReason::MaxTokens,
        },
    );
    drop(current);
    let (AppEvent::ProviderChunk { chunk, .. }, model) =
        lifecycle.admit(receiver.recv().await.unwrap()).unwrap()
    else {
        panic!("current terminal")
    };
    assert_eq!(model.as_deref(), Some("current"));
    let tab = tabs.active_tab_mut();
    reduce(&mut tab.reducer, chunk, &*tab.clock);
    assert!(tab.reducer.committed_turn.is_some());
    let background = tabs.find_by_conversation(&old_owner).unwrap();
    assert!(background.reducer.open_turn.is_none() && background.reducer.committed_turn.is_none());
    assert!(background.reducer.pending_usage.is_none() && background.conversation.usage.is_none());
    assert!(background.conversation.turns.is_empty());
    assert_eq!(
        background
            .conversation
            .messages
            .iter()
            .filter(|row| row.is_ephemeral_display())
            .count(),
        1
    );
    assert!(
        !serde_json::to_string(&background.conversation)
            .unwrap()
            .contains("background stale")
    );
}

#[test]
fn interruption_handles_empty_reasoning_and_unfinished_tool_turns_idempotently() {
    let clock = MockClock::at_wall_ms(1000);
    for kind in 0..3 {
        let mut reducer = ReducerState::new(clock.wall_now_ms(), clock.now());
        let mut lifecycle = TuiTurnLifecycle::default();
        let mut conversation = Conversation {
            id: format!("case-{kind}"),
            ..Default::default()
        };
        let mut streaming = StreamingState::default();
        let mut model = Some("model".into());
        match kind {
            1 => {
                reduce(
                    &mut reducer,
                    StreamChunk::Thinking {
                        content: "reasoning only".into(),
                        parent_tool_use_id: None,
                    },
                    &clock,
                );
            }
            2 => {
                reduce(
                    &mut reducer,
                    StreamChunk::ToolUse {
                        id: "unfinished".into(),
                        name: "tool".into(),
                        input: serde_json::json!({}),
                    },
                    &clock,
                );
            }
            _ => {}
        }
        lifecycle.interrupt(
            &mut streaming,
            &mut conversation,
            &mut reducer,
            &clock,
            &mut model,
        );
        assert!(reducer.open_turn.is_none() && reducer.committed_turn.is_none());
        assert!(reducer.pending_invocations.is_empty());
        assert!(model.is_none());
        assert!(conversation.turns.is_empty());
        assert_eq!(conversation.messages.len(), usize::from(kind != 0));
        if kind != 0 {
            assert!(conversation.messages[0].is_ephemeral_display());
        }
        if kind == 2 {
            assert_eq!(
                conversation.messages[0].tool_calls[0]
                    .result
                    .as_ref()
                    .unwrap()
                    .content,
                "[aborted]"
            );
        }
        lifecycle.interrupt(
            &mut streaming,
            &mut conversation,
            &mut reducer,
            &clock,
            &mut model,
        );
        assert_eq!(conversation.messages.len(), usize::from(kind != 0));
    }
}

struct DropSignal(Option<tokio::sync::oneshot::Sender<()>>);
impl Drop for DropSignal {
    fn drop(&mut self) {
        if let Some(signal) = self.0.take() {
            let _ = signal.send(());
        }
    }
}

#[tokio::test]
async fn conversation_owned_interrupt_and_quit_stop_only_their_tasks() {
    use rustain::domain::models::tab::TabManager;
    for quit in [false, true] {
        let mut tabs = TabManager::new(CancellationToken::new());
        let a = tabs.active_tab().conversation.id.clone();
        tabs.create_tab();
        let b = tabs.active_tab().conversation.id.clone();
        let mut lifecycle = TuiTurnLifecycle::default();
        let (bus, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut dropped = Vec::new();
        let mut release = Vec::new();
        let mut senders = Vec::new();
        for id in [&a, &b] {
            let (identity, sender) = lifecycle.register(
                id.clone(),
                CancellationToken::new(),
                "model".into(),
                bus.clone(),
            );
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let (drop_tx, drop_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
            let task = tokio::spawn(async move {
                let _guard = DropSignal(Some(drop_tx));
                let _ = started_tx.send(());
                let _ = release_rx.await;
            });
            lifecycle.install_task(id, identity, task);
            started_rx.await.unwrap();
            dropped.push(drop_rx);
            release.push(release_tx);
            senders.push(sender);
            send(senders.last().unwrap(), id, text("held partial"));
            let (AppEvent::ProviderChunk { chunk, .. }, _) =
                lifecycle.admit(rx.recv().await.unwrap()).unwrap()
            else {
                panic!("expected chunk")
            };
            let tab = tabs.find_by_conversation_mut(id).unwrap();
            reduce(&mut tab.reducer, chunk, &*tab.clock);
        }
        tabs.switch_to_index(1);
        let mut conversation = tabs.active_tab().conversation.clone();
        let mut streaming = StreamingState::default();
        let mut pending_model = Some("model".into());
        if quit {
            lifecycle.interrupt_all(
                &mut tabs,
                &mut conversation,
                &mut streaming,
                &mut pending_model,
            );
        } else {
            lifecycle.interrupt_active(
                &mut tabs,
                &mut conversation,
                &mut streaming,
                &mut pending_model,
            );
        }
        assert!(dropped.remove(0).await.is_ok());
        assert!(!lifecycle.is_live(&a));
        assert!(
            conversation
                .messages
                .iter()
                .any(ChatMessage::is_ephemeral_display)
        );
        assert!(tabs.active_tab().reducer.open_turn.is_none());
        if quit {
            assert!(dropped.remove(0).await.is_ok());
            let background = tabs.find_by_conversation(&b).unwrap();
            assert!(!lifecycle.is_live(&b));
            assert!(background.reducer.open_turn.is_none());
            assert!(
                background
                    .conversation
                    .messages
                    .iter()
                    .any(ChatMessage::is_ephemeral_display)
            );
        } else {
            assert!(lifecycle.is_live(&b));
            send(
                &senders[1],
                &b,
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::EndTurn,
                },
            );
            drop(senders.remove(1));
            let (AppEvent::ProviderChunk { chunk, .. }, _) =
                lifecycle.admit(rx.recv().await.unwrap()).unwrap()
            else {
                panic!("expected live terminal")
            };
            let tab = tabs.find_by_conversation_mut(&b).unwrap();
            reduce(&mut tab.reducer, chunk, &*tab.clock);
            assert!(tab.reducer.committed_turn.is_some());
            let _ = release.pop().unwrap().send(());
            assert!(dropped.remove(0).await.is_ok());
        }
    }
}

fn render(conversation: &Conversation, state: &TuiState) -> String {
    let mut terminal = Terminal::new(TestBackend::new(80, 40)).unwrap();
    let mut tab = TabRenderState::default();
    terminal
        .draw(|frame| {
            let area = frame.area();
            chat_pane::render_with_search(
                frame,
                area,
                conversation,
                None,
                &StreamingState::default(),
                &ViewState::default(),
                &MockClock::at_wall_ms(1000),
                0,
                true,
                &Theme::dark(),
                &mut tab,
                &HashMap::new(),
                &BTreeMap::new(),
                None,
                None,
                &[],
                &[],
                state.pending_plan_card.as_ref(),
                None,
                None,
                None,
                None,
            );
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    (0..buffer.area().height)
        .map(|y| {
            (0..buffer.area().width)
                .map(|x| buffer.cell((x, y)).unwrap().symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn plan_host_renders_and_real_save_reload_discards_display_only() {
    use rustain::domain::ports::StoragePort;
    for (clean, has_image) in [(false, false), (true, false), (false, true), (true, true)] {
        let dir = tempfile::tempdir().unwrap();
        let store = FileSystemStorage::new(dir.path().join("sessions"));
        let mut conversation = Conversation {
            id: "plan-session".into(),
            ..Default::default()
        };
        conversation.messages.push(ChatMessage {
            id: "user".into(),
            role: MessageRole::User,
            content: "durable user".into(),
            ..Default::default()
        });
        if has_image {
            conversation.messages[0].images.push(
                rustain::domain::models::conversation::ImageReference {
                    file_name: "0123456789abcdef.png".into(),
                    media_type: "image/png".into(),
                    original_size: 8,
                },
            );
        }
        let mut state = TuiState::new(80, 40);
        let plan = Plan {
            id: "plan".into(),
            title: "ephemeral card canary".into(),
            tasks: vec![],
            estimated_effort: None,
            status: PlanStatus::Pending,
            created_at: 1,
            resolved_at: None,
            host_message_id: None,
        };
        handle_plan_proposed(
            &mut conversation,
            &mut state,
            "plan-session",
            plan,
            PermissionMode::Normal,
            None,
        );
        let host = conversation.plans["plan"].host_message_id.clone().unwrap();
        assert!(conversation.messages.iter().any(|m| m.id == host
            && m.is_ephemeral_display()
            && m.content_blocks.contains(&ContentBlockType::PlanCard)));
        assert!(render(&conversation, &state).contains("ephemeral card canary"));
        conversation.messages.push(ChatMessage {
            id: "later".into(),
            role: MessageRole::User,
            content: "later durable".into(),
            ..Default::default()
        });
        conversation.rebuild_messages_mirror();
        assert!(render(&conversation, &state).contains("ephemeral card canary"));
        store
            .save_conversation_with_exit(&conversation, clean)
            .await
            .unwrap();
        assert_eq!(dir.path().join("sessions/plan-session").is_dir(), has_image);
        let meta = store
            .load_session_meta("plan-session")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(meta.message_count, 2);
        let path = if has_image {
            dir.path().join("sessions/plan-session/conversation.json")
        } else {
            dir.path().join("sessions/plan-session.meta.json")
        };
        let saved = std::fs::read_to_string(path).unwrap();
        assert!(!saved.contains(&host));
        assert!(!saved.contains("ephemeralDisplay"));
        let mut tabs = rustain::domain::models::tab::TabManager::new(CancellationToken::new());
        tabs.active_tab_mut().session_meta = meta;
        state.message_boundaries = vec![0, 10, 20];
        state.total_content_height = 40;
        state.viewport_height = 10;
        state.set_auto_scroll(false);
        state.set_scroll_offset(20);
        rustain::adapters::tui::handlers::bookmark::handle_apply_bookmark_toggle(
            &mut tabs,
            &mut state,
            &conversation,
            &store,
        )
        .await;
        assert!(
            tabs.active_tab().session_meta.bookmarks.is_empty(),
            "temporary target rejected"
        );
        state.set_auto_scroll(true);
        rustain::adapters::tui::handlers::bookmark::handle_apply_bookmark_toggle(
            &mut tabs,
            &mut state,
            &conversation,
            &store,
        )
        .await;
        assert_eq!(tabs.active_tab().session_meta.bookmarks, [1]);
        assert_eq!(
            store
                .load_session_meta("plan-session")
                .await
                .unwrap()
                .unwrap()
                .bookmarks,
            [1]
        );
        rustain::adapters::tui::handlers::bookmark::handle_apply_jump_bookmark(
            &tabs,
            &mut state,
            &conversation,
        );
        assert_eq!(state.scroll_offset(), 10);
        let (mut loaded, loaded_clean) = store
            .load_conversation_with_exit("plan-session")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded_clean, clean);
        assert!(
            loaded
                .messages
                .iter()
                .all(|m| !m.is_ephemeral_display() && m.id != host)
        );
        assert!(loaded.messages.iter().any(|m| m.id == "later"));
        assert_eq!(loaded.plans["plan"].host_message_id, None);
        state.message_boundaries = vec![0, 10];
        rustain::adapters::tui::handlers::bookmark::handle_apply_jump_bookmark(
            &tabs, &mut state, &loaded,
        );
        assert_eq!(state.scroll_offset(), 20);
        loaded.rebuild_messages_mirror();
        loaded.rebuild_messages_mirror();
        store
            .save_conversation_with_exit(&loaded, clean)
            .await
            .unwrap();
        let (loaded_again, _) = store
            .load_conversation_with_exit("plan-session")
            .await
            .unwrap()
            .unwrap();
        assert!(loaded_again.messages.iter().all(|m| m.id != host));
        assert!(loaded_again.messages.iter().any(|m| m.id == "later"));
    }
}

struct Replies(std::sync::atomic::AtomicUsize);

impl wiremock::Respond for Replies {
    fn respond(&self, _: &wiremock::Request) -> wiremock::ResponseTemplate {
        let n = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let answer = match n {
            0 => "completed control",
            1 => "cancelled canary",
            _ => "fresh response",
        };
        let marker = match n {
            0 => "control-replay",
            1 => "cancelled-replay",
            _ => "fresh-replay",
        };
        let events = [
            serde_json::json!({"type":"response.output_text.delta","delta":answer}),
            serde_json::json!({"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":format!("rs_{n}"),"summary":[],"encrypted_content":marker}}),
            serde_json::json!({"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":10,"output_tokens":3}}}),
        ];
        let body = events
            .into_iter()
            .map(|data| format!("data: {data}\n\n"))
            .collect::<String>();
        wiremock::ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(body)
    }
}

#[tokio::test]
async fn real_submission_excludes_cancelled_display_from_next_request_and_rebuild() {
    use arc_swap::ArcSwap;
    use rustain::adapters::noop::{
        NoOpApprovalPersistence, NoOpContext, NoOpPersona, NoOpSecurity, NoOpToolSet,
        NoOpUsageLedger,
    };
    use rustain::adapters::skill_activation::SkillActivator;
    use rustain::domain::models::{AppConfig, SessionManager, SessionState};
    use rustain::domain::ports::{ContextAssemblerPort, ContextPort, StoragePort, ToolSetPort};
    use rustain::domain::services::approval_runtime::ApprovalRuntime;
    use rustain::domain::services::plan_mode_injector::DefaultPlanInjector;
    use rustain::domain::services::tool_scheduler::ToolScheduler;
    use rustain::infrastructure::runtime::turn_driver::{
        LocalTurnDriver, TurnViewState, UserSubmission,
    };
    use rustain::infrastructure::telemetry::ActiveRatioWindow;
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let workspace = tempfile::tempdir().unwrap();
        let storage = Arc::new(FileSystemStorage::new(workspace.path().join("sessions")));
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/responses"))
            .respond_with(Replies(Default::default()))
            .mount(&server)
            .await;
        let credentials = rustain::infrastructure::key_resolver::CredentialHandle::runtime(
            rustain::infrastructure::key_resolver::shared(),
            "xai".into(),
            rustain::domain::models::SecretString::new("fixture-key".into()),
        );
        let provider = Arc::new(
            rustain::adapters::xai::XaiAdapter::new(
                credentials,
                rustain::adapters::xai::XaiKind::Xai,
                Some(format!("{}/v1", server.uri())),
            )
            .unwrap(),
        );
        let (bus, mut events) = tokio::sync::mpsc::unbounded_channel();
        let security = Arc::new(NoOpSecurity);
        let tools: Arc<dyn ToolSetPort> = Arc::new(NoOpToolSet);
        let approval = ApprovalRuntime::new(16, Arc::new(NoOpApprovalPersistence));
        let scheduler = ToolScheduler::new(security.clone(), tools.clone(), approval, 16);
        let driver = LocalTurnDriver::new(
            provider,
            Arc::new(ArcSwap::from_pointee(AppConfig {
                model: "grok-4".into(),
                ..Default::default()
            })),
            bus,
            security,
            tools,
            scheduler,
            Arc::new(SkillActivator::new()),
            Arc::new(NoOpPersona),
            Arc::new(ArcSwap::from_pointee(
                Arc::new(NoOpContext) as Arc<dyn ContextPort>
            )),
            Arc::new(ArcSwap::from_pointee(
                None as Option<Arc<dyn ContextAssemblerPort>>,
            )),
            workspace.path().to_path_buf(),
            storage.clone(),
            storage.clone() as Arc<dyn StoragePort>,
            Arc::new(DefaultPlanInjector::new()),
            Arc::new(NoOpUsageLedger),
            ActiveRatioWindow::new_in_memory(),
        );
        let clock = MockClock::at_wall_ms(1000);
        let mut reducer = ReducerState::new(clock.wall_now_ms(), clock.now());
        let mut conversation = Conversation {
            id: "request-session".into(),
            ..Default::default()
        };
        let mut lifecycle = TuiTurnLifecycle::default();
        let mut state = TuiState::new(80, 24);
        let mut streaming = StreamingState::default();
        let mut session = SessionManager::new(SessionState::Empty);
        for (index, prompt) in [
            "control prompt",
            "cancel prompt",
            "next prompt",
            "reload prompt",
        ]
        .into_iter()
        .enumerate()
        {
            if index == 2 {
                assert_eq!(conversation.messages.iter().filter(|m| m.is_ephemeral_display()).count(), 2);
                session.mark_invalidated(None);
            }
            if index == 3 {
                storage
                    .save_conversation_with_exit(&conversation, true)
                    .await
                    .unwrap();
                conversation = storage
                    .load_conversation_with_exit("request-session")
                    .await
                    .unwrap()
                    .unwrap()
                    .0;
                session.mark_invalidated(None);
            }
            driver
                .submit(
                    UserSubmission {
                        text: prompt.into(),
                        images: vec![],
                        synthetic: false,
                        activation_set: None,
                        agent_snapshot: None,
                        turn_cancel: CancellationToken::new(),
                    },
                    TurnViewState {
                        conversation: &mut conversation,
                        streaming: &mut streaming,
                        state: &mut state,
                        lifecycle: &mut lifecycle,
                        session_manager: &mut session,
                    },
                )
                .await;
            loop {
                let event = events.recv().await.unwrap();
                let Some((AppEvent::ProviderChunk { chunk, .. }, _)) = lifecycle.admit(event)
                else {
                    continue;
                };
                let cancel_here =
                    index == 1 && matches!(&chunk, StreamChunk::ProviderReplay { .. });
                let terminal = matches!(
                    &chunk,
                    StreamChunk::TurnComplete {
                        stop_reason: StopReason::EndTurn
                    }
                );
                reduce(&mut reducer, chunk, &clock);
                if cancel_here {
                    let mut model = Some("fixture".into());
                    lifecycle.interrupt(
                        &mut streaming,
                        &mut conversation,
                        &mut reducer,
                        &clock,
                        &mut model,
                    );
                    assert!(reducer.open_turn.is_none() && reducer.committed_turn.is_none());
                    let plan = Plan {
                        id: "request-plan".into(),
                        title: "plan request canary".into(),
                        tasks: vec![],
                        estimated_effort: None,
                        status: PlanStatus::Pending,
                        created_at: 1,
                        resolved_at: None,
                        host_message_id: None,
                    };
                    handle_plan_proposed(
                        &mut conversation,
                        &mut state,
                        "request-session",
                        plan,
                        PermissionMode::Normal,
                        None,
                    );
                    break;
                }
                if terminal {
                    conversation
                        .turns
                        .push(reducer.committed_turn.take().unwrap());
                    conversation.rebuild_messages_mirror();
                    break;
                }
            }
        }
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 4);
        let rebuilt_wire = String::from_utf8_lossy(&requests[2].body);
        assert!(
            rebuilt_wire.contains("Previous conversation context (3 messages):\\n[User]: control prompt\\n[Assistant]: completed control\\n[User]: cancel prompt"),
            "{rebuilt_wire}"
        );
        assert!(rebuilt_wire.contains("next prompt"), "{rebuilt_wire}");
        let rebuilt_request: serde_json::Value = serde_json::from_slice(&requests[2].body).unwrap();
        let assistant_entries: Vec<_> = rebuilt_request["input"].as_array().unwrap().iter()
            .filter(|entry| entry.get("role").and_then(serde_json::Value::as_str) == Some("assistant"))
            .collect();
        assert_eq!(assistant_entries.len(), 1, "no extra empty assistant host in the live rebuilt request");
        assert!(serde_json::to_string(assistant_entries[0]).unwrap().contains("completed control"));
        for request in &requests[2..] {
            let wire = String::from_utf8_lossy(&request.body);
            assert!(wire.contains("completed control"), "{wire}");
            assert!(wire.contains("control-replay"), "{wire}");
            assert!(!wire.contains("cancelled-replay"), "{wire}");
            assert!(!wire.contains("cancelled canary"), "{wire}");
            assert!(!wire.contains("plan request canary"), "{wire}");
        }
        assert!(String::from_utf8_lossy(&requests[3].body).contains("reload prompt"));
    })
    .await
    .expect("loopback submissions must complete");
}

#[path = "conformance_20_9a_tui_ephemeral_rows/held.rs"]
mod held;
#[path = "conformance_20_9a_tui_ephemeral_rows/targets.rs"]
mod targets;
#[path = "conformance_20_9a_tui_ephemeral_rows/usage_tail.rs"]
mod usage_tail;
