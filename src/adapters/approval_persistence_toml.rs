//! TOML-based persistence adapter for approval rules.
//!
//! Writes to:
//! - `~/.rustain/config.toml` for tool/server scopes
//! - `{workspace}/.rustain/permissions.toml` for path scopes

use std::path::PathBuf;
use tokio::sync::Mutex;

use async_trait::async_trait;

use crate::domain::errors::ApprovalPersistenceError;
use crate::domain::models::ApprovalScope;
use crate::domain::ports::ApprovalPersistencePort;
use crate::domain::services::approval_runtime::SessionApprovalSet;

/// TOML persistence adapter with per-target-file locking.
pub struct ApprovalPersistenceToml {
    user_config_path: PathBuf,
    workspace_rules_path: PathBuf,
    user_lock: Mutex<()>,
    workspace_lock: Mutex<()>,
}

impl ApprovalPersistenceToml {
    pub fn new(user_config_path: PathBuf, workspace_rules_path: PathBuf) -> Self {
        Self {
            user_config_path,
            workspace_rules_path,
            user_lock: Mutex::new(()),
            workspace_lock: Mutex::new(()),
        }
    }
}

#[async_trait]
impl ApprovalPersistencePort for ApprovalPersistenceToml {
    async fn load(&self) -> Result<SessionApprovalSet, ApprovalPersistenceError> {
        let mut set = SessionApprovalSet::default();

        match tokio::fs::read_to_string(&self.user_config_path).await {
            Ok(content) => match content.parse::<toml::Table>() {
                Ok(table) => {
                    if let Some(permissions) = table.get("permissions").and_then(|v| v.as_table()) {
                        if let Some(tools) =
                            permissions.get("always_tools").and_then(|v| v.as_array())
                        {
                            for t in tools {
                                if let Some(s) = t.as_str() {
                                    set.always_tools.insert(s.to_string());
                                }
                            }
                        }
                        if let Some(servers) =
                            permissions.get("always_servers").and_then(|v| v.as_array())
                        {
                            for s in servers {
                                if let Some(s) = s.as_str() {
                                    set.always_servers.insert(s.to_string());
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Malformed user config TOML: {}", e);
                }
            },
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                tracing::warn!(
                    "Cannot read user config ({}): {}",
                    self.user_config_path.display(),
                    e
                );
            }
            Err(_) => {}
        }

        match tokio::fs::read_to_string(&self.workspace_rules_path).await {
            Ok(content) => match content.parse::<toml::Table>() {
                Ok(table) => {
                    if let Some(rules) = table.get("rules").and_then(|v| v.as_array()) {
                        for r in rules {
                            if let Some(rule) = r.as_table() {
                                if let Some(pattern) = rule.get("pattern").and_then(|v| v.as_str())
                                {
                                    if let Some(action) =
                                        rule.get("action").and_then(|v| v.as_str())
                                    {
                                        if action == "allow" {
                                            set.always_paths.push(pattern.to_string());
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Malformed workspace permissions TOML: {}", e);
                }
            },
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                tracing::warn!(
                    "Cannot read workspace rules ({}): {}",
                    self.workspace_rules_path.display(),
                    e
                );
            }
            Err(_) => {}
        }

        Ok(set)
    }

    async fn save(&self, scope: ApprovalScope) -> Result<(), ApprovalPersistenceError> {
        match scope {
            ApprovalScope::Tool(_) | ApprovalScope::Server(_) => {
                let _guard = self.user_lock.lock().await;
                let path = &self.user_config_path;
                let parent = path.parent().ok_or_else(|| {
                    ApprovalPersistenceError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "config path has no parent directory",
                    ))
                })?;
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(ApprovalPersistenceError::Io)?;

                let mut table = match tokio::fs::read_to_string(path).await {
                    Ok(content) => content.parse::<toml::Table>().map_err(|e| {
                        tracing::warn!("Existing config parse failed, aborting save: {}", e);
                        ApprovalPersistenceError::Toml(e)
                    })?,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
                    Err(e) => return Err(ApprovalPersistenceError::Io(e)),
                };

                let permissions = table
                    .entry("permissions")
                    .or_insert_with(|| toml::Value::Table(toml::Table::new()));
                let perms_table = permissions.as_table_mut().unwrap();

                match scope {
                    ApprovalScope::Tool(tool_name) => {
                        let tools = perms_table
                            .entry("always_tools")
                            .or_insert_with(|| toml::Value::Array(toml::value::Array::new()));
                        let arr = tools.as_array_mut().unwrap();
                        if !arr.iter().any(|v| v.as_str() == Some(&tool_name)) {
                            arr.push(toml::Value::String(tool_name));
                        }
                    }
                    ApprovalScope::Server(server_id) => {
                        let servers = perms_table
                            .entry("always_servers")
                            .or_insert_with(|| toml::Value::Array(toml::value::Array::new()));
                        let arr = servers.as_array_mut().unwrap();
                        if !arr.iter().any(|v| v.as_str() == Some(&server_id)) {
                            arr.push(toml::Value::String(server_id));
                        }
                    }
                    _ => unreachable!(),
                }

                let content = toml::to_string_pretty(&table)?;
                let temp = tempfile::NamedTempFile::new_in(parent)?;
                tokio::fs::write(temp.path(), content)
                    .await
                    .map_err(ApprovalPersistenceError::Io)?;
                temp.persist(path)
                    .map_err(|e| ApprovalPersistenceError::Io(e.error))?;
                Ok(())
            }
            ApprovalScope::PathPrefix(pattern) => {
                let _guard = self.workspace_lock.lock().await;
                let path = &self.workspace_rules_path;
                let parent = path.parent().ok_or_else(|| {
                    ApprovalPersistenceError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "rules path has no parent directory",
                    ))
                })?;
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(ApprovalPersistenceError::Io)?;

                let mut table = match tokio::fs::read_to_string(path).await {
                    Ok(content) => content.parse::<toml::Table>().map_err(|e| {
                        tracing::warn!("Existing rules parse failed, aborting save: {}", e);
                        ApprovalPersistenceError::Toml(e)
                    })?,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
                    Err(e) => return Err(ApprovalPersistenceError::Io(e)),
                };

                let rules = table
                    .entry("rules")
                    .or_insert_with(|| toml::Value::Array(toml::value::Array::new()));
                let arr = rules.as_array_mut().unwrap();
                let already_exists = arr.iter().any(|v| {
                    v.as_table()
                        .map(|t| {
                            t.get("pattern").and_then(|v| v.as_str()) == Some(&pattern)
                                && t.get("scope").and_then(|v| v.as_str()) == Some("path")
                        })
                        .unwrap_or(false)
                });
                if !already_exists {
                    let new_rule = toml::Table::from_iter([
                        ("pattern".to_string(), toml::Value::String(pattern)),
                        (
                            "action".to_string(),
                            toml::Value::String("allow".to_string()),
                        ),
                        ("scope".to_string(), toml::Value::String("path".to_string())),
                    ]);
                    arr.push(toml::Value::Table(new_rule));
                }

                let content = toml::to_string_pretty(&table)?;
                let temp = tempfile::NamedTempFile::new_in(parent)?;
                tokio::fs::write(temp.path(), content)
                    .await
                    .map_err(ApprovalPersistenceError::Io)?;
                temp.persist(path)
                    .map_err(|e| ApprovalPersistenceError::Io(e.error))?;
                Ok(())
            }
        }
    }
}
