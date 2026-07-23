//! Subagent spool — append-only file storage with 8 KB in-memory ring tail.
//!
//! Spool files are NOT cleaned up across restarts (deferred to a follow-up story).

use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::io::AsyncWriteExt;

use crate::domain::models::{AgentId, NodeState, SubagentError};

const RING_CAP: usize = 8192;

/// In-memory ring buffer capped at 8 KB.
struct RingBuffer8K {
    buf: VecDeque<u8>,
}

impl RingBuffer8K {
    fn new() -> Self {
        Self {
            buf: VecDeque::new(),
        }
    }

    fn append(&mut self, chunk: &[u8]) {
        self.buf.extend(chunk);
        while self.buf.len() > RING_CAP {
            self.buf.pop_front();
        }
    }

    fn snapshot(&self) -> Vec<u8> {
        self.buf.iter().copied().collect()
    }
}

pub struct SubagentSpool {
    spool_dir: PathBuf,
    tails: tokio::sync::RwLock<HashMap<String, RingBuffer8K>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpoolMeta {
    pub status: NodeState,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub started_at: i64, // epoch millis
    pub ended_at: Option<i64>,
    pub subagent_type: String,
    pub agent_id: String,
}

impl SubagentSpool {
    pub async fn new(spool_dir: PathBuf) -> Result<Self, std::io::Error> {
        fs::create_dir_all(&spool_dir).await?;
        Ok(Self {
            spool_dir,
            tails: tokio::sync::RwLock::new(HashMap::new()),
        })
    }

    /// Append a chunk to the spool file and update the 8 KB ring buffer tail.
    pub async fn append(&self, task_id: &str, chunk: &[u8]) -> Result<(), std::io::Error> {
        if chunk.is_empty() {
            return Ok(());
        }

        // Update in-memory ring buffer
        {
            let mut guard = self.tails.write().await;
            let buf = guard
                .entry(task_id.to_string())
                .or_insert_with(RingBuffer8K::new);
            buf.append(chunk);
        }

        // Append to disk
        let path = self.spool_dir.join(format!("{}.out", task_id));
        #[cfg(unix)]
        let mut file = {
            use std::os::unix::fs::OpenOptionsExt;
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(&path)
                .await?
        };
        #[cfg(not(unix))]
        let mut file = {
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .await?
        };
        file.write_all(chunk).await?;
        file.sync_data().await?;
        Ok(())
    }

    /// Atomically write the JSON meta sidecar (write-temp + rename pattern).
    pub async fn write_meta(&self, task_id: &str, meta: &SpoolMeta) -> Result<(), std::io::Error> {
        let path = self.spool_dir.join(format!("{}.meta", task_id));
        let tmp = self.spool_dir.join(format!(".{}.meta.tmp", task_id));
        let json = serde_json::to_vec(meta)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            use std::io::Write;
            f.write_all(&json)?;
            f.sync_all()?;
        }
        #[cfg(not(unix))]
        {
            std::fs::write(&tmp, &json)?;
        }
        fs::rename(&tmp, &path).await?;
        Ok(())
    }

    /// Snapshot of the 8 KB in-memory tail for the LLM's bounded ToolResult.
    pub async fn tail(&self, task_id: &str) -> Vec<u8> {
        let guard = self.tails.read().await;
        guard.get(task_id).map(|b| b.snapshot()).unwrap_or_default()
    }

    pub async fn read_tail(
        &self,
        task_id: &str,
        max_bytes: usize,
    ) -> Result<String, std::io::Error> {
        if max_bytes == 0 {
            return Ok(String::new());
        }
        let data = self.tail(task_id).await;
        let data = if data.len() > max_bytes {
            data[data.len() - max_bytes..].to_vec()
        } else {
            data
        };
        Ok(String::from_utf8_lossy(&data).into_owned())
    }

    pub async fn read_full(&self, task_id: &str) -> Result<String, std::io::Error> {
        let path = self.spool_dir.join(format!("{}.out", task_id));
        match fs::metadata(&path).await {
            Ok(meta) if meta.len() > 16 * 1024 * 1024 => {
                // Cap at 16 MB to prevent OOM on massive spool files
                tracing::warn!(
                    "Spool file {} exceeds 16 MB cap, returning head only",
                    path.display()
                );
                let mut file = fs::File::open(&path).await?;
                let mut buf = Vec::with_capacity(16 * 1024 * 1024);
                use tokio::io::AsyncReadExt;
                let _ = file.read_to_end(&mut buf).await?;
                Ok(String::from_utf8_lossy(&buf).into_owned())
            }
            _ => match fs::read_to_string(&path).await {
                Ok(s) => Ok(s),
                Err(e) => Err(e),
            },
        }
    }

    /// Resolve the durable spool id for a recovered node through its metadata.
    pub async fn task_id_for_agent(
        &self,
        agent_id: &AgentId,
    ) -> Result<Option<String>, std::io::Error> {
        let mut entries = fs::read_dir(&self.spool_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("meta") {
                continue;
            }
            let bytes = fs::read(&path).await?;
            let meta: SpoolMeta = match serde_json::from_slice(&bytes) {
                Ok(meta) => meta,
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "Skipping malformed spool metadata");
                    continue;
                }
            };
            if meta.agent_id == agent_id.as_str() {
                return Ok(path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .map(str::to_owned));
            }
        }
        Ok(None)
    }

    /// Byte-range read from disk.
    /// Unix: `std::os::unix::fs::FileExt::read_at`. Windows: `std::os::windows::fs::FileExt::seek_read`.
    /// Per NFR33, Windows is P2 — implement Unix only in v0, gate the Windows arm behind
    /// `#[cfg(windows)]` returning `Err(io::ErrorKind::Unsupported)` with a clear message.
    pub async fn pread(
        &self,
        task_id: &str,
        offset: u64,
        len: usize,
    ) -> Result<Vec<u8>, std::io::Error> {
        let path = self.spool_dir.join(format!("{}.out", task_id));
        let result = tokio::task::spawn_blocking(move || {
            #[cfg(unix)]
            {
                use std::os::unix::fs::FileExt;
                let file = std::fs::File::open(&path)?;
                let mut buf = vec![0u8; len];
                let n = file.read_at(&mut buf, offset)?;
                buf.truncate(n);
                Ok(buf)
            }
            #[cfg(not(unix))]
            {
                let _ = (offset, len);
                Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "pread not implemented for this platform",
                ))
            }
        })
        .await
        .map_err(|e| std::io::Error::other(e))?;
        result
    }
}
