pub mod agent;
pub mod approval;
pub mod autocomplete;
pub mod checkpoint;
mod completion;
mod config;
mod content;
pub mod conversation;
mod focus;
mod message;
mod notice;
pub mod palette;
mod permission;
pub mod plan;
pub mod project_context;
pub mod sandbox;
mod session;
pub mod session_meta;
mod skill;
mod stream;
pub mod tab;
pub mod tool_call;
mod tools;
pub mod transaction;
pub mod turn;
mod usage;
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
pub use config::{AutoPanelsConfig, LayoutConfig};
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
    EffortEstimate, Plan, PlanDecision, PlanStatus, PlanTask, PlanTaskStatus, TaskResult,
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
    SkillValidationError, validate_skill_frontmatter,
};
#[allow(unused_imports)]
pub use stream::{StopReason, StreamChunk, StreamingPhase, StreamingState};
// TODO(S16.4): remove these upward re-exports once consumers migrate to domain::services::reducer
#[allow(unused_imports)]
pub use crate::domain::services::reducer::{
    LivenessSnapshot, ReducerState, reduce, update_streaming_mirror,
};
#[allow(unused_imports)]
pub use tab::{ConversationId, TabId, TabManager, TabState};
#[allow(unused_imports)]
pub use tool_call::{
    ApprovalSource, RequestId, ToolCall, ToolCallRequest, ToolCallResult, ToolCallTransition,
    status_chip,
};
pub use tools::{ToolCallInfo, ToolDefinition, ToolResult, ToolResultInfo};
#[allow(unused_imports)]
pub use turn::{
    InvocationStatus, PartId, ToolOutput, Turn, TurnId, TurnPart, generate_turn_id,
    migrate_chat_message_to_turn, tool_call_id_for,
};
#[allow(unused_imports)]
pub use usage::{ModelInfo, UsageInfo};
#[allow(unused_imports)]
pub use view_state::{
    AnchorMode, AnchorRef, LayoutMetrics, ScrollDelta, SummaryTier, ViewEvent, ViewState,
};
#[allow(unused_imports)]
pub use visual::{BlockBorder, DensityMode, OverlayType, PanelType, symbols};
