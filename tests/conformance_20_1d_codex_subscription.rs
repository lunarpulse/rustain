//! Subscription requests enter through login, factory, registry and the real CLI.
#![cfg(all(feature = "test-instrumentation", feature = "openai"))]

#[path = "conformance_20_1d_codex_subscription/auth.rs"]
mod auth;
#[path = "conformance_20_1d_codex_subscription/boundaries.rs"]
mod boundaries;
#[path = "conformance_20_1d_codex_subscription/cli.rs"]
mod cli;
#[path = "conformance_20_1d_codex_subscription/credential_binding.rs"]
mod credential_binding;
#[path = "conformance_20_1d_codex_subscription/gate.rs"]
mod gate;
#[path = "conformance_20_1d_codex_subscription/transport.rs"]
mod transport;
