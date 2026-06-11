//! Message-tier context assembly adapters (Story 11.0a, ADR-10-4).
//!
//! Home of the [`ContextAssemblerPort`](crate::domain::ports::ContextAssemblerPort)
//! implementations. `StaticPassthroughAssembler` (impl #1) ships here in 11.0a;
//! Story 11.6's `WindowingAssembler` (impl #2) lands alongside it.
//!
//! Hexagonal placement: infra → domain is allowed, so these impls call the pure
//! `domain::services::message_builder::build_api_messages` seam directly.

pub mod static_passthrough_assembler;
pub mod windowing_assembler;

pub use static_passthrough_assembler::StaticPassthroughAssembler;
pub use windowing_assembler::WindowingAssembler;
