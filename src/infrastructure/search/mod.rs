//! Phase B search infrastructure per ADR-09-02 v2 §Phased Implementation.
//!
//! Module is gated `#[cfg(feature = "meta-search")]` — default builds do
//! NOT compile this code AND do NOT pull the `bm25` dependency.

pub mod bm25_engine;
pub mod merged_index;
pub mod synonym_map;

pub use bm25_engine::Bm25SearchEngine;
pub use merged_index::{CachedProjection, MergedIndex};
pub use synonym_map::{SYNONYMS, SynonymMap};
