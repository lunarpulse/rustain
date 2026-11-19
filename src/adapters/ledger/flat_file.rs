//! Flat-file JSONL usage ledger adapter.
//!
//! Appends one JSON line per `UsageLedgerEntry` to
//! `~/.rustain/usage/{session_id}.jsonl` (or `$RUSTAIN_DATA_DIR/usage/`).
//! The file is opened create+append per call — stateless, lock-free.

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;

use crate::domain::errors::StorageError;
use crate::domain::models::usage::UsageLedgerEntry;
use crate::domain::ports::{LedgerReadMode, UsageLedgerPort};

/// Stateless flat-file ledger — resolves path per entry from `session_id`.
pub struct FileUsageLedger;

impl FileUsageLedger {
    pub fn new() -> Self {
        Self
    }

    async fn read_path(
        path: &std::path::Path,
        since_unix_ms: i64,
        mode: LedgerReadMode,
    ) -> Result<Vec<UsageLedgerEntry>, StorageError> {
        let contents = match tokio::fs::read_to_string(path).await {
            Ok(s) => s,
            Err(e)
                if e.kind() == std::io::ErrorKind::NotFound && mode == LedgerReadMode::Lenient =>
            {
                return Ok(Vec::new());
            }
            Err(e) => return Err(StorageError::IoError(e.to_string())),
        };
        let mut out = Vec::new();
        for line in contents.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let LedgerReadMode::Strict { end_unix_ms } = mode {
                // Derived struct decoders also accept positional arrays.
                if !trimmed.starts_with('{') {
                    return Err(StorageError::SerializationError(
                        "usage ledger row must be a JSON object".into(),
                    ));
                }

                #[derive(serde::Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct Timestamp {
                    timestamp_ms: i64,
                }
                // Validate the whole JSON object, rejecting a missing, duplicate
                // or non-integer timestamp before excluding an out-of-window row.
                let timestamp = serde_json::from_str::<Timestamp>(trimmed)
                    .map_err(|e| StorageError::SerializationError(e.to_string()))?
                    .timestamp_ms;
                if (since_unix_ms..end_unix_ms).contains(&timestamp) {
                    out.push(
                        serde_json::from_str(trimmed)
                            .map_err(|e| StorageError::SerializationError(e.to_string()))?,
                    );
                }
            } else {
                match serde_json::from_str::<UsageLedgerEntry>(trimmed) {
                    Ok(entry) if entry.timestamp_ms >= since_unix_ms => out.push(entry),
                    Ok(_) => {}
                    Err(e) => tracing::warn!("ledger line skipped (path={}): {e}", path.display()),
                }
            }
        }
        Ok(out)
    }

    async fn read_since_in_dir(
        dir: Result<std::path::PathBuf, StorageError>,
        since_unix_ms: i64,
        mode: LedgerReadMode,
    ) -> Result<Vec<UsageLedgerEntry>, StorageError> {
        let dir = dir?;
        let mut rd = match tokio::fs::read_dir(&dir).await {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(StorageError::IoError(e.to_string())),
        };
        let mut out = Vec::new();
        while let Some(entry) = rd
            .next_entry()
            .await
            .map_err(|e| StorageError::IoError(e.to_string()))?
        {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            if matches!(mode, LedgerReadMode::Strict { .. })
                && !entry
                    .file_type()
                    .await
                    .map_err(|e| StorageError::IoError(e.to_string()))?
                    .is_file()
            {
                return Err(StorageError::IoError(format!(
                    "usage ledger entry is not a regular file: {}",
                    path.display()
                )));
            }
            out.extend(Self::read_path(&path, since_unix_ms, mode).await?);
        }
        Ok(out)
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
        // The id names the file: refuse one that could leave the usage directory.
        crate::infrastructure::utils::sanitize_id(&entry.session_id).map_err(|_| {
            StorageError::Other("usage ledger: session id is not a safe file name".into())
        })?;
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

        Self::read_path(&path, i64::MIN, LedgerReadMode::Lenient).await
    }

    async fn read_since(
        &self,
        since_unix_ms: i64,
        mode: LedgerReadMode,
    ) -> Result<Vec<UsageLedgerEntry>, StorageError> {
        if let LedgerReadMode::Strict { end_unix_ms } = mode
            && end_unix_ms <= since_unix_ms
        {
            return Err(StorageError::Other(
                "usage ledger: strict window end must be after its start".into(),
            ));
        }
        let dir = crate::infrastructure::paths::usage_dir()
            .await
            .map_err(|e| StorageError::IoError(e.to_string()));
        Self::read_since_in_dir(dir, since_unix_ms, mode).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::router::{EscalationReason, ModelTier, StepKind};
    use crate::domain::models::usage::TokenUsage;

    #[tokio::test]
    #[serial_test::serial(rustain_data_dir)]
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
            provenance: None,
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
            provenance: None,
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
            provenance: None,
        }
    }

    #[tokio::test]
    async fn read_since_handles_resolution_missing_directory_and_corrupt_lines() {
        let unresolved = Err(StorageError::IoError("unresolvable usage directory".into()));
        assert!(
            FileUsageLedger::read_since_in_dir(unresolved, 0, LedgerReadMode::Lenient)
                .await
                .is_err()
        );

        let temp = tempfile::tempdir().unwrap();
        assert!(
            FileUsageLedger::read_since_in_dir(
                Ok(temp.path().join("missing")),
                0,
                LedgerReadMode::Lenient,
            )
            .await
            .unwrap()
            .is_empty()
        );

        let old = make_entry("s", 10, "model");
        let current = make_entry("s", 20, "model");
        let contents = format!(
            "{}\nnot json\n{}\n",
            serde_json::to_string(&old).unwrap(),
            serde_json::to_string(&current).unwrap()
        );
        tokio::fs::write(temp.path().join("s.jsonl"), contents)
            .await
            .unwrap();
        let rows = FileUsageLedger::read_since_in_dir(
            Ok(temp.path().to_path_buf()),
            20,
            LedgerReadMode::Lenient,
        )
        .await
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].timestamp_ms, 20);
    }

    #[tokio::test]
    async fn strict_read_only_excludes_corrupt_rows_with_proven_outside_timestamps() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let current = serde_json::to_string(&make_entry("s", 20, "model")).unwrap();
        let positional = r#"[20,"s","conv-1","anthropic","model","flagship","edit","none",{"tokensIn":100,"tokensOut":50,"parentCtx":0},"observed"]"#;
        for (additional, accepted) in [
            (r#"{"timestampMs":19}"#, true),
            (r#"{"timestampMs":20}"#, false),
            (r#"{"timestampMs":29}"#, false),
            (r#"{"timestampMs":30}"#, true),
            (r#"{"timestampMs":19,"timestampMs":21}"#, false),
            (r#"{"timestampMs":19,"timestampMs":19}"#, false),
            (r#"{"timestampMs":30,"\u0074imestampMs":31}"#, false),
            (r#"{"timestampMs":"19"}"#, false),
            (r#"{"timestampMs":30.0}"#, false),
            (r#"{"usage":{"timestampMs":19}}"#, false),
            (r#"{"timestampMs":30,"usage":}"#, false),
            (r#"{"timestampMs":30} {}"#, false),
            ("[19]", false),
            ("[30]", false),
            (positional, false),
            ("not json", false),
        ] {
            tokio::fs::write(&path, format!("{current}\n{additional}\n"))
                .await
                .unwrap();
            let result = FileUsageLedger::read_since_in_dir(
                Ok(dir.path().to_path_buf()),
                20,
                LedgerReadMode::Strict { end_unix_ms: 30 },
            )
            .await;
            if accepted {
                let rows = result.unwrap_or_else(|err| panic!("{additional}: {err}"));
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].timestamp_ms, 20);
            } else {
                assert!(
                    matches!(result, Err(StorageError::SerializationError(_))),
                    "{additional}: {result:?}",
                );
            }
        }

        // The full positional row was readable before strict object validation.
        tokio::fs::write(&path, positional).await.unwrap();
        let rows = FileUsageLedger::read_since_in_dir(
            Ok(dir.path().to_path_buf()),
            20,
            LedgerReadMode::Lenient,
        )
        .await
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].timestamp_ms, 20);
    }

    #[tokio::test]
    async fn strict_read_uses_half_open_window() {
        let dir = tempfile::tempdir().unwrap();
        let mut contents = String::new();
        for timestamp in [19, 20, 29, 30, 31] {
            contents
                .push_str(&serde_json::to_string(&make_entry("s", timestamp, "model")).unwrap());
            contents.push('\n');
        }
        tokio::fs::write(dir.path().join("s.jsonl"), contents)
            .await
            .unwrap();
        let rows = FileUsageLedger::read_since_in_dir(
            Ok(dir.path().to_path_buf()),
            20,
            LedgerReadMode::Strict { end_unix_ms: 30 },
        )
        .await
        .unwrap();
        let timestamps: Vec<_> = rows.into_iter().map(|row| row.timestamp_ms).collect();
        assert_eq!(timestamps, [20, 29]);
    }

    #[tokio::test]
    #[serial_test::serial(rustain_data_dir)]
    async fn strict_read_rejects_invalid_window_before_resolving_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("uncreated");
        let _env = data_dir_guard(&root);
        let ledger = FileUsageLedger::new();

        for end_unix_ms in [20, 19] {
            assert!(matches!(
                ledger
                    .read_since(20, LedgerReadMode::Strict { end_unix_ms })
                    .await,
                Err(StorageError::Other(_)),
            ));
        }
        assert!(
            !root.exists(),
            "invalid windows must not create directories"
        );
    }

    #[tokio::test]
    #[serial_test::serial(rustain_data_dir)]
    async fn read_since_lenient_accepts_legacy_and_observed_rows() {
        use crate::domain::models::usage::UsageProvenance;

        let dir = tempfile::tempdir().unwrap();
        let _env = data_dir_guard(dir.path());
        let ledger = FileUsageLedger::new();
        let path = crate::infrastructure::paths::usage_ledger_path("s")
            .await
            .unwrap();
        let legacy = r#"{"timestampMs":20,"sessionId":"s","conversationId":"conv-1","providerId":"anthropic","model":"legacy","tier":"flagship","stepKind":"edit","escalationReason":"none","usage":{"tokensIn":100,"tokensOut":50,"parentCtx":0}}"#;
        tokio::fs::write(path, format!("{legacy}\n")).await.unwrap();

        let mut observed = make_entry("s", 21, "current");
        observed.provenance = Some(UsageProvenance::Observed);
        observed.usage.cache_read_tokens = Some(2);
        ledger.append(observed).await.unwrap();

        let rows = ledger
            .read_since(20, LedgerReadMode::Lenient)
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].model, "legacy");
        assert_eq!(rows[0].usage.tokens_in, 100);
        assert_eq!(rows[0].usage.cache_read_tokens, None);
        assert_eq!(rows[0].provenance, None);
        assert_eq!(rows[1].model, "current");
        assert_eq!(rows[1].usage.cache_read_tokens, Some(2));
        assert_eq!(rows[1].provenance, Some(UsageProvenance::Observed));
    }

    #[tokio::test]
    #[serial_test::serial(rustain_data_dir)]
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
    #[serial_test::serial(rustain_data_dir)]
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
        let mut got = ledger
            .read_since(2000, LedgerReadMode::Lenient)
            .await
            .expect("read_since 2000");
        got.sort_by_key(|e| e.timestamp_ms);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].timestamp_ms, 2000);
        assert_eq!(got[1].timestamp_ms, 3000);

        // since 5000 → empty
        let none = ledger
            .read_since(5000, LedgerReadMode::Lenient)
            .await
            .expect("read_since 5000");
        assert!(none.is_empty());

        match original {
            Some(v) => unsafe { std::env::set_var("RUSTAIN_DATA_DIR", v) }, // CONFORMANCE_EXCEPTION: test-only env var save/restore for tempdir isolation
            None => unsafe { std::env::remove_var("RUSTAIN_DATA_DIR") }, // CONFORMANCE_EXCEPTION: test-only env var save/restore for tempdir isolation
        }
    }

    #[tokio::test]
    #[serial_test::serial(rustain_data_dir)]
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

    /// Points `RUSTAIN_DATA_DIR` at a tempdir until the guard drops.
    fn data_dir_guard(
        dir: &std::path::Path,
    ) -> scopeguard::ScopeGuard<Option<String>, impl FnOnce(Option<String>)> {
        let original = std::env::var("RUSTAIN_DATA_DIR").ok(); // CONFORMANCE_EXCEPTION: test-only env var save/restore for tempdir isolation
        unsafe {
            std::env::set_var("RUSTAIN_DATA_DIR", dir.as_os_str()); // CONFORMANCE_EXCEPTION: test-only env var save/restore for tempdir isolation
        }
        scopeguard::guard(original, |original| match original {
            Some(v) => unsafe { std::env::set_var("RUSTAIN_DATA_DIR", v) }, // CONFORMANCE_EXCEPTION: test-only env var save/restore for tempdir isolation
            None => unsafe { std::env::remove_var("RUSTAIN_DATA_DIR") }, // CONFORMANCE_EXCEPTION: test-only env var save/restore for tempdir isolation
        })
    }

    /// eval-3 AC4: a session id that is not a safe file name writes nothing,
    /// above or inside the usage directory; a safe id is written.
    #[tokio::test]
    #[serial_test::serial(rustain_data_dir)]
    async fn append_refuses_a_session_id_that_is_not_a_safe_file_name() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _env = data_dir_guard(tmp.path());
        let ledger = FileUsageLedger::new();

        for bad in ["../escape", "a/b", ""] {
            let refused = ledger.append(make_entry(bad, 1000, "haiku")).await;
            assert!(refused.is_err(), "session id {bad:?} must be refused");
        }
        assert!(!tmp.path().join("escape.jsonl").exists());
        let usage = tmp.path().join("usage");
        let in_usage = std::fs::read_dir(&usage).map_or(0, Iterator::count);
        assert_eq!(in_usage, 0, "nothing is written in the usage directory");
        let top_level_jsonl = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
            .count();
        assert_eq!(top_level_jsonl, 0, "no .jsonl above the usage directory");

        let entry = make_entry("acp-AbC_9-1", 2000, "haiku");
        ledger.append(entry.clone()).await.expect("safe id appends");
        let text = std::fs::read_to_string(usage.join("acp-AbC_9-1.jsonl")).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 1);
        let parsed: UsageLedgerEntry = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(
            serde_json::to_value(&parsed).unwrap(),
            serde_json::to_value(&entry).unwrap()
        );
    }

    /// eval-3 AC6 (pin): two writers appending to one session file keep every
    /// line whole.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[serial_test::serial(rustain_data_dir)]
    async fn concurrent_appends_to_one_session_stay_whole() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _env = data_dir_guard(tmp.path());

        let mut tasks = Vec::new();
        for writer in ["a", "b"] {
            let ledger = std::sync::Arc::new(FileUsageLedger::new());
            for i in 0..100 {
                let mut entry = make_entry("concurrent-1", 1000 + i, "haiku");
                entry.conversation_id = format!("{writer}-{i}");
                let ledger = ledger.clone();
                tasks.push(tokio::spawn(async move { ledger.append(entry).await }));
            }
        }
        for task in tasks {
            task.await.expect("join").expect("append");
        }

        let text = std::fs::read_to_string(tmp.path().join("usage/concurrent-1.jsonl")).unwrap();
        assert_eq!(text.lines().count(), 200);
        for l in text.lines() {
            serde_json::from_str::<UsageLedgerEntry>(l).expect("every line parses");
        }
        let read = FileUsageLedger::new()
            .read_session("concurrent-1")
            .await
            .unwrap();
        let got: std::collections::BTreeSet<String> =
            read.into_iter().map(|e| e.conversation_id).collect();
        let want: std::collections::BTreeSet<String> = ["a", "b"]
            .iter()
            .flat_map(|w| (0..100).map(move |i| format!("{w}-{i}")))
            .collect();
        assert_eq!(got.len(), 200);
        assert_eq!(got, want);
    }
}
