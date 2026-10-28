//! GitHub Copilot subscription: custom device login with Enterprise domains,
//! routing probe and model-policy enablement; Copilot header identity and
//! fallback; model-dependent Messages / Responses / Chat Completions inference;
//! the resolver-backed Chat Completions adapter; catalogue, gate and discovery;
//! and the debug-binary flow. Every stored Copilot row is created by
//! `flows::login` against a loopback stub reached through
//! `RUSTAIN_TEST_AUTH_ORIGIN`; device waits go through the recording sleeper
//! and its fake clock, so no test sleeps.
#![cfg(all(feature = "test-instrumentation", feature = "openai"))]

#[path = "conformance_20_1d3_copilot/catalog.rs"]
pub(crate) mod catalog;
#[path = "conformance_20_1d3_copilot/chat_d4.rs"]
pub(crate) mod chat_d4;
#[path = "conformance_20_1d3_copilot/cli.rs"]
mod cli;
#[path = "conformance_20_1d3_copilot/common.rs"]
pub(crate) mod common;
#[path = "conformance_20_1d3_copilot/identity.rs"]
mod identity;
#[path = "conformance_20_1d3_copilot/login.rs"]
mod login;
#[path = "conformance_20_1d3_copilot/routes.rs"]
mod routes;
