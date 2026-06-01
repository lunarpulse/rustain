//! Story 11.2a integration — resolve→promote semantics for `/memory consolidate`.
//!
//! The event-loop `MemoryConsolidationResolved` handler promotes each accepted
//! `MemoryFact` through the EXISTING `remember_fact` path after a defense-in-depth
//! `scan_for_secrets` gate (AC4 + AC5). This drives that exact sequence against
//! the real `project-scoped` composite. The handler's promote loop is inline in
//! `event_loop.rs`; this test exercises the same operations end-to-end:
//! scan → `remember_fact` → recall via `recent()`.
//!
//! Daily-log preservation (AC4 — "never deleted") is structural: promotion only
//! ever calls `remember_fact` (→ the long-term `MEMORY.md` tier); neither the
//! composite nor the `MemoryPort` trait exposes any daily-log delete, so nothing
//! in the consolidation path can remove daily entries.

use rustain::adapters::project_scoped_memory::ProjectScopedMemory;
use rustain::domain::models::MemoryFact;
use rustain::domain::ports::MemoryPort;
use rustain::domain::services::secret_scan::scan_for_secrets;

/// Mirror the event-loop promote loop exactly: scan each fact, skip on a
/// high-confidence secret hit, otherwise `remember_fact`.
async fn promote(mem: &ProjectScopedMemory, accepted: Vec<MemoryFact>) -> (usize, usize) {
    let mut promoted = 0usize;
    let mut skipped = 0usize;
    for fact in accepted {
        let blob = format!(
            "{}\n{}\n{}",
            fact.category,
            fact.fact,
            fact.detail.as_deref().unwrap_or("")
        );
        if scan_for_secrets(&blob).is_some() {
            skipped += 1;
            continue;
        }
        mem.remember_fact(fact).await.unwrap();
        promoted += 1;
    }
    (promoted, skipped)
}

#[tokio::test]
async fn promote_writes_clean_facts_and_skips_secrets() {
    let tmp = tempfile::tempdir().unwrap();
    let mem = ProjectScopedMemory::new(tmp.path());

    // Build the AWS key at runtime so the literal never appears in source.
    let akia = format!("AKIA{}", "A".repeat(16));
    let accepted = vec![
        MemoryFact {
            category: "Preferences".to_string(),
            fact: "User prefers snake_case".to_string(),
            detail: None,
        },
        MemoryFact {
            category: "Database".to_string(),
            fact: "Postgres 15".to_string(),
            detail: Some("primary store".to_string()),
        },
        MemoryFact {
            category: "Creds".to_string(),
            fact: format!("aws access key {akia}"),
            detail: None,
        },
    ];

    let (promoted, skipped) = promote(&mem, accepted).await;
    assert_eq!(promoted, 2, "two clean facts promoted (AC4)");
    assert_eq!(skipped, 1, "secret-bearing fact skipped (AC5)");

    let recent = mem.recent(50).await.unwrap();
    let joined: String = recent
        .iter()
        .map(|e| format!("{} {}", e.summary, e.context.as_deref().unwrap_or("")))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        joined.contains("User prefers snake_case"),
        "fact 1 recalled from MEMORY.md"
    );
    assert!(
        joined.contains("Postgres 15"),
        "fact 2 recalled from MEMORY.md"
    );
    assert!(
        !joined.contains(&akia),
        "secret must never be promoted to MEMORY.md (AC5)"
    );
}

#[tokio::test]
async fn declining_promotes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let mem = ProjectScopedMemory::new(tmp.path());

    // Decline = empty accepted set → no `remember_fact` calls, nothing written.
    let (promoted, skipped) = promote(&mem, Vec::new()).await;
    assert_eq!(promoted, 0);
    assert_eq!(skipped, 0);

    let recent = mem.recent(50).await.unwrap();
    assert!(recent.is_empty(), "declining writes nothing (AC3/AC4)");
}
