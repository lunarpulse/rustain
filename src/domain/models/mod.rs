mod completion;
mod config;
mod content;
mod conversation;
mod focus;
mod message;
mod notice;
mod permission;
mod session;
mod stream;
mod tools;
mod usage;

// Re-exports for all domain model types.
// Many are unused until later stories wire port implementations — suppress warnings.
#[allow(unused_imports)]
pub use completion::CompletionOptions;
pub use config::AppConfig;
#[allow(unused_imports)]
pub use content::ContentBlockType;
#[allow(unused_imports)]
pub use conversation::{
    ChatMessage, Conversation, ConversationSummary, ForkSource, generate_conversation_id,
};
pub use focus::FocusState;
#[allow(unused_imports)]
pub use message::{ImageAttachment, Message, MessageRole, ToolResultMessage, UserMessage};
pub use notice::NoticeLevel;
#[allow(unused_imports)]
pub use permission::{
    ApprovalDecision, FileOperation, PathAccessType, PermissionMode, PermissionRule,
};
#[allow(unused_imports)]
pub use session::{SessionId, SessionState};
#[allow(unused_imports)]
pub use stream::{StopReason, StreamChunk, StreamingPhase, StreamingState};
#[allow(unused_imports)]
pub use tools::{ToolCallInfo, ToolDefinition, ToolResult, ToolResultInfo};
#[allow(unused_imports)]
pub use usage::{ModelInfo, UsageInfo};
