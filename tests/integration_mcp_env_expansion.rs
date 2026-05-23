//! Story 9.1 — environment-variable interpolation in workspace `.claude/mcp.json`.
//!
//! `docs/mcp.md` promises that both `$VAR` and `${VAR}` are expanded from the
//! rustain process environment at spawn time, in `command`, `args`, and `env`.
//! Existing unit tests cover `expand_env_vars` in isolation; this test wires
//! the real `parse_workspace_mcp_config` path so a regression in the call-site
//! (e.g. forgetting to expand `args`) is caught.
//!
//! Risk closed:
//!   * R9 — `$VAR` / `${VAR}` expansion at the workspace-config layer
//!
//! The test sets process-level env vars with a unique prefix to avoid
//! collisions with other tests in the same `cargo test` run.

#![cfg(feature = "mcp")]

use std::path::Path;

use rustain::adapters::mcp::workspace_config::parse_workspace_mcp_config;

// Unique-per-test env keys so xdist / parallel tests don't trample each other.
const VAR_COMMAND: &str = "RUSTAIN_R9_FAKE_COMMAND";
const VAR_ARG: &str = "RUSTAIN_R9_FAKE_ARG";
const VAR_ENV_VAL: &str = "RUSTAIN_R9_FAKE_ENV";

/// Test-only env scope. The `expand_env_vars` impl reads via `std::env::var`
/// which is process-global; setting/unsetting around each test is the only
/// way to keep cases isolated. Single-threaded `#[test]` is fine — we don't
/// mark it `#[tokio::test]`.
struct EnvGuard {
    keys: Vec<&'static str>,
}

impl EnvGuard {
    fn set(pairs: &[(&'static str, &str)]) -> Self {
        let keys = pairs.iter().map(|(k, _)| *k).collect();
        for (k, v) in pairs {
            // SAFETY: tests are single-threaded re env mutations.
            unsafe { std::env::set_var(k, v) };
        }
        Self { keys }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for k in &self.keys {
            unsafe { std::env::remove_var(k) };
        }
    }
}

fn write_mcp_json(dir: &Path, body: &str) {
    let claude = dir.join(".claude");
    std::fs::create_dir_all(&claude).expect("mkdir .claude");
    std::fs::write(claude.join("mcp.json"), body).expect("write mcp.json");
}

#[test]
fn r9_env_vars_expand_in_command_args_and_env_at_parse_time() {
    let _g = EnvGuard::set(&[
        (VAR_COMMAND, "/usr/bin/python3"),
        (VAR_ARG, "echo_mcp.py"),
        (VAR_ENV_VAL, "production-token-xyz"),
    ]);

    let tmp = tempfile::tempdir().expect("tempdir");
    // Build the JSON via a serde_json::Value so we don't fight format-string
    // brace-escaping. The point is to write the literal forms `${VAR}` and
    // `$VAR` into the file — the expander runs on parse, not on JSON load.
    let body = serde_json::json!({
        "mcpServers": {
            "expansion-test": {
                "command": format!("${{{}}}", VAR_COMMAND),
                "args": [
                    format!("--script=${{{}}}", VAR_ARG),
                    "$HOME",
                ],
                "env": {
                    "SECRET_TOKEN": format!("${}", VAR_ENV_VAL),
                }
            }
        }
    })
    .to_string();
    write_mcp_json(tmp.path(), &body);

    let specs = parse_workspace_mcp_config(&tmp.path().join(".claude").join("mcp.json"))
        .expect("parse should succeed");
    assert_eq!(specs.len(), 1, "expected one server, got {}", specs.len());
    let spec = &specs[0];

    // ${VAR} form in command.
    assert_eq!(
        spec.command.as_deref(),
        Some("/usr/bin/python3"),
        "command should have ${{{}}} expanded",
        VAR_COMMAND
    );

    // ${VAR} form in args (mixed with literal) — must expand the variable
    // and leave the prefix as-is.
    assert!(
        spec.args.iter().any(|a| a == "--script=echo_mcp.py"),
        "args should contain --script=<expanded>; got {:?}",
        spec.args
    );

    // $VAR form (no braces) — must also resolve. We tested this via $HOME
    // which is set on every reasonable test host; assert the resolved path
    // starts with `/` or is non-empty (HOME might be set differently in CI).
    let home_arg = spec
        .args
        .iter()
        .find(|a| !a.starts_with("--script="))
        .expect("second arg present");
    assert!(
        !home_arg.is_empty() && home_arg != "$HOME",
        "$HOME should have been expanded; got literal {:?}",
        home_arg
    );

    // env values undergo expansion too.
    let token = spec
        .env
        .get("SECRET_TOKEN")
        .expect("env.SECRET_TOKEN present");
    assert_eq!(
        token, "production-token-xyz",
        "env value should have $VAR expanded"
    );
}

#[test]
fn r9_unknown_env_var_is_left_literal_with_warning_not_error() {
    let _g = EnvGuard::set(&[]);
    // SAFETY: ensure the var is absent for this test.
    unsafe { std::env::remove_var("RUSTAIN_R9_DEFINITELY_UNSET") };

    let tmp = tempfile::tempdir().expect("tempdir");
    write_mcp_json(
        tmp.path(),
        r#"{
          "mcpServers": {
            "missing-var": {
              "command": "${RUSTAIN_R9_DEFINITELY_UNSET}",
              "args": []
            }
          }
        }"#,
    );

    let specs = parse_workspace_mcp_config(&tmp.path().join(".claude").join("mcp.json"))
        .expect("parse should succeed even with missing env vars");
    assert_eq!(specs.len(), 1);
    // Unknown vars are preserved verbatim per docs/mcp.md.
    assert_eq!(
        specs[0].command.as_deref(),
        Some("${RUSTAIN_R9_DEFINITELY_UNSET}"),
        "unknown env var should remain literal (warning-only path)"
    );
}
