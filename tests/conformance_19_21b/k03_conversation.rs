use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use futures::stream::BoxStream;
use rustain::adapters::a2a::admission::A2aAdmissionPolicy;
use rustain::adapters::composite_toolset_adapter::CompositeToolsetAdapter;
use rustain::adapters::tui::app::InputAction;
use rustain::domain::errors::{ProviderError, ToolError};
use rustain::domain::events::{AppEvent, DomainInputEvent};
use rustain::domain::models::checkpoint::CheckpointId;
use rustain::domain::models::{
    CompletionOptions, JournalRecord, PermissionMode, ResponseMode, RoomEvent, SemanticMessageType,
    StopReason, StreamChunk, ToolDefinition, ToolResult,
};
use rustain::domain::ports::{ProbeOutcome, StreamingProvider, ToolSetPort};
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::observed_host::{HttpCounts, ObservedHost};
use super::proposal_harness::ProposalFixture;

const TURN_TIMEOUT: Duration = Duration::from_secs(45);
const BODY_A: &str = "A's conversation-bound architecture draft.";
const BODY_B: &str = "B's distinct status snapshot.";

struct ScriptProvider {
    input: Value,
    calls: AtomicUsize,
    first_call_entered: Option<Arc<Notify>>,
    release_first_call: Option<Arc<Notify>>,
}

impl ScriptProvider {
    fn immediate(input: Value) -> Self {
        Self {
            input,
            calls: AtomicUsize::new(0),
            first_call_entered: None,
            release_first_call: None,
        }
    }

    fn barrier(input: Value, entered: Arc<Notify>, release: Arc<Notify>) -> Self {
        Self {
            input,
            calls: AtomicUsize::new(0),
            first_call_entered: Some(entered),
            release_first_call: Some(release),
        }
    }
}

#[async_trait::async_trait]
impl StreamingProvider for ScriptProvider {
    async fn stream_completion(
        &self,
        _messages: Vec<rustain::domain::models::Message>,
        _options: CompletionOptions,
    ) -> Result<BoxStream<'static, StreamChunk>, ProviderError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let chunks = if call == 0 {
            if let (Some(entered), Some(release)) =
                (&self.first_call_entered, &self.release_first_call)
            {
                // The provider is entered only after LocalTurnDriver has prepared this turn.
                entered.notify_one();
                release.notified().await;
            }
            vec![
                StreamChunk::ToolUse {
                    id: format!("interleaved-proposal-{call}"),
                    name: "propose_team_message".into(),
                    input: self.input.clone(),
                },
                StreamChunk::TurnComplete {
                    stop_reason: StopReason::ToolUse,
                },
            ]
        } else {
            vec![StreamChunk::TurnComplete {
                stop_reason: StopReason::EndTurn,
            }]
        };
        Ok(Box::pin(futures::stream::iter(chunks)))
    }

    async fn abort(&self) -> Result<(), ProviderError> {
        Ok(())
    }

    fn provider_id(&self) -> String {
        "interleaved-proposal-proof".into()
    }

    fn list_models(&self) -> Vec<rustain::domain::models::ModelDescriptor> {
        vec![]
    }

    async fn health_check(&self) -> Result<(), ProviderError> {
        Ok(())
    }

    async fn connectivity_probe(&self) -> Result<ProbeOutcome, ProviderError> {
        Ok(ProbeOutcome {
            latency: Duration::ZERO,
        })
    }
}

/// A transparent driver-side port gate. Both context setters still reach the
/// same real composite; the gate only pauses after that composite has accepted
/// each context and before the scheduler dispatches the corresponding tool.
struct InterleaveGate {
    first_ready: Notify,
    release_first: Notify,
    second_ready: Notify,
    release_second: Notify,
    contexts_seen: AtomicUsize,
}

impl InterleaveGate {
    fn new() -> Self {
        Self {
            first_ready: Notify::new(),
            release_first: Notify::new(),
            second_ready: Notify::new(),
            release_second: Notify::new(),
            contexts_seen: AtomicUsize::new(0),
        }
    }
}

struct GatedCompositePort {
    composite: Arc<CompositeToolsetAdapter>,
    gate: Arc<InterleaveGate>,
}

#[async_trait::async_trait]
impl ToolSetPort for GatedCompositePort {
    fn available_tools(&self) -> Vec<ToolDefinition> {
        self.composite.available_tools()
    }

    fn workspace_root(&self) -> Option<std::path::PathBuf> {
        self.composite.workspace_root()
    }

    async fn execute(
        &self,
        tool_name: &str,
        input: Value,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        self.composite.execute(tool_name, input, cancel).await
    }

    async fn set_execution_context(
        &self,
        conversation_id: String,
        checkpoint: CheckpointId,
        activation_depth: u8,
    ) {
        self.composite
            .set_execution_context(conversation_id, checkpoint, activation_depth)
            .await;
    }

    async fn set_parent_context(
        &self,
        parent_ctx_tokens: u32,
        parent_trace: Option<rustain::domain::models::TraceContext>,
        parent_tool_restriction: Option<rustain::domain::models::AgentToolRestriction>,
    ) {
        self.composite
            .set_parent_context(parent_ctx_tokens, parent_trace, parent_tool_restriction)
            .await;
        match self.gate.contexts_seen.fetch_add(1, Ordering::SeqCst) {
            0 => {
                self.gate.first_ready.notify_one();
                self.gate.release_first.notified().await;
            }
            1 => {
                self.gate.second_ready.notify_one();
                self.gate.release_second.notified().await;
            }
            count => panic!("unexpected execution-context gate #{count}"),
        }
    }
}

fn valid_a() -> Value {
    json!({"recipients":["lena-po"],"type":"architecture_update","body":BODY_A})
}

fn valid_b() -> Value {
    json!({"recipients":["jun-dev"],"type":"status_request","body":BODY_B})
}

fn tool_result(events: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>) -> (String, bool) {
    let mut results = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let AppEvent::ProviderChunk {
            chunk: StreamChunk::ToolResult {
                content, is_error, ..
            },
            ..
        } = event
        {
            results.push((content, is_error));
        }
    }
    assert_eq!(results.len(), 1, "one real proposal invocation per turn");
    results.remove(0)
}

async fn wait_for(notify: &Notify, phase: &str) {
    tokio::time::timeout(TURN_TIMEOUT, notify.notified())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {phase}"));
}

async fn finish_turn(handle: tokio::task::JoinHandle<()>, phase: &str) {
    tokio::time::timeout(TURN_TIMEOUT, handle)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {phase}"))
        .unwrap_or_else(|error| panic!("{phase} task failed: {error}"));
}

async fn show_active_tab(fixture: &mut ProposalFixture) {
    fixture.conversation = fixture.tabs.active_tab().conversation.clone();
    fixture.state.active_tab_id = fixture.tabs.active_tab_id();
    rustain::infrastructure::runtime::team_proposal_bridge::synchronize_surface(
        &mut fixture.state,
        &fixture.sender.app_state,
        &fixture.tabs,
        &fixture.conversation,
    )
    .await;
}

#[tokio::test]
async fn k03_interleaved_local_turns_keep_each_proposal_on_its_tab() {
    let lena = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let jun = ObservedHost::start(A2aAdmissionPolicy::Allow, ResponseMode::NotifyAndWait).await;
    let mut fixture = ProposalFixture::start(vec![lena.peer("lena-po"), jun.peer("jun-dev")]).await;
    fixture.set_mode(PermissionMode::Yolo);
    lena.reset_counts();
    jun.reset_counts();

    let conversation_a = fixture.tabs.active_tab().conversation.id.clone();
    let provider_a_entered = Arc::new(Notify::new());
    let release_provider_a = Arc::new(Notify::new());
    let gate = Arc::new(InterleaveGate::new());
    let gated_tools: Arc<dyn ToolSetPort> = Arc::new(GatedCompositePort {
        composite: fixture.tools.clone(),
        gate: gate.clone(),
    });
    let provider_a = Arc::new(ScriptProvider::barrier(
        valid_a(),
        provider_a_entered.clone(),
        release_provider_a.clone(),
    ));
    let (turn_a, mut events_a) = fixture
        .submit_tab_with_provider(
            &conversation_a,
            "scripted turn A",
            provider_a,
            gated_tools.clone(),
        )
        .await;

    // stream_completion is entered after LocalTurnDriver::submit has prepared A.
    wait_for(&provider_a_entered, "provider A preparation barrier").await;

    fixture.tabs.create_tab();
    let conversation_b = fixture.tabs.active_tab().conversation.id.clone();
    assert_ne!(conversation_a, conversation_b);
    show_active_tab(&mut fixture).await;

    // A reaches the real composite context setter first and waits before scheduler dispatch.
    release_provider_a.notify_one();
    wait_for(&gate.first_ready, "A's real composite execution context").await;

    // B's front door calls the same LocalTurnDriver builder, composite and scheduler.
    let provider_b = Arc::new(ScriptProvider::immediate(valid_b()));
    let (turn_b, mut events_b) = fixture
        .submit_tab_with_provider(&conversation_b, "scripted turn B", provider_b, gated_tools)
        .await;
    wait_for(
        &gate.second_ready,
        "B overwriting the shared composite context",
    )
    .await;

    // The underlying composite now holds B's context; execute A first, then B.
    gate.release_first.notify_one();
    finish_turn(turn_a, "A completion").await;
    gate.release_second.notify_one();
    finish_turn(turn_b, "B completion").await;

    let (result_a, error_a) = tool_result(&mut events_a);
    let (result_b, error_b) = tool_result(&mut events_b);
    assert_eq!(
        result_a,
        "Proposal shown to the operator. Nothing has been sent; the operator decides whether to send it to lena-po.",
        "K03 invocation-scoped proposals remain independent"
    );
    assert_eq!(
        result_b,
        "Proposal shown to the operator. Nothing has been sent; the operator decides whether to send it to jun-dev.",
        "K03 invocation-scoped proposals remain independent"
    );
    assert!(!error_a && !error_b);

    while let Ok(event) = fixture.sender.domain_rx.try_recv() {
        if let AppEvent::TeamProposalReady { proposal } = event {
            rustain::infrastructure::runtime::team_proposal_bridge::proposal_ready(
                &mut fixture.state,
                &fixture.sender.app_state,
                proposal,
            )
            .await;
        }
    }
    let snapshot_a = fixture
        .state
        .pending_team_proposals
        .get(&conversation_a)
        .expect("A's production ready event is bound to conversation A");
    let snapshot_b = fixture
        .state
        .pending_team_proposals
        .get(&conversation_b)
        .expect("B's production ready event is bound to conversation B");
    assert_ne!(snapshot_a.id, snapshot_b.id);
    assert_eq!(snapshot_a.conversation_id, conversation_a);
    assert_eq!(snapshot_a.recipients, ["lena-po"]);
    assert_eq!(
        snapshot_a.message_type,
        SemanticMessageType::ArchitectureUpdate
    );
    assert_eq!(snapshot_a.body.as_bytes(), BODY_A.as_bytes());
    assert_eq!(snapshot_b.conversation_id, conversation_b);
    assert_eq!(snapshot_b.recipients, ["jun-dev"]);
    assert_eq!(snapshot_b.message_type, SemanticMessageType::StatusRequest);
    assert_eq!(snapshot_b.body.as_bytes(), BODY_B.as_bytes());
    let sink = fixture
        .proposal_sink
        .as_ref()
        .expect("egress installed the real proposal sink");
    let raised_a = sink.pending(&conversation_a).await.expect("sink retains A");
    let raised_b = sink.pending(&conversation_b).await.expect("sink retains B");
    assert!(Arc::ptr_eq(&raised_a, snapshot_a));
    assert!(Arc::ptr_eq(&raised_b, snapshot_b));

    // The real card surface follows the active tab, and dismiss keys act only on
    // that tab's snapshot; no accepted send is exercised.
    let card_b = fixture.paint(80, 20).join("\n");
    assert!(card_b.contains(BODY_B), "active B card missing: {card_b}");
    assert!(
        !card_b.contains(BODY_A),
        "inactive A card leaked into B: {card_b}"
    );

    fixture.tabs.switch_to_prev();
    show_active_tab(&mut fixture).await;
    let card_a = fixture.paint(80, 20).join("\n");
    assert!(card_a.contains(BODY_A), "active A card missing: {card_a}");
    assert!(
        !card_a.contains(BODY_B),
        "inactive B card leaked into A: {card_a}"
    );
    assert!(matches!(
        fixture.key(DomainInputEvent::KeyPress('n')).await,
        InputAction::TeamProposal {
            conversation_id,
            decision: Some(rustain::domain::models::TeamProposalDecision::Dismiss),
            ..
        } if conversation_id == conversation_a
    ));
    assert!(
        !fixture
            .state
            .pending_team_proposals
            .contains_key(&conversation_a)
    );
    assert!(
        fixture
            .state
            .pending_team_proposals
            .contains_key(&conversation_b)
    );

    fixture.tabs.switch_to_next();
    show_active_tab(&mut fixture).await;
    let card_b = fixture.paint(80, 20).join("\n");
    assert!(
        card_b.contains(BODY_B),
        "B proposal did not retain ownership: {card_b}"
    );
    assert!(matches!(
        fixture.key(DomainInputEvent::KeyPress('n')).await,
        InputAction::TeamProposal {
            conversation_id,
            decision: Some(rustain::domain::models::TeamProposalDecision::Dismiss),
            ..
        } if conversation_id == conversation_b
    ));
    assert!(fixture.state.pending_team_proposals.is_empty());

    assert_eq!(lena.counts(), HttpCounts { get: 0, post: 0 });
    assert_eq!(jun.counts(), HttpCounts { get: 0, post: 0 });
    assert!(lena.journal().entries().await.is_empty());
    assert!(jun.journal().entries().await.is_empty());
    assert!(!fixture.sender.rows().await.iter().any(|row| matches!(
        row.record,
        JournalRecord::Room(RoomEvent::RemoteEnvelopeDispatched { .. })
    )));

    lena.stop().await;
    jun.stop().await;
}
