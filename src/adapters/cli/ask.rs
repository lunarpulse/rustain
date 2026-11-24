use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

use crate::adapters::cli::commands::AskLimits;
use crate::domain::events::AppEvent;
use crate::domain::models::conversation::{
    ChatMessage, Conversation, generate_conversation_id, generate_message_id,
};
use crate::domain::models::session_meta::now_unix;
use serde::Serialize;
use serde_json;

use crate::domain::models::{
    AppConfig, CompletionOptions, ContentBlockType, FileContextProvenance, MessageRole, Plan,
    SkillActivationSet, StopReason, StreamChunk, ToolCallInfo, ToolResultInfo, TurnOrigin,
    UsageInfo, UsageLedgerEntry,
};
use crate::domain::ports::{LedgerReadMode, StoragePort, StreamingProvider, UsageLedgerPort};
use crate::domain::services::message_builder::{self, ResolvedFileContext};
use crate::domain::services::model_router::{ModelResolutionRequest, resolve_effective_model};
use crate::infrastructure::composition::CliCore;
use crate::infrastructure::runtime::ask_limits::AskExecution;
use crate::infrastructure::runtime::turn;

use crate::domain::errors::{ProviderError, StorageError};

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

/// Schema version string. Bumped 1.0 → 1.1 in Story 13.1c (additive: dry_run, plan, tools_would_use)
/// and 1.1 → 1.2 for the additive `call_usage` stream event. 1.2 → 1.3 (eval-2): the
/// `usage` counts are disjoint for every provider (see `UsageInfo`), where OpenAI-style
/// providers reported `input_tokens` including cache reads before.
const SCHEMA_VERSION: &str = "1.3";
const LIMITED_SCHEMA_VERSION: &str = "1.4";

fn ask_limits_configured(limits: AskLimits, config: &AppConfig) -> bool {
    limits.max_turns.is_some()
        || limits.max_duration.is_some()
        || limits.max_cost_usd.is_some()
        || config.budget.daily_limit_usd.is_some()
}

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
    #[serde(skip_serializing_if = "Option::is_none")]
    limit_reason: Option<&'static str>,
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
    /// Present on `call_usage` events: the model that served the call.
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    limit_reason: Option<&'static str>,
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
    limit_reason: Option<&'static str>,
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
    schema_version: &'static str,
    out: &'a mut dyn Write,
}

impl<'a> AskRenderer<'a> {
    fn new(fmt: OutputFormat, limited: bool, out: &'a mut dyn Write) -> Self {
        Self {
            fmt,
            schema_version: if limited {
                LIMITED_SCHEMA_VERSION
            } else {
                SCHEMA_VERSION
            },
            out,
        }
    }

    /// Emit a stream-json event line for an in-flight chunk.
    fn on_chunk(&mut self, chunk: &StreamChunk) -> Result<(), std::io::Error> {
        if self.fmt != OutputFormat::StreamJson {
            return Ok(());
        }
        let event = match chunk {
            StreamChunk::Text { content, .. } => StreamEvent {
                schema_version: self.schema_version,
                event_type: "text",
                content: Some(content.clone()),
                id: None,
                name: None,
                input: None,
                result: None,
                usage: None,
                model: None,
                stop_reason: None,
                limit_reason: None,
                deny_count: None,
                dry_run: None,
                plan: None,
                tools_would_use: None,
                error: None,
            },
            StreamChunk::ToolUse { id, name, input } => StreamEvent {
                schema_version: self.schema_version,
                event_type: "tool_use",
                content: None,
                id: Some(id.clone()),
                name: Some(name.clone()),
                input: Some(input.clone()),
                result: None,
                usage: None,
                model: None,
                stop_reason: None,
                limit_reason: None,
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
                ..
            } => StreamEvent {
                schema_version: self.schema_version,
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
                model: None,
                stop_reason: None,
                limit_reason: None,
                deny_count: None,
                dry_run: None,
                plan: None,
                tools_would_use: None,
                error: None,
            },
            // Operator-private replay state is persisted, never printed.
            StreamChunk::ProviderReplay { .. } => return Ok(()),
            _ => return Ok(()),
        };
        let line = serde_json::to_string(&event)?;
        writeln!(self.out, "{}", line)
    }

    /// Emit a stream-json `call_usage` line for one finished provider call.
    fn on_call_usage(&mut self, model: &str, usage: &UsageInfo) -> Result<(), std::io::Error> {
        if self.fmt != OutputFormat::StreamJson {
            return Ok(());
        }
        let event = StreamEvent {
            schema_version: self.schema_version,
            event_type: "call_usage",
            content: None,
            id: None,
            name: None,
            input: None,
            result: None,
            usage: Some(usage.into()),
            model: Some(model.to_string()),
            stop_reason: None,
            limit_reason: None,
            deny_count: None,
            dry_run: None,
            plan: None,
            tools_would_use: None,
            error: None,
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
                    schema_version: self.schema_version,
                    response: Some(response_text.to_string()),
                    model: outcome.model.clone(),
                    stop_reason: stop_reason_str(&outcome.turn_stop_reason).to_string(),
                    limit_reason: outcome.limit_reason,
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
                        schema_version: self.schema_version,
                        event_type: "usage",
                        content: None,
                        id: None,
                        name: None,
                        input: None,
                        result: None,
                        usage: Some(usage.into()),
                        model: None,
                        stop_reason: None,
                        limit_reason: None,
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
                        schema_version: self.schema_version,
                        event_type: "plan_proposed",
                        content: None,
                        id: None,
                        name: None,
                        input: None,
                        result: None,
                        usage: None,
                        model: None,
                        stop_reason: None,
                        limit_reason: None,
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
                    schema_version: self.schema_version,
                    event_type: "turn_complete",
                    content: None,
                    id: None,
                    name: None,
                    input: None,
                    result: None,
                    usage: None,
                    model: None,
                    stop_reason: Some(stop_reason_str(&outcome.turn_stop_reason).to_string()),
                    limit_reason: outcome.limit_reason,
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
                    schema_version: self.schema_version,
                    response: None,
                    model: String::new(),
                    stop_reason: String::new(),
                    limit_reason: None,
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
                    schema_version: self.schema_version,
                    event_type: "error",
                    content: None,
                    id: None,
                    name: None,
                    input: None,
                    result: None,
                    usage: None,
                    model: None,
                    stop_reason: None,
                    limit_reason: None,
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

/// One finished provider call, as the ledger tap reports it to `ask`.
struct CallBoundary {
    /// Model that served the call.
    model: String,
    /// Lets the turn go on to its next provider call.
    resume: tokio::sync::oneshot::Sender<()>,
}

/// Wraps the ledger to report a call boundary to the renderer. With limits
/// enabled the append settles first; the producer still waits for the
/// renderer to take the boundary before it can send the next call's events.
struct CallUsageTap {
    inner: Arc<dyn UsageLedgerPort>,
    tx: tokio::sync::mpsc::UnboundedSender<CallBoundary>,
    limited: bool,
}

#[async_trait]
impl UsageLedgerPort for CallUsageTap {
    async fn append(&self, entry: UsageLedgerEntry) -> Result<(), StorageError> {
        let model = entry.model.clone();
        if self.limited {
            let result = self.inner.append(entry).await;
            self.handoff(model).await;
            result
        } else {
            self.handoff(model).await;
            self.inner.append(entry).await
        }
    }

    async fn read_session(&self, session_id: &str) -> Result<Vec<UsageLedgerEntry>, StorageError> {
        self.inner.read_session(session_id).await
    }

    async fn read_since(
        &self,
        since_unix_ms: i64,
        mode: LedgerReadMode,
    ) -> Result<Vec<UsageLedgerEntry>, StorageError> {
        self.inner.read_since(since_unix_ms, mode).await
    }
}

impl CallUsageTap {
    async fn handoff(&self, model: String) {
        let (resume, resumed) = tokio::sync::oneshot::channel();
        if self.tx.send(CallBoundary { model, resume }).is_ok() {
            // Dropping the acknowledgement or its receiver's queue releases the producer.
            let _ = resumed.await;
        }
    }
}

/// How long `ask` waits, before it returns, for its turn task to end, so the
/// last call's ledger row is written before the process exits.
const TURN_DRAIN_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Cancel further work and release the tap before joining. Limited runs must
/// not abort a producer while it is settling an admitted call.
async fn finish_turn(
    exit: ExitCode,
    cancel: CancellationToken,
    call_usage_rx: tokio::sync::mpsc::UnboundedReceiver<CallBoundary>,
    turn: Option<tokio::task::JoinHandle<()>>,
    limited: bool,
) -> ExitCode {
    cancel.cancel();
    // The tap stops waiting for a boundary nobody takes.
    drop(call_usage_rx);
    if let Some(mut turn) = turn {
        if limited {
            let _ = turn.await;
        } else if tokio::time::timeout(TURN_DRAIN_GRACE, &mut turn)
            .await
            .is_err()
        {
            turn.abort();
        }
    }
    exit
}

/// First action of every failing exit after the turn is spawned: cancel the
/// turn, then write the error text. No write may sit between the failure and
/// the cancel, or a released turn could start another provider call meanwhile.
fn begin_exit(cancel: &CancellationToken, err: &mut dyn Write, message: std::fmt::Arguments<'_>) {
    cancel.cancel();
    let _ = writeln!(err, "{message}");
}

fn add_tokens(total: Option<u32>, call: Option<u32>) -> Option<u32> {
    match (total, call) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0).saturating_add(b.unwrap_or(0))),
    }
}

/// Fold one provider call into the turn total and emit its `call_usage` line.
fn record_call_usage(
    renderer: &mut AskRenderer<'_>,
    total: &mut Option<UsageInfo>,
    model: &str,
    call: UsageInfo,
) -> Result<(), std::io::Error> {
    let written = renderer.on_call_usage(model, &call);
    *total = Some(match total.take() {
        None => UsageInfo {
            counters_observed: false,
            ..call
        },
        Some(sum) => UsageInfo {
            input_tokens: sum.input_tokens.saturating_add(call.input_tokens),
            output_tokens: sum.output_tokens.saturating_add(call.output_tokens),
            cache_creation_input_tokens: add_tokens(
                sum.cache_creation_input_tokens,
                call.cache_creation_input_tokens,
            ),
            cache_read_input_tokens: add_tokens(
                sum.cache_read_input_tokens,
                call.cache_read_input_tokens,
            ),
            reasoning_tokens: add_tokens(sum.reasoning_tokens, call.reasoning_tokens),
            counters_observed: false,
        },
    });
    written
}

/// Helper: write an error to stderr (all formats) and, for json/stream-json,
/// write a structured error document to stdout. Returns FAILURE.
fn emit_cli_error(
    fmt: OutputFormat,
    limited: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
    message: &str,
    dry_run: bool,
) -> ExitCode {
    let _ = writeln!(err, "Error: {}", message);
    if fmt != OutputFormat::Text {
        let mut renderer = AskRenderer::new(fmt, limited, out);
        let _ = renderer.emit_error(message, dry_run);
    }
    ExitCode::FAILURE
}
/// Used by `run_ask` for pre-turn failures (e.g. unreadable --file) where
/// returning a `Result` would route through the generic text-only error printer.
fn exit_with_cli_error(fmt: OutputFormat, limited: bool, message: &str, dry_run: bool) -> ! {
    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();
    let _ = emit_cli_error(fmt, limited, &mut stdout, &mut stderr, message, dry_run);
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
    pub limits: AskLimits,
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
        persona,
    } = core;

    let limited = ask_limits_configured(opts.limits, app_config);

    // `run_turn` appends one ledger entry per provider call. Tap that boundary:
    // it yields the turn total and the per-call `call_usage` lines.
    let (call_usage_tx, mut call_usage_rx) = tokio::sync::mpsc::unbounded_channel();
    let ledger: Arc<dyn UsageLedgerPort> = Arc::new(CallUsageTap {
        inner: ledger,
        tx: call_usage_tx,
        limited,
    });

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
            limited,
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
                    limited,
                    out,
                    err,
                    &format!("session '{}' not found", sid),
                    opts.dry_run,
                );
            }
            Err(e) => {
                return emit_cli_error(
                    output_format,
                    limited,
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
            peer_row_controls: Default::default(),
            peer_row_revision: 0,
            persist: Default::default(),
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
        authorship: crate::domain::models::Authorship::HumanWritten,
        send_authorization: crate::domain::models::SendAuthorization::Unknown,
        retracted_at_ms: None,
        history_source: Default::default(),
    });

    let messages = message_builder::build_api_messages(&conversation);

    let persona_prompt = persona.system_prompt(workspace);
    let empty_set = SkillActivationSet::new();
    // `None` catalog: `ask` has no skill source (DF-FR41-DAEMON-ASK-NO-SKILL-SOURCE).
    let mut system_prompt =
        crate::domain::services::skill_context::assemble_system_prompt_with_agent(
            &persona_prompt,
            None,
            None,
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
        session_id: Some(conversation.id.clone()),
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
                            .resolve(
                                &id,
                                ApprovalOutcome::Reject { feedback: None },
                                crate::domain::models::PermissionResolution::Policy,
                            )
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
    // Dry-run takes no session write path: the producer's pre-save and first-tool checkpoint
    // go to a store that discards them, and the loaded conversation carries no capture state.
    let turn_storage: Arc<dyn StoragePort> = if opts.dry_run {
        conversation.persist = Default::default();
        Arc::new(crate::adapters::noop::NoOpStorage)
    } else {
        storage.clone()
    };
    let conversation_snapshot = conversation.capture();
    let drain_cancel = turn_cancel.clone();

    // Input, prompt, configuration, and model resolution are complete. Start
    // the deadline before the producer saves the session or admits a call.
    let (execution, stop_rx) = if limited {
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        let execution = AskExecution::new(
            opts.limits.max_turns,
            opts.limits.max_duration,
            opts.limits.max_cost_usd,
            app_config,
            stop_tx,
        );
        (execution, Some(stop_rx))
    } else {
        (None, None)
    };

    let turn_task = tokio::spawn(turn::run_turn(
        provider,
        messages,
        options,
        event_tx,
        security,
        tools,
        tool_scheduler,
        conv_id,
        turn_storage,
        conversation_snapshot,
        None,
        // Story 19.28 AC5 / A16 item 1 — proven-correct `None`, stated rather
        // than left silent. Measured at implementation and re-measured at the
        // 2026-09-13 code review: `grep -n
        // "ActiveAgent\|agent_snapshot\|active_agent" adapters/cli/ask.rs`
        // returns ZERO hits — `rustain ask` has no agent-selection flag and
        // constructs no `ActiveAgent`, so there is no restriction to thread.
        // ⛔ Not an oversight — the absence is structural. Load-bearing-ness is
        // proved by mutant AC5(b): a fabricated `Some([Read])` at this exact
        // site turns the `ask` tests RED (receipted as MUTANT 20).
        None,
        turn_cancel,
        ledger,
        resolved,
        None,
        0,
        None,
        session_id,
        TurnOrigin::Interactive,
        // Story 18.4a scope boundary: `rustain ask` composes its own turn
        // without the TUI's `inject_assembled_context` seam, so no
        // `ContextBundle` exists here and no peer-origin entry can be present.
        // ⛔ Not a suppressed taint — the absence is structural.
        false,
        None,
        execution,
    ));
    let mut turn_task = Some(turn_task);

    let mut assistant_text = String::new();
    let mut last_block_start = 0usize;
    let mut turn_complete = false;
    let mut turn_error: Option<ProviderError> = None;
    let mut turn_stop_reason = if limited {
        StopReason::Cancelled
    } else {
        StopReason::EndTurn
    };
    let mut tool_calls: Vec<ToolCallInfo> = Vec::new();
    // Usage chunk of the provider call that is still open. The ledger tap
    // closes the call; a call that sent no usage is not reported.
    let mut open_call_usage: Option<UsageInfo> = None;
    // Sum over every provider call of the turn that reported usage. Without
    // one, `usage` stays `null`.
    let mut usage_total: Option<UsageInfo> = None;
    // Story 20.1d1b: replay items persist on the assistant row, never on stdout.
    let mut provider_replay = Vec::new();
    // Story 20.1d4a T2: one span per provider response, closed at each
    // `TurnComplete`, so a resume rebuilds the in-turn history.
    let mut span = crate::domain::models::ResponseSpanBuilder::default();
    let mut spans = Vec::new();
    // Story 13.1c: accumulate the last proposed plan (last-wins, AC6).
    let mut proposed_plan: Option<Plan> = None;
    let mut renderer = AskRenderer::new(output_format, limited, out);
    let mut pending_boundary: Option<CallBoundary> = None;
    let mut producer_failed = false;
    // If the caller drops this future, cancel before dropping the tap or
    // releasing any queued acknowledgement; the producer owns settlement.
    let _cancel_on_drop = limited.then(|| drain_cancel.clone().drop_guard());
    loop {
        let event = if let Some(boundary) = pending_boundary.take() {
            match event_rx.try_recv() {
                Ok(event) => {
                    pending_boundary = Some(boundary);
                    Some(event)
                }
                Err(_) => {
                    // The producer sent all of this call's events before
                    // the boundary and cannot send the next call's yet.
                    // Recheck the queue now: polling it first in select!
                    // alone does not order two concurrently sent channels.
                    let written = match open_call_usage.take() {
                        Some(usage) => record_call_usage(
                            &mut renderer,
                            &mut usage_total,
                            &boundary.model,
                            usage,
                        ),
                        None => Ok(()),
                    };
                    if written.is_err() {
                        drain_cancel.cancel();
                    }
                    let _ = boundary.resume.send(());
                    if let Err(e) = written {
                        begin_exit(
                            &drain_cancel,
                            err,
                            format_args!("Error: failed to write stream output: {}", e),
                        );
                        return finish_turn(
                            ExitCode::FAILURE,
                            drain_cancel,
                            call_usage_rx,
                            turn_task,
                            limited,
                        )
                        .await;
                    }
                    continue;
                }
            }
        } else {
            tokio::select! {
                biased;
                Some(result) = async {
                    match turn_task.as_mut() {
                        Some(task) => Some(task.await),
                        None => None,
                    }
                }, if limited && turn_task.is_some() => {
                    producer_failed = result.is_err();
                    turn_task = None;
                    // External tool event senders can outlive the turn. The
                    // producer is joined, so drain only the already queued events.
                    event_rx.close();
                    continue;
                }
                event = event_rx.recv() => event,
                Some(msg) = deny_msg_rx.recv() => {
                    let _ = writeln!(err, "{}", msg);
                    continue;
                }
                Some(boundary) = call_usage_rx.recv() => {
                    pending_boundary = Some(boundary);
                    continue;
                }
            }
        };
        let Some(event) = event else { break };
        match &event {
            AppEvent::ProviderChunk { chunk, .. } => match chunk {
                StreamChunk::Text { content, .. } => {
                    if !turn_complete {
                        assistant_text.push_str(content);
                        span.text(content);
                        if let Err(e) = renderer.on_chunk(chunk) {
                            begin_exit(
                                &drain_cancel,
                                err,
                                format_args!("Error: failed to write stream output: {}", e),
                            );
                            drop(pending_boundary);
                            return finish_turn(
                                ExitCode::FAILURE,
                                drain_cancel,
                                call_usage_rx,
                                turn_task,
                                limited,
                            )
                            .await;
                        }
                    }
                }
                StreamChunk::Thinking { content, .. } => {
                    if !turn_complete {
                        span.reasoning(content);
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
                            provider_id: Some(id.clone()),
                        });
                        span.call();
                    }
                    if !turn_complete {
                        if let Err(e) = renderer.on_chunk(chunk) {
                            begin_exit(
                                &drain_cancel,
                                err,
                                format_args!("Error: failed to write stream output: {}", e),
                            );
                            drop(pending_boundary);
                            return finish_turn(
                                ExitCode::FAILURE,
                                drain_cancel,
                                call_usage_rx,
                                turn_task,
                                limited,
                            )
                            .await;
                        }
                    }
                }
                StreamChunk::ToolResult {
                    id,
                    content,
                    is_error,
                    images,
                    ..
                } => {
                    if !turn_complete {
                        last_block_start = assistant_text.len();
                        // The latest unanswered call of that provider id: a
                        // provider may recycle ids across responses.
                        if let Some(tc) = tool_calls
                            .iter_mut()
                            .rev()
                            .find(|tc| tc.wire_id() == id.as_str() && tc.result.is_none())
                        {
                            tc.result = Some(ToolResultInfo {
                                content: content.clone(),
                                is_error: *is_error,
                                images: images.clone(),
                                diff: crate::domain::models::WriteDiffState::NotAWrite,
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
                            begin_exit(
                                &drain_cancel,
                                err,
                                format_args!("Error: failed to write stream output: {}", e),
                            );
                            drop(pending_boundary);
                            return finish_turn(
                                ExitCode::FAILURE,
                                drain_cancel,
                                call_usage_rx,
                                turn_task,
                                limited,
                            )
                            .await;
                        }
                    }
                }
                StreamChunk::TurnComplete { stop_reason } => {
                    if !turn_complete {
                        spans.push(ContentBlockType::ResponseSpan(span.finish()));
                    }
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
                    // The last usage chunk of a call wins, as in the ledger.
                    open_call_usage = Some(usage.clone());
                }
                StreamChunk::ProviderReplay { item } => {
                    if !turn_complete {
                        provider_replay.push(ContentBlockType::ProviderReplay(item.clone()));
                        span.replay();
                    }
                }
                StreamChunk::Error { content } => {
                    let parsed = parse_provider_error_display(content);
                    if limited {
                        // Stop a stream that may never close, but let the
                        // producer settle and decide bounded-stop precedence.
                        drain_cancel.cancel();
                        turn_error.get_or_insert(parsed);
                        continue;
                    }
                    if !parsed.is_offline() {
                        begin_exit(&drain_cancel, err, format_args!("Error: {}", content));
                    } else {
                        drain_cancel.cancel();
                    }
                    turn_error = Some(parsed);
                    break;
                }
                _ => {}
            },
            AppEvent::SystemNotice { level, message, .. } => {
                if *level == crate::domain::models::NoticeLevel::Error {
                    let parsed = parse_provider_error_display(message);
                    if limited {
                        drain_cancel.cancel();
                        turn_error.get_or_insert(parsed);
                        continue;
                    }
                    // Defer stderr output for offline errors — handled post-loop (AC3).
                    if !parsed.is_offline() {
                        begin_exit(&drain_cancel, err, format_args!("Error: {}", message));
                    } else {
                        drain_cancel.cancel();
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

    // The channel may close just before the producer becomes join-ready.
    // Limited output is not terminal until the producer has settled and joined.
    if limited && let Some(task) = turn_task.take() {
        producer_failed = task.await.is_err();
    }
    let ask_stop = stop_rx.and_then(|mut rx| rx.try_recv().ok());
    let limit_reason = ask_stop.as_ref().map(|stop| stop.reason.as_str());

    // Inactive turns retain the legacy error-time handoff, which may lack an
    // append if the stream never closes. A limited interruption without a
    // receipt may still have display usage (not proven accounting evidence).
    let open_call_model = pending_boundary
        .or_else(|| call_usage_rx.try_recv().ok())
        .map(|boundary| {
            // Cancel first: the released turn must not start another call.
            drain_cancel.cancel();
            let _ = boundary.resume.send(());
            boundary.model
        });
    if let Some(usage) = open_call_usage.take()
        && let Err(e) = record_call_usage(
            &mut renderer,
            &mut usage_total,
            open_call_model.as_deref().unwrap_or(&model_used),
            usage,
        )
    {
        begin_exit(
            &drain_cancel,
            err,
            format_args!("Error: failed to write stream output: {}", e),
        );
        return finish_turn(
            ExitCode::FAILURE,
            drain_cancel,
            call_usage_rx,
            turn_task,
            limited,
        )
        .await;
    }

    if let Some(handle) = deny_consumer {
        handle.abort();
    }
    while let Ok(msg) = deny_msg_rx.try_recv() {
        let _ = writeln!(err, "{}", msg);
    }

    if let Some(stop) = &ask_stop {
        let reason = stop.reason.as_str();
        if let Some(detail) = &stop.detail {
            let _ = writeln!(err, "Ask stopped: {reason}: {detail}");
        } else {
            let _ = writeln!(err, "Ask stopped: {reason}");
        }
    } else if let Some(error) = &turn_error {
        // Story 13.2 (AC3): surface a specific offline message when a transport-level
        // failure occurs — matched on the domain variant, not a string prefix.
        if error.is_offline() {
            let msg = "✗ No provider available (offline). Use --dry-run for plan-only, or configure a local LLM.";
            drain_cancel.cancel();
            let exit = emit_cli_error(output_format, limited, out, err, msg, opts.dry_run);
            return finish_turn(exit, drain_cancel, call_usage_rx, turn_task, limited).await;
        }
        begin_exit(
            &drain_cancel,
            err,
            format_args!("Error: turn failed: {}", error),
        );
        let _ = renderer.emit_error(&format!("turn failed: {}", error), opts.dry_run);
        return finish_turn(
            ExitCode::FAILURE,
            drain_cancel,
            call_usage_rx,
            turn_task,
            limited,
        )
        .await;
    }

    if ask_stop.is_none() && (!turn_complete || producer_failed) {
        let message = if producer_failed {
            "turn producer failed"
        } else {
            "turn did not complete"
        };
        begin_exit(&drain_cancel, err, format_args!("Error: {message}"));
        let _ = renderer.emit_error(message, opts.dry_run);
        return finish_turn(
            ExitCode::FAILURE,
            drain_cancel,
            call_usage_rx,
            turn_task,
            limited,
        )
        .await;
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
        limit_reason,
        turn_usage: usage_total,
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
        begin_exit(
            &drain_cancel,
            err,
            format_args!("Error: failed to write output: {}", e),
        );
        return finish_turn(
            ExitCode::FAILURE,
            drain_cancel,
            call_usage_rx,
            turn_task,
            limited,
        )
        .await;
    }
    if ask_stop.is_some() {
        return ExitCode::FAILURE;
    }

    // Story 13.1c (AC7): dry-run saves no reply and prints no resume hint. Its
    // provider calls are still recorded in the usage ledger, as for every run.
    let mut session_saved = !opts.dry_run;
    if !opts.dry_run {
        provider_replay.extend(spans);
        conversation.messages.push(ChatMessage {
            id: generate_message_id(),
            role: MessageRole::Assistant,
            content: outcome.assistant_text,
            content_blocks: provider_replay,
            tool_calls: outcome.tool_calls,
            created_at: now_unix(),
            token_count: None,
            stop_reason: Some(outcome.turn_stop_reason.clone()),
            images: vec![],
            synthetic: false,
            origin: crate::domain::models::ChannelKind::Terminal,
            authorship: crate::domain::models::Authorship::HumanWritten,
            send_authorization: crate::domain::models::SendAuthorization::Unknown,
            retracted_at_ms: None,
            history_source: Default::default(),
        });
        conversation.updated_at = now_unix();
        conversation.last_response_at = Some(now_unix());

        if let Err(e) = storage.save_conversation(&conversation).await {
            session_saved = false;
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
    if session_saved && !opts.final_message_only {
        let _ = writeln!(
            err,
            "Session saved. Resume with: rustain --session {}",
            conversation.id
        );
    }

    finish_turn(
        ExitCode::SUCCESS,
        drain_cancel,
        call_usage_rx,
        turn_task,
        limited,
    )
    .await
}

pub async fn run_ask(
    query: String,
    files: Vec<PathBuf>,
    yolo: bool,
    final_message_only: bool,
    output_format: String,
    dry_run: bool,
    limits: AskLimits,
    app_config: AppConfig,
    session_id: Option<String>,
    force_new: bool,
    model_override: Option<String>,
    persona: Arc<dyn crate::domain::ports::PersonaPort>,
) -> Result<()> {
    let workspace = std::env::current_dir()?;

    let resolved_output_format = OutputFormat::parse(&output_format).unwrap_or(OutputFormat::Text);
    let limited = ask_limits_configured(limits, &app_config);

    let mut resolved_files = Vec::new();
    for path in &files {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("cannot read file '{}': {}", path.display(), e);
                if resolved_output_format != OutputFormat::Text {
                    exit_with_cli_error(resolved_output_format, limited, &msg, dry_run);
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
                    exit_with_cli_error(resolved_output_format, limited, &msg, dry_run);
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

    let core =
        crate::infrastructure::composition::build_cli_core(&app_config, &workspace, yolo, persona)
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
        limits,
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

#[cfg(test)]
mod exit_order_tests {
    use super::*;

    /// Records whether the token was already cancelled when the write came.
    struct ObservingWriter {
        cancel: CancellationToken,
        cancelled_at_write: Option<bool>,
    }

    impl Write for ObservingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.cancelled_at_write = Some(self.cancel.is_cancelled());
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// eval-3 E3-R01: the cancel comes first, then the text.
    #[test]
    fn begin_exit_cancels_the_turn_before_it_writes() {
        let cancel = CancellationToken::new();
        let mut err = ObservingWriter {
            cancel: cancel.clone(),
            cancelled_at_write: None,
        };
        begin_exit(&cancel, &mut err, format_args!("Error: {}", "x"));
        assert_eq!(err.cancelled_at_write, Some(true));
    }

    /// eval-3 E3-R01: after the turn is spawned, every error text goes through
    /// `begin_exit`; a bare `writeln!(err, "Error: ...")` could sit between a
    /// failure and the cancel (a released turn runs concurrently with `ask`).
    #[test]
    fn run_ask_core_writes_no_error_text_before_cancelling() {
        let source = include_str!("ask.rs");
        let body = source
            .split("let turn_task = tokio::spawn(")
            .nth(1)
            .expect("the turn spawn")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        let run_ask_core_tail = body.split("pub async fn run_ask(").next().unwrap();
        assert!(
            !run_ask_core_tail.contains("writeln!(err, \"Error"),
            "use begin_exit for every error text after the spawn"
        );
        assert!(run_ask_core_tail.matches("begin_exit(").count() >= 10);
    }
}
