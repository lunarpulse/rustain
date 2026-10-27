//! Story 20.1d1b: Codex encrypted reasoning replay (capture, retry boundary,
//! persistence, privacy) and live Codex model discovery (scope and ordering).
//! Every stored row is created by `flows::login` against a loopback stub.
#![cfg(all(feature = "test-instrumentation", feature = "openai"))]

#[path = "conformance_20_1d1b_codex_replay_discovery/cli.rs"]
mod cli;
#[path = "conformance_20_1d1b_codex_replay_discovery/common.rs"]
pub(crate) mod common;
#[path = "conformance_20_1d1b_codex_replay_discovery/daemon.rs"]
mod daemon;
#[path = "conformance_20_1d1b_codex_replay_discovery/discovery.rs"]
mod discovery;
#[path = "conformance_20_1d1b_codex_replay_discovery/privacy.rs"]
mod privacy;
#[path = "conformance_20_1d1b_codex_replay_discovery/replay.rs"]
mod replay;
