// The attached-rail harness composes the daemon through `TeamSendTestHooks`, which
// exists only under `test-instrumentation`; the CI a2a lane always enables it.
#![cfg(all(feature = "a2a", feature = "test-instrumentation"))]

#[path = "conformance_19_21/attached_harness.rs"]
mod attached_harness;
#[allow(clippy::duplicate_mod)] // private copy per root, as when each root was its own crate
#[path = "conformance_19_21/harness.rs"]
mod inherited;
use inherited as harness;
#[path = "conformance_19_20/act5_k01_wire_carrier.rs"]
mod act5_k01_wire_carrier;
#[path = "conformance_19_20/act5_k02_handoff.rs"]
mod act5_k02_handoff;
#[path = "conformance_19_21c/c_k01_thread.rs"]
mod c_k01_thread;
#[path = "conformance_19_21c/c_k02_followup.rs"]
mod c_k02_followup;
#[path = "conformance_19_21c/c_k03_retract.rs"]
mod c_k03_retract;
#[path = "conformance_19_21c/c_k04_retract_gating.rs"]
mod c_k04_retract_gating;
#[path = "conformance_19_21d/d_k01_record_wire.rs"]
mod d_k01_record_wire;
#[path = "conformance_19_21d/d_k02_refusals.rs"]
mod d_k02_refusals;
#[path = "conformance_19_21d/d_k03_ingest.rs"]
mod d_k03_ingest;
#[path = "conformance_19_21d/d_k04_render.rs"]
mod d_k04_render;
#[path = "conformance_19_21d/d_k05_no_authority.rs"]
mod d_k05_no_authority;
#[path = "conformance_19_21d/d_k06_decode.rs"]
mod d_k06_decode;
#[cfg(all(unix, feature = "test-instrumentation"))]
#[path = "conformance_19_21b/g_entrypoints.rs"]
mod g_entrypoints;
#[cfg(all(unix, feature = "test-instrumentation"))]
#[path = "conformance_19_21b/g_harness.rs"]
mod g_harness;
#[cfg(all(unix, feature = "test-instrumentation"))]
#[path = "conformance_19_21b/g_k01_boundaries.rs"]
mod g_k01_boundaries;
#[cfg(all(unix, feature = "test-instrumentation"))]
#[path = "conformance_19_21b/g_k02_authorization.rs"]
mod g_k02_authorization;
#[cfg(all(unix, feature = "test-instrumentation"))]
#[path = "conformance_19_21b/g_k04_lifecycle.rs"]
mod g_k04_lifecycle;
#[cfg(all(unix, feature = "test-instrumentation"))]
#[path = "conformance_19_21b/g_k05_liveness.rs"]
mod g_k05_liveness;
#[cfg(all(unix, feature = "test-instrumentation"))]
#[path = "conformance_19_21b/g_k06_authorship.rs"]
mod g_k06_authorship;
#[cfg(all(unix, feature = "test-instrumentation"))]
#[path = "conformance_19_21b/g_k06_payload.rs"]
mod g_k06_payload;
#[cfg(all(unix, feature = "test-instrumentation"))]
#[path = "conformance_19_21b/g_k20a_send_authorization.rs"]
mod g_k20a_send_authorization;
#[cfg(all(unix, feature = "test-instrumentation"))]
#[path = "conformance_19_21b/g_process_restart.rs"]
mod g_process_restart;
#[path = "conformance_19_21/k01_send_action.rs"]
mod k01_send_action;
#[path = "conformance_19_21/k02_board_rules.rs"]
mod k02_board_rules;
#[path = "conformance_19_21/k03_restart.rs"]
mod k03_restart;
#[path = "conformance_19_21/k03b_liveness.rs"]
mod k03b_liveness;
#[path = "conformance_19_21/k04_board_surface.rs"]
mod k04_board_surface;
#[path = "conformance_19_21/k04b_journal_read.rs"]
mod k04b_journal_read;
#[path = "conformance_19_21/k05_attached_lifecycle.rs"]
mod k05_attached_lifecycle;
#[path = "conformance_19_21/k05_attached_send_board.rs"]
mod k05_attached_send_board;
#[path = "conformance_19_21/k05_sender_local_privacy.rs"]
mod k05_sender_local_privacy;
#[path = "conformance_19_21/k06_attached_status_consent.rs"]
mod k06_attached_status_consent;
#[path = "conformance_19_21/k07_k08_recipient.rs"]
mod k07_k08_recipient;
#[cfg(all(unix, feature = "test-instrumentation"))]
#[allow(clippy::duplicate_mod)] // private copy per root, as when each root was its own crate
#[path = "conformance_19_21b/observed_host.rs"]
mod observed_host;
#[path = "conformance_19_24/s_harness.rs"]
mod s_harness;
#[path = "conformance_19_24/s_k01_share.rs"]
mod s_k01_share;
#[path = "conformance_19_24/s_k02_uniform.rs"]
mod s_k02_uniform;
#[path = "conformance_19_24/s_k03_bounds.rs"]
mod s_k03_bounds;
#[path = "conformance_19_24/s_k04_tolerance.rs"]
mod s_k04_tolerance;
#[path = "conformance_19_24/s_k05_gates.rs"]
mod s_k05_gates;
#[cfg(all(unix, feature = "test-instrumentation"))]
#[allow(clippy::duplicate_mod)] // private copy per root, as when each root was its own crate
#[path = "conformance_19_21b/tool_provider.rs"]
mod tool_provider;
