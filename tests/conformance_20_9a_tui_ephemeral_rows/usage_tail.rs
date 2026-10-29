use super::*;
use arc_swap::ArcSwap;
use futures::StreamExt;
use rustain::adapters::noop::{
    NoOpApprovalPersistence, NoOpContext, NoOpPersona, NoOpSecurity, NoOpToolSet, NoOpUsageLedger,
};
use rustain::adapters::skill_activation::SkillActivator;
use rustain::domain::models::{
    AppConfig, CompletionOptions, Message, SessionManager, SessionState,
};
use rustain::domain::ports::{
    ContextAssemblerPort, ContextPort, StoragePort, StreamingProvider, ToolSetPort,
};
use rustain::domain::services::approval_runtime::ApprovalRuntime;
use rustain::domain::services::plan_mode_injector::DefaultPlanInjector;
use rustain::domain::services::reducer::update_streaming_mirror;
use rustain::domain::services::tool_scheduler::ToolScheduler;
use rustain::infrastructure::runtime::turn_driver::{
    LocalTurnDriver, TurnViewState, UserSubmission,
};
use rustain::infrastructure::telemetry::ActiveRatioWindow;

#[tokio::test]
async fn actual_openai_terminal_tail_usage_precedes_commit_and_persistence() {
    assert_terminal_tail_usage(false).await;
}

#[tokio::test]
async fn actual_ollama_terminal_tail_usage_precedes_commit_and_persistence() {
    assert_terminal_tail_usage(true).await;
}

async fn assert_terminal_tail_usage(ollama: bool) {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let server = wiremock::MockServer::start().await;
        let (model, path, body, input, output) = if ollama {
            ("llama3.3", "/api/chat", concat!(
                "{\"model\":\"llama3.3\",\"message\":{\"content\":\"durable usage control\"},\"done\":false}\n",
                "{\"model\":\"llama3.3\",\"done\":true,\"done_reason\":\"stop\",\"prompt_eval_count\":49,\"eval_count\":7}\n"
            ), 49, 7)
        } else {
            ("gpt-4o", "/v1/chat/completions", concat!(
                "data: {\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"durable usage control\"},\"finish_reason\":null}]}\n\n",
                "data: {\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                "data: {\"object\":\"chat.completion.chunk\",\"choices\":[],\"usage\":{\"prompt_tokens\":37,\"completion_tokens\":11,\"total_tokens\":48}}\n\n",
                "data: [DONE]\n\n"
            ), 37, 11)
        };
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(path))
            .respond_with(wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", if ollama { "application/x-ndjson" } else { "text/event-stream" })
                .set_body_string(body))
            .mount(&server).await;
        let provider: Arc<dyn StreamingProvider> = if ollama {
            Arc::new(rustain::adapters::ollama::OllamaAdapter::new(model.into(), Some(server.uri())).unwrap())
        } else {
            use rustain::adapters::openai::{ChatCredential, OpenAiAdapter, OpenAiCompatibleVariant, StaticChatProfile};
            Arc::new(OpenAiAdapter::new(
                OpenAiCompatibleVariant::OpenAI,
                ChatCredential::Fixed(rustain::domain::models::SecretString::new("fixture-key".into())),
                model.into(), StaticChatProfile::new(&format!("{}/v1", server.uri())),
            ).unwrap())
        };
        // Positive control: these actual adapters intentionally expose terminal
        // before Usage. The runtime adapter, not the provider, must order commit.
        let mut raw = provider.stream_completion(vec![Message {
            role: MessageRole::User, content: "raw adapter control".into(), images: vec![],
            tool_results: vec![], tool_uses: vec![], context_prefix: None,
            reasoning_content: None, provider_replay: vec![],
        }], CompletionOptions { session_id: None, model: model.into(), max_tokens: 64,
            system_prompt: String::new(), temperature: None, tools: vec![] }).await.unwrap();
        let mut raw_order = Vec::new();
        while let Some(chunk) = raw.next().await {
            if matches!(chunk, StreamChunk::TurnComplete { .. }) { raw_order.push("terminal"); }
            if matches!(chunk, StreamChunk::Usage { .. }) { raw_order.push("usage"); }
        }
        assert_eq!(raw_order, ["terminal", "usage"]);

        let workspace = tempfile::tempdir().unwrap();
        let storage = Arc::new(FileSystemStorage::new(workspace.path().join("sessions")));
        let (bus, mut events) = tokio::sync::mpsc::unbounded_channel();
        let security = Arc::new(NoOpSecurity);
        let tools: Arc<dyn ToolSetPort> = Arc::new(NoOpToolSet);
        let scheduler = ToolScheduler::new(security.clone(), tools.clone(), ApprovalRuntime::new(16, Arc::new(NoOpApprovalPersistence)), 16);
        let driver = LocalTurnDriver::new(
            provider, Arc::new(ArcSwap::from_pointee(AppConfig { model: model.into(), ..Default::default() })),
            bus, security, tools, scheduler, Arc::new(SkillActivator::new()), Arc::new(NoOpPersona),
            Arc::new(ArcSwap::from_pointee(Arc::new(NoOpContext) as Arc<dyn ContextPort>)),
            Arc::new(ArcSwap::from_pointee(None as Option<Arc<dyn ContextAssemblerPort>>)),
            workspace.path().to_path_buf(), storage.clone(), storage.clone() as Arc<dyn StoragePort>,
            Arc::new(DefaultPlanInjector::new()), Arc::new(NoOpUsageLedger), ActiveRatioWindow::new_in_memory(),
        );
        let mut lifecycle = TuiTurnLifecycle::default();
        let mut conversation = Conversation { id: format!("usage-tail-{ollama}"), ..Default::default() };
        let mut streaming = StreamingState::default();
        let mut state = TuiState::new(80, 24);
        let mut session = SessionManager::new(SessionState::Empty);
        let clock = MockClock::at_wall_ms(1000);
        let mut reducer = ReducerState::new(clock.wall_now_ms(), clock.now());
        driver.submit(UserSubmission {
            text: "normal local submission".into(), images: vec![], synthetic: false,
            activation_set: None, agent_snapshot: None, turn_cancel: CancellationToken::new(),
        }, TurnViewState { conversation: &mut conversation, streaming: &mut streaming,
            state: &mut state, lifecycle: &mut lifecycle, session_manager: &mut session }).await;
        let mut admitted_order = Vec::new();
        loop {
            let Some((AppEvent::ProviderChunk { chunk, .. }, _)) = lifecycle.admit(events.recv().await.unwrap()) else { continue; };
            let terminal = matches!(&chunk, StreamChunk::TurnComplete { .. });
            if matches!(&chunk, StreamChunk::Usage { .. }) { admitted_order.push("usage"); }
            if terminal { admitted_order.push("terminal"); }
            reduce(&mut reducer, chunk, &clock);
            update_streaming_mirror(&reducer, &mut streaming);
            if let Some(usage) = reducer.pending_usage.take() { conversation.usage = Some(usage); }
            if let Some(turn) = reducer.committed_turn.take() {
                conversation.turns.push(turn);
                conversation.rebuild_messages_mirror();
            }
            if terminal {
                // Match the production completion accounting/persistence order.
                state.token_usage = conversation.usage.clone();
                storage.save_conversation_with_exit(&conversation, true).await.unwrap();
                break;
            }
        }
        assert_eq!(admitted_order, ["usage", "terminal"]);
        for usage in [conversation.usage.as_ref(), state.token_usage.as_ref()] {
            let usage = usage.expect("terminal must see trailing provider usage");
            assert_eq!((usage.input_tokens, usage.output_tokens), (input, output));
        }
        let saved = storage.load_conversation(&conversation.id).await.unwrap().unwrap();
        let usage = saved.usage.unwrap();
        assert_eq!((usage.input_tokens, usage.output_tokens), (input, output));
        assert!(saved.messages.iter().any(|row| row.content == "durable usage control"));
        assert!(reducer.pending_usage.is_none() && reducer.open_turn.is_none());
        assert!(!lifecycle.is_live(&conversation.id));
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }).await.expect("real provider usage tail must complete");
}

#[tokio::test]
async fn normal_terminal_is_buffered_until_eof_with_usage_before_commit() {
    for reason in [StopReason::EndTurn, StopReason::MaxTokens] {
        let mut lifecycle = TuiTurnLifecycle::default();
        let (bus, mut events) = tokio::sync::mpsc::unbounded_channel();
        let (_, sender) =
            lifecycle.register("tail".into(), CancellationToken::new(), "model".into(), bus);
        send(&sender, "tail", text("accepted prose"));
        send(
            &sender,
            "tail",
            StreamChunk::TurnComplete {
                stop_reason: reason.clone(),
            },
        );
        send(
            &sender,
            "tail",
            StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            },
        );
        send(&sender, "tail", text("invalid transcript tail"));
        send(&sender, "tail", replay("invalid replay tail"));
        send(
            &sender,
            "tail",
            StreamChunk::Usage {
                usage: rustain::domain::models::UsageInfo {
                    input_tokens: 31,
                    output_tokens: 9,
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                    reasoning_tokens: None,
                },
                session_id: None,
            },
        );
        sender
            .send(AppEvent::SystemNotice {
                conversation_id: None,
                level: rustain::domain::models::NoticeLevel::Info,
                message: "tail barrier".into(),
            })
            .unwrap();
        let (first, _) = lifecycle.admit(events.recv().await.unwrap()).unwrap();
        assert!(
            matches!(first, AppEvent::ProviderChunk { chunk: StreamChunk::Text { content, .. }, .. } if content == "accepted prose")
        );
        let (usage, _) = lifecycle.admit(events.recv().await.unwrap()).unwrap();
        assert!(matches!(
            usage,
            AppEvent::ProviderChunk {
                chunk: StreamChunk::Usage { .. },
                ..
            }
        ));
        let (barrier, _) = lifecycle.admit(events.recv().await.unwrap()).unwrap();
        assert!(matches!(barrier, AppEvent::SystemNotice { .. }));
        assert!(
            events.try_recv().is_err(),
            "normal completion cannot precede EOF"
        );
        assert!(lifecycle.is_live("tail"));
        drop(sender);
        let (terminal, model) = lifecycle.admit(events.recv().await.unwrap()).unwrap();
        assert!(
            matches!(terminal, AppEvent::ProviderChunk { chunk: StreamChunk::TurnComplete { stop_reason }, .. } if stop_reason == reason)
        );
        assert_eq!(model.as_deref(), Some("model"));
        assert!(
            events.recv().await.is_none(),
            "no duplicate or post-terminal transcript admitted"
        );
    }
}

#[tokio::test]
async fn cancellation_closure_does_not_flush_buffered_success() {
    let mut lifecycle = TuiTurnLifecycle::default();
    let (bus, mut events) = tokio::sync::mpsc::unbounded_channel();
    let (_, sender) = lifecycle.register(
        "cancel-tail".into(),
        CancellationToken::new(),
        "model".into(),
        bus,
    );
    send(
        &sender,
        "cancel-tail",
        StreamChunk::TurnComplete {
            stop_reason: StopReason::EndTurn,
        },
    );
    sender
        .send(AppEvent::SystemNotice {
            conversation_id: None,
            level: rustain::domain::models::NoticeLevel::Info,
            message: "buffered barrier".into(),
        })
        .unwrap();
    let (barrier, _) = lifecycle.admit(events.recv().await.unwrap()).unwrap();
    assert!(matches!(barrier, AppEvent::SystemNotice { .. }));
    lifecycle.retire("cancel-tail");
    sender.closed().await;
    assert!(
        events.recv().await.is_none(),
        "cancel-driven EOF is not successful completion"
    );
}
