use std::path::{Path, PathBuf};

use crate::domain::models::SessionMeta;

/// A plan file resolved by `PlanManager`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanFile {
    pub slug: String,
    pub path: PathBuf,
}

/// Error type for plan manager operations.
#[derive(Debug, thiserror::Error)]
pub enum PlanManagerError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Domain service that manages plan-file lifecycle.
///
/// Slug generation is deterministic per session via `SessionMeta.plan_slug`.
/// Once a slug is assigned it survives across restarts, forks, and rewinds.
/// Tests can inject a custom slug function via `new_with_slug_fn` for
/// snapshot-stable paths.
///
/// The plan file path is the only write exception in Plan mode — the
/// PermissionChain (not the tool layer) grants this carve-out.
pub struct PlanManager {
    plans_dir: PathBuf,
    slug_fn: Box<dyn Fn() -> String + Send + Sync>,
}

impl PlanManager {
    /// Create a new `PlanManager` with the default petname-based slug generator.
    pub fn new(plans_dir: PathBuf) -> Self {
        Self {
            plans_dir,
            slug_fn: Box::new(|| petname::petname(2, "-").unwrap_or_else(|| "plan".to_string())),
        }
    }

    /// Create a new `PlanManager` with a custom slug generator (for tests).
    pub fn new_with_slug_fn(
        plans_dir: PathBuf,
        slug_fn: Box<dyn Fn() -> String + Send + Sync>,
    ) -> Self {
        Self { plans_dir, slug_fn }
    }

    /// Ensure the plans directory exists (idempotent).
    pub async fn ensure_dir(&self) -> std::io::Result<()> {
        tokio::fs::create_dir_all(&self.plans_dir).await
    }

    /// Resolve or create the plan file for the given session.
    /// If `meta.plan_slug` is `None`, generates a new slug and stores it.
    pub fn plan_file_for(&self, meta: &mut SessionMeta) -> PlanFile {
        let slug = meta.plan_slug.clone().unwrap_or_else(|| {
            let s = (self.slug_fn)();
            meta.plan_slug = Some(s.clone());
            s
        });
        let path = self.plans_dir.join(format!("{slug}.md"));
        PlanFile { slug, path }
    }

    /// Read the plan file contents.
    /// Returns an empty string if the file does not exist.
    pub async fn read_plan(&self, path: &Path) -> Result<String, PlanManagerError> {
        match tokio::fs::read_to_string(path).await {
            Ok(contents) => Ok(contents),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(PlanManagerError::Io(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_meta() -> SessionMeta {
        SessionMeta::new("Test".to_string())
    }

    #[test]
    fn slug_generated_once_per_session() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = PlanManager::new_with_slug_fn(
            tmp.path().to_path_buf(),
            Box::new(|| "test-slug-42".to_string()),
        );
        let mut meta = make_meta();
        let plan1 = manager.plan_file_for(&mut meta);
        assert_eq!(plan1.slug, "test-slug-42");
        assert_eq!(meta.plan_slug, Some("test-slug-42".to_string()));

        let plan2 = manager.plan_file_for(&mut meta);
        assert_eq!(plan2.slug, "test-slug-42");
        assert_eq!(plan2.path, plan1.path);
    }

    #[test]
    fn slug_determinism_under_seed() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = PlanManager::new_with_slug_fn(
            tmp.path().to_path_buf(),
            Box::new(|| "seeded-slug".to_string()),
        );
        let mut meta = make_meta();
        let plan = manager.plan_file_for(&mut meta);
        assert_eq!(plan.slug, "seeded-slug");
    }

    #[tokio::test]
    async fn read_plan_missing_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = PlanManager::new(tmp.path().to_path_buf());
        let path = tmp.path().join("nonexistent.md");
        let contents = manager.read_plan(&path).await.unwrap();
        assert_eq!(contents, "");
    }

    #[tokio::test]
    async fn ensure_dir_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let manager = PlanManager::new(tmp.path().join("plans"));
        manager.ensure_dir().await.unwrap();
        assert!(manager.plans_dir.exists());
        manager.ensure_dir().await.unwrap();
        assert!(manager.plans_dir.exists());
    }
}
