pub mod adapter_health;
pub mod agent;
pub mod agent_id;
pub mod approval;
pub mod autocomplete;
pub mod budget;
pub mod capability;
pub mod capability_id;
pub mod capability_kind;
pub mod capability_registry;
pub mod catalog_delta;
pub mod checkpoint;
mod completion;
mod config;
mod content;
pub mod conversation;
pub mod doc_key;
pub mod filtered_catalog;
pub mod filtered_skill_catalog;
mod focus;
pub mod launch_spec;
pub mod mcp_server_spec;
pub mod mcp_server_state;
mod message;
mod notice;
pub mod palette;
mod permission;
pub mod plan;
pub mod pricing;
pub mod profile;
pub mod project_context;
pub mod provider;
pub mod provider_capabilities;
pub mod router;
pub mod sandbox;
pub mod search_hit;
pub mod session;
pub mod session_meta;
mod skill;
pub mod skill_catalog_delta;
pub mod skill_metadata;
mod stream;
pub mod subagent_error;
pub mod subagent_status;
pub mod subagent_view;
pub mod tab;
pub mod task_handle;
pub mod tool_call;
pub mod tool_descriptor;
pub mod tool_policy;
mod tools;
pub mod trace_context;
pub mod transaction;
pub mod turn;
pub mod usage;
pub mod view_state;
#[allow(dead_code)]
pub mod visual;

// Re-exports for all domain model types.
// Many are unused until later stories wire port implementations — suppress warnings.
#[allow(unused_imports)]
pub use agent::{
    ActiveAgent, AgentDef, AgentValidationError, MAX_AGENT_FILE_SIZE, MAX_AGENT_SCAN_FILES,
    validate_agent_frontmatter,
};
#[allow(unused_imports)]
pub use approval::{ApprovalOutcome, ApprovalScope};
#[allow(unused_imports)]
pub use checkpoint::{CheckpointId, CheckpointMeta, RevertStatus, RevertedFile};
#[allow(unused_imports)]
pub use completion::CompletionOptions;
pub use config::AppConfig;
#[allow(unused_imports)]
pub use config::RuntimeConfig;
#[allow(unused_imports)]
pub use config::{
    AutoPanelsConfig, LayoutConfig, MouseConfig, ProviderConfig, SearchConfig, SubTaskFailurePolicy,
    ToolProgressConfig,
};
#[allow(unused_imports)]
pub use content::ContentBlockType;
#[allow(unused_imports)]
pub use conversation::{
    ChatMessage, Conversation, ConversationSummary, ForkSource, ImageReference,
    generate_conversation_id, generate_message_id,
};
pub use focus::FocusState;
#[allow(unused_imports)]
pub use message::{
    ImageAttachment, Message, MessageRole, ToolResultMessage, ToolUseMessage, UserMessage,
};
pub use notice::{
    FeedbackAction, FeedbackBlock, FeedbackLevel, NoticeLevel, RetryState, StatusState, next_delay,
};
#[allow(unused_imports)]
pub use palette::{PaletteAction, PaletteEntry, PaletteScope};
#[allow(unused_imports)]
pub use permission::{
    FileOperation, PathAccessType, PermissionMode, PlanApprovalOutcome, ToolRisk, risk_for_builtin,
};
#[allow(unused_imports)]
pub use plan::{
    DelegationInfo, EffortEstimate, Plan, PlanDecision, PlanStatus, PlanSubTask, PlanTask,
    PlanTaskStatus, TaskResult,
};
pub use sandbox::SandboxPolicy;
#[allow(unused_imports)]
pub use session::{SessionId, SessionManager, SessionState};
#[allow(unused_imports)]
pub use session_meta::{
    ImportSource, SessionMeta, extract_title_from_first_message, now_unix, shorten_text,
};
#[allow(unused_imports)]
pub use skill::{
    ActiveSkill, MAX_SKILL_ACTIVATION_DEPTH, MAX_SKILL_FILE_SIZE, SkillActivationError,
    SkillActivationOutcome, SkillActivationSet, SkillDef, SkillSource, SkillTrustResponse,
    SkillValidationError, SkillValidationWarning, validate_skill_frontmatter,
};
pub use skill_catalog_delta::SkillCatalogDelta;
pub use skill_metadata::SkillMetadata;
#[allow(unused_imports)]
pub use stream::{StopReason, StreamChunk, StreamingPhase, StreamingState};
pub use subagent_error::{SpawnLimitKind, SubagentError};
pub use subagent_status::SubagentStatus;
pub use subagent_status::SubagentStatus as SubagentRunStatus;
pub use subagent_view::{AgentRowView, OwnershipKind};
pub use task_handle::{Op, TaskHandle};
pub use tool_policy::ToolPolicy;
pub use trace_context::TraceContext;
// TODO(S16.4): remove these upward re-exports once consumers migrate to domain::services::reducer
#[allow(unused_imports)]
pub use crate::domain::services::reducer::{
    LivenessSnapshot, ReducerState, reduce, update_streaming_mirror,
};
pub use adapter_health::{HealthLevel, HealthSummary, McpHealthRow};
pub use agent_id::AgentId;
#[allow(unused_imports)]
pub use budget::BudgetConfig;
pub use capability::{Capability, CapabilityError};
pub use capability_id::CapabilityId;
pub use capability_kind::CapabilityKind;
pub use capability_registry::{
    CapabilityRegistry, ProviderId, RegisterHandle, RegisteredCapability, RegistryError,
};
pub use catalog_delta::CatalogDelta;
pub use doc_key::DocKey;
pub use filtered_catalog::FilteredCatalog;
pub use filtered_skill_catalog::FilteredSkillCatalog;
pub use launch_spec::AgentLaunchSpec;
pub use mcp_server_spec::{McpServerSource, McpServerSpec, McpTransport, expand_env_vars};
pub use mcp_server_state::McpConnectionState;
#[allow(unused_imports)]
pub use pricing::PricingConfig;
#[allow(unused_imports)]
pub use profile::{
    ActiveProfileSnapshot, AdapterRef, PortDimension, ProfileDefinition, ProfileDescriptor,
    ProfileId, ProfileIdentityColor, ProfileSelection, ProfileSource, ResolvedProfile,
    TransitionState,
};
#[allow(unused_imports)]
pub use provider::{ModelCapability, ModelDescriptor, ProviderDescriptor};
pub use provider_capabilities::{NativeRetrievalKind, ProviderCapabilities, TransportKind};
#[allow(unused_imports)]
pub use router::{EscalationReason, ModelTier, RouterConfig, StepKind};
pub use search_hit::SearchHit;
#[allow(unused_imports)]
pub use tab::{ConversationId, TabId, TabManager, TabState};
#[allow(unused_imports)]
pub use tool_call::{
    ApprovalSource, RequestId, ToolCall, ToolCallRequest, ToolCallResult, ToolCallTransition,
    status_chip,
};
pub use tool_descriptor::{ToolAnnotations, ToolDescriptor, ToolId};
pub use tools::{ToolCallInfo, ToolDefinition, ToolResult, ToolResultInfo};
#[allow(unused_imports)]
pub use turn::{
    InvocationStatus, PartId, ToolOutput, Turn, TurnId, TurnPart, generate_turn_id,
    migrate_chat_message_to_turn, tool_call_id_for,
};
#[allow(unused_imports)]
pub use usage::{ModelInfo, TokenUsage, UsageInfo, UsageLedgerEntry};
#[allow(unused_imports)]
pub use view_state::{
    AnchorMode, AnchorRef, LayoutMetrics, ScrollDelta, SummaryTier, ViewEvent, ViewState,
};
#[allow(unused_imports)]
pub use visual::{BlockBorder, DensityMode, OverlayType, PanelType, symbols};
