use thiserror::Error;

/// Top-level domain error hierarchy.
#[allow(dead_code)]
#[derive(Debug, Error)]
pub enum DomainError {
    #[error(transparent)]
    Config(#[from] ConfigError),

    #[error(transparent)]
    Event(#[from] EventError),

    #[error(transparent)]
    Provider(#[from] ProviderError),

    #[error(transparent)]
    Storage(#[from] StorageError),

    #[error(transparent)]
    Memory(#[from] MemoryError),

    #[error(transparent)]
    Permission(#[from] PermissionError),

    #[error(transparent)]
    Tool(#[from] ToolError),

    #[error(transparent)]
    Capability(#[from] CapabilityError),

    #[error(transparent)]
    Ownership(#[from] OwnershipError),

    #[error(transparent)]
    Profile(#[from] ProfileError),

    #[error(transparent)]
    Channel(#[from] ChannelError),

    #[error(transparent)]
    Scheduler(#[from] SchedulerError),

    #[error(transparent)]
    Session(#[from] SessionError),

    #[error(transparent)]
    TurnQueue(#[from] TurnQueueError),

    #[error(transparent)]
    Composition(#[from] AdapterCompositionError),
    #[error(transparent)]
    Transition(#[from] TransitionError),

    #[error(transparent)]
    ApprovalPersistence(#[from] ApprovalPersistenceError),

    #[error("Startup error: {0}")]
    Startup(String),

    #[error("{0}")]
    Other(String),
}

#[allow(dead_code)]
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("Missing configuration: {0}")]
    Missing(String),

    #[error("Invalid configuration value: {field} = {value}")]
    Invalid { field: String, value: String },

    #[error("Failed to read config file: {0}")]
    IoError(String),

    /// Story 8.1 AC-11 — parse error in a specific config file.
    #[error("Config parse error in {path}: line {line:?}: {reason}")]
    Parse {
        path: std::path::PathBuf,
        line: Option<u32>,
        reason: String,
    },

    /// Story 8.1 AC-11 — root-level figment extraction failure.
    #[error("Config extraction failed: {reason}")]
    Extract { reason: String },
}

#[allow(dead_code)]
#[derive(Debug, Error)]
pub enum EventError {
    #[error("Channel closed")]
    ChannelClosed,

    #[error("Event processing failed: {0}")]
    ProcessingFailed(String),
}

#[allow(dead_code)]
#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("Connection failed: {0}")]
    ConnectionFailed(String),

    #[error("Authentication failed")]
    AuthenticationFailed,

    #[error("Rate limited{}", .retry_after_ms.map(|ms| format!(", retry after {}ms", ms)).unwrap_or_default())]
    RateLimited { retry_after_ms: Option<u64> },

    #[error("Stream error: {0}")]
    StreamError(String),

    #[error("Request cancelled")]
    Cancelled,

    /// Network unreachable — transport-level connect/timeout/DNS failure.
    /// Classified at the adapter boundary where `reqwest::Error` exists.
    /// All consumers match on this domain variant — zero `reqwest` coupling above the adapter.
    #[error("Offline: {0}")]
    Offline(String),

    /// The provider's probe endpoint is not implemented by this provider/base-url.
    /// Not a failure — the provider may still work for billable operations.
    #[error("endpoint unsupported (HTTP {0})")]
    EndpointUnsupported(u16),

    #[error("{0}")]
    Other(String),
}

impl ProviderError {
    /// True when the error indicates network-level unreachability (connect/timeout/DNS).
    pub fn is_offline(&self) -> bool {
        matches!(self, ProviderError::Offline(_))
    }
}

#[allow(dead_code)]
#[derive(Debug, Error)]
pub enum StorageError {
    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Serialization error: {0}")]
    SerializationError(String),

    #[error("I/O error: {0}")]
    IoError(String),

    #[error("Operation not supported: {0}")]
    NotSupported(String),

    #[error("{0}")]
    Other(String),
}

/// Story 11.1 — daily-log memory adapter errors. Mirrors `StorageError`.
#[allow(dead_code)]
#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("I/O error: {0}")]
    IoError(String),

    #[error("Parse error: {0}")]
    ParseError(String),

    #[error("Operation not supported: {0}")]
    NotSupported(String),

    #[error("{0}")]
    Other(String),
}

/// Context-assembly failure (Story 11.4). Mirrors [`MemoryError`]: a context
/// failure DEGRADES a turn (empty bundle, observable `warn` + counter), it never
/// aborts it. A `MemoryError` raised while gathering converts in via `#[from]`.
#[allow(dead_code)]
#[derive(Debug, Error)]
pub enum ContextError {
    #[error("memory error during context assembly: {0}")]
    Memory(#[from] MemoryError),

    #[error("{0}")]
    Other(String),
}

#[allow(dead_code)]
#[derive(Debug, Error)]
pub enum PermissionError {
    #[error("Permission denied: {0}")]
    Denied(String),

    #[error("Command blocked: {0}")]
    Blocked(String),

    #[error("Workspace violation: {0}")]
    WorkspaceViolation(String),

    #[error("Permission request cancelled")]
    Cancelled,

    #[error("{0}")]
    Other(String),
}

#[allow(dead_code)]
#[derive(Debug, Error)]
pub enum ToolError {
    #[error("Tool not found: {0}")]
    NotFound(String),

    #[error("Tool execution failed: {0}")]
    ExecutionFailed(String),

    #[error("Invalid tool input: {0}")]
    InvalidInput(String),

    #[error("Tool execution timed out")]
    Timeout,

    #[error("Tool execution cancelled")]
    Cancelled,

    #[error("{0}")]
    Other(String),
}

// Stub error types — single Other(String) variant each until implementation sprint.

#[allow(dead_code)]
#[derive(Debug, Error)]
pub enum CapabilityError {
    #[error("{0}")]
    Other(String),
}

#[allow(dead_code)]
#[derive(Debug, Error)]
pub enum OwnershipError {
    #[error("{0}")]
    Other(String),
}

#[allow(dead_code)]
#[derive(Debug, Error)]
pub enum ProfileError {
    #[error(
        "Profile '{name}' not found in any search path: {search_paths:?}. \
             Available built-ins: base, coding, personal-assistant."
    )]
    ProfileNotFound {
        name: String,
        search_paths: Vec<std::path::PathBuf>,
    },

    #[error(
        "Profile '{child}' extends parent '{parent}' which does not exist. \
             Searched: {search_paths:?}."
    )]
    ParentNotFound {
        child: String,
        parent: String,
        search_paths: Vec<std::path::PathBuf>,
    },

    #[error(
        "Circular profile extension chain detected: {}. \
         Remove the cycle by changing one extends pointer.",
        chain.join(" → ")
    )]
    CircularExtends { chain: Vec<String> },

    #[error(
        "Profile extends chain exceeds maximum depth (4): {}. Flatten the hierarchy.",
        chain.join(" → ")
    )]
    ExtendsTooDeep { chain: Vec<String> },

    #[error(
        "Profile '{profile}' missing required dimensions: {dimensions:?}. \
         Either define them in the profile or extend a profile that does."
    )]
    DimensionMissing {
        profile: String,
        dimensions: Vec<crate::domain::models::PortDimension>,
    },

    #[error(
        "Profile '{profile}': unknown adapter '{adapter}' for port '{port:?}'. \
         Available: {available:?}.{}",
        suggestion.as_ref().map(|s| format!(" (Did you mean '{}'?)", s)).unwrap_or_default()
    )]
    AdapterUnknown {
        profile: String,
        port: crate::domain::models::PortDimension,
        adapter: String,
        available: Vec<String>,
        suggestion: Option<String>,
    },

    #[error(
        "Profile '{profile}' references adapter '{adapter}' (port '{port:?}') which requires cargo feature '{feature}'. \
         Recompile with: cargo install rustain --features {feature}"
    )]
    AdapterFeatureGated {
        profile: String,
        port: crate::domain::models::PortDimension,
        adapter: String,
        feature: String,
    },

    // Stub variant; raised by Story 8.3+ when adapters carry version metadata.
    #[error(
        "Profile '{profile}': adapter '{adapter}' (port '{port:?}') version mismatch: requires {required}, found {found}"
    )]
    IncompatibleAdapterVersion {
        profile: String,
        port: crate::domain::models::PortDimension,
        adapter: String,
        required: String,
        found: String,
    },

    #[error("Profile '{path}' failed to parse: {reason}")]
    Parse {
        path: std::path::PathBuf,
        reason: String,
    },

    #[error("Profile I/O error reading {path}: {source}")]
    IoRead {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{0}")]
    Other(String),
}

#[allow(dead_code)]
#[derive(Debug, Error)]
pub enum ChannelError {
    #[error("{0}")]
    Other(String),
}

#[allow(dead_code)]
#[derive(Debug, Error)]
pub enum SchedulerError {
    #[error("{0}")]
    Other(String),
}

#[allow(dead_code)]
#[derive(Debug, Error)]
pub enum SessionError {
    #[error("{0}")]
    Other(String),
}

#[derive(Debug, Error)]
pub enum TurnQueueError {
    #[error("Message queue full")]
    QueueFull,
}

#[allow(dead_code)]
#[derive(Debug, Error)]
pub enum ClipboardError {
    #[error("clipboard backend error: {0}")]
    Backend(String),
    #[error("clipboard read timed out")]
    Timeout,
}

/// Story 8.3 — adapter composition errors.
/// Composition failures are FATAL at startup (exit code 2);
/// on reload they are surfaced as SystemNotice while preserving
/// the previous adapters.
#[derive(Debug, Error)]
pub enum AdapterCompositionError {
    #[error(
        "Internal: profile composer encountered unknown adapter '{name}' for port '{port:?}'. \
         This is a bug — please report. Available: {available:?}."
    )]
    UnknownAdapter {
        port: crate::domain::models::PortDimension,
        name: String,
        available: Vec<String>,
    },

    #[error("Failed to construct adapter '{name}' for port '{port:?}': {source}")]
    AdapterConstructionFailed {
        port: crate::domain::models::PortDimension,
        name: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error(
        "Adapter '{name}' for port '{port:?}' requires field '{missing_field}' on ComposeContext."
    )]
    MissingComposeContext {
        port: crate::domain::models::PortDimension,
        name: String,
        missing_field: String,
    },

    #[error(
        "Profile selection is missing a required adapter for port '{port:?}'. \
         This is likely a profile validation bug."
    )]
    MissingDimension {
        port: crate::domain::models::PortDimension,
    },
}

#[derive(Debug, Error)]
pub enum TransitionError {
    #[error("Adapter '{adapter_id}' for port '{port_type}' failed prepare_detach: {reason}")]
    PrepareFailed {
        port_type: &'static str,
        adapter_id: String,
        reason: String,
    },
    #[error(
        "Adapter '{adapter_id}' for port '{port_type}' rejected receive_state \
         (incompatible version {got} vs expected {expected})"
    )]
    IncompatibleState {
        port_type: &'static str,
        adapter_id: String,
        got: u32,
        expected: u32,
    },
    #[error(
        "post_transition_verify failed for adapter '{adapter_id}' on port '{port_type}': {reason}"
    )]
    VerifyFailed {
        port_type: &'static str,
        adapter_id: String,
        reason: String,
    },
    #[error("Cold-tier adapter '{adapter_id}' on port '{port_type}' loop restart failed: {reason}")]
    RestartFailed {
        port_type: &'static str,
        adapter_id: String,
        reason: String,
    },
    #[error("Profile '{name}' not found")]
    ProfileNotFound { name: String },
}

#[allow(dead_code)]
#[derive(Debug, Error)]
pub enum ApprovalPersistenceError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("TOML deserialization error: {0}")]
    Toml(#[from] toml::de::Error),

    #[error("TOML serialization error: {0}")]
    TomlSer(#[from] toml::ser::Error),
}
