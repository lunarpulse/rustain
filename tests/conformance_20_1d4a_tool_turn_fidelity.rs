//! Story 20.1d4a: tool-turn fidelity. Tool-result images reach the Gemini
//! request (AC1), and every provider response survives the TUI reload, the
//! `ask` session resume and the daemon restart as its own unit, so the first
//! request after the reload starts with the last in-turn request byte for byte
//! (AC2, AC3, AC7); privacy of every outward path and the protocol branch
//! (AC4, AC5); the real debug binary (K7). Every Gemini and Codex row is
//! written by `flows::login` against a loopback stub.
#![cfg(all(feature = "test-instrumentation", feature = "openai"))]

#[path = "conformance_20_1d4a_tool_turn_fidelity/binary.rs"]
mod binary;
#[path = "conformance_20_1d4a_tool_turn_fidelity/gemini.rs"]
mod gemini;
#[path = "conformance_20_1d4a_tool_turn_fidelity/harness.rs"]
mod harness;
#[path = "conformance_20_1d4a_tool_turn_fidelity/providers.rs"]
mod providers;
