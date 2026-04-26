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

    #[error("{0}")]
    Other(String),
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
