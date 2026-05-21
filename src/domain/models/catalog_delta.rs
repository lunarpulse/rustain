// TODO(9.3b AC-9-3b-6): Replace placeholder with full definition:
// pub struct CatalogDelta {
//     pub added: Vec<ToolDescriptor>,
//     pub removed: Vec<ToolId>,
//     pub version: u64,
// }
//
// The current zero-field stub exists so 9.3a compiles standalone.
// 9.3b's CatalogObserver uses `&CatalogDelta` in its trait signature;
// the placeholder satisfies the compiler without committing to a shape.

/// Placeholder type for catalog deltas.
///
/// Replaced by Story 9.3b (AC-9-3b-6) with the full definition including
/// `added`, `removed`, and `version` fields.
#[derive(Debug, Clone)]
pub struct CatalogDelta {}
