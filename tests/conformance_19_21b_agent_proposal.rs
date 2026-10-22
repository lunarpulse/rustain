#![cfg(feature = "a2a")]
#![allow(dead_code)]

#[path = "conformance_19_21/harness.rs"]
mod inherited;
#[path = "conformance_19_21b/k01_boundaries.rs"]
mod k01_boundaries;
#[path = "conformance_19_21b/k01_raise.rs"]
mod k01_raise;
#[path = "conformance_19_21b/k01_raw_privacy.rs"]
mod k01_raw_privacy;
#[path = "conformance_19_21b/k02_grants.rs"]
mod k02_grants;
#[path = "conformance_19_21b/k02_matrix.rs"]
mod k02_matrix;
#[path = "conformance_19_21b/k02_peer_context.rs"]
mod k02_peer_context;
#[path = "conformance_19_21b/k02_subagent.rs"]
mod k02_subagent;
#[path = "conformance_19_21b/k03_card_layout.rs"]
mod k03_card_layout;
#[path = "conformance_19_21b/k03_conversation.rs"]
mod k03_conversation;
#[path = "conformance_19_21b/k03_key_ownership.rs"]
mod k03_key_ownership;
#[path = "conformance_19_21b/k03_lifecycle.rs"]
mod k03_lifecycle;
#[path = "conformance_19_21b/k03_review.rs"]
mod k03_review;
#[path = "conformance_19_21b/k06_authorship.rs"]
mod k06_authorship;
#[path = "conformance_19_21b/k06_payload.rs"]
mod k06_payload;
#[path = "conformance_19_21b/k20a_send_authorization.rs"]
mod k20a_send_authorization;
#[path = "conformance_19_21b/observed_host.rs"]
mod observed_host;
#[path = "conformance_19_21b/permission_guard.rs"]
mod permission_guard;
#[path = "conformance_19_21b/proposal_harness.rs"]
mod proposal_harness;
#[path = "conformance_19_21b/tool_provider.rs"]
mod tool_provider;
