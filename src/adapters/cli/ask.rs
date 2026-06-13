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
use serde::Serialize;
use serde_json;

use crate::domain::models::{
    AppConfig, CompletionOptions, FileContextProvenance, MessageRole, SkillActivationSet,
    StopReason, StreamChunk, ToolCallInfo, ToolResultInfo, UsageInfo,
};
use crate::domain::ports::StreamingProvider;
use crate::domain::services::message_builder::{self, ResolvedFileContext};
use crate::domain::services::model_router::{ModelResolutionRequest, resolve_effective_model};
use crate::infrastructure::composition::CliCore;
use crate::infrastructure::runtime::turn;
const FILE_SIZE_CAP: usize = 100 * 1024;

/// Output format selector for `rustain ask` (Story 13.1b).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// Plain assistant text (default; byte-identical to Story 13.1a).
    Text,
    /// Single structured JSON document.
    Json,
    /// Newline-delimited JSON event stream.
    StreamJson,
}

impl OutputFormat {
    /// Parse the CLI string into a typed selector.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "text" => Some(Self::Text),
            "json" => Some(Self::Json),
            "stream-json" => Some(Self::StreamJson),
            _ => None,
        }
    }
}

/// v1.0 schema version string (Story 13.1b, NFR31 major.minor).
const SCHEMA_VERSION: &str = "1.0";

/// Output-only snake_case DTO for token usage (boundary coercion, AC1/O6).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
struct UsageOut {
    input_tokens: u32,
    output_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_creation_input_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_read_input_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_tokens: Option<u32>,
}

impl From<&UsageInfo> for UsageOut {
    fn from(u: &UsageInfo) -> Self {
        Self {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            cache_creation_input_tokens: u.cache_creation_input_tokens,
            cache_read_input_tokens: u.cache_read_input_tokens,
            reasoning_tokens: u.reasoning_tokens,
        }
    }
}

/// Output-only snake_case DTO for a tool result (boundary coercion, AC1/O6).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
struct ToolResultOut {
    content: String,
    is_error: bool,
}

impl From<&ToolResultInfo> for ToolResultOut {
    fn from(r: &ToolResultInfo) -> Self {
        Self {
            content: r.content.clone(),
            is_error: r.is_error,
        }
    }
}

/// Output-only snake_case DTO for a tool call (boundary coercion, AC1/O6).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
struct ToolCallOut {
    id: String,
    name: String,
    input: serde_json::Value,
    result: Option<ToolResultOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    started_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    completed_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,
}

impl From<&ToolCallInfo> for ToolCallOut {
    fn from(tc: &ToolCallInfo) -> Self {
        Self {
            id: tc.id.clone(),
            name: tc.name.clone(),
            input: tc.input.clone(),
            result: tc.result.as_ref().map(|r| r.into()),
            started_at_ms: tc.started_at_ms,
            completed_at_ms: tc.completed_at_ms,
            status: tc.status.clone(),
        }
    }
}

/// Structured JSON response envelope for `--output-format json` (AC1).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
struct AskResponse {
    schema_version: &'static str,
    response: Option<String>,
    model: String,
    stop_reason: String,
    usage: Option<UsageOut>,
    tool_calls: Vec<ToolCallOut>,
    session_id: String,
    deny_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ErrorOut>,
}

/// Error object inside the shared envelope (AC6).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
struct ErrorOut {
    message: String,
}

/// NDJSON event line for `--output-format stream-json` (AC2).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
struct StreamEvent {
    schema_version: &'static str,
    #[serde(rename = "type")]
    event_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    input: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<ToolResultOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<UsageOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    deny_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ErrorOut>,
}

fn stop_reason_str(reason: &StopReason) -> &'static str {
    match reason {
        StopReason::EndTurn => "end_turn",
        StopReason::ToolUse => "tool_use",
        StopReason::MaxTokens => "max_tokens",
        StopReason::Cancelled => "cancelled",
    }
}

/// Bundled output state handed to the renderer at turn end.
struct AskOutcome {
    assistant_text: String,
    last_block_start: usize,
    tool_calls: Vec<ToolCallInfo>,
    turn_stop_reason: StopReason,
    turn_usage: Option<UsageInfo>,
    model: String,
    session_id: String,
    deny_count: u32,
}

/// Concrete enum-dispatched renderer that owns the `out` writer (AC5).
/// Only this struct writes to `out` in `run_ask_core`.
struct AskRenderer<'a> {
    fmt: OutputFormat,
    out: &'a mut dyn Write,
}

impl<'a> AskRenderer<'a> {
    fn new(fmt: OutputFormat, out: &'a mut dyn Write) -> Self {
        Self { fmt, out }
    }

    /// Emit a stream-json event line for an in-flight chunk.
    fn on_chunk(&mut self, chunk: &StreamChunk) -> Result<(), std::io::Error> {
        if self.fmt != OutputFormat::StreamJson {
            return Ok(());
        }
        let event = match chunk {
            StreamChunk::Text { content, .. } => StreamEvent {
                schema_version: SCHEMA_VERSION,
                event_type: "text",
                content: Some(content.clone()),
                id: None,
                name: None,
                input: None,
                result: None,
                usage: None,
                stop_reason: None,
                deny_count: None,
                error: None,
            },
            StreamChunk::ToolUse { id, name, input } => StreamEvent {
                schema_version: SCHEMA_VERSION,
                event_type: "tool_use",
                content: None,
                id: Some(id.clone()),
                name: Some(name.clone()),
                input: Some(input.clone()),
                result: None,
                usage: None,
                stop_reason: None,
                deny_count: None,
                error: None,
            },
            StreamChunk::ToolResult {
                id,
                content,
                is_error,
            } => StreamEvent {
                schema_version: SCHEMA_VERSION,
                event_type: "tool_result",
                content: None,
                id: Some(id.clone()),
                name: None,
                input: None,
                result: Some(ToolResultOut {
                    content: content.clone(),
                    is_error: *is_error,
                }),
                usage: None,
                stop_reason: None,
                deny_count: None,
                error: None,
            },
            _ => return Ok(()),
        };
        let line = serde_json::to_string(&event)?;
        writeln!(self.out, "{}", line)
    }

    /// Render final output for `text`/`json`; emit terminal stream-json lines.
    fn finish(self, outcome: &AskOutcome, final_message_only: bool) -> Result<(), std::io::Error> {
        debug_assert!(outcome.last_block_start <= outcome.assistant_text.len(),
            "last_block_start ({}) exceeds assistant_text len ({})",
            outcome.last_block_start, outcome.assistant_text.len());
        let response_text = if final_message_only {
            let start = outcome.last_block_start.min(outcome.assistant_text.len());
            &outcome.assistant_text[start..]
        } else {
            &outcome.assistant_text
        };

        match self.fmt {
            OutputFormat::Text => {
                if !response_text.is_empty() {
                    self.out.write_all(response_text.as_bytes())?;
                    if !response_text.ends_with('\n') {
                        self.out.write_all(b"\n")?;
                    }
                }
                self.out.flush()
            }
            OutputFormat::Json => {
                let doc = AskResponse {
                    schema_version: SCHEMA_VERSION,
                    response: Some(response_text.to_string()),
                    model: outcome.model.clone(),
                    stop_reason: stop_reason_str(&outcome.turn_stop_reason).to_string(),
                    usage: outcome.turn_usage.as_ref().map(|u| u.into()),
                    tool_calls: outcome.tool_calls.iter().map(|tc| tc.into()).collect(),
                    session_id: outcome.session_id.clone(),
                    deny_count: outcome.deny_count,
                    error: None,
                };
                let json = serde_json::to_string_pretty(&doc)?;
                writeln!(self.out, "{}", json)?;
                self.out.flush()
            }
            OutputFormat::StreamJson => {
                if let Some(ref usage) = outcome.turn_usage {
                    let event = StreamEvent {
                        schema_version: SCHEMA_VERSION,
                        event_type: "usage",
                        content: None,
                        id: None,
                        name: None,
                        input: None,
                        result: None,
                        usage: Some(usage.into()),
                        stop_reason: None,
                        deny_count: None,
                        error: None,
                    };
                    let line = serde_json::to_string(&event)?;
                    writeln!(self.out, "{}", line)?;
                }
                let complete = StreamEvent {
                    schema_version: SCHEMA_VERSION,
                    event_type: "turn_complete",
                    content: None,
                    id: None,
                    name: None,
                    input: None,
                    result: None,
                    usage: None,
                    stop_reason: Some(stop_reason_str(&outcome.turn_stop_reason).to_string()),
                    deny_count: Some(outcome.deny_count),
                    error: None,
                };
                let line = serde_json::to_string(&complete)?;
                writeln!(self.out, "{}", line)?;
                self.out.flush()
            }
        }
    }

    /// Emit a format-correct error document. Does NOT consume the renderer so it
    /// can be called from mid-turn failure paths before `finish`.
    fn emit_error(&mut self, message: &str) -> Result<(), std::io::Error> {
        match self.fmt {
            OutputFormat::Text => Ok(()),
            OutputFormat::Json => {
                let doc = AskResponse {
                    schema_version: SCHEMA_VERSION,
                    response: None,
                    model: String::new(),
                    stop_reason: String::new(),
                    usage: None,
                    tool_calls: vec![],
                    session_id: String::new(),
                    deny_count: 0,
                    error: Some(ErrorOut {
                        message: message.to_string(),
                    }),
                };
                let json = serde_json::to_string_pretty(&doc)?;
                writeln!(self.out, "{}", json)
            }
            OutputFormat::StreamJson => {
                let event = StreamEvent {
                    schema_version: SCHEMA_VERSION,
                    event_type: "error",
                    content: None,
                    id: None,
                    name: None,
                    input: None,
                    result: None,
                    usage: None,
                    stop_reason: None,
                    deny_count: None,
                    error: Some(ErrorOut {
                        message: message.to_string(),
                    }),
                };
                let line = serde_json::to_string(&event)?;
                writeln!(self.out, "{}", line)
            }
        }
    }
}

/// Helper: write an error to stderr (all formats) and, for json/stream-json,
/// write a structured error document to stdout. Returns FAILURE.
fn emit_cli_error(
    fmt: OutputFormat,
    out: &mut dyn Write,
    err: &mut dyn Write,
    message: &str,
) -> ExitCode {
    let _ = writeln!(err, "Error: {}", message);
    if fmt != OutputFormat::Text {
        let mut renderer = AskRenderer::new(fmt, out);
        let _ = renderer.emit_error(message);
    }
    ExitCode::FAILURE
}

/// Emit a format-correct CLI error and terminate the process.
/// Used by `run_ask` for pre-turn failures (e.g. unreadable --file) where
/// returning a `Result` would route through the generic text-only error printer.
fn exit_with_cli_error(fmt: OutputFormat, message: &str) -> ! {
    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();
    let _ = emit_cli_error(fmt, &mut stdout, &mut stderr, message);
    let _ = stdout.flush();
    let _ = stderr.flush();
    std::process::exit(1);
}

pub struct AskOpts {
    pub query: String,
    pub files: Vec<ResolvedFileContext>,
    pub yolo: bool,
    pub final_message_only: bool,
    pub output_format: OutputFormat,
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

    // Format is parsed first so every failure path can emit a format-correct error document.
    let output_format = opts.output_format;

    if opts.query.trim().is_empty() {
        return emit_cli_error(output_format, out, err, "query cannot be empty");
    }

    let mut conversation = if let Some(ref sid) = opts.session_id {
        match storage.load_conversation(sid).await {
            Ok(Some(conv)) => conv,
            Ok(None) => {
                return emit_cli_error(
                    output_format,
                    out,
                    err,
                    &format!("session '{}' not found", sid),
                );
            }
            Err(e) => {
                return emit_cli_error(output_format, out, err, &format!("loading session: {}", e));
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
    let model_used = options.model.clone();

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
    let mut turn_usage: Option<UsageInfo> = None;
    let mut renderer = AskRenderer::new(output_format, out);
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
                    if !turn_complete {
                        assistant_text.push_str(content);
                        if let Err(e) = renderer.on_chunk(chunk) {
                            let _ = writeln!(err, "Error: failed to write stream output: {}", e);
                            return ExitCode::FAILURE;
                        }
                    }
                }
                StreamChunk::ToolUse { id, name, input } => {
                    if !turn_complete {
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
                    if !turn_complete {
                        if let Err(e) = renderer.on_chunk(chunk) {
                            let _ = writeln!(err, "Error: failed to write stream output: {}", e);
                            return ExitCode::FAILURE;
                        }
                    }
                }
                StreamChunk::ToolResult {
                    id,
                    content,
                    is_error,
                } => {
                    if !turn_complete {
                        last_block_start = assistant_text.len();
                        if let Some(tc) = tool_calls.iter_mut().find(|tc| tc.id == *id) {
                            tc.result = Some(ToolResultInfo {
                                content: content.clone(),
                                is_error: *is_error,
                            });
                        } else {
                            tracing::warn!(
                                tool_id = %id,
                                "Received tool result for unknown tool id; result will not appear in json output"
                            );
                        }
                    }
                    if !turn_complete {
                        if let Err(e) = renderer.on_chunk(chunk) {
                            let _ = writeln!(err, "Error: failed to write stream output: {}", e);
                            return ExitCode::FAILURE;
                        }
                    }
                }
                StreamChunk::TurnComplete { stop_reason } => {
                    turn_stop_reason = stop_reason.clone();
                    if matches!(
                        stop_reason,
                        StopReason::EndTurn | StopReason::MaxTokens | StopReason::Cancelled
                    ) {
                        turn_complete = true;
                        // Drain remaining chunks (e.g. Usage may arrive after TurnComplete)
                        // rather than breaking immediately. The channel closes naturally when
                        // the turn task ends.
                        continue;
                    }
                }
                StreamChunk::Usage { usage, .. } => {
                    turn_usage = Some(usage.clone());
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
        let _ = renderer.emit_error(&format!("turn failed: {}", error));
        return ExitCode::FAILURE;
    }

    if !turn_complete {
        let _ = writeln!(err, "Error: turn did not complete");
        let _ = renderer.emit_error("turn did not complete");
        return ExitCode::FAILURE;
    }

    let outcome = AskOutcome {
        assistant_text,
        last_block_start,
        tool_calls,
        turn_stop_reason,
        turn_usage,
        model: model_used,
        session_id: conversation
            .session_id
            .clone()
            .unwrap_or_else(|| conversation.id.clone()),
        deny_count: approval.rejected_count() as u32,
    };
    if let Err(e) = renderer.finish(&outcome, opts.final_message_only) {
        let _ = writeln!(err, "Error: failed to write output: {}", e);
        return ExitCode::FAILURE;
    }

    conversation.messages.push(ChatMessage {
        id: generate_message_id(),
        role: MessageRole::Assistant,
        content: outcome.assistant_text,
        content_blocks: vec![],
        tool_calls: outcome.tool_calls,
        created_at: now_unix(),
        token_count: None,
        stop_reason: Some(outcome.turn_stop_reason.clone()),
        images: vec![],
        synthetic: false,
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
    output_format: String,
    app_config: AppConfig,
    session_id: Option<String>,
    force_new: bool,
    model_override: Option<String>,
) -> Result<()> {
    let workspace = std::env::current_dir()?;

    let resolved_output_format = OutputFormat::parse(&output_format).unwrap_or(OutputFormat::Text);

    let mut resolved_files = Vec::new();
    for path in &files {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("cannot read file '{}': {}", path.display(), e);
                if resolved_output_format != OutputFormat::Text {
                    exit_with_cli_error(resolved_output_format, &msg);
                }
                return Err(anyhow::anyhow!(msg));
            }
        };
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
        let stdin_content = match read_limited_stdin(FILE_SIZE_CAP).await {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("failed to read stdin: {}", e);
                if resolved_output_format != OutputFormat::Text {
                    exit_with_cli_error(resolved_output_format, &msg);
                }
                return Err(e);
            }
        };
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
        output_format: resolved_output_format,
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
