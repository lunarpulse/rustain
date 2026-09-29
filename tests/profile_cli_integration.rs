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
use rustain::adapters::cli::commands::{Cli, ProfileAction};

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

// ── Story 19.12 AC1(f) — EXECUTING coverage for `profile install` ───────────
//
// ⚠ Before this block, `grep -n install` in this file returned ZERO hits, and
// `tests/conformance_profile_cli.rs`'s `test_profile_help_lists_all_verbs`
// checks eight verb substrings and omits `install`. The existing suite could
// not catch a regression in the verb Story 19.12 changes — which is why these
// tests run the binary instead of parsing a clap tree. ⛔ A clap-parse test is
// not coverage for a write path.
//
// Every test is `RUSTAIN_CONFIG_DIR`-isolated; the file header already requires
// `--test-threads=1`.

/// The shared artifact PRD Journey 8 hands to a teammate: hand-authored and
/// partial, so `extends` has something to do.
const SHARED_PROFILE: &str =
    "name = \"devops\"\nextends = \"coding\"\n\n[scheduler]\nadapter = \"none\"\n";

#[test]
fn install_from_a_local_path_lands_in_the_user_profiles_dir() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    let shared = tmp.path().join("devops.toml");
    std::fs::write(&shared, SHARED_PROFILE).unwrap();

    let output = Command::cargo_bin("rustain")
        .unwrap()
        .env("RUSTAIN_CONFIG_DIR", &config_dir)
        .args(["profile", "install"])
        .arg(&shared)
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "install of a local path should succeed; stdout={stdout} stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    // `import`'s own success line: the local-path arm DELEGATES. A second
    // read/parse/write inside `install.rs` would print install's wording.
    assert!(
        stdout.contains("Profile 'devops' imported."),
        "expected import's success line, got: {stdout}"
    );
    // ⛔ profiles/, where `import` writes — NOT profiles/community/, where the
    // `gh:` route writes.
    assert!(
        config_dir.join("profiles").join("devops.toml").is_file(),
        "the profile should land in profiles/, not community/"
    );
    assert!(
        !config_dir.join("profiles").join("community").exists(),
        "the local-path arm must not create the community/ directory"
    );
}

#[test]
fn install_refuses_a_name_that_collides_with_a_builtin() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    let shadow = tmp.path().join("coding.toml");
    std::fs::write(&shadow, "name = \"coding\"\nextends = \"base\"\n").unwrap();

    let output = Command::cargo_bin("rustain")
        .unwrap()
        .env("RUSTAIN_CONFIG_DIR", &config_dir)
        .args(["profile", "install"])
        .arg(&shadow)
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "stderr={stderr}");
    assert!(
        stderr.contains("collides with a built-in profile"),
        "expected the collision refusal, got: {stderr}"
    );
    assert!(
        !config_dir.join("profiles").join("coding.toml").exists(),
        "a refused install must not write the file it refused"
    );
}

#[test]
fn import_keeps_accepting_a_name_that_collides_with_a_builtin() {
    // The other half of the same guard: `import` is "load MY file" and its
    // shipped behaviour is UNCHANGED. Without this, nothing stops a future
    // author from moving the guard into the shared path and breaking a
    // scripted verb.
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    let shadow = tmp.path().join("coding.toml");
    std::fs::write(&shadow, "name = \"coding\"\nextends = \"base\"\n").unwrap();

    let output = Command::cargo_bin("rustain")
        .unwrap()
        .env("RUSTAIN_CONFIG_DIR", &config_dir)
        .args(["profile", "import"])
        .arg(&shadow)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "import must still accept a built-in name; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(config_dir.join("profiles").join("coding.toml").is_file());
}

#[test]
fn both_verbs_refuse_a_profile_whose_own_name_escapes_the_config_dir() {
    // 🔴 The shipped defect: `import.rs` validated only the `--name` override,
    // so `name = "../../ESCAPED"` wrote OUTSIDE RUSTAIN_CONFIG_DIR at exit 0
    // with a cheerful success line.
    //
    // ⚠ The fixture NEEDS `extends`: a bare `name = …` fails port validation
    // first ("missing required dimensions", exit 2), and a test without it
    // would pass for the wrong reason.
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("deep").join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    let evil = tmp.path().join("evil.toml");
    std::fs::write(&evil, "name = \"../../ESCAPED\"\nextends = \"coding\"\n").unwrap();
    // Where the escape would land: `<config>/profiles/../../ESCAPED.toml`.
    let escaped = tmp.path().join("deep").join("ESCAPED.toml");

    for verb in ["install", "import"] {
        let output = Command::cargo_bin("rustain")
            .unwrap()
            .env("RUSTAIN_CONFIG_DIR", &config_dir)
            .args(["profile", verb])
            .arg(&evil)
            .output()
            .unwrap();

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(2),
            "`profile {verb}` should refuse a traversal name; stderr={stderr}"
        );
        assert!(
            stderr.contains("path traversal attempt"),
            "`profile {verb}` should say why it refused, got: {stderr}"
        );
        assert!(
            !escaped.exists(),
            "`profile {verb}` wrote outside the config dir at {}",
            escaped.display()
        );
    }
}

#[test]
fn install_with_a_gh_spec_still_reports_its_own_spec_errors() {
    // A6: the branch predicate is `spec.starts_with("gh:")`, never
    // `parse_gh_spec`'s error VARIANT — which returns `UnsupportedScheme` for
    // both a non-`gh:` spec AND an empty `gh:` body. Branching on the variant
    // would send these down the delegated path, where a missing file is an
    // `anyhow::Err` routed to `tracing::error!`: exit 1, ZERO bytes on both
    // streams. So the discriminator is PRESENCE of the message.
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();

    for (spec, needle) in [
        ("gh:", "only 'gh:' scheme supported in v1"),
        ("gh:owner", "missing user or repo in 'gh:owner'"),
    ] {
        let output = Command::cargo_bin("rustain")
            .unwrap()
            .env("RUSTAIN_CONFIG_DIR", &config_dir)
            .args(["profile", "install", spec])
            .output()
            .unwrap();

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "spec={spec} stderr={stderr}");
        assert!(
            stderr.contains(needle),
            "spec={spec} should still print its gh-spec error, got: {stderr}"
        );
    }
}

#[test]
fn install_from_stdin_is_refused_with_a_visible_message() {
    // Task 1's recorded decision for A22's `install -` item: `-` is rejected
    // rather than delegated. `import -` without `--name` `bail!`s into an
    // `anyhow::Err` — exit 1, zero bytes on both streams — and adding a surface
    // whose failure mode is silence is the class this story fixes elsewhere.
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();

    let output = Command::cargo_bin("rustain")
        .unwrap()
        .env("RUSTAIN_CONFIG_DIR", &config_dir)
        .args(["profile", "install", "-"])
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "stderr={stderr}");
    assert!(
        stderr.contains("rustain profile import -"),
        "the refusal should name the verb that DOES read stdin, got: {stderr}"
    );
}

#[test]
fn profile_create_without_a_terminal_refuses_out_loud() {
    // Story 19.12 A18. This used to be `anyhow::bail!` -> `tracing::error!`:
    // exit 1 with ZERO bytes on stdout AND stderr, so the precise refusal the
    // code composed reached nobody. ⚠ Named behaviour change: exit 1 -> 2.
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();

    let output = Command::cargo_bin("rustain")
        .unwrap()
        .env("RUSTAIN_CONFIG_DIR", &config_dir)
        .args(["profile", "create", "--name", "unattended"])
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "stderr={stderr}");
    assert!(
        stderr.contains("requires an interactive terminal"),
        "expected the TTY refusal on stderr, got: {stderr}"
    );
    assert!(
        !config_dir.join("profiles").join("unattended.toml").exists(),
        "a refused create must not write a profile"
    );
}

#[test]
fn install_missing_local_path_refuses_with_a_visible_message() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    let missing = tmp.path().join("missing.toml");

    let output = Command::cargo_bin("rustain")
        .unwrap()
        .env("RUSTAIN_CONFIG_DIR", &config_dir)
        .args(["profile", "install"])
        .arg(&missing)
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "stderr={stderr}");
    assert!(
        stderr.contains("Failed to read file"),
        "a bad local source must explain the refusal, got: {stderr}"
    );
}

#[test]
fn local_install_honors_strict_features() {
    let tmp = tempfile::TempDir::new().unwrap();
    let profile = tmp.path().join("cron.toml");
    std::fs::write(
        &profile,
        "name = \"local-cron\"\nextends = \"coding\"\n\n[scheduler]\nadapter = \"cron\"\n",
    )
    .unwrap();

    let permissive_config = tmp.path().join("permissive");
    std::fs::create_dir_all(&permissive_config).unwrap();
    let permissive = Command::cargo_bin("rustain")
        .unwrap()
        .env("RUSTAIN_CONFIG_DIR", &permissive_config)
        .args(["profile", "install"])
        .arg(&profile)
        .output()
        .unwrap();
    let permissive_stderr = String::from_utf8_lossy(&permissive.stderr);
    assert!(
        permissive.status.success(),
        "default local install should fall back to preview; stderr={permissive_stderr}"
    );
    let installed =
        std::fs::read_to_string(permissive_config.join("profiles/local-cron.toml")).unwrap();
    assert!(
        installed.contains("preview = true"),
        "feature fallback must persist preview mode: {installed}"
    );

    let strict_config = tmp.path().join("strict");
    std::fs::create_dir_all(&strict_config).unwrap();
    let strict = Command::cargo_bin("rustain")
        .unwrap()
        .env("RUSTAIN_CONFIG_DIR", &strict_config)
        .args(["profile", "install", "--strict-features"])
        .arg(&profile)
        .output()
        .unwrap();
    let strict_stderr = String::from_utf8_lossy(&strict.stderr);
    assert_eq!(strict.status.code(), Some(2), "stderr={strict_stderr}");
    assert!(
        strict_stderr.contains("requires cargo feature 'cron'"),
        "strict local install must preserve the feature error: {strict_stderr}"
    );
    assert!(
        !strict_config.join("profiles/local-cron.toml").exists(),
        "strict refusal must not write the profile"
    );
}
