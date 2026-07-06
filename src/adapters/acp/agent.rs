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
    Op, SkillActivationSet, SkillSource, StopReason as DomainStopReason, StreamChunk,
};
use crate::domain::ports::{
    PersonaPort, SecurityPort, StoragePort, StreamingProvider, ToolSetPort, UsageLedgerPort,
};
use crate::domain::services::approval_runtime::ApprovalRuntime;
use crate::domain::services::message_builder;
use crate::domain::services::model_router::{ModelResolutionRequest, resolve_effective_model};
use crate::domain::services::tool_scheduler::ToolScheduler;
use crate::infrastructure::composition::{AcpCore, CliCore};
use crate::infrastructure::runtime::turn;
use crate::infrastructure::subagent::node_tree::{AgentHandle, MailboxBudget, NodeTree};

use super::translate::{
    approval_request_to_acp, permission_response_to_outcome, skill_trust_request_to_acp,
    skill_trust_response_allows, stop_reason_to_acp, stream_chunk_to_session_update,
};

pub type CoreFactory = Rc<dyn Fn(&Path) -> acp::Result<CliCore>>;
pub type AcpCoreFactory = Rc<dyn Fn(&Path) -> acp::Result<AcpCore>>;

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
    registry: Arc<crate::adapters::provider::ProviderRegistry>,
    router: Arc<crate::adapters::provider::ProviderRouter>,
    skill_activator: Arc<crate::adapters::skill_activation::SkillActivator>,
}

impl From<AcpCore> for SessionCore {
    fn from(core: AcpCore) -> Self {
        let AcpCore {
            provider,
            security,
            tools,
            tool_scheduler,
            approval,
            storage,
            ledger,
            registry,
            router,
            skill_activator,
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
            registry,
            router,
            skill_activator,
        }
    }
}

pub(crate) struct SessionState {
    pub cwd: PathBuf,
    pub conversation_id: String,
    pub cancel: CancellationToken,
    pub core: SessionCore,
    pub selected: Option<(String, String)>,
}

pub(crate) type SharedSessions = Rc<RefCell<HashMap<String, SessionState>>>;

pub(crate) struct RustainAcpAgent {
    app_config: AppConfig,
    core_factory: AcpCoreFactory,
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
        core_factory: AcpCoreFactory,
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

    fn initial_model_selection(
        provider: &Arc<dyn StreamingProvider>,
        model_override: Option<&str>,
    ) -> Option<(String, String)> {
        let models = provider.list_models();
        if models.is_empty() {
            return None;
        }

        if let Some(override_model) = model_override {
            if let Some(model) = models.iter().find(|model| model.model_id == override_model) {
                return Some((model.provider_id.clone(), model.model_id.clone()));
            }
            return Some((provider.provider_id(), override_model.to_string()));
        }

        models
            .first()
            .map(|model| (model.provider_id.clone(), model.model_id.clone()))
    }

    fn model_config_options(
        provider: &Arc<dyn StreamingProvider>,
        selected: Option<(&str, &str)>,
    ) -> Option<Vec<acp::SessionConfigOption>> {
        let models = provider.list_models();
        if models.is_empty() {
            return None;
        }

        let mut options: Vec<acp::SessionConfigSelectOption> = models
            .iter()
            .map(|model| {
                acp::SessionConfigSelectOption::new(
                    format!("{}:{}", model.provider_id, model.model_id),
                    model.display_name.clone(),
                )
            })
            .collect();

        let current_value = selected
            .map(|(provider_id, model_id)| format!("{provider_id}:{model_id}"))
            .unwrap_or_else(|| {
                let model = models.first().expect("models is non-empty");
                format!("{}:{}", model.provider_id, model.model_id)
            });

        if !options
            .iter()
            .any(|option| option.value.0.as_ref() == current_value)
        {
            options.push(acp::SessionConfigSelectOption::new(
                current_value.clone(),
                current_value.clone(),
            ));
        }

        Some(vec![
            acp::SessionConfigOption::select("model", "Model", current_value, options)
                .category(acp::SessionConfigOptionCategory::Model),
        ])
    }

    async fn available_skill_commands(
        skill_activator: &crate::adapters::skill_activation::SkillActivator,
    ) -> Vec<acp::AvailableCommand> {
        let mut commands = Vec::new();
        for name in skill_activator.discovered_skill_names().await {
            if let Some(def) = skill_activator.lookup_skill(&name).await {
                commands.push(acp::AvailableCommand::new(def.name, def.description).input(
                    acp::AvailableCommandInput::Unstructured(acp::UnstructuredCommandInput::new(
                        "Arguments for this skill",
                    )),
                ));
            }
        }
        commands
    }

    fn parse_skill_prompt(prompt: &str) -> Option<(&str, &str)> {
        let trimmed = prompt.trim_start();
        let without_slash = trimmed.strip_prefix('/')?;
        let mut split = without_slash.splitn(2, char::is_whitespace);
        let name = split.next()?.trim();
        if name.is_empty() {
            return None;
        }
        let args = split.next().unwrap_or("").trim();
        if name == "skill" {
            let mut args_split = args.splitn(2, char::is_whitespace);
            let skill_name = args_split.next()?.trim();
            if skill_name.is_empty() {
                return None;
            }
            return Some((skill_name, args_split.next().unwrap_or("").trim()));
        }
        Some((name, args))
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
        mut prompt: String,
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
            selected_model,
            skill_activator,
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
                state.selected.clone().map(|(_, model_id)| model_id),
                state.core.skill_activator.clone(),
            )
        };

        if let Some((skill_name, skill_args)) = Self::parse_skill_prompt(&prompt) {
            if let Some(def) = skill_activator.lookup_skill(skill_name).await {
                let needs_trust = def.source != SkillSource::GlobalAgents
                    && !skill_activator
                        .is_trusted(&conversation_id, &def.file)
                        .await;
                let trusted = if needs_trust {
                    let request = skill_trust_request_to_acp(
                        session_id.clone(),
                        &def.name,
                        &format!("{:?}", def.source),
                        &def.file,
                    );
                    match tokio::time::timeout(
                        Duration::from_secs(30),
                        self.request_permission(request),
                    )
                    .await
                    {
                        Ok(Ok(response)) => {
                            if skill_trust_response_allows(response) {
                                skill_activator
                                    .mark_trusted(&conversation_id, def.file.clone())
                                    .await;
                                true
                            } else {
                                false
                            }
                        }
                        _ => false,
                    }
                } else {
                    true
                };
                if trusted {
                    if let Ok(active) = skill_activator
                        .activate(&def, skill_args.to_string(), &conversation_id, 0)
                        .await
                    {
                        security.add_active_skill_dir(active.directory.clone());
                        prompt = format!("ARGUMENTS: {}", skill_args.trim());
                    }
                }
            }
        }

        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<AppEvent>();
        let now = now_unix();
        let mut conversation = match storage.load_conversation(&conversation_id).await {
            Ok(Some(conv)) => conv,
            _ => {
                // First turn (or storage error) — start a fresh conversation.
                crate::domain::models::Conversation {
                    id: conversation_id.clone(),
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
        let activation_set = skill_activator.snapshot_for_turn(&conversation_id).await;
        let system_prompt =
            crate::domain::services::skill_context::assemble_system_prompt_with_agent(
                &persona_prompt,
                None,
                activation_set.as_ref().unwrap_or(&empty_set),
                &cwd,
            );
        let resolved = resolve_effective_model(
            &ModelResolutionRequest {
                explicit_override: selected_model.or_else(|| self.model_override.clone()),
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
            activation_set,
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
        let selected =
            Self::initial_model_selection(&core.provider, self.model_override.as_deref());
        if let Some((provider_id, _)) = selected.as_ref() {
            if core
                .registry
                .list_providers()
                .iter()
                .any(|p| p.provider_id == *provider_id)
            {
                let _ = core.router.set_active(provider_id);
            }
        }
        let config_options = Self::model_config_options(
            &core.provider,
            selected
                .as_ref()
                .map(|(provider_id, model_id)| (provider_id.as_str(), model_id.as_str())),
        );
        let skill_activator = core.skill_activator.clone();
        self.sessions.borrow_mut().insert(
            session_id.clone(),
            SessionState {
                cwd,
                conversation_id,
                cancel,
                core,
                selected,
            },
        );
        let mut response = acp::NewSessionResponse::new(session_id.clone());
        if let Some(config_options) = config_options {
            response = response.config_options(config_options);
        }
        let commands = Self::available_skill_commands(&skill_activator).await;
        if !commands.is_empty() {
            self.send_session_update(
                session_id.into(),
                acp::SessionUpdate::AvailableCommandsUpdate(acp::AvailableCommandsUpdate::new(
                    commands,
                )),
            )
            .await?;
        }
        Ok(response)
    }

    async fn prompt(&self, args: acp::PromptRequest) -> Result<acp::PromptResponse, acp::Error> {
        let text = Self::prompt_text(&args.prompt);
        let stop_reason = self.run_prompt(args.session_id, text).await?;
        Ok(acp::PromptResponse::new(stop_reason))
    }

    async fn set_session_config_option(
        &self,
        args: acp::SetSessionConfigOptionRequest,
    ) -> Result<acp::SetSessionConfigOptionResponse, acp::Error> {
        if args.config_id.0.as_ref() != "model" {
            return Err(acp::Error::invalid_params());
        }

        let requested = args.value.0.as_ref();
        let Some((provider_id, model_id)) = requested.split_once(':') else {
            return Err(acp::Error::invalid_params());
        };
        if provider_id.is_empty() || model_id.is_empty() {
            return Err(acp::Error::invalid_params());
        }

        let mut sessions = self.sessions.borrow_mut();
        let state = sessions
            .get_mut(args.session_id.0.as_ref())
            .ok_or_else(acp::Error::invalid_params)?;

        if state
            .core
            .registry
            .get_model(provider_id, model_id)
            .is_none()
        {
            return Err(acp::Error::invalid_params());
        }
        state
            .core
            .router
            .set_active(provider_id)
            .map_err(|_| acp::Error::invalid_params())?;

        state.selected = Some((provider_id.to_string(), model_id.to_string()));
        let config_options =
            Self::model_config_options(&state.core.provider, Some((provider_id, model_id)))
                .ok_or_else(acp::Error::invalid_params)?;

        Ok(acp::SetSessionConfigOptionResponse::new(config_options))
    }

    async fn cancel(&self, args: acp::CancelNotification) -> Result<(), acp::Error> {
        let session_id = args.session_id.0.to_string();
        if let Some(state) = self.sessions.borrow().get(&session_id) {
            state.cancel.cancel();
        }
        Ok(())
    }
}
