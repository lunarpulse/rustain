use std::collections::HashMap;
use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

// ── Capability ──────────────────────────────────────────────────

/// A discovered capability — protocol-agnostic description.
/// Could be a skill, MCP tool, A2A agent, or any future protocol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capability {
    pub id: String,
    pub name: String,
    pub description: String,
    /// Protocol that provided this capability (e.g., "skills", "mcp", "a2a")
    pub protocol: String,
    pub kind: CapabilityKind,
    pub source: CapabilitySource,
    pub metadata: HashMap<String, String>,
}

/// What kind of thing is this capability?
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CapabilityKind {
    /// Procedural knowledge loaded into agent context (Agent Skills)
    Knowledge,
    /// Callable tool with structured I/O (MCP)
    Tool,
    /// Autonomous agent for task delegation (A2A)
    Agent,
    /// Future protocol — extensible
    Custom(String),
}

/// Where was this capability discovered?
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CapabilitySource {
    /// Skill from filesystem (.agents/skills/, .claude/skills/)
    LocalFile(PathBuf),
    /// MCP server via local process (stdio)
    LocalProcess(String),
    /// Remote endpoint (A2A agent URL, MCP SSE/HTTP)
    NetworkEndpoint(String),
    /// Spawned and owned rustain instance
    Spawned(String),
}

// ── Capability Events ───────────────────────────────────────────

/// Universal event type emitted during capability execution.
/// The TUI renders these protocol-agnostically — it doesn't know
/// whether the event came from MCP, A2A, or a future protocol.
#[derive(Debug)]
pub enum CapabilityEvent {
    /// Text output (stream into chat)
    Output(String),

    /// Status update (for capability panel rendering)
    Status { state: String, detail: String },

    /// Structured result (tool output, agent artifact)
    Result {
        content: serde_json::Value,
        mime_type: String,
    },

    /// Needs user input (permission prompt, question)
    InputRequired {
        prompt: String,
        response_tx: oneshot::Sender<String>,
    },

    /// Capability execution complete
    Complete,

    /// Error during execution
    Error(String),
}

// ── Mention Categories ──────────────────────────────────────────

/// How capabilities appear in the @mention dropdown
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub enum MentionCategory {
    Files,
    Skills,
    Tools,
    Agents,
    /// Future protocol — automatically gets its own @section
    Custom(String),
}

impl MentionCategory {
    pub fn display_name(&self) -> &str {
        match self {
            Self::Files => "Files",
            Self::Skills => "Skills",
            Self::Tools => "MCP",
            Self::Agents => "A2A",
            Self::Custom(name) => name,
        }
    }

    pub fn icon(&self) -> &str {
        match self {
            Self::Files => "",
            Self::Skills => "📚",
            Self::Tools => "⚡",
            Self::Agents => "🤖",
            Self::Custom(_) => "🔌",
        }
    }
}

// ── Permission Scope ────────────────────────────────────────────

/// What permission rules apply to this capability
#[derive(Debug, Clone)]
pub enum PermissionScope {
    /// Built-in tool — governed by permission mode (YOLO/Normal/Plan)
    BuiltinTool,
    /// Skill with tool restrictions (allowed-tools field)
    ToolRestricted(Vec<String>),
    /// Owned agent — owner has absolute authority
    Owned,
    /// Peer agent — mutual consent, can refuse
    PeerConsent,
    /// Unrestricted (e.g., knowledge injection, read-only)
    Unrestricted,
}

// ── Capability Provider Trait ───────────────────────────────────

/// Input to a capability execution
#[derive(Debug, Clone)]
pub struct CapabilityInput {
    pub message: String,
    pub context: HashMap<String, String>,
}

/// Result of activating a capability
#[derive(Debug)]
pub struct ActivatedCapability {
    pub id: String,
    pub protocol: String,
    pub state: serde_json::Value,
}

/// The core abstraction — any interop protocol implements this.
///
/// Adding a new protocol to rustain:
/// 1. Implement CapabilityProvider for the protocol
/// 2. Register with CapabilityRegistry at startup
/// 3. Done — @mentions, permissions, TUI rendering all work automatically
#[async_trait]
pub trait CapabilityProvider: Send + Sync {
    /// Protocol identifier (e.g., "skills", "mcp", "a2a")
    fn protocol(&self) -> &str;

    /// What @mention category does this protocol use?
    fn mention_category(&self) -> MentionCategory;

    /// Discover available capabilities from this provider
    async fn discover(&self, config: &WorkspaceConfig) -> Result<Vec<Capability>, anyhow::Error>;

    /// Activate a specific capability for the current session
    async fn activate(
        &self,
        capability: &Capability,
        session: &SessionContext,
    ) -> Result<ActivatedCapability, anyhow::Error>;

    /// Execute/communicate with an activated capability
    async fn execute(
        &self,
        activated: &ActivatedCapability,
        input: CapabilityInput,
        tx: &mpsc::UnboundedSender<CapabilityEvent>,
    ) -> Result<(), anyhow::Error>;

    /// What permission check does this capability require?
    fn permission_scope(&self, capability: &Capability) -> PermissionScope;
}

// ── Placeholder config types (will be expanded) ─────────────────

/// Workspace configuration for capability discovery
#[derive(Debug, Clone)]
pub struct WorkspaceConfig {
    pub workspace_path: String,
    pub home_dir: String,
}

/// Session context for capability activation
#[derive(Debug, Clone)]
pub struct SessionContext {
    pub session_id: String,
    pub conversation_id: String,
}
