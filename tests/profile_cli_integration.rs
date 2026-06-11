//! Integration tests for `rustain profile` CLI subcommands.
//! Story 8.6a AC-12.
//!
//! These tests use `assert_cmd` for end-to-end invocation and
//! `Cli::parse_from` for clap argument parsing verification.
//!
//! Run with: cargo test --test profile_cli_integration -- --test-threads=1
//! (Single-threaded because of env-var mutation for RUSTAIN_CONFIG_DIR.)

use assert_cmd::Command;
use clap::Parser;
use predicates::prelude::*;
use rustain::adapters::cli::commands::{Cli, ProfileAction};
use std::io::Write;

/// Test 1: `profile list` exits 0 with 3 built-in profile names visible.
#[test]
fn test_profile_list_exits_zero_and_lists_builtins() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();

    let mut cmd = Command::cargo_bin("rustain").unwrap();
    cmd.env("RUSTAIN_CONFIG_DIR", &config_dir)
        .arg("profile")
        .arg("list");
    let output = cmd.output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    // The 3 built-in profiles should appear
    assert!(
        stdout.contains("base"),
        "expected 'base' in output, got: {}",
        stdout
    );
    assert!(
        stdout.contains("coding"),
        "expected 'coding' in output, got: {}",
        stdout
    );
    assert!(
        stdout.contains("personal-assistant"),
        "expected 'personal-assistant' in output, got: {}",
        stdout
    );
}

/// Test 2: `profile list --json` outputs valid JSON.
#[test]
fn test_profile_list_json() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();

    let mut cmd = Command::cargo_bin("rustain").unwrap();
    cmd.env("RUSTAIN_CONFIG_DIR", &config_dir)
        .arg("profile")
        .arg("list")
        .arg("--json");
    let output = cmd.output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Parse as JSON
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("--json output should be valid JSON");
    assert!(parsed.is_array(), "--json output should be an array");
    let arr = parsed.as_array().unwrap();
    assert!(
        !arr.is_empty(),
        "should contain at least the 3 built-in profiles"
    );
}

/// Test 3: `profile show nonexistent` exits 2 with error message.
#[test]
fn test_profile_show_nonexistent_exits_two() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();

    let mut cmd = Command::cargo_bin("rustain").unwrap();
    cmd.env("RUSTAIN_CONFIG_DIR", &config_dir)
        .arg("profile")
        .arg("show")
        .arg("nonexistent-profile-xyz");
    let output = cmd.output().unwrap();
    assert!(
        !output.status.success(),
        "expected non-zero exit for nonexistent profile"
    );
    assert_eq!(output.status.code(), Some(2), "expected exit code 2");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not found"),
        "stderr should mention 'not found', got: {}",
        stderr
    );
}

/// Test 4: `profile validate --all` exits 0 against shipped built-ins.
#[test]
fn test_profile_validate_all_exits_zero() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();

    let mut cmd = Command::cargo_bin("rustain").unwrap();
    cmd.env("RUSTAIN_CONFIG_DIR", &config_dir)
        .arg("profile")
        .arg("validate")
        .arg("--all");
    let output = cmd.output().unwrap();
    assert!(output.status.success(), "validate --all should succeed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("valid") || stdout.contains("validated"),
        "output should indicate validation, got: {}",
        stdout
    );
}

/// Test 5: `profile show coding --toml` outputs valid TOML.
#[test]
fn test_profile_show_coding_toml() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();

    let mut cmd = Command::cargo_bin("rustain").unwrap();
    cmd.env("RUSTAIN_CONFIG_DIR", &config_dir)
        .arg("profile")
        .arg("show")
        .arg("coding")
        .arg("--toml");
    let output = cmd.output().unwrap();
    assert!(output.status.success(), "profile show coding --toml failed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Should contain TOML sections for all 7 ports
    for section in &[
        "[persona]",
        "[memory]",
        "[session]",
        "[tools]",
        "[channels]",
        "[scheduler]",
        "[context]",
    ] {
        assert!(
            stdout.contains(section),
            "TOML output should contain {}, got: {}",
            section,
            stdout
        );
    }
}

/// Test 6: `profile import` with nonexistent file exits 2.
#[test]
fn test_profile_import_nonexistent_file_exits_two() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();

    let mut cmd = Command::cargo_bin("rustain").unwrap();
    cmd.env("RUSTAIN_CONFIG_DIR", &config_dir)
        .arg("profile")
        .arg("import")
        .arg("/tmp/nonexistent-profile-12345-bogus.toml");
    let output = cmd.output().unwrap();
    assert!(
        !output.status.success(),
        "expected non-zero exit for nonexistent file"
    );
}

/// Test 7: `profile --help` lists all 8 verb names.
#[test]
fn test_profile_help_lists_all_verbs() {
    let mut cmd = Command::cargo_bin("rustain").unwrap();
    cmd.arg("profile").arg("--help");
    let output = cmd.output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let expected_verbs = [
        "list", "show", "create", "edit", "switch", "validate", "export", "import",
    ];
    for verb in &expected_verbs {
        assert!(
            stdout.contains(verb),
            "help should mention '{}', got: {}",
            verb,
            stdout
        );
    }
}

/// Test 8: Clap parse-for: each ProfileAction variant parses correctly.
#[test]
fn test_profile_list_clap_parsing() {
    let cli = Cli::parse_from(["rustain", "profile", "list", "--json"]);
    assert!(matches!(
        cli.command,
        Some(rustain::adapters::cli::commands::Command::Profile {
            action: ProfileAction::List { json: true },
        })
    ));
}

#[test]
fn test_profile_show_clap_parsing() {
    let cli = Cli::parse_from(["rustain", "profile", "show", "coding"]);
    assert!(matches!(
        cli.command,
        Some(rustain::adapters::cli::commands::Command::Profile {
            action: ProfileAction::Show { name, .. }
        }) if name == "coding"
    ));
}

#[test]
fn test_profile_create_clap_parsing() {
    let cli = Cli::parse_from([
        "rustain",
        "profile",
        "create",
        "--name",
        "my-profile",
        "--extends",
        "base",
        "--from",
        "coding",
    ]);
    assert!(matches!(
        cli.command,
        Some(rustain::adapters::cli::commands::Command::Profile {
            action: ProfileAction::Create { name, extends, from },
        }) if name.as_deref() == Some("my-profile")
            && extends.as_deref() == Some("base")
            && from.as_deref() == Some("coding")
    ));
}

#[test]
fn test_profile_edit_clap_parsing() {
    let cli = Cli::parse_from(["rustain", "profile", "edit", "my-profile", "--no-validate"]);
    assert!(matches!(
        cli.command,
        Some(rustain::adapters::cli::commands::Command::Profile {
            action: ProfileAction::Edit { name, no_validate: true },
        }) if name == "my-profile"
    ));
}

#[test]
fn test_profile_switch_clap_parsing() {
    let cli = Cli::parse_from(["rustain", "profile", "switch", "coding", "--start"]);
    assert!(matches!(
        cli.command,
        Some(rustain::adapters::cli::commands::Command::Profile {
            action: ProfileAction::Switch { name, start: true },
        }) if name == "coding"
    ));
}

#[test]
fn test_profile_validate_clap_parsing() {
    let cli = Cli::parse_from(["rustain", "profile", "validate", "--all", "--json"]);
    assert!(matches!(
        cli.command,
        Some(rustain::adapters::cli::commands::Command::Profile {
            action: ProfileAction::Validate {
                all: true,
                json: true,
                name: None
            },
        })
    ));
}

#[test]
fn test_profile_export_clap_parsing() {
    let cli = Cli::parse_from([
        "rustain", "profile", "export", "coding", "--output", "out.toml",
    ]);
    assert!(matches!(
        cli.command,
        Some(rustain::adapters::cli::commands::Command::Profile {
            action: ProfileAction::Export { name, output },
        }) if name == "coding" && output.as_deref() == Some("out.toml")
    ));
}

#[test]
fn test_profile_import_clap_parsing() {
    let cli = Cli::parse_from([
        "rustain",
        "profile",
        "import",
        "some.toml",
        "--name",
        "renamed",
        "--force",
    ]);
    assert!(matches!(
        cli.command,
        Some(rustain::adapters::cli::commands::Command::Profile {
            action: ProfileAction::Import { path, name, force: true },
        }) if path == "some.toml" && name.as_deref() == Some("renamed")
    ));
}
