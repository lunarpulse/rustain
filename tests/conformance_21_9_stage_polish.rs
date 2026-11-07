//! Story 21.9 slice 9-A: reasoning fold, reasoning window, verbatim user text
//! and Stage hygiene, driven through the production front doors (the reducer,
//! the event-loop renderer, the keys and the S-BLOCK host effects).

#[path = "conformance_21_9_stage_polish/k1_fold.rs"]
mod k1_fold;
#[path = "conformance_21_9_stage_polish/k2_reasoning_facts.rs"]
mod k2_reasoning_facts;
#[path = "conformance_21_9_stage_polish/k3_reasoning_window.rs"]
mod k3_reasoning_window;
#[path = "conformance_21_9_stage_polish/k5_verbatim.rs"]
mod k5_verbatim;
#[path = "conformance_21_9_stage_polish/k8_hygiene.rs"]
mod k8_hygiene;
#[path = "conformance_21_9_stage_polish/smoke_frames.rs"]
mod smoke_frames;
#[path = "conformance_21_9_stage_polish/support.rs"]
pub(crate) mod support;
