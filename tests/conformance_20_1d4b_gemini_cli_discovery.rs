//! Story 20.1d4b: Gemini CLI live model discovery (Antigravity roster with the
//! `retrieveUserQuota` fallback, reviewed variant collapse, derived pairs),
//! no-effort wire routing of collapsed logical ids, live eligibility, the
//! byte-bounded tool-schema `$ref` inlining, and the real debug binary.
//! Every stored row is created by `flows::login` against a loopback stub.
#![cfg(all(feature = "test-instrumentation", feature = "openai"))]

#[path = "conformance_20_1d4b_gemini_cli_discovery/cli.rs"]
mod cli;
#[path = "conformance_20_1d4b_gemini_cli_discovery/common.rs"]
mod common;
#[path = "conformance_20_1d4b_gemini_cli_discovery/discovery.rs"]
mod discovery;
#[path = "conformance_20_1d4b_gemini_cli_discovery/eligibility.rs"]
mod eligibility;
#[path = "conformance_20_1d4b_gemini_cli_discovery/routing.rs"]
mod routing;
