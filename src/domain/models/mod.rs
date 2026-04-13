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
pub mod project_context;
mod session;
pub mod session_meta;
mod stream;
pub mod tab;
mod tools;
mod usage;
#[allow(dead_code)]
pub mod visual;

// Re-exports for all domain model types.
// Many are unused until later stories wire port implementations — suppress warnings.
#[allow(unused_imports)]
pub use checkpoint::{CheckpointId, CheckpointMeta, RevertStatus, RevertedFile};
#[allow(unused_imports)]
pub use completion::CompletionOptions;
pub use config::AppConfig;
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
    ApprovalDecision, FileOperation, PathAccessType, PermissionMode, PermissionRule,
};
#[allow(unused_imports)]
pub use session::{SessionId, SessionManager, SessionState};
#[allow(unused_imports)]
pub use session_meta::{SessionMeta, extract_title_from_first_message, now_unix, shorten_text};
#[allow(unused_imports)]
pub use stream::{StopReason, StreamChunk, StreamingPhase, StreamingState, apply_chunk};
#[allow(unused_imports)]
pub use tab::{ConversationId, TabId, TabManager, TabState};
#[allow(unused_imports)]
pub use tools::{ToolCallInfo, ToolDefinition, ToolResult, ToolResultInfo};
#[allow(unused_imports)]
pub use usage::{ModelInfo, UsageInfo};
#[allow(unused_imports)]
pub use visual::{BlockBorder, DensityMode, OverlayType, PanelType, symbols};
