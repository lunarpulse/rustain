//! Conformance tests for MCP configuration layer precedence (AC-1).
//!
//! Verifies that workspace `.claude/mcp.json` entries override profile
//! `[tools.config.mcp.*]` entries by server name, and distinct names are additive.

use rustain::domain::models::{McpServerSource, McpServerSpec, McpTransport};
use std::collections::BTreeMap;

#[test]
fn test_mcp_config_layer_precedence_workspace_wins() {
    let profile_spec = McpServerSpec {
        id: "postgres".into(),
        transport: McpTransport::Stdio,
        command: Some("profile-cmd".into()),
        args: vec![],
        env: BTreeMap::new(),
        url: None,
        persistent: false,
        source: McpServerSource::Profile {
            profile_name: "coding".into(),
        },
    };

    let workspace_spec = McpServerSpec {
        id: "postgres".into(),
        transport: McpTransport::Stdio,
        command: Some("workspace-cmd".into()),
        args: vec!["--ws".into()],
        env: BTreeMap::new(),
        url: None,
        persistent: false,
        source: McpServerSource::Workspace,
    };

    let merged = rustain::adapters::mcp::merge_mcp_specs(vec![workspace_spec], vec![profile_spec]);

    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].command.as_deref(), Some("workspace-cmd"));
    assert_eq!(merged[0].source, McpServerSource::Workspace);
}

#[test]
fn test_mcp_config_layer_precedence_additive() {
    let profile_spec = McpServerSpec {
        id: "postgres".into(),
        transport: McpTransport::Stdio,
        command: Some("pg-cmd".into()),
        args: vec![],
        env: BTreeMap::new(),
        url: None,
        persistent: false,
        source: McpServerSource::Profile {
            profile_name: "coding".into(),
        },
    };

    let workspace_spec = McpServerSpec {
        id: "git".into(),
        transport: McpTransport::Stdio,
        command: Some("git-cmd".into()),
        args: vec![],
        env: BTreeMap::new(),
        url: None,
        persistent: false,
        source: McpServerSource::Workspace,
    };

    let merged = rustain::adapters::mcp::merge_mcp_specs(vec![workspace_spec], vec![profile_spec]);

    assert_eq!(merged.len(), 2);
    let ids: Vec<_> = merged.iter().map(|s| s.id.as_str()).collect();
    assert!(ids.contains(&"postgres"));
    assert!(ids.contains(&"git"));
}

#[test]
fn test_mcp_config_layer_precedence_empty() {
    let merged = rustain::adapters::mcp::merge_mcp_specs(vec![], vec![]);
    assert!(merged.is_empty());
}

// ─────────────────────────────────────────────────────────────────────────────
// Story 9.9 AC2 — both config paths carry the URL, `command` is optional, and a
// malformed HTTP entry fails LOUDLY instead of deleting the file's other
// servers.
//
// 🔴 The central regression pin is `ac2_shape_a_*` below. Ruling A1 measured
// what actually happened on 2026-08-30: `McpJsonServer.command` was a required,
// non-`Option` `String` and the whole file is one `serde_json::from_str`, so the
// natural Claude Code HTTP entry — no `command` — failed the ENTIRE file with
// ``missing field `command` `` and `toml_resolver.rs` swallowed it into a log
// line. The operator lost every MCP server in `.claude/mcp.json`, healthy stdio
// ones included, and was told nothing.
// ─────────────────────────────────────────────────────────────────────────────

use rustain::adapters::mcp::workspace_config::parse_workspace_mcp_config;

/// Serializes the one test that must move the process cwd (the resolver reads
/// `./.claude/mcp.json` through `std::env::current_dir()`).
static CWD_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn write_workspace_mcp(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
    let claude = dir.join(".claude");
    std::fs::create_dir_all(&claude).expect("create .claude");
    let path = claude.join("mcp.json");
    std::fs::write(&path, body).expect("write mcp.json");
    path
}

#[test]
fn ac2_shape_a_http_entry_without_a_command_parses_and_keeps_its_stdio_sibling() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = write_workspace_mcp(
        tmp.path(),
        r#"{
          "mcpServers": {
            "git": { "command": "uvx", "args": ["mcp-server-git"] },
            "remote": { "type": "http", "url": "http://127.0.0.1:13001/mcp" }
          }
        }"#,
    );

    let specs = parse_workspace_mcp_config(&path)
        .expect("MUTANT: revert `command` to a required String and this whole file fails to parse");

    let ids: Vec<&str> = specs.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["git", "remote"],
        "the healthy stdio sibling must survive an http entry that carries no command"
    );

    let remote = specs.iter().find(|s| s.id == "remote").expect("remote");
    assert_eq!(remote.transport, McpTransport::Http);
    assert!(
        remote.command.is_none(),
        "an http-only entry has no command, and that is legal now"
    );
    assert_eq!(
        remote.url.as_ref().map(|u| u.expose_url()),
        Some("http://127.0.0.1:13001/mcp"),
        "MUTANT: drop the `url` read and the spec cannot dial"
    );
    assert!(remote.validate_transport_fields().is_ok());

    let git = specs.iter().find(|s| s.id == "git").expect("git");
    assert_eq!(git.command.as_deref(), Some("uvx"));
    assert_eq!(git.transport, McpTransport::Stdio);
}

#[test]
fn ac2_shape_b_http_entry_with_a_dummy_command_also_carries_its_url() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = write_workspace_mcp(
        tmp.path(),
        r#"{
          "mcpServers": {
            "remote": { "command": "unused", "type": "http", "url": "https://mcp.example.com/mcp" }
          }
        }"#,
    );
    let specs = parse_workspace_mcp_config(&path).expect("shape B always parsed");
    assert_eq!(
        specs[0].url.as_ref().map(|u| u.expose_url()),
        Some("https://mcp.example.com/mcp"),
        "shape B parsed before 9.9 but dropped the url on the floor"
    );
}

#[test]
fn ac2_url_is_env_expanded_through_the_same_helper_as_command() {
    // Unique key so parallel targets cannot trample it.
    const KEY: &str = "RUSTAIN_9_9_MCP_URL";
    // SAFETY: the key is unique to this test and set before any read of it.
    unsafe { std::env::set_var(KEY, "http://127.0.0.1:13001/mcp") };

    let tmp = tempfile::tempdir().expect("tempdir");
    let path = write_workspace_mcp(
        tmp.path(),
        r#"{
          "mcpServers": {
            "expanded": { "type": "http", "url": "${RUSTAIN_9_9_MCP_URL}" },
            "unset": { "type": "http", "url": "${RUSTAIN_9_9_MCP_URL_ABSENT}/mcp" }
          }
        }"#,
    );
    let specs = parse_workspace_mcp_config(&path).expect("parse");

    let expanded = specs.iter().find(|s| s.id == "expanded").expect("expanded");
    assert_eq!(
        expanded.url.as_ref().map(|u| u.expose_url()),
        Some("http://127.0.0.1:13001/mcp"),
        "MUTANT: skip url expansion and the client dials the literal ${{VAR}} string"
    );

    // Unset vars keep the same semantics as command/args/env: the literal is
    // preserved plus a warning, never an error.
    let unset = specs.iter().find(|s| s.id == "unset").expect("unset");
    assert_eq!(
        unset.url.as_ref().map(|u| u.expose_url()),
        Some("${RUSTAIN_9_9_MCP_URL_ABSENT}/mcp")
    );
    // SAFETY: as above.
    unsafe { std::env::remove_var(KEY) };
}

#[test]
fn ac2_a_bad_entry_degrades_per_entry_and_never_deletes_its_siblings() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = write_workspace_mcp(
        tmp.path(),
        r#"{
          "mcpServers": {
            "healthy": { "command": "uvx" },
            "no-url": { "type": "http" },
            "bad-url": { "type": "http", "url": "notaurl" },
            "no-command": { "type": "stdio" }
          }
        }"#,
    );
    let specs = parse_workspace_mcp_config(&path).expect("a bad ENTRY is never a file failure");

    let ids: Vec<&str> = specs.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["bad-url", "healthy", "no-command", "no-url"],
        "every entry survives, including the broken ones — the fault surfaces at connect \
         as ConnectionFailed, it does not remove the row"
    );

    let verdicts: Vec<(&str, bool)> = specs
        .iter()
        .map(|s| (s.id.as_str(), s.validate_transport_fields().is_ok()))
        .collect();
    assert_eq!(
        verdicts,
        vec![
            ("bad-url", false),
            ("healthy", true),
            ("no-command", false),
            ("no-url", false)
        ],
        "the shared transport↔field gate must name each fault: {verdicts:?}"
    );

    // ⛔ `RedactedUrl::new` is infallible and validates nothing — the parse
    // check has to be explicit, and it is.
    let bad = specs.iter().find(|s| s.id == "bad-url").expect("bad-url");
    let message = bad.validate_transport_fields().expect_err("must fail");
    assert!(
        message.contains("not a valid URL"),
        "the message must say what is wrong: {message}"
    );
}

#[test]
fn ac2_profile_toml_path_reads_url_into_the_same_field() {
    let toml_str = r#"
        [mcp.remote]
        transport = "http"
        url = "http://127.0.0.1:13001/mcp"
    "#;
    let value: toml::Value = toml::from_str(toml_str).expect("toml");
    let specs =
        rustain::adapters::mcp::profile_config::extract_profile_mcp_servers(Some(&value), "coding");
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].transport, McpTransport::Http);
    assert!(
        specs[0].command.is_none(),
        "the profile path already tolerated a missing command; it dropped the url instead"
    );
    assert_eq!(
        specs[0].url.as_ref().map(|u| u.expose_url()),
        Some("http://127.0.0.1:13001/mcp"),
        "third failure shape, same fix"
    );
}

/// ⚑ Ruling A17's paired keystone. **Neither half suffices alone:** "rustain
/// still starts" already passes today (silently), and "the notice fired" is
/// exactly what today fails.
///
/// Front door: `TomlProfileResolver::new` — the production constructor whose
/// `unwrap_or_else(… Vec::new())` is the swallow this AC replaces. The drained
/// notices are pushed into `startup.rs`'s `accumulated_notices`, which is
/// emitted through `event_bus.emit_domain(AppEvent::SystemNotice { level:
/// Warning, .. })`.
///
/// MUTANT: make either shape fatal (`ProfileError::Parse`, matching the a2a
/// sibling 15 lines above it) and the `expect` below turns RED — and Story 9.1's
/// Decision Gate 1.5 breaks with it.
#[test]
fn ac2_a_syntactically_broken_mcp_config_still_starts_and_surfaces_a_notice() {
    let _serialized = CWD_GUARD.lock().expect("cwd guard");
    let tmp = tempfile::tempdir().expect("tempdir");
    write_workspace_mcp(tmp.path(), "{ this is not json");
    let profiles_dir = tmp.path().join("profiles");
    std::fs::create_dir_all(&profiles_dir).expect("profiles dir");

    let original = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(tmp.path()).expect("enter workspace");
    let resolved = rustain::adapters::profile_resolver::toml_resolver::TomlProfileResolver::new(
        "coding",
        profiles_dir,
    );
    std::env::set_current_dir(&original).expect("restore cwd");

    let mut resolver = resolved.expect(
        "⛔ NEVER FATAL: a broken .claude/mcp.json must not stop rustain starting in the \
         very workspace the operator is editing it from (A17)",
    );
    let notices = resolver.take_mcp_config_notices();
    assert_eq!(
        notices.len(),
        1,
        "the whole-file failure must be SURFACED, not swallowed into ~/.rustain/rustain.log"
    );
    assert!(
        notices[0].contains("mcp.json") && notices[0].contains("no MCP servers"),
        "the notice must name the file and say what the operator lost: {:?}",
        notices[0]
    );
    assert!(
        resolver.take_mcp_config_notices().is_empty(),
        "draining is once — a second flush must not duplicate the notice"
    );
}
