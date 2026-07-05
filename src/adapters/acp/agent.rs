use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol as acp;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::domain::events::AppEvent;
use crate::domain::models::agent_node::AgentMetrics;
use crate::domain::models::session_meta::now_unix;
use crate::domain::models::{
    AgentId, AppConfig, CapabilityTokenId, ChatMessage, CompletionOptions, MessageRole, NodeState,
    Op, SkillActivationSet, StopReason as DomainStopReason, StreamChunk,
};
use crate::domain::ports::{
    PersonaPort, SecurityPort, StoragePort, StreamingProvider, ToolSetPort, UsageLedgerPort,
};
use crate::domain::services::approval_runtime::ApprovalRuntime;
use crate::domain::services::message_builder;
use crate::domain::services::model_router::{ModelResolutionRequest, resolve_effective_model};
use crate::domain::services::tool_scheduler::ToolScheduler;
use crate::infrastructure::composition::CliCore;
use crate::infrastructure::runtime::turn;
use crate::infrastructure::subagent::node_tree::{AgentHandle, MailboxBudget, NodeTree};

use super::translate::{
    approval_request_to_acp, permission_response_to_outcome, stop_reason_to_acp,
    stream_chunk_to_session_update,
};

pub type CoreFactory = Rc<dyn Fn(&Path) -> acp::Result<CliCore>>;

pub(crate) struct SessionNotify {
    pub notification: acp::SessionNotification,
    pub ack: oneshot::Sender<acp::Result<()>>,
}

pub(crate) struct PermissionAsk {
    pub request: acp::RequestPermissionRequest,
    pub ack: oneshot::Sender<acp::Result<acp::RequestPermissionResponse>>,
}

pub(crate) struct SessionCore {
    provider: Arc<dyn StreamingProvider>,
    security: Arc<dyn SecurityPort>,
    tools: Arc<dyn ToolSetPort>,
    tool_scheduler: Arc<ToolScheduler>,
    approval: Arc<ApprovalRuntime>,
    storage: Arc<dyn StoragePort>,
    ledger: Arc<dyn UsageLedgerPort>,
}

impl From<CliCore> for SessionCore {
    fn from(core: CliCore) -> Self {
        let CliCore {
            provider,
            security,
            tools,
            tool_scheduler,
            approval,
            storage,
            ledger,
            ..
        } = core;
        Self {
            provider,
            security,
            tools,
            tool_scheduler,
            approval,
            storage,
            ledger,
        }
    }
}

pub(crate) struct SessionState {
    pub cwd: PathBuf,
    pub conversation_id: String,
    pub cancel: CancellationToken,
    pub core: SessionCore,
}

pub(crate) type SharedSessions = Rc<RefCell<HashMap<String, SessionState>>>;

pub(crate) struct RustainAcpAgent {
    app_config: AppConfig,
    core_factory: CoreFactory,
    model_override: Option<String>,
    session_updates: mpsc::UnboundedSender<SessionNotify>,
    permission_requests: mpsc::UnboundedSender<PermissionAsk>,
    sessions: SharedSessions,
    next_session_id: Cell<u64>,
    node_tree: NodeTree,
}

impl RustainAcpAgent {
    pub(crate) fn new(
        app_config: AppConfig,
        core_factory: CoreFactory,
        model_override: Option<String>,
        session_updates: mpsc::UnboundedSender<SessionNotify>,
        permission_requests: mpsc::UnboundedSender<PermissionAsk>,
        sessions: SharedSessions,
        node_tree: NodeTree,
    ) -> Self {
        Self {
            app_config,
            core_factory,
            model_override,
            session_updates,
            permission_requests,
            sessions,
            next_session_id: Cell::new(1),
            node_tree,
        }
    }

    async fn send_session_update(
        &self,
        session_id: acp::SessionId,
        update: acp::SessionUpdate,
    ) -> acp::Result<()> {
        let (ack, rx) = oneshot::channel();
        self.session_updates
            .send(SessionNotify {
                notification: acp::SessionNotification::new(session_id, update),
                ack,
            })
            .map_err(|_| acp::Error::internal_error())?;
        rx.await.map_err(|_| acp::Error::internal_error())?
    }

    async fn request_permission(
        &self,
        request: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        let (ack, rx) = oneshot::channel();
        self.permission_requests
            .send(PermissionAsk { request, ack })
            .map_err(|_| acp::Error::internal_error())?;
        rx.await.map_err(|_| acp::Error::internal_error())?
    }

    fn prompt_text(blocks: &[acp::ContentBlock]) -> String {
        let mut out = String::new();
        for block in blocks {
            match block {
                acp::ContentBlock::Text(text) => {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&text.text);
                }
                acp::ContentBlock::ResourceLink(resource) => {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&resource.uri);
                }
                _ => {}
            }
        }
        out
    }

    fn dummy_handle(agent_id: AgentId, cancel: CancellationToken) -> AgentHandle {
        let (command_tx, _command_rx) = mpsc::channel(1);
        let (status_tx, _status_rx) = watch::channel(crate::domain::models::NodeState::Created);
        let (_metrics_tx, metrics_rx) = watch::channel(AgentMetrics::default());
        AgentHandle {
            agent_id,
            token: CapabilityTokenId::nil(),
            command_tx,
            cancel_token: cancel,
            depth: 1,
            subagent_type: String::from("acp"),
            spawned_at: 0,
            status: status_tx,
            metrics: metrics_rx,
            isolated: false,
            mailbox_budget: MailboxBudget::new(),
        }
    }

    async fn run_prompt(
        &self,
        session_id: acp::SessionId,
        prompt: String,
    ) -> acp::Result<acp::StopReason> {
        let session_key = session_id.0.to_string();
        if !self.sessions.borrow().contains_key(&session_key) {
            return Err(acp::Error::invalid_params());
        }

        if prompt.trim().is_empty() {
            return Ok(acp::StopReason::EndTurn);
        }

        let (
            cwd,
            conversation_id,
            turn_cancel,
            provider,
            security,
            tools,
            tool_scheduler,
            approval,
            storage,
            ledger,
        ) = {
            let sessions = self.sessions.borrow();
            let state = sessions
                .get(&session_key)
                .ok_or_else(acp::Error::invalid_params)?;
            (
                state.cwd.clone(),
                state.conversation_id.clone(),
                state.cancel.clone(),
                state.core.provider.clone(),
                state.core.security.clone(),
                state.core.tools.clone(),
                state.core.tool_scheduler.clone(),
                state.core.approval.clone(),
                state.core.storage.clone(),
                state.core.ledger.clone(),
            )
        };

        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<AppEvent>();
        let now = now_unix();
        let mut conversation = match storage.load_conversation(&conversation_id).await {
            Ok(Some(conv)) => conv,
            _ => {
                // First turn (or storage error) — start a fresh conversation.
                crate::domain::models::Conversation {
                    id: conversation_id,
                    title: String::new(),
                    messages: vec![],
                    turns: vec![],
                    created_at: now,
                    updated_at: now,
                    last_response_at: None,
                    session_id: Some(session_key.clone()),
                    usage: None,
                    plans: Default::default(),
                    fork_source: None,
                    compaction: None,
                }
            }
        };
        conversation.messages.push(ChatMessage {
            id: crate::domain::models::conversation::generate_message_id(),
            role: MessageRole::User,
            content: prompt,
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: now_unix(),
            token_count: None,
            stop_reason: None,
            synthetic: false,
            images: vec![],
            origin: crate::domain::models::ChannelKind::Terminal,
        });

        let messages = message_builder::build_api_messages(&conversation);
        let project_context = crate::domain::models::project_context::ProjectContext::empty();
        let persona = crate::adapters::persona_adapter::PersonaAdapter::new(project_context);
        let persona_prompt = PersonaPort::system_prompt(&persona, &cwd);
        let empty_set = SkillActivationSet::new();
        let system_prompt = crate::domain::services::skill_context::assemble_system_prompt(
            &persona_prompt,
            &empty_set,
            &cwd,
        );
        let resolved = resolve_effective_model(
            &ModelResolutionRequest {
                explicit_override: self.model_override.clone(),
                tier_hint: None,
                step_kind: None,
                retry_count: 0,
                input_tokens: 0,
                fallback_model: self.app_config.model.clone(),
            },
            &self.app_config.router,
        );
        let options = CompletionOptions {
            model: resolved.model.clone(),
            max_tokens: 8192,
            system_prompt,
            temperature: None,
            tools: tools.available_tools(),
        };

        let mut approval_rx = approval.subscribe();
        let turn_handle = tokio::spawn(turn::run_turn(
            provider,
            messages,
            options,
            event_tx,
            security,
            tools.clone(),
            tool_scheduler.clone(),
            conversation.id.clone(),
            storage,
            conversation,
            None,
            turn_cancel.clone(),
            ledger,
            resolved,
            None,
            0,
            None,
            session_key,
        ));
        drop(tools);
        drop(tool_scheduler);

        let mut stop_reason: Option<acp::StopReason> = None;
        loop {
            tokio::select! {
                event = event_rx.recv() => {
                    let Some(event) = event else { break; };
                    match event {
                        AppEvent::ProviderChunk { chunk, .. } => match chunk {
                            StreamChunk::TurnComplete { stop_reason: reason } => {
                                stop_reason = Some(stop_reason_to_acp(&reason));
                            }
                            StreamChunk::Error { content } => {
                                let error_text = if content.is_empty() {
                                    "Unknown error"
                                } else {
                                    content.as_str()
                                };
                                self.send_session_update(
                                    session_id.clone(),
                                    acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                                        acp::ContentBlock::from(format!("Error: {error_text}")),
                                    )),
                                )
                                .await?;
                                stop_reason = Some(acp::StopReason::Refusal);
                            }
                            other => {
                                if let Some(update) = stream_chunk_to_session_update(&other) {
                                    self.send_session_update(session_id.clone(), update).await?;
                                }
                            }
                        },
                        AppEvent::SystemNotice { level, message, .. } => {
                            if level == crate::domain::models::NoticeLevel::Error {
                                self.send_session_update(
                                    session_id.clone(),
                                    acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                                        acp::ContentBlock::from(format!("Error: {message}")),
                                    )),
                                ).await?;
                                stop_reason = Some(acp::StopReason::Refusal);
                            }
                        }
                        _ => {}
                    }
                }
                approval_event = approval_rx.recv() => {
                    match approval_event {
                        Ok(event) => {
                            if let Some(request) = approval_request_to_acp(session_id.clone(), &event) {
                                let tool_name = match &event {
                                    crate::domain::services::approval_runtime::ApprovalRuntimeEvent::Requested { tool, .. } => tool.clone(),
                                    _ => String::new(),
                                };
                                let response = match tokio::time::timeout(
                                    Duration::from_secs(30),
                                    self.request_permission(request),
                                )
                                .await
                                {
                                    Ok(Ok(resp)) => resp,
                                    Ok(Err(e)) => {
                                        // P-3: resolve the pending approval before propagating.
                                        if let crate::domain::services::approval_runtime::ApprovalRuntimeEvent::Requested { id, .. } = event {
                                            approval.resolve(&id, crate::domain::models::ApprovalOutcome::Reject { feedback: None }).await;
                                        }
                                        return Err(e);
                                    }
                                    Err(_) => {
                                        // P-2: approval timed out — reject and keep the loop alive.
                                        if let crate::domain::services::approval_runtime::ApprovalRuntimeEvent::Requested { id, .. } = event {
                                            approval.resolve(&id, crate::domain::models::ApprovalOutcome::Reject { feedback: None }).await;
                                        }
                                        continue;
                                    }
                                };
                                if let crate::domain::services::approval_runtime::ApprovalRuntimeEvent::Requested { id, .. } = event {
                                    approval.resolve(&id, permission_response_to_outcome(response, &tool_name)).await;
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!("ACP approval broadcast lagged {n} events");
                            continue;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            break;
                        }
                    }
                }
            }
        }
        if let Err(join_err) = turn_handle.await {
            tracing::error!("ACP run_turn task panicked: {join_err}");
            stop_reason = Some(acp::StopReason::Refusal);
        }

        if turn_cancel.is_cancelled() {
            Ok(acp::StopReason::Cancelled)
        } else {
            Ok(stop_reason.unwrap_or(acp::StopReason::EndTurn))
        }
    }
}

#[async_trait::async_trait(?Send)]
impl acp::Agent for RustainAcpAgent {
    async fn initialize(
        &self,
        _args: acp::InitializeRequest,
    ) -> Result<acp::InitializeResponse, acp::Error> {
        let version = acp::ProtocolVersion::V1;
        Ok(
            acp::InitializeResponse::new(version).agent_info(acp::Implementation::new(
                "rustain",
                env!("CARGO_PKG_VERSION"),
            )),
        )
    }

    async fn authenticate(
        &self,
        _args: acp::AuthenticateRequest,
    ) -> Result<acp::AuthenticateResponse, acp::Error> {
        Ok(acp::AuthenticateResponse::default())
    }

    async fn new_session(
        &self,
        args: acp::NewSessionRequest,
    ) -> Result<acp::NewSessionResponse, acp::Error> {
        let next = self.next_session_id.get();
        self.next_session_id.set(next + 1);
        let session_id = format!("acp-{next}");
        let conversation_id = crate::domain::models::conversation::generate_conversation_id();
        let cancel = CancellationToken::new();
        let agent_id = AgentId(session_id.clone());
        self.node_tree
            .register_self_session(
                agent_id.clone(),
                Self::dummy_handle(agent_id.clone(), cancel.clone()),
            )
            .await
            .map_err(|_| acp::Error::internal_error())?;
        let cwd = args.cwd;
        let core = match (self.core_factory)(&cwd) {
            Ok(c) => SessionCore::from(c),
            Err(e) => {
                // Rollback: deregister the node we just registered.
                self.node_tree
                    .set_state(&agent_id, NodeState::Cancelled)
                    .await;
                self.node_tree.deregister(&agent_id).await;
                return Err(e);
            }
        };
        self.sessions.borrow_mut().insert(
            session_id.clone(),
            SessionState {
                cwd,
                conversation_id,
                cancel,
                core,
            },
        );
        Ok(acp::NewSessionResponse::new(session_id))
    }

    async fn prompt(&self, args: acp::PromptRequest) -> Result<acp::PromptResponse, acp::Error> {
        let text = Self::prompt_text(&args.prompt);
        let stop_reason = self.run_prompt(args.session_id, text).await?;
        Ok(acp::PromptResponse::new(stop_reason))
    }

    async fn cancel(&self, args: acp::CancelNotification) -> Result<(), acp::Error> {
        let session_id = args.session_id.0.to_string();
        if let Some(state) = self.sessions.borrow().get(&session_id) {
            state.cancel.cancel();
        }
        Ok(())
    }
}
