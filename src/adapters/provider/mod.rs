pub mod registry;
pub mod router;

pub use registry::ProviderRegistry;
pub use router::ProviderRouter;

/// Classify a `reqwest::Error` into the correct domain `ProviderError` variant.
///
/// Transport-level connect/timeout/DNS/request failures → `Offline`;
/// builder/redirect/body/decode errors → `ConnectionFailed`.
/// Called at the adapter boundary — the ONLY place `reqwest::Error` is inspected.
/// All consumers above the adapter match on `ProviderError::{Offline, ConnectionFailed, …}`.
#[cfg(any(feature = "anthropic", feature = "openai", feature = "ollama"))]
pub fn classify_reqwest_error(e: &reqwest::Error) -> crate::domain::errors::ProviderError {
    if e.is_connect() || e.is_timeout() || e.is_request() {
        crate::domain::errors::ProviderError::Offline(e.to_string())
    } else {
        // Builder misconfiguration, redirect loops, body/decode errors, etc.
        crate::domain::errors::ProviderError::ConnectionFailed(e.to_string())
    }
}
