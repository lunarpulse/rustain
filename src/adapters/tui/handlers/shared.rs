//! Cross-module utility helpers shared by ≥2 handler modules.
//!
//! Per ADR-08-01 §D4: this module starts EMPTY and is populated ONLY when a
//! second consumer for a helper appears. First utility added requires a one-paragraph
//! justification in the PR description.
//!
//! Per ADR-08-01 §D7 decision-gate trigger: if `>100` lines accumulate here at any
//! point during Phase 2 prototyping, HALT and re-scope per the gate criteria.
//!
//! Empty at Phase 1 / Task 2 module scaffolding. Populated only by demand.
