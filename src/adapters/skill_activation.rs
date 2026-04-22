//! Skill activation engine — adapter-layer orchestration.
//!
//! Manages Tier 2 body loading, depth guarding, and per-conversation
//! active-skill state. Trust gating is orchestrated by the event loop
//! (which has access to the TUI for user prompts).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{RwLock, mpsc, oneshot};

use crate::adapters::skill_registry::SkillRegistry;
use crate::domain::events::AppEvent;
use crate::domain::models::{
    ActiveSkill, ConversationId, MAX_SKILL_ACTIVATION_DEPTH, MAX_SKILL_FILE_SIZE,
    SkillActivationError, SkillActivationOutcome, SkillActivationSet, SkillDef, SkillSource,
    SkillTrustResponse,
};

#[allow(dead_code)]
pub struct SkillActivator {
    registry: Arc<RwLock<SkillRegistry>>,
    conversation_sets: Arc<RwLock<HashMap<ConversationId, SkillActivationSet>>>,
    /// Event sender used for model-driven trust prompts — when set, `activate_by_name`
    /// emits `AppEvent::SkillTrustPrompt` + awaits the oneshot response before activating
    /// a workspace-tier skill that is not yet trusted for the conversation (Story 5-2 AC9).
    /// When None (e.g., unit-test fixtures), workspace-tier activation proceeds without
    /// prompting — tests that need gating must use the user-driven event-loop path.
    event_tx: RwLock<Option<mpsc::UnboundedSender<AppEvent>>>,
}

impl Default for SkillActivator {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(dead_code)]
impl SkillActivator {
    pub fn new() -> Self {
        Self {
            registry: Arc::new(RwLock::new(SkillRegistry::new())),
            conversation_sets: Arc::new(RwLock::new(HashMap::new())),
            event_tx: RwLock::new(None),
        }
    }

    pub async fn set_event_tx(&self, tx: mpsc::UnboundedSender<AppEvent>) {
        *self.event_tx.write().await = Some(tx);
    }

    pub async fn set_registry(&self, registry: SkillRegistry) {
        let mut guard = self.registry.write().await;
        *guard = registry;
    }

    pub async fn lookup_skill(&self, name: &str) -> Option<SkillDef> {
        let guard = self.registry.read().await;
        guard.find(name).cloned()
    }

    pub async fn is_skill_disabled(&self, name: &str) -> bool {
        let guard = self.registry.read().await;
        guard.find(name).is_none()
            && guard
                .all_including_disabled()
                .iter()
                .any(|s| s.name == name)
    }

    /// Return the names of all discovered (enabled) skills, sorted alphabetically —
    /// used to compose the `Discovered skills: [...]` hint on AC9 NotFound errors.
    pub async fn discovered_skill_names(&self) -> Vec<String> {
        let guard = self.registry.read().await;
        let mut names: Vec<String> = guard.skills().iter().map(|s| s.name.clone()).collect();
        names.sort();
        names
    }

    pub async fn activate(
        &self,
        def: &SkillDef,
        arguments: String,
        conversation_id: &str,
        caller_depth: u8,
    ) -> Result<ActiveSkill, SkillActivationError> {
        let new_depth = caller_depth + 1;
        if new_depth > MAX_SKILL_ACTIVATION_DEPTH {
            return Err(SkillActivationError::DepthExceeded {
                depth: new_depth,
                max: MAX_SKILL_ACTIVATION_DEPTH,
            });
        }

        let file_path = def.file.clone();
        let skill_name = def.name.clone();
        let read_result =
            tokio::task::spawn_blocking(move || -> Result<String, SkillActivationError> {
                if !file_path.exists() {
                    return Err(SkillActivationError::FileMissing {
                        name: skill_name.clone(),
                        path: file_path.clone(),
                    });
                }
                let metadata = std::fs::metadata(&file_path).map_err(|e| {
                    SkillActivationError::BodyReadFailed(format!("metadata error: {}", e))
                })?;
                if metadata.len() > MAX_SKILL_FILE_SIZE {
                    return Err(SkillActivationError::FileTooLarge {
                        name: skill_name.clone(),
                        size: metadata.len(),
                    });
                }
                read_body_after_frontmatter(&file_path)
                    .map_err(|e| SkillActivationError::BodyReadFailed(e.to_string()))
            })
            .await
            .map_err(|e| SkillActivationError::BodyReadFailed(format!("spawn error: {}", e)))?;
        let body = read_result?;

        let active_skill = ActiveSkill {
            name: def.name.clone(),
            directory: def.directory.clone(),
            allowed_tools: def.allowed_tools.clone(),
            body,
            arguments,
            activation_depth: new_depth,
            source: def.source,
        };

        let mut sets = self.conversation_sets.write().await;
        let set = sets
            .entry(conversation_id.to_string())
            .or_insert_with(SkillActivationSet::new);
        set.push(active_skill.clone());

        Ok(active_skill)
    }

    /// Trust-gated activation.
    ///
    /// Returns `Ok(Activated(_))` on success, `Ok(TrustDeclined(name))` when the
    /// user declines the trust prompt (Decision 4 — a user choice, not an
    /// error), and `Err(_)` only for mechanical errors.
    pub async fn activate_by_name(
        &self,
        name: &str,
        arguments: String,
        conversation_id: &str,
        caller_depth: u8,
    ) -> Result<SkillActivationOutcome, SkillActivationError> {
        let guard = self.registry.read().await;
        let def = guard.find(name).cloned();
        let is_disabled = if def.is_none() {
            guard
                .all_including_disabled()
                .iter()
                .any(|s| s.name == name)
        } else {
            false
        };
        drop(guard);
        if is_disabled {
            return Err(SkillActivationError::Disabled(name.to_string()));
        }
        let def = def.ok_or_else(|| SkillActivationError::NotFound(name.to_string()))?;

        // Workspace-tier skills require trust before model-driven activation (AC9 + NFR17).
        // `GlobalAgents` is trusted by default; `trusted_in_session` skips prompts for
        // already-accepted skills. When `event_tx` is unset (unit-test fixtures) we proceed
        // without prompting — gating must be tested via the event-loop integration path.
        if def.source != SkillSource::GlobalAgents
            && !self.is_trusted(conversation_id, &def.file).await
        {
            let tx_guard = self.event_tx.read().await;
            if let Some(tx) = tx_guard.as_ref().cloned() {
                drop(tx_guard);
                let (resp_tx, resp_rx) = oneshot::channel::<SkillTrustResponse>();
                tx.send(AppEvent::SkillTrustPrompt {
                    skill_name: name.to_string(),
                    skill_file: def.file.clone(),
                    response_tx: resp_tx,
                })
                .map_err(|_| {
                    SkillActivationError::BodyReadFailed("trust-prompt channel closed".to_string())
                })?;
                match resp_rx.await {
                    Ok(SkillTrustResponse::Accepted) => {
                        self.mark_trusted(conversation_id, def.file.clone()).await;
                    }
                    Ok(SkillTrustResponse::Declined) | Err(_) => {
                        return Ok(SkillActivationOutcome::TrustDeclined(name.to_string()));
                    }
                }
            }
        }

        let skill = self
            .activate(&def, arguments, conversation_id, caller_depth)
            .await?;
        Ok(SkillActivationOutcome::Activated(skill))
    }

    pub async fn deactivate(&self, conversation_id: &str, name: &str) -> Option<ActiveSkill> {
        let mut sets = self.conversation_sets.write().await;
        if let Some(set) = sets.get_mut(conversation_id) {
            set.deactivate(name)
        } else {
            None
        }
    }

    pub async fn deactivate_all(&self, conversation_id: &str) -> Vec<ActiveSkill> {
        let mut sets = self.conversation_sets.write().await;
        if let Some(set) = sets.get_mut(conversation_id) {
            set.deactivate_all()
        } else {
            Vec::new()
        }
    }

    pub async fn on_new_conversation(&self, conversation_id: &str) {
        let mut sets = self.conversation_sets.write().await;
        sets.entry(conversation_id.to_string())
            .or_insert_with(SkillActivationSet::new);
    }

    pub async fn snapshot_for_turn(&self, conversation_id: &str) -> Option<SkillActivationSet> {
        let sets = self.conversation_sets.read().await;
        sets.get(conversation_id).cloned()
    }

    pub async fn active_skill_dirs(&self, conversation_id: &str) -> Vec<PathBuf> {
        let sets = self.conversation_sets.read().await;
        if let Some(set) = sets.get(conversation_id) {
            set.active_skills()
                .iter()
                .map(|s| s.directory.clone())
                .collect()
        } else {
            Vec::new()
        }
    }

    /// Record a trust decision keyed on the skill file path (Group 1 Decision 1).
    /// Keying on path prevents a workspace-tier skill from inheriting trust
    /// granted to a global-tier skill of the same name (or vice-versa).
    pub async fn mark_trusted(&self, conversation_id: &str, skill_file: PathBuf) {
        let mut sets = self.conversation_sets.write().await;
        let set = sets
            .entry(conversation_id.to_string())
            .or_insert_with(SkillActivationSet::new);
        set.mark_trusted(skill_file);
    }

    pub async fn is_trusted(&self, conversation_id: &str, skill_file: &Path) -> bool {
        let sets = self.conversation_sets.read().await;
        if let Some(set) = sets.get(conversation_id) {
            set.is_trusted(skill_file)
        } else {
            false
        }
    }

    pub async fn active_count(&self, conversation_id: &str) -> usize {
        let sets = self.conversation_sets.read().await;
        sets.get(conversation_id)
            .map(|s| s.active_skills().len())
            .unwrap_or(0)
    }
}

/// Read the markdown body after the closing frontmatter delimiter.
/// Mirrors `read_frontmatter_only` in skill_registry.rs but returns
/// the content *after* the second `---`.
pub fn read_body_after_frontmatter(path: &Path) -> std::io::Result<String> {
    use std::io::{BufRead, BufReader};
    let file = std::fs::File::open(path)?;
    let reader = BufReader::new(file);
    let lines = reader.lines();
    let mut found_opening = false;
    let mut found_closing = false;
    let mut body_lines: Vec<String> = Vec::new();

    for line in lines {
        let line = line?;
        let trimmed = line.trim_end_matches('\r').trim_end_matches('\n');
        if !found_opening {
            if trimmed == "---" {
                found_opening = true;
            }
            continue;
        }
        if !found_closing {
            if trimmed == "---" {
                found_closing = true;
            }
            continue;
        }
        let normalized = line.trim_end_matches('\r');
        body_lines.push(normalized.to_string());
    }

    Ok(body_lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_skill_file(dir: &Path, name: &str, body: &str) -> PathBuf {
        let skill_dir = dir.join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        let file = skill_dir.join("SKILL.md");
        let mut f = std::fs::File::create(&file).unwrap();
        write!(f, "---\nname: {}\ndescription: test\n---\n{}", name, body).unwrap();
        file
    }

    #[test]
    fn test_read_body_after_frontmatter() {
        let tmp = tempfile::TempDir::new().unwrap();
        let file = tmp.path().join("test.md");
        std::fs::write(
            &file,
            "---\nname: foo\ndescription: bar\n---\n\n# Body\nHello world\n",
        )
        .unwrap();
        let body = read_body_after_frontmatter(&file).unwrap();
        assert!(body.contains("# Body"));
        assert!(body.contains("Hello world"));
        assert!(!body.contains("name: foo"));
    }

    #[test]
    fn test_read_body_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let file = tmp.path().join("test.md");
        std::fs::write(&file, "---\nname: foo\n---\n").unwrap();
        let body = read_body_after_frontmatter(&file).unwrap();
        assert!(body.is_empty());
    }

    #[tokio::test]
    async fn test_activate_file_missing() {
        let activator = SkillActivator::new();
        let def = crate::domain::models::SkillDef {
            name: "ghost".to_string(),
            description: "test".to_string(),
            file: PathBuf::from("/nonexistent/SKILL.md"),
            directory: PathBuf::from("/nonexistent"),
            source: crate::domain::models::SkillSource::WorkspaceAgents,
            allowed_tools: None,
        };
        let result = activator.activate(&def, String::new(), "conv-1", 0).await;
        assert!(matches!(
            result,
            Err(SkillActivationError::FileMissing { .. })
        ));
    }

    #[tokio::test]
    async fn test_activate_depth_exceeded() {
        const _: () = assert!(MAX_SKILL_ACTIVATION_DEPTH == 3);
    }

    #[tokio::test]
    async fn test_deactivate_unknown_returns_none() {
        let activator = SkillActivator::new();
        activator.on_new_conversation("conv-1").await;
        let result = activator.deactivate("conv-1", "nope").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_deactivate_all_empty() {
        let activator = SkillActivator::new();
        activator.on_new_conversation("conv-1").await;
        let result = activator.deactivate_all("conv-1").await;
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_snapshot_for_nonexistent_conversation() {
        let activator = SkillActivator::new();
        let result = activator.snapshot_for_turn("nope").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_per_conversation_isolation() {
        let activator = SkillActivator::new();
        activator.on_new_conversation("conv-a").await;
        activator.on_new_conversation("conv-b").await;

        let snap_a = activator.snapshot_for_turn("conv-a").await;
        let snap_b = activator.snapshot_for_turn("conv-b").await;
        assert!(snap_a.unwrap().is_empty());
        assert!(snap_b.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_active_count_empty() {
        let activator = SkillActivator::new();
        activator.on_new_conversation("conv-1").await;
        assert_eq!(activator.active_count("conv-1").await, 0);
    }

    #[tokio::test]
    async fn test_mark_and_check_trusted() {
        let activator = SkillActivator::new();
        activator.on_new_conversation("conv-1").await;
        let path = PathBuf::from("/tmp/my-skill/SKILL.md");
        assert!(!activator.is_trusted("conv-1", &path).await);
        activator.mark_trusted("conv-1", path.clone()).await;
        assert!(activator.is_trusted("conv-1", &path).await);
        // Different path with same basename does NOT inherit trust.
        let other = PathBuf::from("/etc/other/my-skill/SKILL.md");
        assert!(!activator.is_trusted("conv-1", &other).await);
    }

    #[tokio::test]
    async fn test_activate_inherits_caller_depth() {
        // When the model invokes `activate_skill` from within an already-active
        // skill at depth N, the new activation must land at depth N+1 (Story 5-2 AC9).
        // MAX depth = 3, so depth-3 caller should trip DepthExceeded on next call.
        let tmp = tempfile::TempDir::new().unwrap();
        let file = write_skill_file(tmp.path(), "deep", "# Body\n");
        let activator = SkillActivator::new();
        let def = crate::domain::models::SkillDef {
            name: "deep".to_string(),
            description: "test".to_string(),
            file,
            directory: tmp.path().join("deep"),
            source: crate::domain::models::SkillSource::GlobalAgents,
            allowed_tools: None,
        };

        // caller_depth=0 → new skill at depth 1
        let r1 = activator
            .activate(&def, String::new(), "c1", 0)
            .await
            .unwrap();
        assert_eq!(r1.activation_depth, 1);

        // caller_depth=2 → new skill at depth 3 (at the cap)
        let r2 = activator
            .activate(&def, String::new(), "c1", 2)
            .await
            .unwrap();
        assert_eq!(r2.activation_depth, 3);

        // caller_depth=3 → would be depth 4, exceeds cap
        let r3 = activator.activate(&def, String::new(), "c1", 3).await;
        assert!(matches!(
            r3,
            Err(SkillActivationError::DepthExceeded { depth: 4, max: 3 })
        ));
    }

    #[tokio::test]
    async fn test_activate_with_real_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let file = write_skill_file(tmp.path(), "test-skill", "# Test Body\n");
        let activator = SkillActivator::new();
        activator.on_new_conversation("conv-1").await;
        let def = crate::domain::models::SkillDef {
            name: "test-skill".to_string(),
            description: "test".to_string(),
            file,
            directory: tmp.path().join("test-skill"),
            source: crate::domain::models::SkillSource::GlobalAgents,
            allowed_tools: None,
        };
        let result = activator.activate(&def, String::new(), "conv-1", 0).await;
        assert!(result.is_ok());
        let active = result.unwrap();
        assert_eq!(active.name, "test-skill");
        assert!(active.body.contains("# Test Body"));
        assert_eq!(activator.active_count("conv-1").await, 1);
    }
}
