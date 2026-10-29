use super::*;
use arc_swap::ArcSwap;
use rustain::adapters::noop::{NoOpApprovalPersistence, NoOpContext, NoOpPersona, NoOpUsageLedger};
use rustain::adapters::security_adapter::SecurityAdapter;
use rustain::adapters::skill_activation::SkillActivator;
use rustain::domain::models::{AppConfig, tab::TabManager};
use rustain::domain::ports::{
    ContextAssemblerPort, ContextPort, SecurityPort, StoragePort, ToolSetPort,
};
use rustain::domain::services::approval_runtime::ApprovalRuntime;
use rustain::domain::services::plan_mode_injector::DefaultPlanInjector;
use rustain::domain::services::tool_scheduler::ToolScheduler;
use rustain::infrastructure::runtime::turn_driver::{
    LocalTurnDriver, TurnViewState, UserSubmission,
};
use rustain::infrastructure::telemetry::ActiveRatioWindow;

struct EchoTool;

#[async_trait::async_trait]
impl ToolSetPort for EchoTool {
    fn available_tools(&self) -> Vec<rustain::domain::models::ToolDefinition> {
        vec![rustain::domain::models::ToolDefinition {
            name: "echo".into(),
            description: "fixture".into(),
            input_schema: serde_json::json!({"type":"object"}),
            parallel_safe: true,
        }]
    }
    async fn execute(
        &self,
        _: &str,
        _: serde_json::Value,
        _: CancellationToken,
    ) -> Result<rustain::domain::models::ToolResult, rustain::domain::errors::ToolError> {
        Ok(rustain::domain::models::ToolResult {
            tool_use_id: String::new(),
            content: "tool result".into(),
            is_error: false,
            images: vec![],
        })
    }
}

struct HeldProvider(
    tokio::sync::mpsc::UnboundedSender<tokio::sync::mpsc::UnboundedSender<StreamChunk>>,
);

#[async_trait::async_trait]
impl rustain::domain::ports::StreamingProvider for HeldProvider {
    async fn stream_completion(
        &self,
        _: Vec<rustain::domain::models::Message>,
        _: rustain::domain::models::CompletionOptions,
    ) -> Result<
        futures::stream::BoxStream<'static, StreamChunk>,
        rustain::domain::errors::ProviderError,
    > {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.0.send(tx).unwrap();
        Ok(Box::pin(
            tokio_stream::wrappers::UnboundedReceiverStream::new(rx),
        ))
    }
    async fn abort(&self) -> Result<(), rustain::domain::errors::ProviderError> {
        Ok(())
    }
    fn provider_id(&self) -> String {
        "held".into()
    }
    fn list_models(&self) -> Vec<rustain::domain::models::ModelDescriptor> {
        vec![]
    }
    async fn health_check(&self) -> Result<(), rustain::domain::errors::ProviderError> {
        Ok(())
    }
    async fn connectivity_probe(
        &self,
    ) -> Result<rustain::domain::ports::ProbeOutcome, rustain::domain::errors::ProviderError> {
        Ok(rustain::domain::ports::ProbeOutcome {
            latency: std::time::Duration::ZERO,
        })
    }
}

#[tokio::test]
async fn submitted_tab_producers_are_cancelled_by_owner_and_quit_closes_both() {
    use rustain::adapters::tui::app::{InputAction, handle_input};
    use rustain::domain::events::{DomainInputEvent, DomainKey};
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
    for key in [DomainKey::CtrlC, DomainKey::CtrlQ, DomainKey::CtrlT] {
        let quit = key == DomainKey::CtrlQ;
        let workspace = tempfile::tempdir().unwrap();
        let storage = Arc::new(FileSystemStorage::new(workspace.path().join("sessions")));
        let (provider_tx, mut provider_rx) = tokio::sync::mpsc::unbounded_channel();
        let (bus, mut events) = tokio::sync::mpsc::unbounded_channel();
        let security = Arc::new(SecurityAdapter::new(workspace.path().to_path_buf()));
        security.set_mode(PermissionMode::Yolo);
        let tools: Arc<dyn ToolSetPort> = Arc::new(EchoTool);
        let approval = ApprovalRuntime::new(16, Arc::new(NoOpApprovalPersistence));
        let scheduler = ToolScheduler::new(security.clone(), tools.clone(), approval, 16);
        let driver = LocalTurnDriver::new(
            Arc::new(HeldProvider(provider_tx)), Arc::new(ArcSwap::from_pointee(AppConfig::default())), bus,
            security, tools, scheduler, Arc::new(SkillActivator::new()), Arc::new(NoOpPersona),
            Arc::new(ArcSwap::from_pointee(Arc::new(NoOpContext) as Arc<dyn ContextPort>)),
            Arc::new(ArcSwap::from_pointee(None as Option<Arc<dyn ContextAssemblerPort>>)),
            workspace.path().to_path_buf(), storage.clone(), storage as Arc<dyn StoragePort>,
            Arc::new(DefaultPlanInjector::new()), Arc::new(NoOpUsageLedger), ActiveRatioWindow::new_in_memory(),
        );
        let mut tabs = TabManager::new(CancellationToken::new());
        let mut lifecycle = TuiTurnLifecycle::default();
        let mut state = TuiState::new(80, 24);
        let mut streams = Vec::new();
        let mut ids = Vec::new();
        let mut prior = serde_json::Value::Null;
        for index in 0..2 {
            if index == 1 { tabs.create_tab(); }
            let tab = tabs.active_tab_mut();
            ids.push(tab.conversation.id.clone());
            if index == 0 {
                driver.submit(UserSubmission { text: "control prompt".into(), images: vec![], synthetic: false,
                    activation_set: None, agent_snapshot: None, turn_cancel: CancellationToken::new() },
                    TurnViewState { conversation: &mut tab.conversation, streaming: &mut tab.streaming,
                        state: &mut state, lifecycle: &mut lifecycle, session_manager: &mut tab.session }).await;
                let control = provider_rx.recv().await.unwrap();
                control.send(text("completed control")).unwrap();
                control.send(replay("control replay")).unwrap();
                control.send(StreamChunk::TurnComplete { stop_reason: StopReason::EndTurn }).unwrap();
                drop(control);
                loop {
                    let Some((AppEvent::ProviderChunk { chunk, .. }, _)) = lifecycle.admit(events.recv().await.unwrap()) else { continue };
                    let terminal = matches!(&chunk, StreamChunk::TurnComplete { stop_reason: StopReason::EndTurn });
                    reduce(&mut tab.reducer, chunk, &*tab.clock);
                    if terminal { break; }
                }
                tab.conversation.turns.push(tab.reducer.committed_turn.take().unwrap());
                tab.conversation.rebuild_messages_mirror();
                prior = serde_json::to_value(&tab.conversation.turns).unwrap();
            }
            driver.submit(UserSubmission { text: format!("prompt {index}"), images: vec![], synthetic: false,
                activation_set: None, agent_snapshot: None, turn_cancel: tab.turn_cancel.clone() },
                TurnViewState { conversation: &mut tab.conversation, streaming: &mut tab.streaming,
                    state: &mut state, lifecycle: &mut lifecycle, session_manager: &mut tab.session }).await;
            let mut stream = provider_rx.recv().await.unwrap();
            stream.send(text(&format!("held {index}"))).unwrap();
            loop {
                let Some((AppEvent::ProviderChunk { chunk, .. }, _)) = lifecycle.admit(events.recv().await.unwrap()) else { continue };
                reduce(&mut tab.reducer, chunk, &*tab.clock);
                break;
            }
            if index == 0 {
                stream.send(StreamChunk::ToolUse { id: "finished".into(), name: "echo".into(), input: serde_json::json!({}) }).unwrap();
                stream.send(StreamChunk::TurnComplete { stop_reason: StopReason::ToolUse }).unwrap();
                // A provider response ends at EOF; ToolUse alone is not EOF.
                // Close this response so run_turn can execute the real scheduler.
                drop(stream);
                loop {
                    let Some((AppEvent::ProviderChunk { chunk, .. }, _)) = lifecycle.admit(events.recv().await.unwrap()) else { continue };
                    let result = matches!(&chunk, StreamChunk::ToolResult { content, .. } if content == "tool result");
                    reduce(&mut tab.reducer, chunk, &*tab.clock);
                    if result { break; }
                }
                stream = provider_rx.recv().await.unwrap();
                stream.send(StreamChunk::Thinking { content: "second reasoning".into(), parent_tool_use_id: None }).unwrap();
                stream.send(text("second prose")).unwrap();
                stream.send(replay("cancelled replay")).unwrap();
                stream.send(StreamChunk::ToolUse { id: "unfinished".into(), name: "echo".into(), input: serde_json::json!({}) }).unwrap();
                stream.send(StreamChunk::Usage { usage: rustain::domain::models::UsageInfo {
                    input_tokens: 11, output_tokens: 7, cache_creation_input_tokens: None,
                    cache_read_input_tokens: None, reasoning_tokens: None,
                }, session_id: None }).unwrap();
                for _ in 0..5 {
                    loop {
                        let Some((AppEvent::ProviderChunk { chunk, .. }, _)) = lifecycle.admit(events.recv().await.unwrap()) else { continue };
                        reduce(&mut tab.reducer, chunk, &*tab.clock);
                        break;
                    }
                }
            }
            streams.push(stream);
        }
        streams[0].send(text("background stale")).unwrap();
        streams[0].send(replay("background replay")).unwrap();
        let mut queued = Vec::new();
        while queued.len() < 2 {
            let event = events.recv().await.unwrap();
            if matches!(&event, AppEvent::LocalProviderChunk { conversation_id, .. } if conversation_id == &ids[0]) {
                queued.push(event);
            }
        }
        tabs.switch_to_index(1);
        let mut conversation = tabs.active_tab().conversation.clone();
        let mut streaming = tabs.active_tab().streaming.clone();
        let mut model = Some("model".into());
        let action = handle_input(&mut state, &DomainInputEvent::SpecialKey(key));
        match action {
            InputAction::Quit => lifecycle.interrupt_all(&mut tabs, &mut conversation, &mut streaming, &mut model),
            InputAction::CancelOrQuit | InputAction::NewTab => lifecycle.interrupt_active(&mut tabs, &mut conversation, &mut streaming, &mut model),
            other => panic!("unexpected shortcut action {other:?}"),
        }
        streams[0].closed().await;
        for event in queued { assert!(lifecycle.admit(event).is_none()); }
        assert!(!lifecycle.is_live(&ids[0]));
        assert!(conversation.messages.iter().any(ChatMessage::is_ephemeral_display));
        let partial = conversation.messages.iter().find(|m| m.is_ephemeral_display()).unwrap();
        assert!(partial.content.contains("held 0") && partial.content.contains("second prose"));
        assert_eq!(partial.stop_reason, Some(StopReason::Cancelled));
        assert!(partial.tool_calls.iter().any(|call| call.result.as_ref().is_some_and(|result| result.content == "tool result")));
        assert!(partial.tool_calls.iter().any(|call| call.result.as_ref().is_some_and(|result| result.content == "[aborted]")));
        assert!(!serde_json::to_string(partial).unwrap().contains("cancelled replay"));
        let screen = render(&conversation, &state);
        assert!(screen.contains("held 0") && screen.contains("second prose"), "{screen}");
        assert!(!screen.contains("cancelled replay"), "{screen}");
        assert!(tabs.active_tab().reducer.open_turn.is_none() && tabs.active_tab().reducer.committed_turn.is_none());
        assert!(tabs.active_tab().reducer.pending_invocations.is_empty());
        assert!(tabs.active_tab().reducer.pending_usage.is_none());
        assert!(tabs.active_tab().reducer.liveness().active_tool_name.is_none());
        assert!(!streaming.is_streaming && model.is_none());
        assert_eq!(serde_json::to_value(&conversation.turns).unwrap(), prior);
        if quit {
            streams[1].closed().await;
            assert!(!lifecycle.is_live(&ids[1]));
            assert!(tabs.find_by_conversation(&ids[1]).unwrap().reducer.open_turn.is_none());
        } else {
            assert!(lifecycle.is_live(&ids[1]));
            streams[1].send(text("other finishes")).unwrap();
            streams[1].send(StreamChunk::TurnComplete { stop_reason: StopReason::EndTurn }).unwrap();
            drop(streams.remove(1));
            loop {
                let Some((AppEvent::ProviderChunk { chunk, .. }, _)) = lifecycle.admit(events.recv().await.unwrap()) else { continue };
                let terminal = matches!(&chunk, StreamChunk::TurnComplete { stop_reason: StopReason::EndTurn });
                let tab = tabs.find_by_conversation_mut(&ids[1]).unwrap();
                reduce(&mut tab.reducer, chunk, &*tab.clock);
                if terminal { assert!(tab.reducer.committed_turn.is_some()); break; }
            }
            let tab = tabs.active_tab_mut();
            driver.submit(UserSubmission { text: "replacement prompt".into(), images: vec![], synthetic: false,
                activation_set: None, agent_snapshot: None, turn_cancel: CancellationToken::new() },
                TurnViewState { conversation: &mut conversation, streaming: &mut streaming,
                    state: &mut state, lifecycle: &mut lifecycle, session_manager: &mut tab.session }).await;
            let replacement = provider_rx.recv().await.unwrap();
            replacement.send(text("replacement response")).unwrap();
            replacement.send(replay("replacement replay")).unwrap();
            replacement.send(StreamChunk::TurnComplete { stop_reason: StopReason::EndTurn }).unwrap();
            drop(replacement);
            loop {
                let Some((AppEvent::ProviderChunk { chunk, .. }, _)) = lifecycle.admit(events.recv().await.unwrap()) else { continue };
                let terminal = matches!(&chunk, StreamChunk::TurnComplete { stop_reason: StopReason::EndTurn });
                reduce(&mut tab.reducer, chunk, &*tab.clock);
                if terminal { break; }
            }
            let turn = tab.reducer.committed_turn.take().unwrap();
            let fresh = serde_json::to_string(&turn).unwrap();
            assert!(fresh.contains("replacement response") && fresh.contains("replacement replay"));
            assert!(!fresh.contains("second prose") && !fresh.contains("cancelled replay") && !fresh.contains("unfinished"));
            conversation.turns.push(turn);
            conversation.rebuild_messages_mirror();
            assert!(conversation.messages.iter().any(|message| message.is_ephemeral_display() && message.content.contains("second prose")));
            tabs.active_tab_mut().conversation = conversation.clone();
            tabs.switch_to_index(2);
            tabs.switch_to_index(1);
            assert!(tabs.active_tab().conversation.messages.iter().any(ChatMessage::is_ephemeral_display));
        }
        assert_durable_projection(&conversation).await;
    }
    }).await.expect("held producer handshakes must complete");
}

async fn assert_durable_projection(source: &Conversation) {
    for clean in [false, true] {
        for has_image in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let store = FileSystemStorage::new(directory.path().join("live"));
            let control_store = FileSystemStorage::new(directory.path().join("control"));
            let mut conversation = source.clone();
            let mut state = TuiState::new(80, 24);
            let id = conversation.id.clone();
            let plan = Plan {
                id: "projection-plan".into(),
                title: "durable plan data".into(),
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
                &id,
                plan,
                PermissionMode::Normal,
                None,
            );
            conversation.messages.push(ChatMessage {
                id: "durable-after-both-displays".into(),
                role: MessageRole::User,
                content: "durable after both displays".into(),
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
            conversation.rebuild_messages_mirror();
            conversation.rebuild_messages_mirror();
            assert!(
                conversation
                    .messages
                    .iter()
                    .filter(|row| row.is_ephemeral_display())
                    .count()
                    >= 2
            );
            let mut control = conversation.clone();
            control.messages.retain(|row| !row.is_ephemeral_display());
            control
                .plans
                .get_mut("projection-plan")
                .unwrap()
                .host_message_id = None;
            store
                .save_conversation_with_exit(&conversation, clean)
                .await
                .unwrap();
            control_store
                .save_conversation_with_exit(&control, clean)
                .await
                .unwrap();
            let payload = |root: &str| {
                let base = directory.path().join(root);
                let path = if has_image {
                    base.join(&id).join("conversation.json")
                } else {
                    base.join(format!("{id}.meta.json"))
                };
                serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(path).unwrap())
                    .unwrap()
            };
            let expected = payload("control");
            assert_eq!(payload("live"), expected);
            let meta = store.load_session_meta(&id).await.unwrap().unwrap();
            assert_eq!(meta.message_count, control.messages.len());
            let (mut loaded, loaded_clean) = store
                .load_conversation_with_exit(&id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(loaded_clean, clean);
            loaded.rebuild_messages_mirror();
            loaded.rebuild_messages_mirror();
            assert!(
                loaded
                    .messages
                    .iter()
                    .all(|row| !row.is_ephemeral_display())
            );
            assert!(
                loaded
                    .messages
                    .iter()
                    .any(|row| row.id == "durable-after-both-displays")
            );
            store
                .save_conversation_with_exit(&loaded, clean)
                .await
                .unwrap();
            assert_eq!(
                payload("live"),
                expected,
                "second save matches display-free control"
            );
        }
    }
}
