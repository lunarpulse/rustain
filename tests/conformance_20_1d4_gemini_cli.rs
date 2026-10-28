//! Gemini CLI subscription through the real login flow, factory, transport and CLI.
#![cfg(all(feature = "test-instrumentation", feature = "openai"))]

#[path = "conformance_20_1d4_gemini_cli/auth.rs"]
pub(crate) mod auth;
#[path = "conformance_20_1d4_gemini_cli/cli.rs"]
mod cli;
#[path = "conformance_20_1d4_gemini_cli/onboarding.rs"]
mod onboarding;
#[path = "conformance_20_1d4_gemini_cli/replay.rs"]
mod replay;
#[path = "conformance_20_1d4_gemini_cli/transport.rs"]
mod transport;
