//! Device logins (the RFC 8628 engine with its xAI consumer, OpenAI's
//! custom Codex device protocol) and working xAI inference on the Responses core.
//! Every stored row is created by `flows::login` against a loopback stub reached
//! through `RUSTAIN_TEST_AUTH_ORIGIN`; device waits go through a recording sleeper
//! that advances a shared fake clock, so no test sleeps.
#![cfg(all(feature = "test-instrumentation", feature = "openai"))]

#[path = "conformance_20_1d2_device_and_xai/cli.rs"]
mod cli;
#[path = "conformance_20_1d2_device_and_xai/codex_device.rs"]
mod codex_device;
#[path = "conformance_20_1d2_device_and_xai/device.rs"]
pub(crate) mod device;
#[path = "conformance_20_1d2_device_and_xai/xai_catalog.rs"]
mod xai_catalog;
#[path = "conformance_20_1d2_device_and_xai/xai_transport.rs"]
mod xai_transport;
