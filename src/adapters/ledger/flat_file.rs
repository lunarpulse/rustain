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

    async fn read_session(&self, session_id: &str) -> Result<Vec<UsageLedgerEntry>, StorageError> {
        let path = crate::infrastructure::paths::usage_ledger_path(session_id)
            .await
            .map_err(|e| StorageError::IoError(e.to_string()))?;

        let contents = match tokio::fs::read_to_string(&path).await {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(StorageError::IoError(e.to_string())),
        };

        let mut out = Vec::new();
        for line in contents.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str::<UsageLedgerEntry>(trimmed) {
                Ok(entry) => out.push(entry),
                Err(e) => {
                    // Story 7.5 AC9 — graceful degradation: skip corrupt lines.
                    tracing::warn!("ledger line skipped (session={session_id}): {e}");
                }
            }
        }
        Ok(out)
    }

    async fn read_since(&self, since_unix_ms: i64) -> Result<Vec<UsageLedgerEntry>, StorageError> {
        let dir = match crate::infrastructure::paths::usage_dir().await {
            Ok(d) => d,
            Err(_) => return Ok(Vec::new()),
        };

        let mut rd = match tokio::fs::read_dir(&dir).await {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(StorageError::IoError(e.to_string())),
        };

        let mut sessions: Vec<String> = Vec::new();
        while let Some(entry) = rd
            .next_entry()
            .await
            .map_err(|e| StorageError::IoError(e.to_string()))?
        {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            sessions.push(stem.to_string());
        }

        let mut out = Vec::new();
        for session in sessions {
            let entries = self.read_session(&session).await?;
            out.extend(
                entries
                    .into_iter()
                    .filter(|e| e.timestamp_ms >= since_unix_ms),
            );
        }
        Ok(out)
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
            std::env::set_var("RUSTAIN_DATA_DIR", tmp.path().as_os_str()); // CONFORMANCE_EXCEPTION: test-only env var save/restore for tempdir isolation
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
                cache_creation_tokens: None,
                cache_read_tokens: None,
                reasoning_tokens: None,
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
                cache_creation_tokens: Some(40_000),
                cache_read_tokens: Some(10_000),
                reasoning_tokens: None,
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
        // Story 7.5 AC8 — cache fields serialize/deserialize correctly.
        assert_eq!(parsed2.usage.cache_creation_tokens, Some(40_000));
        assert_eq!(parsed2.usage.cache_read_tokens, Some(10_000));
        assert_eq!(parsed2.usage.reasoning_tokens, None);

        // Story 7.5 AC8 — backward-compat: a legacy ledger line WITHOUT the 3
        // new cache/reasoning fields must deserialize cleanly with `None`.
        let legacy_line = r#"{"timestampMs":1747238400002,"sessionId":"sess-legacy","conversationId":"conv-1","providerId":"anthropic","model":"claude-haiku-4-5-20251001","tier":"cheap_agentic","stepKind":"edit","escalationReason":"none","usage":{"tokensIn":100,"tokensOut":50,"parentCtx":0}}"#;
        let legacy_parsed: UsageLedgerEntry =
            serde_json::from_str(legacy_line).expect("parse legacy line without cache fields");
        assert_eq!(legacy_parsed.usage.tokens_in, 100);
        assert_eq!(legacy_parsed.usage.cache_creation_tokens, None);
        assert_eq!(legacy_parsed.usage.cache_read_tokens, None);
        assert_eq!(legacy_parsed.usage.reasoning_tokens, None);

        // restore env
        match original {
            Some(v) => unsafe { std::env::set_var("RUSTAIN_DATA_DIR", v) }, // CONFORMANCE_EXCEPTION: test-only env var save/restore for tempdir isolation
            None => unsafe { std::env::remove_var("RUSTAIN_DATA_DIR") }, // CONFORMANCE_EXCEPTION: test-only env var save/restore for tempdir isolation
        }
    }

    fn make_entry(session: &str, ts_ms: i64, model: &str) -> UsageLedgerEntry {
        UsageLedgerEntry {
            timestamp_ms: ts_ms,
            session_id: session.to_string(),
            conversation_id: "conv-1".to_string(),
            provider_id: "anthropic".to_string(),
            model: model.to_string(),
            tier: ModelTier::Flagship,
            step_kind: Some(StepKind::Edit),
            escalation_reason: EscalationReason::None,
            usage: TokenUsage {
                tokens_in: 100,
                tokens_out: 50,
                parent_ctx: 0,
                cache_creation_tokens: None,
                cache_read_tokens: None,
                reasoning_tokens: None,
            },
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn read_session_returns_appended_entries() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let original = std::env::var("RUSTAIN_DATA_DIR").ok(); // CONFORMANCE_EXCEPTION: test-only env var save/restore for tempdir isolation
        unsafe {
            std::env::set_var("RUSTAIN_DATA_DIR", tmp.path().as_os_str()); // CONFORMANCE_EXCEPTION: test-only env var save/restore for tempdir isolation
        }

        let ledger = FileUsageLedger::new();
        ledger
            .append(make_entry("sess-r1", 1000, "haiku"))
            .await
            .expect("append 1");
        ledger
            .append(make_entry("sess-r1", 2000, "sonnet"))
            .await
            .expect("append 2");

        let read = ledger.read_session("sess-r1").await.expect("read_session");
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].timestamp_ms, 1000);
        assert_eq!(read[0].model, "haiku");
        assert_eq!(read[1].timestamp_ms, 2000);
        assert_eq!(read[1].model, "sonnet");

        // Story 7.5 AC9 — missing session file returns Ok(vec![])
        let empty = ledger
            .read_session("does-not-exist")
            .await
            .expect("read_session missing");
        assert!(empty.is_empty());

        match original {
            Some(v) => unsafe { std::env::set_var("RUSTAIN_DATA_DIR", v) }, // CONFORMANCE_EXCEPTION: test-only env var save/restore for tempdir isolation
            None => unsafe { std::env::remove_var("RUSTAIN_DATA_DIR") }, // CONFORMANCE_EXCEPTION: test-only env var save/restore for tempdir isolation
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn read_since_filters_by_timestamp() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let original = std::env::var("RUSTAIN_DATA_DIR").ok(); // CONFORMANCE_EXCEPTION: test-only env var save/restore for tempdir isolation
        unsafe {
            std::env::set_var("RUSTAIN_DATA_DIR", tmp.path().as_os_str()); // CONFORMANCE_EXCEPTION: test-only env var save/restore for tempdir isolation
        }

        let ledger = FileUsageLedger::new();
        ledger
            .append(make_entry("sess-a", 1000, "haiku"))
            .await
            .expect("append a1");
        ledger
            .append(make_entry("sess-a", 3000, "haiku"))
            .await
            .expect("append a2");
        ledger
            .append(make_entry("sess-b", 2000, "sonnet"))
            .await
            .expect("append b1");

        // since 2000 → 2 entries (sess-a@3000 + sess-b@2000)
        let mut got = ledger.read_since(2000).await.expect("read_since 2000");
        got.sort_by_key(|e| e.timestamp_ms);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].timestamp_ms, 2000);
        assert_eq!(got[1].timestamp_ms, 3000);

        // since 5000 → empty
        let none = ledger.read_since(5000).await.expect("read_since 5000");
        assert!(none.is_empty());

        match original {
            Some(v) => unsafe { std::env::set_var("RUSTAIN_DATA_DIR", v) }, // CONFORMANCE_EXCEPTION: test-only env var save/restore for tempdir isolation
            None => unsafe { std::env::remove_var("RUSTAIN_DATA_DIR") }, // CONFORMANCE_EXCEPTION: test-only env var save/restore for tempdir isolation
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn read_session_skips_corrupt_lines() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let original = std::env::var("RUSTAIN_DATA_DIR").ok(); // CONFORMANCE_EXCEPTION: test-only env var save/restore for tempdir isolation
        unsafe {
            std::env::set_var("RUSTAIN_DATA_DIR", tmp.path().as_os_str()); // CONFORMANCE_EXCEPTION: test-only env var save/restore for tempdir isolation
        }

        let ledger = FileUsageLedger::new();
        ledger
            .append(make_entry("sess-corrupt", 1000, "haiku"))
            .await
            .expect("append good 1");

        // Manually inject a garbage line between good ones
        let path = crate::infrastructure::paths::usage_ledger_path("sess-corrupt")
            .await
            .expect("path");
        let mut existing = std::fs::read_to_string(&path).expect("read");
        existing.push_str("this is not json\n");
        std::fs::write(&path, existing).expect("write garbage");

        ledger
            .append(make_entry("sess-corrupt", 2000, "sonnet"))
            .await
            .expect("append good 2");

        let read = ledger
            .read_session("sess-corrupt")
            .await
            .expect("read_session");
        assert_eq!(
            read.len(),
            2,
            "corrupt line should be skipped, 2 valid entries returned"
        );

        match original {
            Some(v) => unsafe { std::env::set_var("RUSTAIN_DATA_DIR", v) }, // CONFORMANCE_EXCEPTION: test-only env var save/restore for tempdir isolation
            None => unsafe { std::env::remove_var("RUSTAIN_DATA_DIR") }, // CONFORMANCE_EXCEPTION: test-only env var save/restore for tempdir isolation
        }
    }
}
