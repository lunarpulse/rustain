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
    AppConfig, CompletionOptions, FileContextProvenance, MessageRole, Plan, SkillActivationSet,
    StopReason, StreamChunk, ToolCallInfo, ToolResultInfo, TurnOrigin, UsageInfo,
};
use crate::domain::ports::StreamingProvider;
use crate::domain::services::message_builder::{self, ResolvedFileContext};
use crate::domain::services::model_router::{ModelResolutionRequest, resolve_effective_model};
use crate::infrastructure::composition::CliCore;
use crate::infrastructure::runtime::turn;

use crate::domain::errors::ProviderError;

/// Parse a `ProviderError` from its `Display` string representation.
/// Used at the two string boundaries (StreamChunk::Error and SystemNotice)
/// where the structured error has been flattened to a string.
fn parse_provider_error_display(s: &str) -> ProviderError {
    if let Some(rest) = s.strip_prefix("Offline: ") {
        ProviderError::Offline(rest.to_string())
    } else if let Some(rest) = s.strip_prefix("Connection failed: ") {
        ProviderError::ConnectionFailed(rest.to_string())
    } else if s == "Authentication failed" {
        ProviderError::AuthenticationFailed
    } else if s == "Request cancelled" {
        ProviderError::Cancelled
    } else if let Some(rest) = s.strip_prefix("Stream error: ") {
        ProviderError::StreamError(rest.to_string())
    } else if let Some(rest) = s.strip_prefix("endpoint unsupported (HTTP ") {
        let code = rest.trim_end_matches(')').parse::<u16>().unwrap_or(0);
        ProviderError::EndpointUnsupported(code)
    } else {
        ProviderError::Other(s.to_string())
    }
}
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

/// Schema version string. Bumped 1.0 → 1.1 in Story 13.1c (additive: dry_run, plan, tools_would_use).
const SCHEMA_VERSION: &str = "1.1";

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

/// Output-only snake_case DTO for `PlanSubTask` (boundary coercion, Story 13.1c AC5).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
struct PlanSubTaskOut {
    number: u32,
    title: String,
    description: String,
}

/// Output-only snake_case DTO for effort estimates (boundary coercion, Story 13.1c AC5).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
struct EffortOut {
    tool_calls: Option<u32>,
    seconds: Option<u32>,
}

/// Output-only snake_case DTO for a plan task (boundary coercion, Story 13.1c AC5).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
struct PlanTaskOut {
    number: u32,
    title: String,
    description: String,
    depends_on: Vec<u32>,
    sub_tasks: Vec<PlanSubTaskOut>,
}

/// Output-only snake_case DTO for a proposed plan (boundary coercion, Story 13.1c AC5).
/// Renders only proposal-relevant fields from the domain `Plan` (omits execution-tracking
/// fields like `started_at_ms`/`result` which are always empty in dry-run).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
struct PlanOut {
    id: String,
    title: String,
    tasks: Vec<PlanTaskOut>,
    estimated_effort: Option<EffortOut>,
    status: String,
    created_at: i64,
}

/// Map `PlanStatus` to its stable snake_case output string (Story 13.1c AC5).
/// Avoids relying on `Debug` formatting, which is not a stable contract.
fn plan_status_to_string(status: crate::domain::models::PlanStatus) -> String {
    match status {
        crate::domain::models::PlanStatus::Pending => "pending".to_string(),
        crate::domain::models::PlanStatus::Executing => "executing".to_string(),
        crate::domain::models::PlanStatus::Completed => "completed".to_string(),
        crate::domain::models::PlanStatus::Rejected => "rejected".to_string(),
        crate::domain::models::PlanStatus::Editing => "editing".to_string(),
        crate::domain::models::PlanStatus::Cancelled => "cancelled".to_string(),
    }
}

impl From<&Plan> for PlanOut {
    fn from(p: &Plan) -> Self {
        Self {
            id: p.id.clone(),
            title: p.title.clone(),
            tasks: p
                .tasks
                .iter()
                .map(|t| PlanTaskOut {
                    number: t.number,
                    title: t.title.clone(),
                    description: t.description.clone(),
                    depends_on: t.depends_on.clone(),
                    sub_tasks: t
                        .sub_tasks
                        .iter()
                        .map(|st| PlanSubTaskOut {
                            number: st.number,
                            title: st.title.clone(),
                            description: st.description.clone(),
                        })
                        .collect(),
                })
                .collect(),
            estimated_effort: p.estimated_effort.as_ref().map(|e| EffortOut {
                tool_calls: e.tool_calls,
                seconds: e.seconds,
            }),
            status: plan_status_to_string(p.status),
            created_at: p.created_at,
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
    /// Story 13.1c: ALWAYS-PRESENT (no skip_serializing_if) — flag-invariant field-path set (O3).
    dry_run: bool,
    /// Story 13.1c: ALWAYS-PRESENT — serializes `null` when no plan, NOT omitted (O3).
    plan: Option<PlanOut>,
    /// Story 13.1c: ALWAYS-PRESENT — serializes `[]` when empty, NOT omitted (O3).
    tools_would_use: Vec<String>,
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
    /// Story 13.1c: present on `plan_proposed` and `turn_complete` events.
    #[serde(skip_serializing_if = "Option::is_none")]
    dry_run: Option<bool>,
    /// Story 13.1c: present on `plan_proposed` event.
    #[serde(skip_serializing_if = "Option::is_none")]
    plan: Option<PlanOut>,
    /// Story 13.1c: present on `turn_complete` event.
    #[serde(skip_serializing_if = "Option::is_none")]
    tools_would_use: Option<Vec<String>>,
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
    /// Story 13.1c: whether this was a dry-run turn.
    dry_run: bool,
    /// Story 13.1c: plan proposed via `propose_plan` (last-wins).
    proposed_plan: Option<Plan>,
    /// Story 13.1c: deduplicated sorted tool names the model attempted, minus plan-control builtins.
    tools_would_use: Vec<String>,
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
                dry_run: None,
                plan: None,
                tools_would_use: None,
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
                dry_run: None,
                plan: None,
                tools_would_use: None,
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
                dry_run: None,
                plan: None,
                tools_would_use: None,
                error: None,
            },
            _ => return Ok(()),
        };
        let line = serde_json::to_string(&event)?;
        writeln!(self.out, "{}", line)
    }

    /// Render final output for `text`/`json`; emit terminal stream-json lines.
    fn finish(self, outcome: &AskOutcome, final_message_only: bool) -> Result<(), std::io::Error> {
        debug_assert!(
            outcome.last_block_start <= outcome.assistant_text.len(),
            "last_block_start ({}) exceeds assistant_text len ({})",
            outcome.last_block_start,
            outcome.assistant_text.len()
        );
        let response_text = if final_message_only {
            let start = outcome.last_block_start.min(outcome.assistant_text.len());
            &outcome.assistant_text[start..]
        } else {
            &outcome.assistant_text
        };

        match self.fmt {
            OutputFormat::Text => {
                // Story 13.1c: dry-run text rendering.
                if outcome.dry_run {
                    if let Some(ref plan) = outcome.proposed_plan {
                        // Title
                        writeln!(self.out, "{}", plan.title)?;
                        writeln!(self.out)?;
                        // Numbered tasks
                        for task in &plan.tasks {
                            writeln!(self.out, "{}. {}", task.number, task.title)?;
                            if !task.description.is_empty() {
                                for line in task.description.lines() {
                                    writeln!(self.out, "   {}", line)?;
                                }
                            }
                            for st in &task.sub_tasks {
                                writeln!(
                                    self.out,
                                    "   {}.{}. {}",
                                    task.number, st.number, st.title
                                )?;
                                if !st.description.is_empty() {
                                    for line in st.description.lines() {
                                        writeln!(self.out, "      {}", line)?;
                                    }
                                }
                            }
                        }
                        // Estimated effort
                        if let Some(ref effort) = plan.estimated_effort {
                            let mut parts = Vec::new();
                            if let Some(tc) = effort.tool_calls {
                                parts.push(format!("~{} tool calls", tc));
                            }
                            if let Some(s) = effort.seconds {
                                parts.push(format!("~{}s", s));
                            }
                            if !parts.is_empty() {
                                writeln!(self.out, "\nEstimated: {}", parts.join(", "))?;
                            }
                        }
                    } else {
                        // No propose_plan call — fall back to assistant prose.
                        if !response_text.is_empty() {
                            self.out.write_all(response_text.as_bytes())?;
                            if !response_text.ends_with('\n') {
                                self.out.write_all(b"\n")?;
                            }
                        }
                    }
                    // Attempted-tools line (always, even when empty).
                    if outcome.tools_would_use.is_empty() {
                        writeln!(self.out, "Tools attempted: (none)")?;
                    } else {
                        writeln!(
                            self.out,
                            "Tools attempted: {}",
                            outcome.tools_would_use.join(", ")
                        )?;
                    }
                    self.out.flush()
                } else {
                    if !response_text.is_empty() {
                        self.out.write_all(response_text.as_bytes())?;
                        if !response_text.ends_with('\n') {
                            self.out.write_all(b"\n")?;
                        }
                    }
                    self.out.flush()
                }
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
                    dry_run: outcome.dry_run,
                    plan: outcome.proposed_plan.as_ref().map(|p| p.into()),
                    tools_would_use: outcome.tools_would_use.clone(),
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
                        dry_run: None,
                        plan: None,
                        tools_would_use: None,
                        error: None,
                    };
                    let line = serde_json::to_string(&event)?;
                    writeln!(self.out, "{}", line)?;
                }
                // Story 13.1c: emit plan_proposed line when a plan exists (before turn_complete).
                if let Some(ref plan) = outcome.proposed_plan {
                    let event = StreamEvent {
                        schema_version: SCHEMA_VERSION,
                        event_type: "plan_proposed",
                        content: None,
                        id: None,
                        name: None,
                        input: None,
                        result: None,
                        usage: None,
                        stop_reason: None,
                        deny_count: None,
                        dry_run: Some(outcome.dry_run),
                        plan: Some(plan.into()),
                        tools_would_use: None,
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
                    dry_run: Some(outcome.dry_run),
                    plan: None,
                    tools_would_use: Some(outcome.tools_would_use.clone()),
                    error: None,
                };
                let line = serde_json::to_string(&complete)?;
                writeln!(self.out, "{}", line)?;
                self.out.flush()
            }
        }
    }

    fn emit_error(&mut self, message: &str, dry_run: bool) -> Result<(), std::io::Error> {
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
                    dry_run,
                    plan: None,
                    tools_would_use: vec![],
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
                    dry_run: Some(dry_run),
                    plan: None,
                    tools_would_use: Some(vec![]),
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
    dry_run: bool,
) -> ExitCode {
    let _ = writeln!(err, "Error: {}", message);
    if fmt != OutputFormat::Text {
        let mut renderer = AskRenderer::new(fmt, out);
        let _ = renderer.emit_error(message, dry_run);
    }
    ExitCode::FAILURE
}
/// Used by `run_ask` for pre-turn failures (e.g. unreadable --file) where
/// returning a `Result` would route through the generic text-only error printer.
fn exit_with_cli_error(fmt: OutputFormat, message: &str, dry_run: bool) -> ! {
    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();
    let _ = emit_cli_error(fmt, &mut stdout, &mut stderr, message, dry_run);
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
    /// Story 13.1c: dry-run plan mode — no state-mutating tools, no session write.
    pub dry_run: bool,
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

    // Story 13.1c: enter Plan mode when dry_run so the permission_chain gate
    // denies all state-mutating (Standard/Elevated-risk) tools.
    if opts.dry_run {
        security.set_mode(crate::domain::models::PermissionMode::Plan);
    }

    // Format is parsed first so every failure path can emit a format-correct error document.
    let output_format = opts.output_format;

    if opts.query.trim().is_empty() {
        return emit_cli_error(
            output_format,
            out,
            err,
            "query cannot be empty",
            opts.dry_run,
        );
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
                    opts.dry_run,
                );
            }
            Err(e) => {
                return emit_cli_error(
                    output_format,
                    out,
                    err,
                    &format!("loading session: {}", e),
                    opts.dry_run,
                );
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
        authorship: Default::default(),
        retracted_at_ms: None,
    });

    let messages = message_builder::build_api_messages(&conversation);

    let project_context = crate::domain::models::project_context::ProjectContext::empty();
    let persona = crate::adapters::persona_adapter::PersonaAdapter::new(project_context);
    let persona_prompt = crate::domain::ports::PersonaPort::system_prompt(&persona, workspace);
    let empty_set = SkillActivationSet::new();
    let mut system_prompt = crate::domain::services::skill_context::assemble_system_prompt(
        &persona_prompt,
        &empty_set,
        workspace,
    );

    // Story 13.1c (AC2, O7): append plan-mode addendum when dry_run.
    // Byte-unchanged when dry_run is false.
    if opts.dry_run {
        system_prompt.push_str("\n\n<plan-mode>\n");
        system_prompt.push_str(crate::domain::services::plan_mode_injector::dry_run_reminder());
        system_prompt.push_str("\n</plan-mode>");
    }

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
        TurnOrigin::Interactive,
    ));
    // Drop local Arc clones of tools/tool_scheduler so any event senders
    // held inside ToolSetPort adaptors are released. The spawned run_turn
    // task now holds the only remaining clones; when it finishes, the channel
    // closes and the event loop below can exit.
    drop(tools);
    drop(tool_scheduler);

    let mut assistant_text = String::new();
    let mut last_block_start = 0usize;
    let mut turn_complete = false;
    let mut turn_error: Option<ProviderError> = None;
    let mut turn_stop_reason = StopReason::EndTurn;
    let mut tool_calls: Vec<ToolCallInfo> = Vec::new();
    let mut turn_usage: Option<UsageInfo> = None;
    // Story 13.1c: accumulate the last proposed plan (last-wins, AC6).
    let mut proposed_plan: Option<Plan> = None;
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
                    let parsed = parse_provider_error_display(content);
                    if !parsed.is_offline() {
                        let _ = writeln!(err, "Error: {}", content);
                    }
                    turn_error = Some(parsed);
                    break;
                }
                _ => {}
            },
            AppEvent::SystemNotice { level, message, .. } => {
                if *level == crate::domain::models::NoticeLevel::Error {
                    let parsed = parse_provider_error_display(message);
                    // Defer stderr output for offline errors — handled post-loop (AC3).
                    if !parsed.is_offline() {
                        let _ = writeln!(err, "Error: {}", message);
                    }
                    turn_error = Some(parsed);
                    break;
                }
            }
            // Story 13.1c (AC6): capture the last proposed plan (last-wins).
            AppEvent::PlanProposed { plan, .. } => {
                proposed_plan = Some(plan.clone());
            }
            // Story 13.1c (AC6): benign no-op — no approval card in headless.
            AppEvent::PlanApprovalRequested { .. } => {}
            _ => {}
        }
    }

    if let Some(handle) = deny_consumer {
        handle.abort();
    }
    while let Ok(msg) = deny_msg_rx.try_recv() {
        let _ = writeln!(err, "{}", msg);
    }

    if let Some(ref error) = turn_error {
        // Story 13.2 (AC3): surface a specific offline message when a transport-level
        // failure occurs — matched on the domain variant, not a string prefix.
        if error.is_offline() {
            let msg = "✗ No provider available (offline). Use --dry-run for plan-only, or configure a local LLM.";
            return emit_cli_error(output_format, out, err, msg, opts.dry_run);
        }
        let _ = writeln!(err, "Error: turn failed: {}", error);
        let _ = renderer.emit_error(&format!("turn failed: {}", error), opts.dry_run);
        return ExitCode::FAILURE;
    }

    if !turn_complete {
        let _ = writeln!(err, "Error: turn did not complete");
        let _ = renderer.emit_error("turn did not complete", opts.dry_run);
        return ExitCode::FAILURE;
    }

    // Story 13.1c (AC4): derive tools_would_use — deduplicated, sorted tool names
    // the model attempted via ToolUse, excluding plan-control builtins.
    // Only meaningful in dry-run; normal runs report an empty array (O3).
    let tools_would_use = if opts.dry_run {
        let mut names: Vec<String> = tool_calls
            .iter()
            .map(|tc| tc.name.clone())
            .filter(|n| n != "propose_plan" && n != "exit_plan_mode")
            .collect();
        names.sort();
        names.dedup();
        names
    } else {
        vec![]
    };
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
        dry_run: opts.dry_run,
        proposed_plan,
        tools_would_use,
    };
    if let Err(e) = renderer.finish(&outcome, opts.final_message_only) {
        let _ = writeln!(err, "Error: failed to write output: {}", e);
        return ExitCode::FAILURE;
    }

    // Story 13.1c (AC7): dry-run is fully read-only — skip all session writes
    // and the resume hint. A dry-run leaves zero trace on disk.
    if !opts.dry_run {
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
            authorship: Default::default(),
            retracted_at_ms: None,
        });
        conversation.updated_at = now_unix();
        conversation.last_response_at = Some(now_unix());

        if let Err(e) = storage.save_conversation(&conversation).await {
            if !opts.final_message_only {
                let _ = writeln!(err, "Warning: failed to save session: {}", e);
            }
        }
    }

    let dc = approval.rejected_count();
    if dc > 0 && !opts.final_message_only {
        let _ = writeln!(err, "{} tool call(s) auto-denied", dc);
    }

    // Story 13.1c (AC7): no resume hint in dry-run (nothing was saved).
    if !opts.dry_run && !opts.final_message_only {
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
    dry_run: bool,
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
                    exit_with_cli_error(resolved_output_format, &msg, dry_run);
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
                    exit_with_cli_error(resolved_output_format, &msg, dry_run);
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
        dry_run,
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
