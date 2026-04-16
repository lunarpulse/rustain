//! Import domain — `ConversationImporter` trait, `ImporterRegistry`, and shared types.
//!
//! Hexagonal placement (Amendment 7):
//! - Trait + registry + value types → `domain/services/import.rs` (pure domain)
//! - Concrete importers → `adapters/importers/`

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::domain::errors::StorageError;
use crate::domain::ports::StoragePort;

/// Metadata extracted from a candidate session during discovery.
#[derive(Clone, Debug)]
pub struct ImportCandidate {
    /// The filename stem (UUID) from the source tool.
    pub source_session_id: String,
    /// Derived title (first user message, truncated to 50 chars).
    pub title: String,
    /// Unix timestamp (seconds) from the first line's timestamp field.
    pub created_at: i64,
    /// Count of real user + assistant messages (excluding meta/system lines).
    pub message_count: usize,
    /// Absolute path of the source file.
    pub source_path: PathBuf,
}

/// Outcome of importing a single session.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub enum ImportResult {
    /// Successfully imported; contains the new rustain conversation ID.
    Imported(String),
    /// This session was already imported on a previous run; skipped.
    AlreadyImported,
    /// The source file contained no real user/assistant messages after
    /// filtering meta/system lines — nothing written to storage.
    /// Reported as "Skipped (empty)" in the CLI summary (AC5).
    SkippedEmpty,
    /// Import failed; contains a human-readable error message.
    Failed(String),
}

/// Abstract importer for a specific source tool.
///
/// Implementors live in `adapters/importers/` (outbound adapters).
/// The trait itself lives in the domain so that CLI handlers can depend
/// on it without creating a cycle through the adapter layer.
#[async_trait]
pub trait ConversationImporter: Send + Sync {
    /// Human-readable display name (e.g., "Claude Code").
    #[allow(dead_code)]
    fn source_name(&self) -> &'static str;

    /// Machine identifier used in `--from <id>` and `ImportSource.source`
    /// (e.g., "claude-code").
    #[allow(dead_code)]
    fn source_id(&self) -> &'static str;

    /// Discover candidate sessions from the given directory (or the importer's
    /// default root if `path` is `None`).
    ///
    /// Returns all candidates found in the source directory, sorted by
    /// `created_at` ascending. Does NOT filter already-imported sessions —
    /// callers in `run_migrate` apply idempotency filtering against storage.
    ///
    /// Returns `Err(StorageError::NotFound)` if the source directory is missing.
    async fn discover(&self, path: Option<&Path>) -> Result<Vec<ImportCandidate>, StorageError>;

    /// Import a single candidate into rustain's storage.
    ///
    /// Idempotency: if the candidate has already been imported (detected via
    /// `storage.list_conversations()` + `load_session_meta()` scan), returns
    /// `Ok(ImportResult::AlreadyImported)` without writing anything.
    ///
    /// Partial-success policy: the caller (CLI handler) decides whether to
    /// continue after failures; this method returns `Ok(ImportResult::Failed(msg))`
    /// rather than `Err` for per-candidate failures.
    async fn import(
        &self,
        candidate: &ImportCandidate,
        storage: &dyn StoragePort,
    ) -> Result<ImportResult, StorageError>;
}

/// Registry of all registered importers, keyed by `source_id`.
pub struct ImporterRegistry {
    importers: HashMap<String, Box<dyn ConversationImporter>>,
}

impl ImporterRegistry {
    pub fn new() -> Self {
        Self {
            importers: HashMap::new(),
        }
    }

    /// Register an importer under its `source_id`.
    pub fn register(&mut self, id: impl Into<String>, importer: Box<dyn ConversationImporter>) {
        self.importers.insert(id.into(), importer);
    }

    /// Look up an importer by its source identifier.
    pub fn get(&self, id: &str) -> Option<&dyn ConversationImporter> {
        self.importers.get(id).map(|b| b.as_ref())
    }

    /// List all registered source IDs (stable sorted for deterministic output).
    pub fn available_sources(&self) -> Vec<&str> {
        let mut ids: Vec<&str> = self.importers.keys().map(|s| s.as_str()).collect();
        ids.sort();
        ids
    }
}

impl Default for ImporterRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    struct FakeImporter;

    #[async_trait]
    impl ConversationImporter for FakeImporter {
        fn source_name(&self) -> &'static str {
            "Fake Source"
        }
        fn source_id(&self) -> &'static str {
            "fake"
        }
        async fn discover(
            &self,
            _path: Option<&Path>,
        ) -> Result<Vec<ImportCandidate>, StorageError> {
            Ok(vec![])
        }
        async fn import(
            &self,
            _candidate: &ImportCandidate,
            _storage: &dyn StoragePort,
        ) -> Result<ImportResult, StorageError> {
            Ok(ImportResult::Imported("new-id".to_string()))
        }
    }

    struct OtherFakeImporter;

    #[async_trait]
    impl ConversationImporter for OtherFakeImporter {
        fn source_name(&self) -> &'static str {
            "Other Source"
        }
        fn source_id(&self) -> &'static str {
            "other"
        }
        async fn discover(
            &self,
            _path: Option<&Path>,
        ) -> Result<Vec<ImportCandidate>, StorageError> {
            Ok(vec![])
        }
        async fn import(
            &self,
            _candidate: &ImportCandidate,
            _storage: &dyn StoragePort,
        ) -> Result<ImportResult, StorageError> {
            Ok(ImportResult::Imported("other-id".to_string()))
        }
    }

    #[test]
    fn test_registry_register_and_get() {
        let mut registry = ImporterRegistry::new();
        registry.register("fake", Box::new(FakeImporter));
        let found = registry.get("fake");
        assert!(found.is_some());
        assert_eq!(found.unwrap().source_id(), "fake");
    }

    #[test]
    fn test_registry_get_unknown_returns_none() {
        let registry = ImporterRegistry::new();
        assert!(registry.get("aider").is_none());
    }

    #[test]
    fn test_registry_available_sources_lists_all_registered() {
        let mut registry = ImporterRegistry::new();
        registry.register("fake", Box::new(FakeImporter));
        registry.register("other", Box::new(OtherFakeImporter));
        let sources = registry.available_sources();
        assert_eq!(sources.len(), 2);
        assert!(sources.contains(&"fake"));
        assert!(sources.contains(&"other"));
    }
}
