use std::cell::RefCell;
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
    AgentId, AppConfig, CapabilityTokenId, ChatMessage, CompletionOptions, ImageAttachment,
    MessageRole, NodeState, Op, SkillActivationSet, SkillSource, StopReason as DomainStopReason,
    StreamChunk, ToolCallInfo, ToolResultInfo,
};
use crate::domain::ports::{
    AuthStorePort, PersonaPort, SecurityPort, StoragePort, StreamingProvider, ToolSetPort,
    UsageLedgerPort,
};
use crate::domain::services::approval_runtime::ApprovalRuntime;
use crate::domain::services::message_builder;
use crate::domain::services::model_router::{ModelResolutionRequest, resolve_effective_model};
use crate::domain::services::tool_scheduler::ToolScheduler;
use crate::infrastructure::composition::{AcpCore, CliCore};
use crate::infrastructure::runtime::turn;
use crate::infrastructure::subagent::node_tree::{AgentHandle, MailboxBudget, NodeTree};

use super::translate::{
    approval_request_to_acp, mcp_servers_from_acp, message_to_replay_updates,
    permission_response_to_outcome, skill_trust_request_to_acp, skill_trust_response_allows,
    stop_reason_to_acp, stream_chunk_to_session_update,
};

pub type CoreFactory =
    Rc<dyn Fn(&Path, &[crate::domain::models::McpServerSpec]) -> acp::Result<CliCore>>;
pub type AcpCoreFactory =
    Rc<dyn Fn(&Path, &[crate::domain::models::McpServerSpec]) -> acp::Result<AcpCore>>;

/// Delay before emitting the post-`session/new` `available_commands_update`.
///
/// Zed's async session layer needs a beat after the `NewSessionResponse` to
/// finish registering the session before it will accept the advertisement into
/// its command list; emitting it immediately makes Zed drop the update and
/// report "Available commands: none". Mirrors codex's proven 200 ms
/// spawned-task deferral. Exposed so tests can pin the deferral against an
/// immediate fire-and-forget regression (Story 14-8b).
pub const SKILL_ADVERTISEMENT_DELAY: Duration = Duration::from_millis(200);

/// Fixed page size for `session/list` pagination (codex `SESSION_LIST_PAGE_SIZE`).
/// An opaque cursor encodes the last-seen `(updated_at, id)` so a client can
/// resume. Eventual consistency is accepted for R1 (a concurrent write
/// mid-pagination may repeat/skip a row — no MVCC; the client re-lists).
const SESSION_LIST_PAGE_SIZE: usize = 25;

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
    default_workspace: PathBuf,
    session_updates: mpsc::UnboundedSender<SessionNotify>,
    permission_requests: mpsc::UnboundedSender<PermissionAsk>,
    sessions: SharedSessions,
    id_source: Rc<dyn Fn() -> String>,
    node_tree: NodeTree,
    auth_store: Arc<dyn AuthStorePort>,
}

impl RustainAcpAgent {
    pub(crate) fn new(
        app_config: AppConfig,
        core_factory: AcpCoreFactory,
        model_override: Option<String>,
        default_workspace: PathBuf,
        session_updates: mpsc::UnboundedSender<SessionNotify>,
        permission_requests: mpsc::UnboundedSender<PermissionAsk>,
        sessions: SharedSessions,
        node_tree: NodeTree,
        auth_store: Arc<dyn AuthStorePort>,
        id_source: Rc<dyn Fn() -> String>,
    ) -> Self {
        Self {
            app_config,
            core_factory,
            model_override,
            default_workspace,
            session_updates,
            permission_requests,
            sessions,
            id_source,
            node_tree,
            auth_store,
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

    fn prompt_parts(blocks: &[acp::ContentBlock]) -> (String, Vec<ImageAttachment>) {
        let mut out = String::new();
        let mut images = Vec::new();
        for block in blocks {
            match block {
                acp::ContentBlock::Text(text) => {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&text.text);
                }
                acp::ContentBlock::Image(image) => {
                    // Only inline base64 image data is supported (rustain
                    // forwards bytes to the provider). A URI-only image (empty
                    // `data`) cannot be resolved here, so warn and skip rather
                    // than forward an empty attachment the provider would treat
                    // as a broken image.
                    if image.data.is_empty() {
                        match image.uri.as_deref() {
                            Some(uri) => tracing::warn!(
                                uri = %uri,
                                "ACP image block carried a URI but no inline data; \
                                 rustain supports inline base64 images only — dropping"
                            ),
                            None => {
                                tracing::warn!("ACP image block has no data and no URI; dropping")
                            }
                        }
                    } else {
                        images.push(ImageAttachment {
                            media_type: image.mime_type.clone(),
                            data: image.data.clone(),
                        });
                    }
                }
                acp::ContentBlock::ResourceLink(resource) => {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&resource.uri);
                }
                // Audio and embedded Resource blocks require capabilities
                // rustain does NOT advertise (`audio`, `embeddedContext`), so
                // they are dropped — but loudly, mirroring the Http/Sse MCP
                // drop, so a spec-violating client gets a signal instead of
                // silent data loss. (If such a block is the only block, the
                // prompt text stays empty and `run_prompt` short-circuits to
                // `EndTurn`.)
                acp::ContentBlock::Audio(_) => {
                    tracing::warn!(
                        "ACP prompt contained an Audio content block, which \
                         rustain does not support (no `audio` capability \
                         advertised); dropping"
                    );
                }
                acp::ContentBlock::Resource(_) => {
                    tracing::warn!(
                        "ACP prompt contained an embedded Resource content \
                         block, which rustain does not support (no \
                         `embeddedContext` capability advertised); dropping"
                    );
                }
                _ => {
                    tracing::warn!(
                        block = ?block,
                        "ACP prompt contained an unsupported content block type; dropping"
                    );
                }
            }
        }
        (out, images)
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

    fn builtin_commands() -> Vec<acp::AvailableCommand> {
        vec![acp::AvailableCommand::new(
            "init",
            "create a context file with instructions for rustain.",
        )]
    }

    fn builtin_init_prompt(args: &str) -> String {
        let mut prompt = String::from(
            "Analyze this repository and create or update a concise context file with instructions for future rustain sessions. Include repository purpose, architecture, key commands, test strategy, coding conventions, safety constraints, and any project-specific workflow rules.",
        );
        if !args.trim().is_empty() {
            prompt.push_str("\n\nAdditional user instructions: ");
            prompt.push_str(args.trim());
        }
        prompt
    }

    fn dispatch_builtin_prompt(prompt: &str) -> Option<String> {
        // Only the DIRECT `/init` form is a builtin. This deliberately does NOT
        // reuse `parse_skill_prompt`: that helper re-parses `/skill init` into
        // `("init", "")`, which would let this builtin swallow the explicit
        // skill-activation form — shadowing a user/workspace skill named `init`
        // and making it advertised-but-unreachable (a 14-8b-class false
        // surface). Parsing the bare form here leaves `/skill <name>` for the
        // skill-lookup branch in `run_prompt`, restoring `/skill init` as the
        // escape hatch for a colliding skill name.
        let trimmed = prompt.trim_start();
        let without_slash = trimmed.strip_prefix('/')?;
        let mut split = without_slash.splitn(2, char::is_whitespace);
        let name = split.next()?.trim();
        if name != "init" {
            return None;
        }
        let args = split.next().unwrap_or("").trim();
        Some(Self::builtin_init_prompt(args))
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

    fn auth_methods() -> Vec<acp::AuthMethod> {
        crate::adapters::cli::auth::providers::all_providers()
            .iter()
            .filter(|provider| provider.requires_key)
            .map(|provider| {
                acp::AuthMethod::Agent(
                    acp::AuthMethodAgent::new(provider.id, provider.display_name).description(
                        format!(
                            "Set {} or run `rustain auth login {}`",
                            provider.api_key_env, provider.id
                        ),
                    ),
                )
            })
            .collect()
    }

    async fn has_credential_for(
        &self,
        provider: &crate::adapters::cli::auth::providers::ProviderMeta,
    ) -> acp::Result<bool> {
        let statuses = self
            .auth_store
            .list()
            .await
            .map_err(|err| acp::Error::internal_error().data(err.to_string()))?;
        let auth_by_provider: HashMap<&str, &crate::domain::models::credential::ProviderStatus> =
            statuses
                .iter()
                .map(|status| (status.provider.as_str(), status))
                .collect();
        Ok(
            crate::adapters::cli::auth::detect_source(provider, &auth_by_provider, &|key| {
                crate::infrastructure::utils::env_var_trimmed(key)
            })
            .is_some(),
        )
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

    fn assistant_message(
        content: String,
        tool_calls: Vec<ToolCallInfo>,
        stop_reason: DomainStopReason,
    ) -> ChatMessage {
        ChatMessage {
            id: crate::domain::models::conversation::generate_message_id(),
            role: MessageRole::Assistant,
            content,
            content_blocks: vec![],
            tool_calls,
            created_at: now_unix(),
            token_count: None,
            stop_reason: Some(stop_reason),
            synthetic: false,
            images: vec![],
            origin: crate::domain::models::ChannelKind::Terminal,
        }
    }

    fn tool_result_boundary_message() -> ChatMessage {
        ChatMessage {
            id: crate::domain::models::conversation::generate_message_id(),
            role: MessageRole::User,
            content: String::new(),
            content_blocks: vec![],
            tool_calls: vec![],
            created_at: now_unix(),
            token_count: None,
            stop_reason: None,
            synthetic: true,
            images: vec![],
            origin: crate::domain::models::ChannelKind::Terminal,
        }
    }

    async fn run_prompt(
        &self,
        session_id: acp::SessionId,
        mut prompt: String,
        images: Vec<ImageAttachment>,
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

        if let Some(expanded) = Self::dispatch_builtin_prompt(&prompt) {
            prompt = expanded;
        } else if let Some((skill_name, skill_args)) = Self::parse_skill_prompt(&prompt) {
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

        let mut messages = message_builder::build_api_messages(&conversation);
        if !images.is_empty() {
            if let Some(last_user) = messages
                .iter_mut()
                .rev()
                .find(|message| message.role == MessageRole::User)
            {
                last_user.images = images;
            }
        }
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
            storage.clone(),
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
        // F0 (AC1/AC7): accumulate assistant output as it streams so it can be
        // persisted AFTER the turn. Tool-use iterations are split into
        // assistant/tool-result-boundary/final-assistant messages so reload
        // preserves provider role ordering instead of replaying tool results
        // after the next user prompt.
        let mut assistant_text = String::new();
        let mut assistant_tool_calls: Vec<ToolCallInfo> = Vec::new();
        let mut assistant_messages: Vec<ChatMessage> = Vec::new();
        let mut assistant_stop_reason = DomainStopReason::EndTurn;
        loop {
            tokio::select! {
                event = event_rx.recv() => {
                    let Some(event) = event else { break; };
                    match event {
                        AppEvent::ProviderChunk { chunk, .. } => match chunk {
                            StreamChunk::TurnComplete { stop_reason: reason } => {
                                assistant_stop_reason = reason.clone();
                                if matches!(reason, DomainStopReason::ToolUse)
                                    && (!assistant_text.is_empty()
                                        || !assistant_tool_calls.is_empty())
                                {
                                    assistant_messages.push(Self::assistant_message(
                                        std::mem::take(&mut assistant_text),
                                        std::mem::take(&mut assistant_tool_calls),
                                        DomainStopReason::ToolUse,
                                    ));
                                    assistant_messages.push(Self::tool_result_boundary_message());
                                }
                                stop_reason = Some(stop_reason_to_acp(&reason));
                            }
                            StreamChunk::Error { content } => {
                                let error_text = if content.is_empty() {
                                    "Unknown error"
                                } else {
                                    content.as_str()
                                };
                                let rendered = format!("Error: {error_text}");
                                self.send_session_update(
                                    session_id.clone(),
                                    acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                                        acp::ContentBlock::from(rendered.clone()),
                                    )),
                                )
                                .await?;
                                if !assistant_text.is_empty() {
                                    assistant_text.push('\n');
                                }
                                assistant_text.push_str(&rendered);
                                assistant_stop_reason = DomainStopReason::Cancelled;
                                stop_reason = Some(acp::StopReason::Refusal);
                            }
                            other => {
                                // F0: capture the assistant text + tool calls for
                                // post-turn persistence (see the commit below). The
                                // chunk is STILL forwarded to the client unchanged.
                                match &other {
                                    StreamChunk::Text { content, .. } => {
                                        assistant_text.push_str(content);
                                    }
                                    StreamChunk::ToolUse { id, name, input } => {
                                        assistant_tool_calls.push(ToolCallInfo {
                                            id: id.clone(),
                                            name: name.clone(),
                                            input: input.clone(),
                                            result: None,
                                            started_at_ms: None,
                                            completed_at_ms: None,
                                            status: None,
                                        });
                                    }
                                    StreamChunk::ToolResult {
                                        id,
                                        content,
                                        is_error,
                                    } => {
                                        let result = ToolResultInfo {
                                            content: content.clone(),
                                            is_error: *is_error,
                                        };
                                        let status =
                                            Some(if *is_error { "✗ Error" } else { "✓ Success" }.to_string());
                                        if let Some(call) = assistant_tool_calls
                                            .iter_mut()
                                            .rev()
                                            .find(|tc| tc.id == *id)
                                        {
                                            call.result = Some(result);
                                            call.completed_at_ms = Some((now_unix().max(0) as u64) * 1000);
                                            call.status = status;
                                        } else {
                                            'outer: for message in assistant_messages.iter_mut().rev() {
                                                for call in message.tool_calls.iter_mut().rev() {
                                                    if call.id == *id {
                                                        call.result = Some(result);
                                                        call.completed_at_ms =
                                                            Some((now_unix().max(0) as u64) * 1000);
                                                        call.status = status;
                                                        break 'outer;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                                if let Some(update) = stream_chunk_to_session_update(&other, &cwd) {
                                    self.send_session_update(session_id.clone(), update).await?;
                                }
                            }
                        },
                        AppEvent::SystemNotice { level, message, .. } => {
                            if level == crate::domain::models::NoticeLevel::Error {
                                let rendered = format!("Error: {message}");
                                self.send_session_update(
                                    session_id.clone(),
                                    acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                                        acp::ContentBlock::from(rendered.clone()),
                                    )),
                                ).await?;
                                if !assistant_text.is_empty() {
                                    assistant_text.push('\n');
                                }
                                assistant_text.push_str(&rendered);
                                assistant_stop_reason = DomainStopReason::Cancelled;
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
            if assistant_text.is_empty() {
                assistant_text.push_str("Error: ACP turn task panicked");
            }
            assistant_stop_reason = DomainStopReason::Cancelled;
            stop_reason = Some(acp::StopReason::Refusal);
        }

        // F0: persist the assistant response (text + tool calls/results) now
        // that the turn's event pump has drained. Error/cancel/refusal paths
        // still save the timestamp advance and any client-visible error text so
        // reload does not resurrect a dangling user-only turn.
        if turn_cancel.is_cancelled() {
            assistant_stop_reason = DomainStopReason::Cancelled;
        }
        if !assistant_text.is_empty() || !assistant_tool_calls.is_empty() {
            assistant_messages.push(Self::assistant_message(
                std::mem::take(&mut assistant_text),
                std::mem::take(&mut assistant_tool_calls),
                assistant_stop_reason.clone(),
            ));
        }
        let should_save =
            !assistant_messages.is_empty() || stop_reason.is_some() || turn_cancel.is_cancelled();
        if should_save {
            let now = now_unix();
            if let Ok(Some(mut conv)) = storage.load_conversation(&conversation_id).await {
                conv.messages.extend(assistant_messages);
                conv.updated_at = now;
                conv.last_response_at = Some(now);
                if let Err(e) = storage.save_conversation(&conv).await {
                    tracing::warn!(error = %e, "ACP: persisting assistant turn failed");
                }
            }
        }

        if turn_cancel.is_cancelled() {
            Ok(acp::StopReason::Cancelled)
        } else {
            Ok(stop_reason.unwrap_or(acp::StopReason::EndTurn))
        }
    }
    /// Shared reconstruction for `session/load` and `session/resume` (Tasks 2/3).
    ///
    /// Both rebuild `SessionState` from the persisted conversation rooted at the
    /// REQUEST cwd; the ONLY difference is whether history is re-streamed to the
    /// client (`load` replays it, `resume` does not — model-side context is
    /// restored either way via `load_conversation` populating the in-memory
    /// `Conversation` the next `prompt` reads). Returns the session
    /// `config_options` (best-effort model, AI-14.9-1) for the response.
    async fn restore_session(
        &self,
        session_id: &acp::SessionId,
        request_cwd: PathBuf,
        mcp_servers: Vec<acp::McpServer>,
        replay_history: bool,
    ) -> acp::Result<Option<Vec<acp::SessionConfigOption>>> {
        let session_key = session_id.0.to_string();
        // DD-2: resolve the durable conversation id from the wire session id
        // (prefix strip). A non-`acp-` id is an orphan → fail-closed (AC8).
        let conversation_id = super::conversation_id_from_acp_session_id(&session_key)
            .ok_or_else(|| acp::Error::resource_not_found(None))?
            .to_string();

        // Rebuild the per-session core from the REQUEST cwd (NEVER disk). This
        // roots storage at `sessions_dir(request_cwd)`, so the load below can
        // only see conversations under the gated cwd — the store-you-read-is-
        // the-cwd-you're-gated-on invariant (DD-1/Vex). `build_acp_core` also
        // reconstructs SecurityAdapter/SkillRegistry/tools against request_cwd,
        // so a loaded cwd/tool set that would not pass a fresh `new_session`
        // trust posture is likewise not trusted resumed (AC6 fail-closed).
        // Skill trust (SkillActivator.conversation_sets) starts empty after a
        // restart ⇒ workspace-tier skills fail-closed by construction.
        let forwarded_mcp_servers = mcp_servers_from_acp(mcp_servers);
        let core =
            (self.core_factory)(&request_cwd, &forwarded_mcp_servers).map(SessionCore::from)?;

        // Load the conversation from the request-cwd-rooted store. A miss
        // (orphan / cross-cwd id) is fail-closed `resource_not_found` (AC8).
        let conversation = match core.storage.load_conversation(&conversation_id).await {
            Ok(Some(conv)) => conv,
            _ => return Err(acp::Error::resource_not_found(None)),
        };

        // Idempotent on re-load: if the id is already live, cancel and remove
        // the old state before replacing it. Otherwise an in-flight old prompt
        // can keep streaming and race persistence against the restored session.
        let agent_id = AgentId(session_key.clone());
        if let Some(old_state) = self.sessions.borrow_mut().remove(&session_key) {
            old_state.cancel.cancel();
            self.node_tree
                .set_state(&agent_id, NodeState::Cancelled)
                .await;
            self.node_tree.deregister(&agent_id).await;
        } else if self
            .node_tree
            .list()
            .await
            .iter()
            .any(|e| e.agent_id == agent_id)
        {
            self.node_tree
                .set_state(&agent_id, NodeState::Cancelled)
                .await;
            self.node_tree.deregister(&agent_id).await;
        }
        let cancel = CancellationToken::new();
        self.node_tree
            .register_self_session(
                agent_id.clone(),
                Self::dummy_handle(agent_id.clone(), cancel.clone()),
            )
            .await
            .map_err(|_| acp::Error::internal_error())?;

        // best-effort model selection (AI-14.9-1: non-blocking). Reintroducing
        // a binding record just to remember a dropdown is rejected; recover
        // from the configured/first model. Set the router if registered.
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

        // Capture replay data BEFORE moving `core` into the live map.
        let replay_messages = if replay_history {
            conversation.messages.clone()
        } else {
            Vec::new()
        };

        // Replay history to the client (load only) BEFORE committing the live
        // session map entry. If replay fails, roll back the registered node so
        // the client does not see a failed load while the server considers the
        // session active.
        if replay_history {
            for message in &replay_messages {
                for update in message_to_replay_updates(message, &request_cwd) {
                    if let Err(e) = self.send_session_update(session_id.clone(), update).await {
                        cancel.cancel();
                        self.node_tree
                            .set_state(&agent_id, NodeState::Cancelled)
                            .await;
                        self.node_tree.deregister(&agent_id).await;
                        return Err(e);
                    }
                }
            }
        }

        // Insert into the live sessions map only after replay succeeds.
        self.sessions.borrow_mut().insert(
            session_key.clone(),
            SessionState {
                cwd: request_cwd,
                conversation_id,
                cancel,
                core,
                selected,
            },
        );

        Ok(config_options)
    }

    /// Build the `session/list` response for a target cwd (Task 4).
    ///
    /// The store rooted at `sessions_dir(cwd)` IS the cwd filter — every
    /// conversation under it belongs to `cwd` (DD-1 dissolved). `SessionInfo.cwd`
    /// is the request cwd echoed back, never read from a per-session field.
    /// Sorted `updated_at` desc; opaque cursor = `{updated_at}:{id}` of the
    /// last emitted row (R1 eventual consistency accepted).
    async fn list_sessions_for_cwd(
        &self,
        cwd: Option<PathBuf>,
        cursor: Option<String>,
    ) -> acp::Result<acp::ListSessionsResponse> {
        let cwd = cwd.unwrap_or_else(|| self.default_workspace.clone());
        let core = (self.core_factory)(&cwd, &[]).map(SessionCore::from)?;
        let all_summaries = core
            .storage
            .list_conversations()
            .await
            .map_err(|_| acp::Error::internal_error())?;
        let mut summaries = Vec::with_capacity(all_summaries.len());
        for summary in all_summaries {
            let Ok(Some(conversation)) = core.storage.load_conversation(&summary.id).await else {
                continue;
            };
            if conversation
                .session_id
                .as_deref()
                .is_some_and(super::is_acp_session_id)
            {
                summaries.push(summary);
            }
        }
        // Stable order for pagination: newest first, id as ascending tiebreak.
        summaries.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(a.id.cmp(&b.id)));

        // Decode the opaque cursor to skip already-emitted rows. Malformed
        // cursors are client errors; silently treating them as "no cursor"
        // causes duplicate first pages and infinite pagination loops.
        let after: Option<(i64, String)> = match cursor {
            Some(c) => {
                let (ts, id) = c.split_once(':').ok_or_else(acp::Error::invalid_params)?;
                let ts = ts
                    .parse::<i64>()
                    .map_err(|_| acp::Error::invalid_params())?;
                Some((ts, id.to_string()))
            }
            None => None,
        };
        let mut filtered: Vec<_> = summaries
            .into_iter()
            .filter(|s| match &after {
                Some((ts, id)) => {
                    s.updated_at < *ts || (s.updated_at == *ts && s.id.as_str() > id.as_str())
                }
                None => true,
            })
            .collect();

        let take = filtered.len().min(SESSION_LIST_PAGE_SIZE);
        let page: Vec<_> = filtered.drain(..take).collect();
        let next_cursor = if filtered.is_empty() {
            None
        } else {
            page.last().map(|s| format!("{}:{}", s.updated_at, s.id))
        };
        let sessions = page
            .into_iter()
            .map(|s| {
                acp::SessionInfo::new(super::format_acp_session_id(&s.id), cwd.clone())
                    .title(if s.title.is_empty() {
                        None
                    } else {
                        Some(s.title)
                    })
                    .updated_at(
                        chrono::DateTime::<chrono::Utc>::from_timestamp(s.updated_at, 0)
                            .map(|dt| dt.to_rfc3339()),
                    )
            })
            .collect();
        Ok(acp::ListSessionsResponse::new(sessions).next_cursor(next_cursor))
    }
}

#[async_trait::async_trait(?Send)]
impl acp::Agent for RustainAcpAgent {
    async fn initialize(
        &self,
        _args: acp::InitializeRequest,
    ) -> Result<acp::InitializeResponse, acp::Error> {
        let version = acp::ProtocolVersion::V1;
        // Task 7: advertise the four lifecycle capabilities so a compliant
        // client (Zed) actually invokes load/resume/list/close. `load_session`
        // is top-level; list/resume/close nest under `session_capabilities`.
        // Honest advertisement — ONLY what this story implements (14.10 owns
        // the broader MCP/auth surface). This is the deliberate, diff-verified
        // `initialize` re-baseline (AC9).
        let caps = acp::AgentCapabilities::new()
            .load_session(true)
            .session_capabilities(
                acp::SessionCapabilities::new()
                    .list(Some(acp::SessionListCapabilities::default()))
                    .resume(Some(acp::SessionResumeCapabilities::default()))
                    .close(Some(acp::SessionCloseCapabilities::default())),
            )
            .prompt_capabilities(acp::PromptCapabilities::new().image(true));
        Ok(acp::InitializeResponse::new(version)
            .agent_info(acp::Implementation::new(
                "rustain",
                env!("CARGO_PKG_VERSION"),
            ))
            .agent_capabilities(caps)
            .auth_methods(Self::auth_methods()))
    }

    async fn authenticate(
        &self,
        args: acp::AuthenticateRequest,
    ) -> Result<acp::AuthenticateResponse, acp::Error> {
        let provider_id = args.method_id.0.as_ref();
        let Some(provider) = crate::adapters::cli::auth::providers::lookup(provider_id)
            .filter(|provider| provider.requires_key)
        else {
            return Err(acp::Error::invalid_params().data(format!(
                "Unknown ACP auth method `{provider_id}`. Supported methods: {}",
                crate::adapters::cli::auth::providers::all_providers()
                    .iter()
                    .filter(|provider| provider.requires_key)
                    .map(|provider| provider.id)
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        };
        if self.has_credential_for(provider).await? {
            return Ok(acp::AuthenticateResponse::new());
        }
        Err(acp::Error::auth_required().data(format!(
            "Missing credential for {}. Set {} or run `rustain auth login {}`.",
            provider.display_name, provider.api_key_env, provider.id
        )))
    }

    async fn new_session(
        &self,
        args: acp::NewSessionRequest,
    ) -> Result<acp::NewSessionResponse, acp::Error> {
        // DD-2: an ACP session IS a conversation. The durable conversation id
        // (unique nanoid in production; an injectable counter in tests for
        // deterministic goldens) is the on-disk file key, and the wire
        // `SessionId` is `acp-{conversation_id}` — bijective by construction,
        // no persisted index, survives restart. The old per-process `Cell<u64>`
        // counter is deleted (it modeled a distinction that doesn't exist).
        let conversation_id = (self.id_source)();
        let session_id = super::format_acp_session_id(&conversation_id);
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
        let forwarded_mcp_servers = mcp_servers_from_acp(args.mcp_servers);
        let core = match (self.core_factory)(&cwd, &forwarded_mcp_servers) {
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
        let mut commands = Self::builtin_commands();
        commands.extend(Self::available_skill_commands(&skill_activator).await);
        if !commands.is_empty() {
            // Story 14-8b host-smoke (2026-07-06): wire order alone is
            // NECESSARY but NOT SUFFICIENT for real async editors. Zed
            // receives `available_commands_update` but reports "Available
            // commands: none" when it lands immediately after the response —
            // Zed's session layer has not finished registering the session,
            // so it drops the update. codex (proven in Zed) defers via a
            // spawned task + 200 ms delay (thread.rs:2826-2833); mirror that
            // exactly. This REVERSES DD-1 (immediate fire-and-forget) on
            // evidence: the in-process harness modeled a synchronous client;
            // Zed is async and needs a beat to register the session before it
            // will accept the update into its command list. The task is
            // bounded + fire-and-forget: a closed connection makes the send
            // error harmlessly; the LocalSet drops it on EOF (no teardown
            // race — there is no ack to await and the send is best-effort).
            let tx = self.session_updates.clone();
            let sid: acp::SessionId = session_id.into();
            let notification = acp::SessionNotification::new(
                sid,
                acp::SessionUpdate::AvailableCommandsUpdate(acp::AvailableCommandsUpdate::new(
                    commands,
                )),
            );
            tokio::task::spawn_local(async move {
                tokio::time::sleep(SKILL_ADVERTISEMENT_DELAY).await;
                let (ack, _rx) = oneshot::channel();
                let _ = tx.send(SessionNotify { notification, ack });
            });
        }
        Ok(response)
    }

    async fn prompt(&self, args: acp::PromptRequest) -> Result<acp::PromptResponse, acp::Error> {
        let (text, images) = Self::prompt_parts(&args.prompt);
        let stop_reason = self.run_prompt(args.session_id, text, images).await?;
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
    async fn load_session(
        &self,
        args: acp::LoadSessionRequest,
    ) -> Result<acp::LoadSessionResponse, acp::Error> {
        let cwd = args.cwd.clone();
        // load = restore + replay history to the client (AC2). The resume-trust-
        // gate is structural (build_acp_core on request cwd) inside restore_session.
        let config_options = self
            .restore_session(&args.session_id, cwd, args.mcp_servers, true)
            .await?;
        let mut response = acp::LoadSessionResponse::new();
        if let Some(co) = config_options {
            response = response.config_options(co);
        }
        Ok(response)
    }

    async fn resume_session(
        &self,
        args: acp::ResumeSessionRequest,
    ) -> Result<acp::ResumeSessionResponse, acp::Error> {
        let cwd = args.cwd.clone();
        // resume = restore WITHOUT client replay (AC3). Model-side context is
        // still fully reconstructed (load_conversation populates the in-memory
        // Conversation the next prompt reads); only the session/update
        // re-stream is skipped.
        let config_options = self
            .restore_session(&args.session_id, cwd, args.mcp_servers, false)
            .await?;
        let mut response = acp::ResumeSessionResponse::new();
        if let Some(co) = config_options {
            response = response.config_options(co);
        }
        Ok(response)
    }

    async fn list_sessions(
        &self,
        args: acp::ListSessionsRequest,
    ) -> Result<acp::ListSessionsResponse, acp::Error> {
        self.list_sessions_for_cwd(args.cwd, args.cursor).await
    }

    async fn close_session(
        &self,
        args: acp::CloseSessionRequest,
    ) -> Result<acp::CloseSessionResponse, acp::Error> {
        let session_id = args.session_id.0.to_string();
        let agent_id = AgentId(session_id.clone());
        let (cancel, was_cancelled) = {
            let sessions = self.sessions.borrow();
            let state = sessions
                .get(&session_id)
                .ok_or_else(|| acp::Error::resource_not_found(None))?;
            (state.cancel.clone(), state.cancel.is_cancelled())
        };
        // Teardown order (mirrors run.rs EOF cleanup 244-257): fire cancel →
        // set terminal state → deregister → evict the map entry. The persisted
        // conversation is RETAINED — close is teardown, not archival; the
        // session stays listable/resumable (AC5).
        cancel.cancel();
        let terminal = if was_cancelled {
            NodeState::Cancelled
        } else {
            NodeState::Completed
        };
        self.node_tree.set_state(&agent_id, terminal).await;
        self.node_tree.deregister(&agent_id).await;
        self.sessions.borrow_mut().remove(&session_id);
        Ok(acp::CloseSessionResponse::new())
    }
}
