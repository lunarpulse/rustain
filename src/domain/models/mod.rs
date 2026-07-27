pub mod a2a_peer_spec;
pub mod adapter_health;
pub mod agent;
pub mod agent_envelope;
pub mod agent_id;
pub mod agent_message;
pub mod agent_node;
pub mod approval;
pub mod artifact;
pub mod assembled_context;
pub mod autocomplete;
pub mod budget;
pub mod capability;
pub mod capability_id;
pub mod capability_kind;
pub mod capability_registry;
pub mod capability_token;
pub mod catalog_delta;
pub mod channel_kind;
pub mod channel_turn_request;
pub mod checkpoint;
mod completion;
mod config;
mod content;
pub mod context_bundle;
pub mod conversation;
pub mod credential;
pub mod cron_config;
pub mod daemon_crash;
pub mod doc_key;
pub mod execution_sandbox;
pub mod filtered_catalog;
pub mod filtered_skill_catalog;
mod focus;
pub mod invocation_fingerprint;
pub mod isolation;
pub mod launch_spec;
pub mod mcp_server_spec;
pub mod mcp_server_state;
pub mod memory_entry;
pub mod memory_fact;
mod message;
pub mod node_journal;
pub mod node_state;
mod notice;
pub mod orchestration;
pub mod orchestration_room;
pub mod palette;
pub mod peer_identity;
mod permission;
pub mod plan;
pub mod pricing;
pub mod profile;
pub mod project_context;
pub mod provider;
pub mod provider_capabilities;
pub mod redacted_url;
pub mod redaction;
pub mod router;
pub mod sandbox;
pub mod search_hit;
pub mod secret;
pub mod session;
pub mod session_boundary;
pub mod session_meta;
pub mod session_queue;
mod skill;
pub mod skill_catalog_delta;
pub mod skill_metadata;
mod stream;
pub mod subagent_envelope;
pub mod subagent_error;
mod subagent_status;
pub mod subagent_view;
pub mod tab;
pub mod taint;
pub mod task_handle;
pub mod tool_call;
pub mod tool_descriptor;
pub mod tool_policy;
mod tools;
pub mod trace_context;
pub mod transaction;
pub mod turn;
pub mod turn_group;
pub mod usage;
pub mod view_state;
#[allow(dead_code)]
pub mod visual;
pub mod waiting_hazard;

// Re-exports for all domain model types.
// Many are unused until later stories wire port implementations — suppress warnings.
#[allow(unused_imports)]
pub use a2a_peer_spec::{
    A2aPeerSource, A2aPeerSpec, A2aPeerSpecError, PinnedKey, PinnedKeyAlgorithm, TrustTier,
};
pub use agent::{
    ActiveAgent, AgentDef, AgentValidationError, MAX_AGENT_FILE_SIZE, MAX_AGENT_SCAN_FILES,
    validate_agent_frontmatter,
};
#[allow(unused_imports)]
pub use agent_message::{
    AgentDelivery, AgentMessage, CorrelationId, DeliveryDisposition, DeliveryMode, DeliveryOutcome,
    Envelope, MessageHeader, MessageKind, RefuseReason, delivery_decision,
    relationship_disposition,
};
#[allow(unused_imports)]
pub use agent_node::{
    AbandonmentAction, AgentMetrics, AgentNode, CheckpointTrust, NodeCheckpoint, NodeOrigin,
    abandonment_action,
};
#[allow(unused_imports)]
pub use approval::{ApprovalOutcome, ApprovalScope};
#[allow(unused_imports)]
pub use artifact::{
    ArtifactId, ArtifactKind, ArtifactRef, ContentHash, ContentHashError, EvidenceArtifact,
    EvidenceArtifactDraft, ReviewStatus,
};
#[allow(unused_imports)]
pub use capability_token::{
    Budget, CapabilityFlag, CapabilitySet, CapabilityToken, CapabilityTokenId, DelegateConstraint,
    DelegateRequest,
};
pub use channel_kind::ChannelKind;
pub use channel_turn_request::ChannelTurnRequest;
pub use checkpoint::{CheckpointId, CheckpointMeta, RevertStatus, RevertedFile};
#[allow(unused_imports)]
pub use completion::CompletionOptions;
pub use config::AppConfig;
#[allow(unused_imports)]
pub use config::RuntimeConfig;
#[allow(unused_imports)]
pub use config::{
    AutoApprovePolicy, AutoPanelsConfig, DaemonConfig, LayoutConfig, MouseConfig, ProviderConfig,
    SearchConfig, SubTaskFailurePolicy, SubagentsConfig, ToolProgressConfig,
};
#[allow(unused_imports)]
pub use content::ContentBlockType;
pub use conversation::{
    ChatMessage, Conversation, ConversationSummary, ForkSource, ImageReference,
    generate_conversation_id, generate_message_id,
};
#[allow(unused_imports)]
pub use credential::{
    AuthMethod, AuthSource, AuthStatus, Credential, ProviderStatus, ResolvedAuth,
};
pub use cron_config::{CronConfig, CronJob};
#[allow(unused_imports)]
pub use daemon_crash::{DaemonCrashRecord, LAST_N_CRASH_CAP};
#[allow(unused_imports)]
pub use execution_sandbox::{
    CapabilityGrant, ComponentRef, HostImport, PreopenGrant, ResourceCaps, SandboxInvocation,
    SandboxOutcome, TrapKind,
};
pub use focus::FocusState;
#[allow(unused_imports)]
pub use invocation_fingerprint::{FingerprintError, InvocationFingerprint};
#[allow(unused_imports)]
pub use isolation::{IsolationError, IsolationHandle, ProvisioningTier, UnifiedDiff};
#[allow(unused_imports)]
pub use message::{
    ImageAttachment, Message, MessageRole, ToolResultMessage, ToolUseMessage, UserMessage,
};
#[allow(unused_imports)]
pub use node_journal::{
    JournalEntry, JournalRecord, JournaledTerminalCheckpoint, LedgerConservationRecord,
    NODE_JOURNAL_SCHEMA_VERSION,
};
#[allow(unused_imports)]
pub use node_state::{NodeState, NodeStateError};
pub use notice::{
    FeedbackAction, FeedbackBlock, FeedbackLevel, NoticeLevel, RetryState, StatusState, next_delay,
};
#[allow(unused_imports)]
pub use orchestration::{
    CoverageLine, DrillBody, DrillId, FORK_JOIN_SPAWN_CAP, ForkJoinOutcome, ForkJoinSpec,
    OrchestrationError, SpokeCitation, SpokeResult, SpokeSpec, SynthesisView, WaitPolicy,
    WaitReason,
};
#[allow(unused_imports)]
pub use orchestration_room::{
    ApprovalView, Direction, HostBinding, NodeView, OrchestrationRoom, OrchestrationRoomId,
    RejectReason, RemoteRejectionView, ReviewVerdict, RoomEvent, RoomIdError, TicketResolution,
    WaveId, WaveOutcome, WaveView,
};
#[allow(unused_imports)]
pub use palette::{PaletteAction, PaletteEntry, PaletteScope};
#[allow(unused_imports)]
pub use peer_identity::{Ed25519Sig, PeerId, PeerIdentity, PeerIdentityError};
#[allow(unused_imports)]
pub use permission::{
    FileContextProvenance, FileOperation, PathAccessType, PermissionMode, PlanApprovalOutcome,
    ToolRisk, risk_for_builtin,
};
#[allow(unused_imports)]
pub use plan::{
    DelegationInfo, EffortEstimate, Plan, PlanDecision, PlanStatus, PlanSubTask, PlanTask,
    PlanTaskStatus, TaskResult,
};
pub use redacted_url::RedactedUrl;
pub use sandbox::SandboxPolicy;
pub use secret::SecretString;
#[allow(unused_imports)]
pub use session::{SessionId, SessionManager, SessionState};
pub use session_boundary::SessionBoundary;
#[allow(unused_imports)]
pub use session_meta::{
    ImportSource, SessionMeta, extract_title_from_first_message, now_unix, shorten_text,
};
pub use session_queue::{ConsolidationDueMarker, MemoryMdPurgeNotice, PURGE_NOTICE_PREVIEW_CAP};
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
pub use subagent_view::{AgentRowView, OwnershipKind, WireOwnershipKind};
pub use task_handle::{Op, TaskHandle};
pub use tool_policy::ToolPolicy;
pub use trace_context::TraceContext;
#[allow(unused_imports)]
pub use waiting_hazard::{WAITING_HAZARD_THRESHOLD_MS, WaitingHazard, waiting_hazard};
// TODO(S16.4): remove these upward re-exports once consumers migrate to domain::services::reducer
#[allow(unused_imports)]
pub use crate::domain::services::reducer::{
    LivenessSnapshot, ReducerState, reduce, update_streaming_mirror,
};
pub use adapter_health::{HealthLevel, HealthSummary, McpHealthRow};
#[allow(unused_imports)]
pub use agent_envelope::{AgentEnvelope, AgentEnvelopeHeader, RapTaskState, RapTaskStateError};
pub use agent_id::{AgentId, AgentIdError};
#[allow(unused_imports)]
pub use assembled_context::{AssembledContext, AssemblyBudget};
#[allow(unused_imports)]
pub use budget::BudgetConfig;
pub use capability::{Capability, CapabilityError};
pub use capability_id::CapabilityId;
pub use capability_kind::CapabilityKind;
pub use capability_registry::{
    CapabilityRegistry, ProviderId, RegisterHandle, RegisteredCapability, RegistryError,
};
pub use catalog_delta::CatalogDelta;
#[allow(unused_imports)]
pub use context_bundle::{
    AssembleDiagnostics, ContextBudget, ContextBundle, ContextSource, ProvenancedEntry, Relevance,
    RetrievalMethod, estimate_tokens,
};
pub use doc_key::DocKey;
pub use filtered_catalog::FilteredCatalog;
pub use filtered_skill_catalog::FilteredSkillCatalog;
pub use launch_spec::AgentLaunchSpec;
pub use mcp_server_spec::{McpServerSource, McpServerSpec, McpTransport, expand_env_vars};
pub use mcp_server_state::McpConnectionState;
#[allow(unused_imports)]
pub use memory_entry::MemoryEntry;
#[allow(unused_imports)]
pub use memory_fact::MemoryFact;
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
pub use redaction::{RedactionOp, RedactionRecord};
#[allow(unused_imports)]
pub use router::{EscalationReason, ModelTier, RouterConfig, StepKind};
pub use search_hit::SearchHit;
#[allow(unused_imports)]
pub use subagent_envelope::{SubagentEnvelope, SubagentEvent};
#[allow(unused_imports)]
pub use tab::{ConversationId, TabId, TabManager, TabState};
#[allow(unused_imports)]
pub use taint::{ProvenanceTag, TaintDecision};
#[allow(unused_imports)]
pub use tool_call::{
    ApprovalSource, RequestId, ToolCall, ToolCallRequest, ToolCallResult, ToolCallTransition,
    status_chip,
};
pub use tool_descriptor::{ToolAnnotations, ToolDescriptor, ToolId};
pub use tools::{ToolCallInfo, ToolDefinition, ToolResult, ToolResultInfo};
#[allow(unused_imports)]
pub use turn::{
    InvocationStatus, PartId, ToolOutput, Turn, TurnId, TurnOrigin, TurnPart, generate_turn_id,
    migrate_chat_message_to_turn, tool_call_id_for,
};
#[allow(unused_imports)]
pub use turn_group::{
    BoundaryRule, GroupId, GroupSignature, GroupingConfig, RoleCounts, TurnGroup, jaccard_distance,
    jaccard_similarity,
};
#[allow(unused_imports)]
pub use usage::{ModelInfo, TokenUsage, UsageInfo, UsageLedgerEntry};
#[allow(unused_imports)]
pub use view_state::{
    AnchorMode, AnchorRef, LayoutMetrics, ScrollDelta, SummaryTier, ViewEvent, ViewState,
};
#[allow(unused_imports)]
pub use visual::{BlockBorder, DensityMode, OverlayType, PanelType, symbols};
