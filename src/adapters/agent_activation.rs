//! AgentActivator: per-conversation active agent state, lazy body load. See Story 5.4.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

#[cfg(test)]
use std::path::Path;

use tokio::sync::RwLock;

use crate::adapters::agent_registry::AgentRegistry;
use crate::domain::models::{ActiveAgent, FileOperation, MAX_AGENT_FILE_SIZE};
use crate::domain::ports::SecurityPort;

#[derive(Debug, Clone)]
pub enum AgentActivationError {
    NotFound(String),
    OutsideWorkspace(PathBuf),
    FileMissing {
        name: String,
        path: PathBuf,
    },
    FileTooLarge {
        name: String,
        #[allow(dead_code)]
        size: u64,
    },
    BodyReadFailed(String),
}

impl std::fmt::Display for AgentActivationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentActivationError::NotFound(name) => {
                write!(f, "Unknown agent: '{}'", name)
            }
            AgentActivationError::OutsideWorkspace(path) => {
                write!(
                    f,
                    "Agent resolves outside the workspace — refusing to load: {}",
                    path.display()
                )
            }
            AgentActivationError::FileMissing { name, path } => {
                write!(
                    f,
                    "Agent '{}' file no longer exists at '{}' — re-scan via /new or restart",
                    name,
                    path.display()
                )
            }
            AgentActivationError::FileTooLarge { name, .. } => {
                write!(f, "Agent '{}' exceeds 1 MiB — refusing to load", name)
            }
            AgentActivationError::BodyReadFailed(msg) => {
                write!(f, "Failed to read agent body: {}", msg)
            }
        }
    }
}

impl std::error::Error for AgentActivationError {}

#[allow(dead_code)]
pub struct AgentActivator {
    active_per_conversation: Arc<RwLock<HashMap<String, ActiveAgent>>>,
    registry: Arc<RwLock<Option<AgentRegistry>>>,
    security: Arc<dyn SecurityPort>,
}

#[allow(dead_code)]
impl AgentActivator {
    pub fn new(security: Arc<dyn SecurityPort>) -> Self {
        Self {
            active_per_conversation: Arc::new(RwLock::new(HashMap::new())),
            registry: Arc::new(RwLock::new(None)),
            security,
        }
    }

    pub async fn set_registry(&self, registry: AgentRegistry) {
        let mut guard = self.registry.write().await;
        *guard = Some(registry);
    }

    pub async fn activate(
        &self,
        conversation_id: &str,
        agent_name: &str,
    ) -> Result<ActiveAgent, AgentActivationError> {
        let registry_guard = self.registry.read().await;
        let def = registry_guard
            .as_ref()
            .and_then(|r| r.find(agent_name))
            .cloned();
        drop(registry_guard);

        let def = def.ok_or_else(|| AgentActivationError::NotFound(agent_name.to_string()))?;

        let file_path = def.file.clone();
        let agent_name = def.name.clone();
        let security = Arc::clone(&self.security);
        let body_result =
            tokio::task::spawn_blocking(move || -> Result<String, AgentActivationError> {
                if !file_path.exists() {
                    return Err(AgentActivationError::FileMissing {
                        name: agent_name.clone(),
                        path: file_path.clone(),
                    });
                }
                let canonical = match std::fs::canonicalize(&file_path) {
                    Ok(c) => c,
                    Err(_) => {
                        return Err(AgentActivationError::FileMissing {
                            name: agent_name.clone(),
                            path: file_path.clone(),
                        });
                    }
                };
                if security
                    .check_workspace_access(&canonical, FileOperation::Read)
                    .is_err()
                {
                    return Err(AgentActivationError::OutsideWorkspace(canonical));
                }
                let metadata = std::fs::metadata(&file_path).map_err(|e| {
                    AgentActivationError::BodyReadFailed(format!("metadata error: {}", e))
                })?;
                if metadata.len() > MAX_AGENT_FILE_SIZE {
                    return Err(AgentActivationError::FileTooLarge {
                        name: agent_name.clone(),
                        size: metadata.len(),
                    });
                }
                let content = std::fs::read_to_string(&file_path).map_err(|e| {
                    AgentActivationError::BodyReadFailed(format!("read error: {}", e))
                })?;
                let body = match crate::domain::services::frontmatter::parse_frontmatter(&content) {
                    Some((_fm, body)) => body.to_string(),
                    None => {
                        tracing::warn!(
                            "Agent '{}' frontmatter parse failed — using empty body",
                            agent_name
                        );
                        String::new()
                    }
                };
                Ok(body)
            })
            .await
            .map_err(|e| AgentActivationError::BodyReadFailed(format!("spawn error: {}", e)))?;
        let body = body_result?;

        let active = ActiveAgent {
            name: def.name.clone(),
            file: def.file.clone(),
            body,
            allowed_tools: def.allowed_tools.clone(),
            exclude_tools: def.exclude_tools.clone(),
            model: def.model.clone(),
        };

        let mut convs = self.active_per_conversation.write().await;
        convs.insert(conversation_id.to_string(), active.clone());

        Ok(active)
    }

    pub async fn deactivate(&self, conversation_id: &str) -> Option<ActiveAgent> {
        let mut convs = self.active_per_conversation.write().await;
        convs.remove(conversation_id)
    }

    pub async fn snapshot(&self, conversation_id: &str) -> Option<ActiveAgent> {
        let convs = self.active_per_conversation.read().await;
        convs.get(conversation_id).cloned()
    }

    pub async fn on_new_conversation(&self, conversation_id: &str) {
        let mut convs = self.active_per_conversation.write().await;
        convs.remove(conversation_id);
    }

    pub async fn active_agent_name(&self, conversation_id: &str) -> Option<String> {
        let convs = self.active_per_conversation.read().await;
        convs.get(conversation_id).map(|a| a.name.clone())
    }

    pub async fn on_tab_closed(&self, conversation_id: &str) {
        let mut convs = self.active_per_conversation.write().await;
        convs.remove(conversation_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::noop::NoOpSecurity;
    use std::io::Write;

    fn write_agent_file(workspace: &Path, name: &str, body: &str) -> PathBuf {
        let agents_dir = workspace.join(".claude").join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        let path = agents_dir.join(format!("{}.md", name));
        let mut f = std::fs::File::create(&path).unwrap();
        write!(
            f,
            "---\nname: {}\ndescription: test agent\n---\n{}",
            name, body
        )
        .unwrap();
        path
    }

    #[tokio::test]
    async fn test_activate_loads_body_lazily() {
        let tmp = tempfile::tempdir().unwrap();
        write_agent_file(tmp.path(), "foo", "# Review everything\n");
        let reg = AgentRegistry::discover(tmp.path());
        let activator = AgentActivator::new(Arc::new(NoOpSecurity));
        activator.set_registry(reg).await;

        let result = activator.activate("conv-1", "foo").await;
        assert!(result.is_ok());
        let active = result.unwrap();
        assert_eq!(active.name, "foo");
        assert!(active.body.contains("Review everything"));
    }

    #[tokio::test]
    async fn test_activate_replaces_prior_agent() {
        let tmp = tempfile::tempdir().unwrap();
        write_agent_file(tmp.path(), "agent-a", "body a\n");
        write_agent_file(tmp.path(), "agent-b", "body b\n");
        let reg = AgentRegistry::discover(tmp.path());
        let activator = AgentActivator::new(Arc::new(NoOpSecurity));
        activator.set_registry(reg).await;

        activator.activate("conv-1", "agent-a").await.unwrap();
        activator.activate("conv-1", "agent-b").await.unwrap();

        let snap = activator.snapshot("conv-1").await.unwrap();
        assert_eq!(snap.name, "agent-b");
    }

    #[tokio::test]
    async fn test_activate_unknown_returns_not_found() {
        let _tmp = tempfile::tempdir().unwrap();
        let activator = AgentActivator::new(Arc::new(NoOpSecurity));
        activator.set_registry(AgentRegistry::new()).await;

        let result = activator.activate("conv-1", "nope").await;
        assert!(matches!(result, Err(AgentActivationError::NotFound(_))));
    }

    #[tokio::test]
    async fn test_activate_file_missing_after_scan() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_agent_file(tmp.path(), "ghost", "body\n");
        let reg = AgentRegistry::discover(tmp.path());
        std::fs::remove_file(&path).unwrap();

        let activator = AgentActivator::new(Arc::new(NoOpSecurity));
        activator.set_registry(reg).await;

        let result = activator.activate("conv-1", "ghost").await;
        assert!(matches!(
            result,
            Err(AgentActivationError::FileMissing { .. })
        ));
    }

    #[tokio::test]
    async fn test_activate_file_too_large_after_scan() {
        let tmp = tempfile::tempdir().unwrap();
        let agents_dir = tmp.path().join(".claude").join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        let path = agents_dir.join("big.md");
        // Write a valid small file for scan
        std::fs::write(
            &path,
            "---\nname: big\ndescription: test\n---\nsmall body\n",
        )
        .unwrap();
        let reg = AgentRegistry::discover(tmp.path());
        // Now grow the file beyond the limit
        std::fs::write(
            &path,
            format!(
                "---\nname: big\ndescription: test\n---\n{}",
                "y".repeat(2_000_000)
            ),
        )
        .unwrap();

        let activator = AgentActivator::new(Arc::new(NoOpSecurity));
        activator.set_registry(reg).await;

        let result = activator.activate("conv-1", "big").await;
        assert!(matches!(
            result,
            Err(AgentActivationError::FileTooLarge { .. })
        ));
    }

    #[tokio::test]
    async fn test_deactivate_clears_state() {
        let tmp = tempfile::tempdir().unwrap();
        write_agent_file(tmp.path(), "foo", "body\n");
        let reg = AgentRegistry::discover(tmp.path());
        let activator = AgentActivator::new(Arc::new(NoOpSecurity));
        activator.set_registry(reg).await;

        activator.activate("conv-1", "foo").await.unwrap();
        let removed = activator.deactivate("conv-1").await;
        assert!(removed.is_some());
        assert!(activator.snapshot("conv-1").await.is_none());
    }

    #[tokio::test]
    async fn test_on_new_conversation_clears_state() {
        let tmp = tempfile::tempdir().unwrap();
        write_agent_file(tmp.path(), "foo", "body\n");
        let reg = AgentRegistry::discover(tmp.path());
        let activator = AgentActivator::new(Arc::new(NoOpSecurity));
        activator.set_registry(reg).await;

        activator.activate("conv-1", "foo").await.unwrap();
        activator.on_new_conversation("conv-1").await;
        assert!(activator.snapshot("conv-1").await.is_none());
    }

    #[tokio::test]
    async fn test_snapshot_returns_clone() {
        let tmp = tempfile::tempdir().unwrap();
        write_agent_file(tmp.path(), "foo", "body\n");
        let reg = AgentRegistry::discover(tmp.path());
        let activator = AgentActivator::new(Arc::new(NoOpSecurity));
        activator.set_registry(reg).await;

        activator.activate("conv-1", "foo").await.unwrap();
        let mut snap = activator.snapshot("conv-1").await.unwrap();
        snap.body = "modified".to_string();

        let original = activator.snapshot("conv-1").await.unwrap();
        assert_ne!(original.body, "modified");
    }

    #[tokio::test]
    async fn test_active_agent_name() {
        let tmp = tempfile::tempdir().unwrap();
        write_agent_file(tmp.path(), "foo", "body\n");
        let reg = AgentRegistry::discover(tmp.path());
        let activator = AgentActivator::new(Arc::new(NoOpSecurity));
        activator.set_registry(reg).await;

        assert!(activator.active_agent_name("conv-1").await.is_none());
        activator.activate("conv-1", "foo").await.unwrap();
        assert_eq!(activator.active_agent_name("conv-1").await.unwrap(), "foo");
    }

    #[test]
    fn test_error_display_formats() {
        assert_eq!(
            AgentActivationError::NotFound("foo".to_string()).to_string(),
            "Unknown agent: 'foo'"
        );
        assert!(
            AgentActivationError::OutsideWorkspace(PathBuf::from("/tmp/x"))
                .to_string()
                .contains("outside the workspace")
        );
        assert!(
            AgentActivationError::FileMissing {
                name: "ghost".to_string(),
                path: PathBuf::from("/tmp/ghost.md")
            }
            .to_string()
            .contains("ghost")
        );
        assert!(
            AgentActivationError::FileTooLarge {
                name: "big".to_string(),
                size: 2_000_000
            }
            .to_string()
            .contains("exceeds 1 MiB")
        );
    }
}
