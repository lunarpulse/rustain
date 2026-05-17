#![allow(dead_code)]
//! SDK STABILITY: New methods MUST carry a default impl. Removing a method
//! or changing a signature is a MAJOR version bump. See
//! docs/adapter-composition.md § Adapter SDK Compatibility (Story 8.3 AC-6).
/// Session lifecycle management.
///
/// Claudian equivalent: `src/core/session/sessionManager.ts`
pub trait SessionPort: Send + Sync {
    // v0.75: session lifecycle methods
}
