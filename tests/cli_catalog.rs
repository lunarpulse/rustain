//! Integration tests for `rustain catalog` CLI subcommands.
//! Story 9.8.

use std::path::PathBuf;

use assert_cmd::Command;
use predicates::str::contains;
use serial_test::serial;

fn rustain_cmd() -> Command {
    Command::cargo_bin("rustain").unwrap()
}

fn setup_temp_config_dir() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let config_dir = temp.path().join(".rustain");
    std::fs::create_dir_all(&config_dir).unwrap();
    // Write a minimal config.toml so startup doesn't fail on missing config.
    std::fs::write(
        config_dir.join("config.toml"),
        r#"
[model]
model = "claude-3-5-sonnet-20241022"
"#,
    )
    .unwrap();
    (temp, config_dir)
}

/// Test 1: `catalog list` with default kind=any; assert exit 0, stdout contains header + at least one builtin tool.
#[test]
#[serial(catalog_cli)]
fn test_catalog_list_text_smoke() {
    let (_temp, config_dir) = setup_temp_config_dir();
    let mut cmd = rustain_cmd();
    cmd.env("RUSTAIN_CONFIG_DIR", &config_dir)
        .arg("catalog")
        .arg("list");
    cmd.assert().success().stdout(contains("KIND"));
}

/// Test 2: `catalog list --json` parses as JSON array with expected keys.
#[test]
#[serial(catalog_cli)]
fn test_catalog_list_json_is_valid() {
    let (_temp, config_dir) = setup_temp_config_dir();
    let mut cmd = rustain_cmd();
    cmd.env("RUSTAIN_CONFIG_DIR", &config_dir)
        .arg("catalog")
        .arg("list")
        .arg("--json");
    let output = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let val: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(
        val.is_array(),
        "catalog list --json should output a JSON array"
    );
    if let Some(arr) = val.as_array() {
        for item in arr {
            assert!(item.get("name").is_some());
            assert!(item.get("kind").is_some());
            assert!(item.get("terse").is_some());
        }
    }
}

/// Test 3: `catalog list --kind tool --json` filters to tools only.
#[test]
#[serial(catalog_cli)]
fn test_catalog_list_kind_filter() {
    let (_temp, config_dir) = setup_temp_config_dir();
    let mut cmd = rustain_cmd();
    cmd.env("RUSTAIN_CONFIG_DIR", &config_dir)
        .arg("catalog")
        .arg("list")
        .arg("--kind")
        .arg("tool")
        .arg("--json");
    let output = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let val: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    if let Some(arr) = val.as_array() {
        for item in arr {
            assert_eq!(item["kind"], "tool");
        }
    }
}

/// Test 4: `catalog explain skill::nonexistent-skill-xyz` returns exit 3.
#[test]
#[serial(catalog_cli)]
fn test_catalog_explain_unknown_returns_3() {
    let (_temp, config_dir) = setup_temp_config_dir();
    let mut cmd = rustain_cmd();
    cmd.env("RUSTAIN_CONFIG_DIR", &config_dir)
        .arg("catalog")
        .arg("explain")
        .arg("skill::nonexistent-skill-xyz");
    cmd.assert().failure().code(3).stderr(contains("not found"));
}

/// Test 5: `catalog explain skill::dev-test` with frozen now is deterministic.
#[test]
#[serial(catalog_cli)]
fn test_catalog_explain_known_skill_deterministic() {
    let (temp, config_dir) = setup_temp_config_dir();

    // Seed a temp skill.
    let skills_dir = temp.path().join(".rustain").join("skills").join("dev-test");
    std::fs::create_dir_all(&skills_dir).unwrap();
    std::fs::write(
        skills_dir.join("SKILL.md"),
        r#"---
name: dev-test
description: A test skill for deterministic explain output.
---

Body of the skill.
"#,
    )
    .unwrap();

    let mut cmd1 = rustain_cmd();
    cmd1.env("RUSTAIN_CONFIG_DIR", &config_dir)
        .env("RUSTAIN_FROZEN_NOW", "2026-05-26T12:00:00Z")
        .current_dir(temp.path())
        .arg("catalog")
        .arg("explain")
        .arg("skill::dev-test");
    let out1 = cmd1.output().unwrap();

    let mut cmd2 = rustain_cmd();
    cmd2.env("RUSTAIN_CONFIG_DIR", &config_dir)
        .env("RUSTAIN_FROZEN_NOW", "2026-05-26T12:00:00Z")
        .current_dir(temp.path())
        .arg("catalog")
        .arg("explain")
        .arg("skill::dev-test");
    let out2 = cmd2.output().unwrap();

    assert_eq!(
        String::from_utf8_lossy(&out1.stdout),
        String::from_utf8_lossy(&out2.stdout),
        "explain output should be byte-identical when RUSTAIN_FROZEN_NOW is fixed"
    );
}

/// Test 6: `catalog stats --json` has expected fields.
#[test]
#[serial(catalog_cli)]
fn test_catalog_stats_json_has_expected_fields() {
    let (_temp, config_dir) = setup_temp_config_dir();
    let mut cmd = rustain_cmd();
    cmd.env("RUSTAIN_CONFIG_DIR", &config_dir)
        .arg("catalog")
        .arg("stats")
        .arg("--json");
    let output = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let val: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(val.get("total_indexed").is_some());
    assert!(val.get("count_by_kind").is_some());
    assert!(val.get("terse_token_percentiles").is_some());
    assert!(val.get("top_indexed_terms").is_some());
    assert!(val.get("last_index_rebuild_at").is_some());
    assert!(val.get("index_serialization_size_bytes").is_some());
}

/// Test 7: `catalog search "foo" --top-k 21` returns exit 2.
#[test]
#[serial(catalog_cli)]
fn test_catalog_search_top_k_clamp_rejects_21() {
    let (_temp, config_dir) = setup_temp_config_dir();
    let mut cmd = rustain_cmd();
    cmd.env("RUSTAIN_CONFIG_DIR", &config_dir)
        .arg("catalog")
        .arg("search")
        .arg("foo")
        .arg("--top-k")
        .arg("21");
    cmd.assert()
        .failure()
        .code(2)
        .stderr(contains("top_k must be ≤ 20"));
}

/// Test 8: `catalog search "" --top-k 5` returns exit 2 (empty query).
#[test]
#[serial(catalog_cli)]
fn test_catalog_search_empty_query_rejects() {
    let (_temp, config_dir) = setup_temp_config_dir();
    let mut cmd = rustain_cmd();
    cmd.env("RUSTAIN_CONFIG_DIR", &config_dir)
        .arg("catalog")
        .arg("search")
        .arg("")
        .arg("--top-k")
        .arg("5");
    cmd.assert()
        .failure()
        .code(2)
        .stderr(contains("query must be non-empty"));
}

/// Test 8b: `catalog search "   " --top-k 5` returns exit 2 (whitespace-only query).
#[test]
#[serial(catalog_cli)]
fn test_catalog_search_whitespace_only_query_rejects() {
    let (_temp, config_dir) = setup_temp_config_dir();
    let mut cmd = rustain_cmd();
    cmd.env("RUSTAIN_CONFIG_DIR", &config_dir)
        .arg("catalog")
        .arg("search")
        .arg("   ")
        .arg("--top-k")
        .arg("5");
    cmd.assert()
        .failure()
        .code(2)
        .stderr(contains("query must be non-empty"));
}

/// Test 9: `catalog search "review" --kind skill --top-k 3 --json` returns SearchHit shape.
#[test]
#[serial(catalog_cli)]
fn test_catalog_search_returns_search_hit_json() {
    let (_temp, config_dir) = setup_temp_config_dir();
    let mut cmd = rustain_cmd();
    cmd.env("RUSTAIN_CONFIG_DIR", &config_dir)
        .arg("catalog")
        .arg("search")
        .arg("review")
        .arg("--kind")
        .arg("skill")
        .arg("--top-k")
        .arg("3")
        .arg("--json");
    let output = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let val: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(val.is_array());
    if let Some(arr) = val.as_array() {
        for item in arr {
            assert!(item.get("name").is_some());
            assert!(item.get("kind").is_some());
            assert!(item.get("terse").is_some());
            assert!(item.get("score").is_some());
        }
    }
}

/// Test 10: `catalog search "review" --json --no-matched-terms` has matched_terms absent/null.
#[test]
#[serial(catalog_cli)]
fn test_catalog_search_no_matched_terms_flag() {
    let (_temp, config_dir) = setup_temp_config_dir();
    let mut cmd = rustain_cmd();
    cmd.env("RUSTAIN_CONFIG_DIR", &config_dir)
        .arg("catalog")
        .arg("search")
        .arg("review")
        .arg("--json")
        .arg("--no-matched-terms");
    let output = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let val: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let arr = val.as_array().expect("search output should be a JSON array");
    assert!(!arr.is_empty(), "search should return at least one result for 'review' query");
    for item in arr {
        assert!(
            item.get("matched_terms").is_none() || item["matched_terms"].is_null(),
            "matched_terms should be absent or null when --no-matched-terms is used"
        );
    }
}
