//! Area test binary `memory` — Grand Plan P0-3 (docs/rustain-grand-plan-2026-10-22.md §4).
//!
//! 12 former `tests/*.rs` roots, compiled and linked once instead of 11 times.
//! Each root stays at its original path (story records cite them); it is loaded
//! here as a module via `#[path]`. Add a new root by appending one `mod` line to
//! exactly one area binary — `every_integration_test_root_is_owned_by_one_area_binary`
//! (src/domain/ports/capability_provider.rs) fails the lib tests otherwise.
//!
//! Feature-gated roots keep their `#![cfg(...)]` inner attribute, so this binary
//! compiles in every lane and only the modules whose feature is on carry tests.

#[path = "../conformance_meta_search.rs"]
mod conformance_meta_search;

#[path = "../conformance_search_hit_schema.rs"]
mod conformance_search_hit_schema;

#[path = "../consolidation_promote.rs"]
mod consolidation_promote;

#[path = "../context_assembly_nfr58.rs"]
mod context_assembly_nfr58;

#[path = "../integration_epic11_memory_pipeline.rs"]
mod integration_epic11_memory_pipeline;

#[path = "../integration_search_doors_invocation.rs"]
mod integration_search_doors_invocation;

#[path = "../profile_composition_memory.rs"]
mod profile_composition_memory;

#[path = "../search_hit_roundtrip.rs"]
mod search_hit_roundtrip;

#[path = "../vector_search.rs"]
mod vector_search;

#[path = "../windowing_causal_chain.rs"]
mod windowing_causal_chain;

#[path = "../windowing_diagnostics.rs"]
mod windowing_diagnostics;

#[path = "../regression_removal_integrity.rs"]
mod regression_removal_integrity;
