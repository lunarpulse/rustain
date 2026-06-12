use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

use crate::domain::events::AppEvent;
use crate::domain::models::conversation::{
    ChatMessage, Conversation, generate_conversation_id, generate_message_id,
};
use crate::domain::models::session_meta::now_unix;
use crate::domain::models::{
    AppConfig, CompletionOptions, FileContextProvenance, MessageRole, SkillActivationSet,
    StopReason, StreamChunk, ToolCallInfo, ToolResultInfo,
};
use crate::domain::ports::StreamingProvider;
use crate::domain::services::message_builder::{self, ResolvedFileContext};
use crate::domain::services::model_router::{ModelResolutionRequest, resolve_effective_model};
use crate::infrastructure::composition::CliCore;
use crate::infrastructure::runtime::turn;
const FILE_SIZE_CAP: usize = 100 * 1024;

pub struct AskOpts {
    pub query: String,
    pub files: Vec<ResolvedFileContext>,
    pub yolo: bool,
    pub final_message_only: bool,
    pub model_override: Option<String>,
    pub session_id: Option<String>,
}
pub async fn run_ask_core(
    provider: Arc<dyn StreamingProvider>,
    core: CliCore,
    opts: AskOpts,
    app_config: &AppConfig,
    workspace: &std::path::Path,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    // Destructure core up front; clone Arcs we need after the turn.
    let CliCore {
        provider: _,
        security,
        tools,
        tool_scheduler,
        approval,
        storage,
        event_tx,
        event_rx,
        ledger,
    } = core;

    if opts.query.trim().is_empty() {
        let _ = writeln!(err, "Error: query cannot be empty");
        return ExitCode::FAILURE;
    }

    let mut conversation = if let Some(ref sid) = opts.session_id {
        match storage.load_conversation(sid).await {
            Ok(Some(conv)) => conv,
            Ok(None) => {
                let _ = writeln!(err, "Error: session '{}' not found", sid);
                return ExitCode::FAILURE;
            }
            Err(e) => {
                let _ = writeln!(err, "Error loading session: {}", e);
                return ExitCode::FAILURE;
            }
        }
    } else {
        let now = now_unix();
        Conversation {
            id: generate_conversation_id(),
            title: String::new(),
            messages: vec![],
            turns: vec![],
            created_at: now,
            updated_at: now,
            last_response_at: None,
            session_id: None,
            usage: None,
            plans: Default::default(),
            fork_source: None,
            compaction: None,
        }
    };

    let file_prefix = message_builder::build_file_context_prefix(&opts.files);
    let user_text = if file_prefix.is_empty() {
        opts.query.clone()
    } else {
        format!("{}{}", file_prefix, opts.query)
    };

    conversation.messages.push(ChatMessage {
        id: generate_message_id(),
        role: MessageRole::User,
        content: user_text,
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
    let persona_prompt = crate::domain::ports::PersonaPort::system_prompt(&persona, workspace);
    let empty_set = SkillActivationSet::new();
    let system_prompt = crate::domain::services::skill_context::assemble_system_prompt(
        &persona_prompt,
        &empty_set,
        workspace,
    );

    let tool_defs = tools.available_tools();

    let req = ModelResolutionRequest {
        explicit_override: opts.model_override.clone(),
        tier_hint: None,
        step_kind: None,
        retry_count: 0,
        input_tokens: 0,
        fallback_model: app_config.model.clone(),
    };
    let resolved = resolve_effective_model(&req, &app_config.router);

    let options = CompletionOptions {
        model: resolved.model.clone(),
        max_tokens: 8192,
        system_prompt,
        temperature: None,
        tools: tool_defs,
    };

    let session_id = conversation
        .session_id
        .clone()
        .unwrap_or_else(|| conversation.id.clone());

    let turn_cancel = CancellationToken::new();

    let mut approval_rx = approval.subscribe();
    let approval_arc = approval.clone();
    let final_message_only = opts.final_message_only;
    let (deny_msg_tx, mut deny_msg_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let deny_consumer = if !opts.yolo {
        Some(tokio::spawn(async move {
            use crate::domain::services::approval_runtime::ApprovalRuntimeEvent;
            loop {
                match approval_rx.recv().await {
                    Ok(ApprovalRuntimeEvent::Requested { id, tool, .. }) => {
                        if !final_message_only {
                            let _ = deny_msg_tx.send(format!(
                                "Tool '{}' requires approval. Use --yolo for auto-approve or --dry-run for plan only.",
                                tool
                            ));
                        }
                        use crate::domain::models::ApprovalOutcome;
                        approval_arc
                            .resolve(&id, ApprovalOutcome::Reject { feedback: None })
                            .await;
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                }
            }
        }))
    } else {
        None
    };

    let conv_id = conversation.id.clone();
    let mut event_rx = event_rx;

    // Move event_tx into the turn — when the turn ends, the channel closes,
    // unblocking the event_rx.recv() loop below.
    tokio::spawn(turn::run_turn(
        provider,
        messages,
        options,
        event_tx,
        security.clone(),
        tools.clone(),
        tool_scheduler.clone(),
        conv_id.clone(),
        storage.clone(),
        conversation.clone(),
        None,
        turn_cancel,
        ledger.clone(),
        resolved,
        None,
        0,
        None,
        session_id,
    ));

    let mut assistant_text = String::new();
    let mut last_block_start = 0usize;
    let mut turn_complete = false;
    let mut turn_error: Option<String> = None;
    let mut turn_stop_reason = StopReason::EndTurn;
    let mut tool_calls: Vec<ToolCallInfo> = Vec::new();

    loop {
        let event = tokio::select! {
            biased;
            event = event_rx.recv() => event,
            msg = deny_msg_rx.recv() => {
                if let Some(msg) = msg {
                    let _ = writeln!(err, "{}", msg);
                }
                continue;
            }
        };
        let Some(event) = event else { break };
        match &event {
            AppEvent::ProviderChunk { chunk, .. } => match chunk {
                StreamChunk::Text { content, .. } => {
                    assistant_text.push_str(content);
                }
                StreamChunk::ToolUse { id, name, input } => {
                    last_block_start = assistant_text.len();
                    tool_calls.push(ToolCallInfo {
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
                    last_block_start = assistant_text.len();
                    if let Some(tc) = tool_calls.iter_mut().find(|tc| tc.id == *id) {
                        tc.result = Some(ToolResultInfo {
                            content: content.clone(),
                            is_error: *is_error,
                        });
                    }
                }
                StreamChunk::TurnComplete { stop_reason } => {
                    turn_stop_reason = stop_reason.clone();
                    if matches!(
                        stop_reason,
                        StopReason::EndTurn | StopReason::MaxTokens | StopReason::Cancelled
                    ) {
                        turn_complete = true;
                        break;
                    }
                }
                StreamChunk::Error { content } => {
                    let _ = writeln!(err, "Error: {}", content);
                    turn_error = Some(content.clone());
                    break;
                }
                _ => {}
            },
            AppEvent::SystemNotice { level, message, .. } => {
                if *level == crate::domain::models::NoticeLevel::Error {
                    let _ = writeln!(err, "Error: {}", message);
                    turn_error = Some(message.clone());
                    break;
                }
            }
            _ => {}
        }
    }

    if let Some(handle) = deny_consumer {
        handle.abort();
    }
    while let Ok(msg) = deny_msg_rx.try_recv() {
        let _ = writeln!(err, "{}", msg);
    }

    if let Some(error) = turn_error {
        let _ = writeln!(err, "Error: turn failed: {}", error);
        return ExitCode::FAILURE;
    }

    if !turn_complete {
        let _ = writeln!(err, "Error: turn did not complete");
        return ExitCode::FAILURE;
    }

    let output_text = if opts.final_message_only {
        &assistant_text[last_block_start..]
    } else {
        &assistant_text
    };

    if !output_text.is_empty() {
        if out.write_all(output_text.as_bytes()).is_err() {
            let _ = writeln!(err, "Error: failed to write output");
            return ExitCode::FAILURE;
        }
        if !output_text.ends_with('\n') {
            if out.write_all(b"\n").is_err() {
                let _ = writeln!(err, "Error: failed to write output");
                return ExitCode::FAILURE;
            }
        }
        if out.flush().is_err() {
            let _ = writeln!(err, "Error: failed to flush output");
            return ExitCode::FAILURE;
        }
    }

    conversation.messages.push(ChatMessage {
        id: generate_message_id(),
        role: MessageRole::Assistant,
        content: assistant_text,
        content_blocks: vec![],
        tool_calls,
        created_at: now_unix(),
        token_count: None,
        stop_reason: Some(turn_stop_reason),
        synthetic: false,
        images: vec![],
        origin: crate::domain::models::ChannelKind::Terminal,
    });
    conversation.updated_at = now_unix();
    conversation.last_response_at = Some(now_unix());

    if let Err(e) = storage.save_conversation(&conversation).await {
        if !opts.final_message_only {
            let _ = writeln!(err, "Warning: failed to save session: {}", e);
        }
    }

    let dc = approval.rejected_count();
    if dc > 0 && !opts.final_message_only {
        let _ = writeln!(err, "{} tool call(s) auto-denied", dc);
    }

    if !opts.final_message_only {
        let _ = writeln!(
            err,
            "Session saved. Resume with: rustain --session {}",
            conversation.id
        );
    }

    ExitCode::SUCCESS
}

pub async fn run_ask(
    query: String,
    files: Vec<PathBuf>,
    yolo: bool,
    final_message_only: bool,
    app_config: AppConfig,
    session_id: Option<String>,
    force_new: bool,
    model_override: Option<String>,
) -> Result<()> {
    let workspace = std::env::current_dir()?;

    let mut resolved_files = Vec::new();
    for path in &files {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read file '{}'", path.display()))?;
        let truncated = if content.len() > FILE_SIZE_CAP {
            let boundary = content.floor_char_boundary(FILE_SIZE_CAP);
            content[..boundary].to_string()
        } else {
            content
        };
        resolved_files.push(ResolvedFileContext {
            path: path.to_string_lossy().to_string(),
            content: truncated,
            provenance: FileContextProvenance::UserProvided,
        });
    }

    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        let stdin_content = read_limited_stdin(FILE_SIZE_CAP).await?;
        if !stdin_content.is_empty() {
            resolved_files.push(ResolvedFileContext {
                path: "<stdin>".to_string(),
                content: stdin_content,
                provenance: FileContextProvenance::UserProvided,
            });
        }
    }

    let effective_session = if force_new { None } else { session_id };

    let core = crate::infrastructure::composition::build_cli_core(&app_config, &workspace, yolo)
        .map_err(|e| anyhow::anyhow!("Failed to initialize: {}", e))?;

    let provider = core.provider.clone();

    let opts = AskOpts {
        query,
        files: resolved_files,
        yolo,
        final_message_only,
        model_override,
        session_id: effective_session,
    };

    let exit_code = run_ask_core(
        provider,
        core,
        opts,
        &app_config,
        &workspace,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    )
    .await;

    if exit_code != ExitCode::SUCCESS {
        std::process::exit(1);
    }

    Ok(())
}

async fn read_limited_stdin(cap: usize) -> Result<String> {
    let mut buf = Vec::new();
    let mut reader = tokio::io::stdin().take(cap as u64);
    reader.read_to_end(&mut buf).await?;
    match String::from_utf8(buf) {
        Ok(s) => Ok(s),
        Err(e) => {
            let valid_up_to = e.utf8_error().valid_up_to();
            Ok(String::from_utf8_lossy(&e.into_bytes()[..valid_up_to]).into_owned())
        }
    }
}
