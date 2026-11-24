#![cfg(unix)]
//! Story Q-6 AC1 — ambient credentials never reach a Bash or MCP stdio child, and
//! an explicit stdio reference to an app-owned credential is refused before the
//! value is looked up (owner decision 2026-11-19, `mcp_app_credentials=deny`).
//!
//! Every witness reports fixture case IDs and `present`/`absent` only: no
//! environment dump, value or digest. All values are synthetic placeholders.
//!
//! Doors exercised:
//! - K1a — `ToolScheduler` → real `SecurityAdapter` → real `ToolSetAdapter` Bash;
//! - K1b — `.claude/mcp.json` / profile TOML resolution → `CompositeToolsetAdapter::
//!   start_mcp_connections` → `McpClientAdapter::connect` → `TaskGuardTransport::spawn`
//!   with the local `fake-mcp-server` fixture (`test-fake-mcp`);
//! - K1c — the same composite spawn inside a real daemon (`conformance_daemon_mcp`).

use std::path::Path;
use std::sync::Arc;

use rustain::adapters::mcp::profile_config::extract_profile_mcp_servers;
use rustain::adapters::mcp::workspace_config::parse_workspace_mcp_config;
use rustain::adapters::security_adapter::SecurityAdapter;
use rustain::adapters::toolset_adapter::ToolSetAdapter;
use rustain::domain::models::tool_call::{ApprovalSource, ToolCallRequest};
use rustain::domain::models::{McpServerSpec, PermissionMode, ProvenanceTag, ToolCall};
use rustain::domain::ports::{SecurityPort, ToolSetPort};
use rustain::domain::services::app_credentials::register_configured_credential_names;
use rustain::domain::services::approval_runtime::ApprovalRuntime;
use rustain::domain::services::tool_scheduler::ToolScheduler;
use tokio_util::sync::CancellationToken;

/// One synthetic placeholder per category the policy must cover.
const PROTECTED: &[(&str, &str)] = &[
    ("default-name", "OPENAI_API_KEY"),
    ("default-alias", "GEMINI_API_KEY"),
    ("channel-telegram", "TELEGRAM_BOT_TOKEN"),
    ("configured-custom", "Q6_K1_CONFIGURED_PROVIDER_KEY"),
    ("mcp-auth-parent-only", "RUSTAIN_MCP_HTTP_AUTH_TOKEN_Q6K1"),
];
const PLACEHOLDER: &str = "q6-synthetic-placeholder";
const ORDINARY: &str = "Q6_K1_ORDINARY_RUNTIME";

/// Process-global environment is shared by every test of this binary, so the
/// witnesses that set synthetic variables take turns.
static ENV_TURN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct SyntheticParentEnv {
    names: Vec<&'static str>,
}

impl SyntheticParentEnv {
    fn install() -> Self {
        register_configured_credential_names(["Q6_K1_CONFIGURED_PROVIDER_KEY"]);
        let mut names: Vec<&'static str> = PROTECTED.iter().map(|(_, name)| *name).collect();
        names.push(ORDINARY);
        for name in &names {
            // SAFETY: the `ENV_TURN` guard serialises every user of these names.
            unsafe { std::env::set_var(name, PLACEHOLDER) };
        }
        Self { names }
    }
}

impl Drop for SyntheticParentEnv {
    fn drop(&mut self) {
        for name in &self.names {
            // SAFETY: as in `install`.
            unsafe { std::env::remove_var(name) };
        }
    }
}

/// `<case>=present|absent` for every protected category and the ordinary key.
fn expected_child_report() -> String {
    let mut report = String::new();
    for (case, _) in PROTECTED {
        report.push_str(&format!("{case}=absent\n"));
    }
    report.push_str("ordinary-runtime=present\npath=present\n");
    report
}

// ───────────────────────────── K1a — Bash ─────────────────────────────

fn bash_probe_command() -> String {
    let mut script = String::new();
    for (case, name) in PROTECTED {
        script.push_str(&format!(
            "if [ -n \"${{{name}+x}}\" ]; then echo {case}=present; else echo {case}=absent; fi; "
        ));
    }
    script.push_str(&format!(
        "if [ -n \"${{{ORDINARY}+x}}\" ]; then echo ordinary-runtime=present; else echo ordinary-runtime=absent; fi; \
         if [ -n \"${{PATH+x}}\" ]; then echo path=present; else echo path=absent; fi"
    ));
    script
}

async fn run_bash(mode: PermissionMode, command: &str) -> ToolCall {
    let tmp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(tmp.path()).unwrap();
    let workspace = base.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let security = Arc::new(SecurityAdapter::new(workspace.clone()));
    security.set_mode(mode);
    let storage: Arc<dyn rustain::domain::ports::StoragePort> = Arc::new(
        rustain::adapters::filesystem::FileSystemStorage::new(base.join("sessions")),
    );
    let tools: Arc<dyn ToolSetPort> = Arc::new(ToolSetAdapter::new(
        workspace,
        storage,
        Arc::new(arc_swap::ArcSwap::from_pointee(
            Arc::new(rustain::adapters::sandbox::NoOpSandbox)
                as Arc<dyn rustain::domain::ports::SandboxManager>,
        )),
        Arc::new(tokio::sync::RwLock::new(
            rustain::domain::models::sandbox::SandboxPolicy::Permissive,
        )),
    ));
    let approvals = ApprovalRuntime::new(
        16,
        Arc::new(rustain::adapters::noop::NoOpApprovalPersistence),
    );
    let scheduler = ToolScheduler::new(security, tools, approvals, 16);
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        scheduler.schedule_with_provenance_and_restriction(
            ApprovalSource::ForegroundTurn {
                conversation_id: "conv-q6-k1a".to_string(),
            },
            vec![ToolCallRequest {
                id: "bash-q6".to_string(),
                tool_name: "Bash".to_string(),
                input: serde_json::json!({ "command": command }),
            }],
            CancellationToken::new(),
            None,
            None,
            ProvenanceTag::UserOriginated,
            None,
        ),
    )
    .await
    .expect("the benign probe is admitted without an approval card")
    .remove(0)
}

#[tokio::test(flavor = "multi_thread")]
async fn k1a_bash_child_omits_every_ambient_credential_category_in_yolo_mode() {
    let _turn = ENV_TURN.lock().await;
    let _env = SyntheticParentEnv::install();

    let call = run_bash(PermissionMode::Yolo, &bash_probe_command()).await;

    let ToolCall::Success { result, .. } = call else {
        panic!("the probe must run: {call:?}");
    };
    assert_eq!(result.output.replace("\r\n", "\n"), expected_child_report());
    // The parent keeps its own environment: omission is per child, never a
    // mutation of this process.
    for (_, name) in PROTECTED {
        assert_eq!(
            std::env::var(name).as_deref(),
            Ok(PLACEHOLDER),
            "the parent environment is untouched"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn k1a_the_builtin_executor_boundary_omits_credentials_without_the_scheduler() {
    let _turn = ENV_TURN.lock().await;
    let _env = SyntheticParentEnv::install();

    // An approved Normal-mode call parks on an approval card before it runs, so
    // the mode-independent witness is the executor's own final spawn boundary:
    // whatever mode admitted the call, it reaches this code.
    let tmp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(tmp.path()).unwrap();
    let workspace = base.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let storage: Arc<dyn rustain::domain::ports::StoragePort> = Arc::new(
        rustain::adapters::filesystem::FileSystemStorage::new(base.join("sessions")),
    );
    let tools = ToolSetAdapter::new(
        workspace,
        storage,
        Arc::new(arc_swap::ArcSwap::from_pointee(
            Arc::new(rustain::adapters::sandbox::NoOpSandbox)
                as Arc<dyn rustain::domain::ports::SandboxManager>,
        )),
        Arc::new(tokio::sync::RwLock::new(
            rustain::domain::models::sandbox::SandboxPolicy::Permissive,
        )),
    );
    let result = tools
        .execute(
            "Bash",
            serde_json::json!({ "command": bash_probe_command() }),
            CancellationToken::new(),
        )
        .await
        .expect("executor runs the probe");
    assert_eq!(
        result.content.replace("\r\n", "\n"),
        expected_child_report(),
        "an approved call reaches the same final spawn boundary"
    );
}

// ───────────────── resolution refusal (shared by K1b and K1c) ─────────────────

fn write_workspace_json(dir: &Path, body: serde_json::Value) -> std::path::PathBuf {
    let path = dir.join(".claude").join("mcp.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, body.to_string()).unwrap();
    path
}

/// One stdio declaration per protected category, each in a different field.
/// `(case id, mcp.json server entry, profile TOML table)`.
fn refusal_cases() -> Vec<(String, serde_json::Value, String)> {
    let mut cases = Vec::new();
    for (case, name) in PROTECTED {
        let braced = format!("${{{name}}}");
        let plain = format!("${name}");
        cases.push((
            format!("{case}/command"),
            serde_json::json!({ "command": braced }),
            format!("command = \"{braced}\"\n"),
        ));
        cases.push((
            format!("{case}/args"),
            serde_json::json!({ "command": "srv", "args": ["--token", format!("--k={plain}")] }),
            format!("command = \"srv\"\nargs = [\"--token\", \"--k={plain}\"]\n"),
        ));
        cases.push((
            format!("{case}/env"),
            serde_json::json!({ "command": "srv", "env": { "SERVER_SCOPED": braced } }),
            format!("command = \"srv\"\n[mcp.declared.env]\nSERVER_SCOPED = \"{braced}\"\n"),
        ));
    }
    cases
}

fn assert_refused(case: &str, source: &str, specs: &[McpServerSpec]) {
    assert_eq!(specs.len(), 1, "{case}/{source}: the entry stays listed");
    let spec = &specs[0];
    let reason = spec
        .validate_transport_fields()
        .expect_err(&format!("{case}/{source}: must be refused"));
    for (_, name) in PROTECTED {
        assert!(
            !reason.contains(name),
            "{case}/{source}: the diagnostic names a protected variable"
        );
        let debug = format!("{spec:?}");
        assert!(
            !debug.contains(name) && !debug.contains(PLACEHOLDER),
            "{case}/{source}: neither the reference nor a looked-up value survives in the spec"
        );
    }
    assert!(spec.references_refused_app_credential(), "{case}/{source}");
}

#[test]
fn k1_stdio_references_to_app_credentials_are_refused_before_lookup_for_workspace_and_profile() {
    // The witness sets the synthetic values, so a lookup that did happen would
    // leave `PLACEHOLDER` in the spec.
    let _turn = ENV_TURN.blocking_lock();
    let _env = SyntheticParentEnv::install();

    for (case, workspace_entry, profile_table) in refusal_cases() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_workspace_json(
            tmp.path(),
            serde_json::json!({ "mcpServers": { "declared": workspace_entry } }),
        );
        let specs = parse_workspace_mcp_config(&path).unwrap();
        assert_refused(&case, "workspace", &specs);

        let profile: toml::Value =
            toml::from_str(&format!("[mcp.declared]\n{profile_table}")).unwrap();
        let specs = extract_profile_mcp_servers(Some(&profile), "profile-under-test");
        assert_refused(&case, "profile", &specs);
    }
}

#[test]
fn k1_a_server_name_or_override_never_grants_an_exception() {
    let _turn = ENV_TURN.blocking_lock();
    let _env = SyntheticParentEnv::install();
    let tmp = tempfile::tempdir().unwrap();
    // A server named like the credential, declared in the workspace over an
    // ordinary profile declaration of the same name.
    let path = write_workspace_json(
        tmp.path(),
        serde_json::json!({ "mcpServers": {
            "openai-api-key": { "command": "srv", "env": { "K": "$OPENAI_API_KEY" } },
            "gemini": { "command": "srv", "env": { "K": "$GEMINI_API_KEY" } },
        }}),
    );
    let workspace = parse_workspace_mcp_config(&path).unwrap();
    let profile_toml: toml::Value =
        toml::from_str("[mcp.gemini]\ncommand = \"ordinary\"\n").unwrap();
    let profile = extract_profile_mcp_servers(Some(&profile_toml), "p");
    let merged = rustain::adapters::mcp::merge_mcp_specs(workspace, profile);
    assert_eq!(merged.len(), 2);
    for spec in &merged {
        assert!(
            spec.validate_transport_fields().is_err(),
            "{}: the refusal survives matching names and workspace-over-profile precedence",
            spec.id
        );
    }
}

#[test]
fn k1_dedicated_server_scoped_references_and_ordinary_declarations_still_resolve() {
    let _turn = ENV_TURN.blocking_lock();
    let _env = SyntheticParentEnv::install();
    // SAFETY: serialised by `ENV_TURN`.
    unsafe { std::env::set_var("Q6_K1_DEDICATED_PARENT", "dedicated-synthetic") };
    let tmp = tempfile::tempdir().unwrap();
    let path = write_workspace_json(
        tmp.path(),
        serde_json::json!({ "mcpServers": { "ok": {
            "command": "srv",
            "args": ["--region=$Q6_K1_DEDICATED_PARENT"],
            "env": { "Q6_K1_DEDICATED": "${Q6_K1_DEDICATED_PARENT}", "LANG": "C" },
        }}}),
    );
    let specs = parse_workspace_mcp_config(&path).unwrap();
    // SAFETY: as above.
    unsafe { std::env::remove_var("Q6_K1_DEDICATED_PARENT") };
    let spec = &specs[0];
    assert!(spec.validate_transport_fields().is_ok());
    assert_eq!(spec.args, vec!["--region=dedicated-synthetic".to_owned()]);
    assert_eq!(
        spec.env.get("Q6_K1_DEDICATED").map(String::as_str),
        Some("dedicated-synthetic")
    );
    assert_eq!(spec.env.get("LANG").map(String::as_str), Some("C"));
}

// ──────────────────────────── K1b — TUI composite ────────────────────────────

#[cfg(feature = "test-fake-mcp")]
mod stdio_door {
    use super::*;
    use rustain::adapters::composite_toolset_adapter::CompositeToolsetAdapter;
    use rustain::adapters::mcp::client::McpClientAdapter;
    use rustain::adapters::noop::NoOpToolSet;

    fn probe_spec_json(probe_file: &Path) -> serde_json::Value {
        let mut cases: Vec<String> = PROTECTED
            .iter()
            .map(|(case, name)| format!("{case}:{name}"))
            .collect();
        cases.push(format!("ordinary-runtime:{ORDINARY}"));
        cases.push("path:PATH".to_owned());
        cases.push("dedicated-server-scoped:Q6_K1_DEDICATED".to_owned());
        serde_json::json!({
            "command": crate::common::fake_mcp_binary().to_string_lossy(),
            "env": {
                "FAKE_MCP_ENV_PROBE": cases.join(","),
                "FAKE_MCP_ENV_PROBE_FILE": probe_file.to_string_lossy(),
                "Q6_K1_DEDICATED": "${Q6_K1_DEDICATED_PARENT}",
            },
        })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn k1b_tui_composite_stdio_child_omits_credentials_and_keeps_explicit_env() {
        let _turn = ENV_TURN.lock().await;
        let _env = SyntheticParentEnv::install();
        // SAFETY: serialised by `ENV_TURN`.
        unsafe { std::env::set_var("Q6_K1_DEDICATED_PARENT", "dedicated-synthetic") };
        let tmp = tempfile::tempdir().unwrap();
        let probe_file = tmp.path().join("probe");
        let path = write_workspace_json(
            tmp.path(),
            serde_json::json!({ "mcpServers": { "probe": probe_spec_json(&probe_file) } }),
        );
        let specs = parse_workspace_mcp_config(&path).unwrap();
        // SAFETY: as above.
        unsafe { std::env::remove_var("Q6_K1_DEDICATED_PARENT") };
        let spec = specs[0].clone();
        spec.validate_transport_fields().expect("a healthy spec");

        let client = Arc::new(McpClientAdapter::new(spec.clone(), None));
        let composite = CompositeToolsetAdapter::new(
            Arc::new(NoOpToolSet),
            vec![Arc::clone(&client)],
            vec![spec],
            true,
            None,
            None,
            None,
        );
        composite.start_mcp_connections();
        assert!(
            composite
                .mcp_initial_connect_settled(std::time::Duration::from_secs(30))
                .await,
            "the composite connect settles"
        );
        assert!(
            !client.live_child_pids().await.is_empty(),
            "positive control: the child launched and handshook"
        );

        let report = std::fs::read_to_string(&probe_file).expect("the child wrote its status");
        let mut expected = expected_child_report();
        expected.push_str("dedicated-server-scoped=present\n");
        assert_eq!(report, expected);

        composite
            .shutdown_mcp_clients(std::time::Duration::from_secs(3))
            .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn k1b_a_refused_reference_launches_no_child_and_reports_a_safe_category() {
        let _turn = ENV_TURN.lock().await;
        let _env = SyntheticParentEnv::install();
        for (case, workspace_entry, _) in refusal_cases() {
            let tmp = tempfile::tempdir().unwrap();
            let probe_file = tmp.path().join("probe");
            // The refused declaration would launch the fixture if it were
            // honoured; point it there so a launch is observable.
            let mut entry = workspace_entry;
            let object = entry.as_object_mut().unwrap();
            object.insert(
                "command".into(),
                if case.ends_with("/command") {
                    object["command"].clone()
                } else {
                    serde_json::Value::String(
                        crate::common::fake_mcp_binary()
                            .to_string_lossy()
                            .into_owned(),
                    )
                },
            );
            let env = object
                .entry("env")
                .or_insert_with(|| serde_json::json!({}))
                .as_object_mut()
                .unwrap();
            env.insert(
                "FAKE_MCP_ENV_PROBE".into(),
                format!("launched:{ORDINARY}").into(),
            );
            env.insert(
                "FAKE_MCP_ENV_PROBE_FILE".into(),
                serde_json::Value::String(probe_file.to_string_lossy().into_owned()),
            );
            let path = write_workspace_json(
                tmp.path(),
                serde_json::json!({ "mcpServers": { "refused": entry } }),
            );
            let specs = parse_workspace_mcp_config(&path).unwrap();
            let client = Arc::new(McpClientAdapter::new(specs[0].clone(), None));

            let error = client
                .connect()
                .await
                .expect_err(&format!("{case}: connect must refuse"));
            let text = error.to_string();
            for (_, name) in PROTECTED {
                assert!(!text.contains(name), "{case}: diagnostic names a variable");
            }
            assert!(
                !text.contains(PLACEHOLDER),
                "{case}: diagnostic has a value"
            );
            assert!(
                client.live_child_pids().await.is_empty() && !probe_file.exists(),
                "{case}: no child was launched"
            );
        }
    }
}
