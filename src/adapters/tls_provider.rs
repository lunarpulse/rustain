//! Explicit process-default crypto provider for every rustain TLS consumer.

#![cfg(any(feature = "mcp", feature = "a2a", feature = "relay-server"))]

/// Install ring before constructing TLS clients or server configurations.
///
/// Repeated calls are safe: an error means a process default already exists.
/// The dependency graph admits ring only, so startup and library entry points
/// share one provider without relying on backend-specific fallback behavior.
pub fn install_ring_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}
