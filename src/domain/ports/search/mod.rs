//! Shared search infrastructure ports per ADR-09-02 v2 §LLM-Only Payload.
//!
//! Phase B-only; consumed by `MetaSearchExposure` (skill + tool sides) and
//! the `search_capabilities` builtin tool. All types in this module are
//! corpus-agnostic — `DocKey` carries `kind` so a single engine can rank
//! tools and skills against each other in one merged BM25 index per
//! Amelia's IDF correctness argument.

pub mod indexable;
pub mod meta_search_engine;

pub use indexable::IndexableItem;
pub use meta_search_engine::{MetaSearchEngine, MetaSearchError};
