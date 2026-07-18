use super::{CapabilityId, TrustTier};
use serde::{Deserialize, Serialize};

/// A discovered capability from a provider.
///
/// This is the output of `CapabilityProvider::discover()` and is used
/// to populate the `CapabilityRegistry`. It carries the minimal shape
/// needed for the registry to construct a `RegisteredCapability`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capability {
    /// Unique identifier across all protocols.
    pub id: CapabilityId,
    /// Human-readable name (e.g., "echo", "Bash").
    pub name: String,
    /// Description for tooltips and autocomplete.
    pub description: String,
    /// JSON Schema for the tool's input parameters.
    pub input_schema: serde_json::Value,
    /// Whether the capability is safe for parallel execution.
    pub parallel_safe: bool,
    /// Trust assigned by the provider's configuration and preserved by the registry.
    pub trust: TrustTier,
}

/// Errors from capability provider operations.
#[derive(Debug, thiserror::Error)]
pub enum CapabilityError {
    /// Discovery of capabilities failed.
    #[error("capability discovery failed: {0}")]
    Discover(String),

    /// Generic invocation failure.
    #[error("capability invocation failed: {0}")]
    Invoke(String),

    /// Invocation failed with an error message from the underlying transport.
    #[error("capability invocation failed for {0}: {1}")]
    InvocationFailed(String, String),
}
