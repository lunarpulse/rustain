use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::domain::ports::{
    WorkspaceEntry, WorkspaceRegistrarPort, WorkspaceRegistryError, WorkspaceRegistryReaderPort,
};
use crate::infrastructure::{clock_util, paths};

pub const WORKSPACE_REGISTRY_SCHEMA_VERSION: &str = "1.0";
const NOTE_THROTTLE_SECS: i64 = 300;

#[derive(Serialize, Deserialize)]
struct WorkspaceRegistryFile {
    schema_version: String,
    workspaces: Vec<WorkspaceRegistryRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkspaceRegistryRecord {
    path: String,
    last_seen: i64,
}

pub struct FileWorkspaceRegistry {
    /// In-process serialisation + per-workspace throttle state.
    write_state: tokio::sync::Mutex<HashMap<PathBuf, i64>>,
}

impl Default for FileWorkspaceRegistry {
    fn default() -> Self {
        Self::new().expect("workspace registry path resolution should succeed")
    }
}

impl FileWorkspaceRegistry {
    pub fn new() -> Result<Self, WorkspaceRegistryError> {
        let _ = Self::registry_path()?;
        Ok(Self {
            write_state: tokio::sync::Mutex::new(HashMap::new()),
        })
    }

    fn registry_path() -> Result<PathBuf, WorkspaceRegistryError> {
        paths::data_dir()
            .map(|dir| dir.join("workspaces.json"))
            .map_err(|e| WorkspaceRegistryError::Io(e.to_string()))
    }

    fn lock_path(path: &Path) -> PathBuf {
        path.with_extension("json.lock")
    }

    fn normalize_workspace_path(workspace: &Path) -> Result<PathBuf, WorkspaceRegistryError> {
        let absolute = if workspace.is_absolute() {
            workspace.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|e| WorkspaceRegistryError::Io(e.to_string()))?
                .join(workspace)
        };
        Ok(std::fs::canonicalize(&absolute).unwrap_or(absolute))
    }

    fn empty_file() -> WorkspaceRegistryFile {
        WorkspaceRegistryFile {
            schema_version: WORKSPACE_REGISTRY_SCHEMA_VERSION.to_string(),
            workspaces: vec![],
        }
    }

    fn read_registry(path: &Path) -> Result<WorkspaceRegistryFile, WorkspaceRegistryError> {
        match std::fs::read_to_string(path) {
            Ok(contents) => {
                let parsed = serde_json::from_str::<WorkspaceRegistryFile>(&contents)
                    .map_err(|e| WorkspaceRegistryError::Parse(e.to_string()))?;
                if parsed.schema_version != WORKSPACE_REGISTRY_SCHEMA_VERSION {
                    return Err(WorkspaceRegistryError::UnsupportedVersion(
                        parsed.schema_version.clone(),
                    ));
                }
                Ok(parsed)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::empty_file()),
            Err(e) => Err(WorkspaceRegistryError::Io(e.to_string())),
        }
    }

    fn modify_locked<F>(path: &Path, modify: F) -> Result<(), WorkspaceRegistryError>
    where
        F: FnOnce(&mut WorkspaceRegistryFile) -> Result<(), WorkspaceRegistryError>,
    {
        let lock_path = Self::lock_path(path);
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&lock_path)
            .map_err(|e| WorkspaceRegistryError::Io(format!("opening lock file: {e}")))?;

        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let fd = lock_file.as_raw_fd();
            // SAFETY: fd is valid and owned by `lock_file` for the duration of this scope.
            let ret = unsafe { libc::flock(fd, libc::LOCK_EX) };
            if ret != 0 {
                return Err(WorkspaceRegistryError::Locked(format!(
                    "flock(LOCK_EX): {}",
                    std::io::Error::last_os_error()
                )));
            }
        }

        let result = (|| {
            let mut file = Self::read_registry(path)?;
            modify(&mut file)?;
            file.workspaces.sort_by(|a, b| a.path.cmp(&b.path));
            Self::write_inner(path, &file)
        })();

        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let fd = lock_file.as_raw_fd();
            // SAFETY: fd is still valid here.
            unsafe {
                libc::flock(fd, libc::LOCK_UN);
            }
        }

        drop(lock_file);
        result
    }

    fn write_inner(
        path: &Path,
        file: &WorkspaceRegistryFile,
    ) -> Result<(), WorkspaceRegistryError> {
        let tmp_path = path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(file)
            .map_err(|e| WorkspaceRegistryError::Serialize(e.to_string()))?;

        if tmp_path.exists() {
            std::fs::remove_file(&tmp_path)
                .map_err(|e| WorkspaceRegistryError::Io(format!("removing stale tmp: {e}")))?;
        }

        let mut tmp = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
            .map_err(|e| WorkspaceRegistryError::Io(format!("creating tmp file: {e}")))?;

        tmp.write_all(json.as_bytes())
            .map_err(|e| WorkspaceRegistryError::Io(format!("writing tmp file: {e}")))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| WorkspaceRegistryError::Io(format!("chmod 0600: {e}")))?;
        }

        tmp.sync_all()
            .map_err(|e| WorkspaceRegistryError::Io(format!("fsync: {e}")))?;
        drop(tmp);

        if let Err(e) = std::fs::rename(&tmp_path, path) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(WorkspaceRegistryError::Io(format!("rename: {e}")));
        }

        Ok(())
    }
}

#[async_trait]
impl WorkspaceRegistrarPort for FileWorkspaceRegistry {
    async fn note_workspace(&self, workspace: &Path) -> Result<(), WorkspaceRegistryError> {
        let normalized = Self::normalize_workspace_path(workspace)?;
        let now = clock_util::now_unix();
        let path = Self::registry_path()?;

        {
            let write_state = self.write_state.lock().await;
            if write_state
                .get(&normalized)
                .is_some_and(|last| now.saturating_sub(*last) < NOTE_THROTTLE_SECS)
            {
                return Ok(());
            }
        }

        Self::modify_locked(&path, |file| {
            let normalized_string = normalized.to_string_lossy().to_string();
            if let Some(existing) = file
                .workspaces
                .iter_mut()
                .find(|entry| entry.path == normalized_string)
            {
                existing.last_seen = now;
            } else {
                file.workspaces.push(WorkspaceRegistryRecord {
                    path: normalized_string,
                    last_seen: now,
                });
            }
            Ok(())
        })?;

        {
            let mut write_state = self.write_state.lock().await;
            write_state.insert(normalized, now);
        }
        Ok(())
    }
}

#[async_trait]
impl WorkspaceRegistryReaderPort for FileWorkspaceRegistry {
    async fn live_workspaces(&self) -> Result<Vec<WorkspaceEntry>, WorkspaceRegistryError> {
        let path = Self::registry_path()?;
        let file = match Self::read_registry(&path) {
            Ok(file) => file,
            Err(WorkspaceRegistryError::Parse(err)) => {
                tracing::warn!("workspace registry unreadable; ignoring contents: {err}");
                return Ok(vec![]);
            }
            Err(WorkspaceRegistryError::UnsupportedVersion(version)) => {
                tracing::warn!(
                    "workspace registry schema {version} unsupported; ignoring contents"
                );
                return Ok(vec![]);
            }
            Err(err) => return Err(err),
        };

        let mut live = Vec::with_capacity(file.workspaces.len());
        for entry in file.workspaces {
            let path = PathBuf::from(entry.path);
            let sessions_dir = path.join(".claude").join("sessions");
            let is_live = tokio::fs::metadata(&sessions_dir)
                .await
                .map(|meta| meta.is_dir())
                .unwrap_or(false);
            if is_live {
                live.push(WorkspaceEntry {
                    path,
                    last_seen: entry.last_seen,
                });
            }
        }

        live.sort_by(|a, b| {
            b.last_seen
                .cmp(&a.last_seen)
                .then_with(|| a.path.cmp(&b.path))
        });
        Ok(live)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serial_test::serial;

    use super::*;

    fn read_registry_file(path: &Path) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[tokio::test]
    #[serial(workspace_registry)]
    async fn note_workspace_writes_minimal_schema_and_0600() {
        let data_dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("RUSTAIN_DATA_DIR", data_dir.path());
        }
        let registry = FileWorkspaceRegistry::new().unwrap();

        registry.note_workspace(workspace.path()).await.unwrap();

        let path = FileWorkspaceRegistry::registry_path().unwrap();
        let json = read_registry_file(&path);
        assert_eq!(json["schema_version"].as_str().unwrap(), "1.0");
        let workspaces = json["workspaces"].as_array().unwrap();
        assert_eq!(workspaces.len(), 1);
        let row = &workspaces[0];
        assert!(row.get("path").is_some());
        assert!(row.get("last_seen").is_some());
        assert_eq!(row.as_object().unwrap().len(), 2);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[tokio::test]
    #[serial(workspace_registry)]
    async fn note_workspace_throttles_same_workspace() {
        let data_dir = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("RUSTAIN_DATA_DIR", data_dir.path());
        }
        let registry = FileWorkspaceRegistry::new().unwrap();

        registry.note_workspace(workspace.path()).await.unwrap();
        let path = FileWorkspaceRegistry::registry_path().unwrap();
        let before = std::fs::read_to_string(&path).unwrap();
        let before_mtime = std::fs::metadata(&path).unwrap().modified().unwrap();

        registry.note_workspace(workspace.path()).await.unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        let after_mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(before, after);
        assert_eq!(before_mtime, after_mtime);
    }

    #[tokio::test]
    #[serial(workspace_registry)]
    async fn live_workspaces_omits_dead_entries_without_rewriting() {
        let data_dir = tempfile::tempdir().unwrap();
        let live_workspace = tempfile::tempdir().unwrap();
        let dead_workspace = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("RUSTAIN_DATA_DIR", data_dir.path());
        }
        tokio::fs::create_dir_all(live_workspace.path().join(".claude").join("sessions"))
            .await
            .unwrap();

        let path = FileWorkspaceRegistry::registry_path().unwrap();
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&WorkspaceRegistryFile {
                schema_version: WORKSPACE_REGISTRY_SCHEMA_VERSION.to_string(),
                workspaces: vec![
                    WorkspaceRegistryRecord {
                        path: live_workspace.path().to_string_lossy().to_string(),
                        last_seen: 20,
                    },
                    WorkspaceRegistryRecord {
                        path: dead_workspace.path().to_string_lossy().to_string(),
                        last_seen: 10,
                    },
                ],
            })
            .unwrap(),
        )
        .unwrap();
        let before = std::fs::read_to_string(&path).unwrap();
        let before_mtime = std::fs::metadata(&path).unwrap().modified().unwrap();

        let registry = FileWorkspaceRegistry::new().unwrap();
        let live = registry.live_workspaces().await.unwrap();

        assert_eq!(live.len(), 1);
        assert_eq!(live[0].path, live_workspace.path());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before_mtime
        );
    }

    #[tokio::test]
    #[serial(workspace_registry)]
    async fn live_workspaces_gracefully_ignores_corrupt_or_newer_registry() {
        let data_dir = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("RUSTAIN_DATA_DIR", data_dir.path());
        }
        let path = FileWorkspaceRegistry::registry_path().unwrap();
        let registry = FileWorkspaceRegistry::new().unwrap();

        std::fs::write(&path, "not-json").unwrap();
        assert!(registry.live_workspaces().await.unwrap().is_empty());

        std::fs::write(
            &path,
            serde_json::json!({
                "schema_version": "99.0",
                "workspaces": []
            })
            .to_string(),
        )
        .unwrap();
        assert!(registry.live_workspaces().await.unwrap().is_empty());
    }

    #[tokio::test]
    #[serial(workspace_registry)]
    async fn note_workspace_save_save_contention_keeps_both_entries() {
        let data_dir = tempfile::tempdir().unwrap();
        let workspace_a = tempfile::tempdir().unwrap();
        let workspace_b = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("RUSTAIN_DATA_DIR", data_dir.path());
        }

        let registry = Arc::new(FileWorkspaceRegistry::new().unwrap());
        let a = registry.clone();
        let b = registry.clone();
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let barrier_a = barrier.clone();
        let barrier_b = barrier.clone();

        let task_a = tokio::spawn(async move {
            barrier_a.wait().await;
            a.note_workspace(workspace_a.path()).await.unwrap();
        });
        let task_b = tokio::spawn(async move {
            barrier_b.wait().await;
            b.note_workspace(workspace_b.path()).await.unwrap();
        });

        task_a.await.unwrap();
        task_b.await.unwrap();

        let registry_path = FileWorkspaceRegistry::registry_path().unwrap();
        let json = read_registry_file(&registry_path);
        let workspaces = json["workspaces"].as_array().unwrap();
        assert_eq!(workspaces.len(), 2);
    }
}
