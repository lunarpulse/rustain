//! Story 7.5 AC10 + AC12 — end-to-end cost-calculator + ledger integration.
//!
//! Writes synthetic `UsageLedgerEntry` rows via `FileUsageLedger` into a
//! tempdir, then asserts `cost_calculator::cost_breakdown` produces the
//! expected `CostBreakdown`. Uses `RUSTAIN_DATA_DIR` env override +
//! `#[serial_test::serial]` to avoid env-var races with other tests.

use std::collections::HashMap;
use std::sync::Arc;

use rustain::adapters::ledger::flat_file::FileUsageLedger;
use rustain::domain::models::pricing::PricingConfig;
use rustain::domain::models::router::{EscalationReason, ModelTier, StepKind};
use rustain::domain::models::usage::{TokenUsage, UsageLedgerEntry};
use rustain::domain::ports::UsageLedgerPort;
use rustain::domain::services::cost_calculator::{cost_breakdown, cumulative_cost};

fn entry(session: &str, ts: i64, model: &str, tin: u32, tout: u32) -> UsageLedgerEntry {
    UsageLedgerEntry {
        timestamp_ms: ts,
        session_id: session.to_string(),
        conversation_id: "conv-test".to_string(),
        provider_id: "anthropic".to_string(),
        model: model.to_string(),
        tier: ModelTier::Flagship,
        step_kind: Some(StepKind::Codegen),
        escalation_reason: EscalationReason::None,
        usage: TokenUsage {
            tokens_in: tin,
            tokens_out: tout,
            parent_ctx: 0,
            cache_creation_tokens: None,
            cache_read_tokens: None,
            reasoning_tokens: None,
        },
    }
}

fn pricing() -> HashMap<String, PricingConfig> {
    let mut m = HashMap::new();
    m.insert(
        "sonnet".to_string(),
        PricingConfig {
            input_per_million: 3.0,
            output_per_million: 15.0,
            cache_creation_per_million: None,
            cache_read_per_million: None,
            reasoning_per_million: None,
        },
    );
    m.insert(
        "haiku".to_string(),
        PricingConfig {
            input_per_million: 0.80,
            output_per_million: 4.00,
            cache_creation_per_million: None,
            cache_read_per_million: None,
            reasoning_per_million: None,
        },
    );
    m
}

#[tokio::test]
#[serial_test::serial]
async fn ledger_to_cost_breakdown_end_to_end() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let original = std::env::var("RUSTAIN_DATA_DIR").ok();
    unsafe {
        std::env::set_var("RUSTAIN_DATA_DIR", tmp.path().as_os_str());
    }

    let ledger: Arc<dyn UsageLedgerPort> = Arc::new(FileUsageLedger::new());

    // Today entries
    ledger
        .append(entry("sess-1", 1000, "sonnet", 1_000_000, 500_000))
        .await
        .expect("append 1");
    ledger
        .append(entry("sess-1", 2000, "haiku", 1_000_000, 0))
        .await
        .expect("append 2");
    ledger
        .append(entry("sess-1", 3000, "sonnet", 1_000_000, 0))
        .await
        .expect("append 3");
    ledger
        .append(entry("sess-1", 4000, "unknown-model", 5_000_000, 0))
        .await
        .expect("append 4");

    let all = ledger.read_since(0).await.expect("read_since");
    assert_eq!(all.len(), 4);

    let pricing = pricing();
    let bd = cost_breakdown(&all, &pricing);
    // sonnet: 1M in + 0.5M out + 1M in + 0 = 2M in + 0.5M out
    // = 2 × $3.00 + 0.5 × $15.00 = $6.00 + $7.50 = $13.50
    let sonnet = bd.per_model.get("sonnet").expect("sonnet row");
    assert!((sonnet.cost_usd.unwrap() - 13.50).abs() < 1e-9);
    assert_eq!(sonnet.call_count, 2);

    // haiku: 1M in = $0.80
    let haiku = bd.per_model.get("haiku").expect("haiku row");
    assert!((haiku.cost_usd.unwrap() - 0.80).abs() < 1e-9);

    // unknown-model has no pricing → cost_usd is None
    let unknown = bd.per_model.get("unknown-model").expect("unknown row");
    assert_eq!(unknown.cost_usd, None);
    assert!(
        bd.missing_pricing_models
            .contains(&"unknown-model".to_string())
    );

    // Total = $13.50 + $0.80 = $14.30
    assert!((bd.total_usd - 14.30).abs() < 1e-9);

    // cumulative_cost skips unknown-model
    let cum = cumulative_cost(&all, &pricing);
    assert!((cum - 14.30).abs() < 1e-9);

    // read_since(2500) filters out ts < 2500 → returns 2 entries (ts=3000, 4000)
    let recent = ledger.read_since(2500).await.expect("read_since recent");
    assert_eq!(recent.len(), 2);

    match original {
        Some(v) => unsafe { std::env::set_var("RUSTAIN_DATA_DIR", v) },
        None => unsafe { std::env::remove_var("RUSTAIN_DATA_DIR") },
    }
}

#[tokio::test]
#[serial_test::serial]
async fn ledger_returns_empty_when_no_usage_dir() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let original = std::env::var("RUSTAIN_DATA_DIR").ok();
    unsafe {
        std::env::set_var("RUSTAIN_DATA_DIR", tmp.path().as_os_str());
    }

    let ledger: Arc<dyn UsageLedgerPort> = Arc::new(FileUsageLedger::new());

    // No appends yet
    let empty = ledger.read_since(0).await.expect("read_since empty");
    assert!(empty.is_empty());
    let empty_session = ledger
        .read_session("nope")
        .await
        .expect("read_session empty");
    assert!(empty_session.is_empty());

    match original {
        Some(v) => unsafe { std::env::set_var("RUSTAIN_DATA_DIR", v) },
        None => unsafe { std::env::remove_var("RUSTAIN_DATA_DIR") },
    }
}
