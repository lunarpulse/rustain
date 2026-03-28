use thiserror::Error;

/// Top-level domain error hierarchy.
#[allow(dead_code)]
#[derive(Debug, Error)]
pub enum DomainError {
    #[error("Configuration error: {0}")]
    Config(#[from] ConfigError),

    #[error("Event error: {0}")]
    Event(#[from] EventError),

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
