//! Flat-file JSONL usage ledger adapter.
//!
//! Appends one JSON line per `UsageLedgerEntry` to
//! `~/.rustain/usage/{session_id}.jsonl` (or `$RUSTAIN_DATA_DIR/usage/`).
//! The file is opened create+append per call — stateless, lock-free.

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;

use crate::domain::errors::StorageError;
use crate::domain::models::usage::UsageLedgerEntry;
use crate::domain::ports::UsageLedgerPort;

/// Stateless flat-file ledger — resolves path per entry from `session_id`.
pub struct FileUsageLedger;

impl FileUsageLedger {
    pub fn new() -> Self {
        Self
    }
}

impl Default for FileUsageLedger {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UsageLedgerPort for FileUsageLedger {
    async fn append(&self, entry: UsageLedgerEntry) -> Result<(), StorageError> {
        let path = crate::infrastructure::paths::usage_ledger_path(&entry.session_id)
            .await
            .map_err(|e| StorageError::IoError(e.to_string()))?;

        let line = serde_json::to_string(&entry)
            .map_err(|e| StorageError::SerializationError(e.to_string()))?;

        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .map_err(|e| StorageError::IoError(e.to_string()))?;

        let mut buf = line.into_bytes();
        buf.push(b'\n');
        file.write_all(&buf)
            .await
            .map_err(|e| StorageError::IoError(e.to_string()))?;
        file.flush()
            .await
            .map_err(|e| StorageError::IoError(e.to_string()))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::router::{EscalationReason, ModelTier, StepKind};
    use crate::domain::models::usage::TokenUsage;

    #[tokio::test]
    #[serial_test::serial]
    async fn ledger_append_roundtrip() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let original = std::env::var("RUSTAIN_DATA_DIR").ok(); // CONFORMANCE_EXCEPTION: test-only env var save/restore for tempdir isolation
        unsafe {
            std::env::set_var("RUSTAIN_DATA_DIR", tmp.path().as_os_str());
        }

        let ledger = FileUsageLedger::new();

        let entry1 = UsageLedgerEntry {
            timestamp_ms: 1_747_238_400_000,
            session_id: "sess-abc".to_string(),
            conversation_id: "conv-1".to_string(),
            provider_id: "anthropic".to_string(),
            model: "claude-haiku-4-5-20251001".to_string(),
            tier: ModelTier::CheapAgentic,
            step_kind: Some(StepKind::Edit),
            escalation_reason: EscalationReason::None,
            usage: TokenUsage {
                tokens_in: 1200,
                tokens_out: 340,
                parent_ctx: 0,
            },
        };

        let entry2 = UsageLedgerEntry {
            timestamp_ms: 1_747_238_400_001,
            session_id: "sess-abc".to_string(),
            conversation_id: "conv-1".to_string(),
            provider_id: "anthropic".to_string(),
            model: "claude-sonnet-4-20250514".to_string(),
            tier: ModelTier::Flagship,
            step_kind: Some(StepKind::Codegen),
            escalation_reason: EscalationReason::Budget,
            usage: TokenUsage {
                tokens_in: 150_000,
                tokens_out: 5000,
                parent_ctx: 1200,
            },
        };

        ledger.append(entry1.clone()).await.expect("append 1");
        ledger.append(entry2.clone()).await.expect("append 2");

        let path = tmp.path().join("usage").join("sess-abc.jsonl");
        let contents = std::fs::read_to_string(&path).expect("read ledger");
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2, "ledger should have exactly 2 lines");

        let parsed1: UsageLedgerEntry = serde_json::from_str(lines[0]).expect("parse line 1");
        assert_eq!(parsed1.timestamp_ms, entry1.timestamp_ms);
        assert_eq!(parsed1.session_id, entry1.session_id);
        assert_eq!(parsed1.conversation_id, entry1.conversation_id);
        assert_eq!(parsed1.provider_id, entry1.provider_id);
        assert_eq!(parsed1.model, entry1.model);
        assert_eq!(parsed1.tier, entry1.tier);
        assert_eq!(parsed1.step_kind, entry1.step_kind);
        assert_eq!(parsed1.escalation_reason, entry1.escalation_reason);
        assert_eq!(parsed1.usage.tokens_in, entry1.usage.tokens_in);
        assert_eq!(parsed1.usage.tokens_out, entry1.usage.tokens_out);
        assert_eq!(parsed1.usage.parent_ctx, entry1.usage.parent_ctx);

        let parsed2: UsageLedgerEntry = serde_json::from_str(lines[1]).expect("parse line 2");
        assert_eq!(parsed2.escalation_reason, EscalationReason::Budget);
        assert_eq!(parsed2.tier, ModelTier::Flagship);

        // restore env
        match original {
            Some(v) => unsafe { std::env::set_var("RUSTAIN_DATA_DIR", v) },
            None => unsafe { std::env::remove_var("RUSTAIN_DATA_DIR") },
        }
    }
}
