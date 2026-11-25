//! Story C-2 — `[persona] config = { file = "…" }` conformance target.
//!
//! AC1 (schema refusals at load), AC3 (prompt content), AC5 (partly: the
//! binary refuses a bad profile) and AC7 (export / `show --toml`).
//!
//! Two sections:
//! * `Section A` — compiles against the cf10c636 API; the REDs.
//! * `mod new_api` — needs `compose_persona` and `ComposeContext.config_dir`.
//!
//! No in-process environment mutation: everything that reads `paths::*` runs
//! as a subprocess with the environment set on the child only.

use std::path::Path;
use std::process::{Command, Output};

use rustain::adapters::profile_resolver::toml_resolver::TomlProfileResolver;
use rustain::domain::models::{AdapterRef, PortDimension};
use rustain::domain::ports::ProfileResolver;

// ═══════════════════════════ Section A — base-compilable ═══════════════════════════

const DEFAULT_BLOCKS: [(&str, &str); 7] = [
    ("persona", "[persona]\nadapter = \"minimal\"\n"),
    ("memory", "[memory]\nadapter = \"noop\"\n"),
    ("session", "[session]\nadapter = \"basic\"\n"),
    ("tools", "[tools]\nadapter = \"builtin-only\"\n"),
    ("channels", "[channels]\nadapter = \"terminal\"\n"),
    ("scheduler", "[scheduler]\nadapter = \"none\"\n"),
    ("context", "[context]\nadapter = \"default\"\n"),
];

/// A full 7-port profile; `blocks` replace the default section for their label.
fn full_profile(name: &str, blocks: &[(&str, &str)]) -> String {
    let mut out = format!("name = \"{name}\"\n");
    for (label, default) in DEFAULT_BLOCKS {
        let block = blocks
            .iter()
            .find(|(l, _)| *l == label)
            .map_or(default, |(_, b)| *b);
        out.push_str(block);
    }
    out
}

fn write_profile(profiles: &Path, name: &str, toml_text: &str) {
    std::fs::create_dir_all(profiles).unwrap();
    std::fs::write(profiles.join(format!("{name}.toml")), toml_text).unwrap();
}

/// Resolve `name` from `profiles`; `Err` is the Display text.
fn resolve(
    profiles: &Path,
    name: &str,
) -> Result<rustain::domain::models::ResolvedProfile, String> {
    match TomlProfileResolver::new(name, profiles.to_path_buf()) {
        Ok(r) => Ok(r.resolve_active().expect("resolve_active")),
        Err(e) => Err(e.to_string()),
    }
}

fn resolve_blocks(
    name: &str,
    blocks: &[(&str, &str)],
) -> Result<rustain::domain::models::ResolvedProfile, String> {
    let tmp = tempfile::tempdir().unwrap();
    let profiles = tmp.path().join("profiles");
    write_profile(&profiles, name, &full_profile(name, blocks));
    resolve(&profiles, name)
}

fn persona_ref(resolved: &rustain::domain::models::ResolvedProfile) -> &AdapterRef {
    &resolved.selection.dimensions[&PortDimension::Persona]
}

fn persona_file(resolved: &rustain::domain::models::ResolvedProfile) -> Option<String> {
    persona_ref(resolved)
        ._config
        .as_ref()
        .and_then(|c| c.get("file"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

#[test]
fn ac1_inline_persona_file_resolves() {
    for file in ["devops.md", "team/devops.md"] {
        let block = format!("[persona]\nadapter = \"coding\"\nconfig = {{ file = \"{file}\" }}\n");
        let resolved = resolve_blocks("ok-inline", &[("persona", &block)])
            .unwrap_or_else(|e| panic!("inline file {file:?} must resolve: {e}"));
        assert_eq!(persona_ref(&resolved).adapter, "coding");
        assert_eq!(persona_file(&resolved).as_deref(), Some(file));
    }
}

#[test]
fn ac1_table_persona_file_resolves() {
    for file in ["devops.md", "team/devops.md"] {
        let block =
            format!("[persona]\nadapter = \"coding\"\n[persona.config]\nfile = \"{file}\"\n");
        let resolved = resolve_blocks("ok-table", &[("persona", &block)])
            .unwrap_or_else(|e| panic!("table file {file:?} must resolve: {e}"));
        assert_eq!(persona_ref(&resolved).adapter, "coding");
        assert_eq!(persona_file(&resolved).as_deref(), Some(file));
    }
}

/// `(config value as TOML source, needle that must appear in the Display)`.
/// For path cases the raw path (or its Debug form) must also be named.
const PERSONA_REFUSALS: &[(&str, &str, Option<&str>)] = &[
    (r#"{ tone = "concise" }"#, "tone", None),
    (r#"{ file = "a.md", tone = "x" }"#, "tone", None),
    (r#""devops.md""#, "config", None),
    (r#"{ file = 3 }"#, "file", None),
    (r#"{ file = "" }"#, "file", None),
    (r#"{ file = "/etc/passwd" }"#, "file", Some("/etc/passwd")),
    (r#"{ file = "../x.md" }"#, "file", Some("../x.md")),
    (r#"{ file = "a/../b.md" }"#, "file", Some("a/../b.md")),
    (r#"{ file = "./a.md" }"#, "file", Some("./a.md")),
    (r#"{ file = "a/./b.md" }"#, "file", Some("a/./b.md")),
    (r#"{ file = "a//b.md" }"#, "file", Some("a//b.md")),
    (r#"{ file = 'a\b.md' }"#, "file", Some(r"a\b.md")),
];

#[test]
fn ac1_inline_persona_config_refusals_name_profile_and_key() {
    for (config, needle, path) in PERSONA_REFUSALS {
        let block = format!("[persona]\nadapter = \"coding\"\nconfig = {config}\n");
        let err = match resolve_blocks("bad-inline", &[("persona", &block)]) {
            Ok(_) => panic!("persona config {config} must be refused at load, but resolved Ok"),
            Err(e) => e,
        };
        assert!(err.contains("bad-inline"), "{config}: names profile: {err}");
        assert!(
            err.contains(needle),
            "{config}: names key {needle:?}: {err}"
        );
        if let Some(p) = path {
            assert!(
                err.contains(p) || err.contains(&format!("{p:?}")),
                "{config}: names path {p:?}: {err}"
            );
        }
    }
}

#[test]
fn ac1_table_persona_config_refusals_name_profile_and_key() {
    for (config, needle, _) in PERSONA_REFUSALS {
        // A non-table `config = "devops.md"` has no `[persona.config]` spelling.
        if !config.starts_with('{') {
            continue;
        }
        let body = config
            .trim_start_matches('{')
            .trim_end_matches('}')
            .replace(", ", "\n");
        let block = format!("[persona]\nadapter = \"coding\"\n[persona.config]\n{body}\n");
        let err = match resolve_blocks("bad-table", &[("persona", &block)]) {
            Ok(_) => panic!("[persona.config] {body:?} must be refused at load, but resolved Ok"),
            Err(e) => e,
        };
        assert!(err.contains("bad-table"), "{body:?}: names profile: {err}");
        assert!(
            err.contains(needle),
            "{body:?}: names key {needle:?}: {err}"
        );
    }
}

#[test]
fn ac1_config_on_memory_session_scheduler_is_refused_naming_port_and_adapter() {
    let cases: &[(&str, &str, &str)] = &[
        (
            "memory",
            "noop",
            "[memory]\nadapter = \"noop\"\n[memory.config]\nx = 1\n",
        ),
        (
            "memory",
            "project-scoped",
            "[memory]\nadapter = \"project-scoped\"\n[memory.config]\nx = 1\n",
        ),
        (
            "memory",
            "daily-log",
            "[memory]\nadapter = \"daily-log\"\n[memory.config]\nx = 1\n",
        ),
        (
            "session",
            "basic",
            "[session]\nadapter = \"basic\"\n[session.config]\nx = 1\n",
        ),
        (
            "session",
            "workspace",
            "[session]\nadapter = \"workspace\"\n[session.config]\nx = 1\n",
        ),
        (
            "scheduler",
            "none",
            "[scheduler]\nadapter = \"none\"\n[scheduler.config]\nx = 1\n",
        ),
    ];
    for (port, adapter, block) in cases {
        let err = match resolve_blocks("cfg-port", &[(port, block)]) {
            Ok(_) => {
                panic!("{port} '{adapter}' with a config table must be refused, but resolved Ok")
            }
            Err(e) => e,
        };
        assert!(
            err.contains("cfg-port"),
            "{port}/{adapter}: names profile: {err}"
        );
        assert!(
            err.contains(adapter),
            "{port}/{adapter}: names adapter: {err}"
        );
        assert!(
            err.to_lowercase().contains(port),
            "{port}/{adapter}: names port: {err}"
        );
    }
}

#[test]
fn ac1_positive_controls_tools_context_channels_config_resolve() {
    let cases: &[(&str, &str)] = &[
        (
            "tools",
            "[tools]\nadapter = \"builtin-full\"\n[tools.config]\ninclude_builtin = false\n",
        ),
        (
            "context",
            "[context]\nadapter = \"default\"\n[context.config]\nnote = \"x\"\n",
        ),
        (
            "channels",
            "[channels]\nadapter = \"terminal\"\n[channels.config]\nbot_token = \"t\"\nallowed_chat_ids = [1]\n",
        ),
    ];
    for (port, block) in cases {
        let resolved = resolve_blocks("pos", &[(port, block)])
            .unwrap_or_else(|e| panic!("{port} config must still resolve: {e}"));
        let dim = match *port {
            "tools" => PortDimension::Tools,
            "context" => PortDimension::Context,
            _ => PortDimension::Channels,
        };
        assert!(
            resolved.selection.dimensions[&dim]._config.is_some(),
            "{port} config must be carried on the AdapterRef"
        );
    }
}

#[test]
fn ac1_persona_config_is_validated_through_extends_chain() {
    // A child that inherits a parent's bad persona config is refused too.
    let tmp = tempfile::tempdir().unwrap();
    let profiles = tmp.path().join("profiles");
    write_profile(
        &profiles,
        "bad-parent",
        &full_profile(
            "bad-parent",
            &[(
                "persona",
                "[persona]\nadapter = \"coding\"\nconfig = { tone = \"x\" }\n",
            )],
        ),
    );
    write_profile(
        &profiles,
        "kid",
        "name = \"kid\"\nextends = \"bad-parent\"\n[memory]\nadapter = \"daily-log\"\n",
    );
    let err = match resolve(&profiles, "kid") {
        Ok(_) => panic!("inherited bad persona config must be refused"),
        Err(e) => e,
    };
    assert!(err.contains("tone"), "names the key: {err}");
}

#[test]
fn ac1_extends_child_persona_without_config_drops_parent_file() {
    let tmp = tempfile::tempdir().unwrap();
    let profiles = tmp.path().join("profiles");
    write_profile(
        &profiles,
        "parent",
        &full_profile(
            "parent",
            &[(
                "persona",
                "[persona]\nadapter = \"coding\"\nconfig = { file = \"parent.md\" }\n",
            )],
        ),
    );
    write_profile(
        &profiles,
        "kid",
        "name = \"kid\"\nextends = \"parent\"\n[persona]\nadapter = \"coding\"\n",
    );
    let parent = resolve(&profiles, "parent").unwrap();
    assert_eq!(
        persona_file(&parent).as_deref(),
        Some("parent.md"),
        "control"
    );
    let kid = resolve(&profiles, "kid").unwrap();
    assert_eq!(persona_ref(&kid).adapter, "coding");
    assert!(
        persona_ref(&kid)._config.is_none(),
        "whole-replacement: no file for the child, got {:?}",
        persona_ref(&kid)._config
    );
}

#[test]
fn ac1_extends_child_without_persona_inherits_parent_file() {
    let tmp = tempfile::tempdir().unwrap();
    let profiles = tmp.path().join("profiles");
    write_profile(
        &profiles,
        "parent",
        &full_profile(
            "parent",
            &[(
                "persona",
                "[persona]\nadapter = \"coding\"\nconfig = { file = \"parent.md\" }\n",
            )],
        ),
    );
    write_profile(
        &profiles,
        "kid",
        "name = \"kid\"\nextends = \"parent\"\n[memory]\nadapter = \"daily-log\"\n",
    );
    let kid = resolve(&profiles, "kid").unwrap();
    assert_eq!(persona_ref(&kid).adapter, "coding");
    assert_eq!(persona_file(&kid).as_deref(), Some("parent.md"));
}

// ── subprocess helpers (environment on the CHILD only) ──

struct Sandbox {
    _tmp: tempfile::TempDir,
    config: std::path::PathBuf,
    data: std::path::PathBuf,
    home: std::path::PathBuf,
    work: std::path::PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let s = Sandbox {
            config: root.join("config"),
            data: root.join("data"),
            home: root.join("home"),
            work: root.join("work"),
            _tmp: tmp,
        };
        for d in [&s.config, &s.data, &s.home, &s.work] {
            std::fs::create_dir_all(d).unwrap();
        }
        s
    }

    fn profiles(&self) -> std::path::PathBuf {
        self.config.join("profiles")
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_rustain"))
            .env("RUSTAIN_CONFIG_DIR", &self.config)
            .env("RUSTAIN_DATA_DIR", &self.data)
            .env("HOME", &self.home)
            .current_dir(&self.work)
            .args(args)
            .output()
            .expect("spawn rustain")
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn ac1_binary_refuses_a_profile_with_an_unknown_persona_key() {
    let sb = Sandbox::new();
    write_profile(
        &sb.profiles(),
        "bad",
        &full_profile(
            "bad",
            &[(
                "persona",
                "[persona]\nadapter = \"coding\"\nconfig = { tone = \"concise\" }\n",
            )],
        ),
    );
    let out = sb.run(&["--profile", "bad", "profile", "show", "bad"]);
    let stderr = text(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(2),
        "stdout={} stderr={stderr}",
        text(&out.stdout)
    );
    assert!(stderr.contains("Profile load failed:"), "stderr: {stderr}");
    assert!(stderr.contains("tone"), "stderr names the key: {stderr}");
}

// ── C2-07 / AC5 / AD-16: `profile validate` does the persona file I/O check ──

fn persona_p_profile(sb: &Sandbox) {
    write_profile(
        &sb.profiles(),
        "p",
        &full_profile(
            "p",
            &[(
                "persona",
                "[persona]\nadapter = \"coding\"\nconfig = { file = \"p.md\" }\n",
            )],
        ),
    );
}

#[test]
fn validate_fails_a_missing_persona_file_with_the_path_and_a_fix_line() {
    let sb = Sandbox::new();
    persona_p_profile(&sb);
    let out = sb.run(&["profile", "validate", "p"]);
    let stderr = text(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(2),
        "stdout={} stderr={stderr}",
        text(&out.stdout)
    );
    assert!(stderr.contains("p.md"), "names the file: {stderr}");
    assert!(
        stderr.contains("Fix: create"),
        "prints the fix line: {stderr}"
    );
    assert!(
        stderr.contains("correct [persona] config.file"),
        "fix names the key: {stderr}"
    );
}

#[test]
fn validate_json_reports_kind_persona_file() {
    let sb = Sandbox::new();
    persona_p_profile(&sb);
    let out = sb.run(&["profile", "validate", "p", "--json"]);
    assert_eq!(out.status.code(), Some(2), "stderr={}", text(&out.stderr));
    let json: serde_json::Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("json: {e}: {}", text(&out.stdout)));
    assert_eq!(json["valid"], false);
    let entry = &json["profiles"][0];
    assert_eq!(entry["name"], "p");
    assert_eq!(entry["error"]["kind"], "PersonaFile");
    assert!(entry["error"]["detail"].as_str().unwrap().contains("p.md"));
    assert!(
        entry["error"]["fix"]
            .as_str()
            .unwrap()
            .starts_with("Fix: create")
    );
}

#[test]
fn validate_passes_when_the_persona_file_exists() {
    let sb = Sandbox::new();
    persona_p_profile(&sb);
    std::fs::create_dir_all(sb.config.join("personas")).unwrap();
    std::fs::write(sb.config.join("personas/p.md"), "C2-VALID-BODY\n").unwrap();
    let out = sb.run(&["profile", "validate", "p"]);
    assert_eq!(out.status.code(), Some(0), "stderr={}", text(&out.stderr));
    assert!(
        text(&out.stdout).contains("Profile 'p' is valid"),
        "{}",
        text(&out.stdout)
    );
}

// ── AC7: export and `show --toml` ──

/// Everything `to_flat_toml` emits at cf10c636 for the profile below
/// (alphabetical sections: the `toml` crate here has no `preserve_order`).
const PIN_EXPECTED: &str = "name = \"pin\"\n\
\n[channels]\nadapter = \"terminal\"\n\
\n[context]\nadapter = \"default\"\n\
\n[memory]\nadapter = \"noop\"\n\
\n[persona]\nadapter = \"coding\"\n\
\n[scheduler]\nadapter = \"none\"\n\
\n[session]\nadapter = \"basic\"\n\
\n[tools]\nadapter = \"builtin-full\"\n";

fn pin_profile() -> String {
    full_profile(
        "pin",
        &[
            ("persona", "[persona]\nadapter = \"coding\"\n"),
            (
                "tools",
                "[tools]\nadapter = \"builtin-full\"\n[tools.config]\ninclude_builtin = false\n",
            ),
        ],
    )
}

/// Drop only the `# Exported at:` line.
fn normalise_export(s: &str) -> String {
    s.split_inclusive('\n')
        .filter(|l| !l.starts_with("# Exported at: "))
        .collect()
}

#[test]
fn ac7_export_without_persona_config_is_byte_identical_to_base() {
    let sb = Sandbox::new();
    write_profile(&sb.profiles(), "pin", &pin_profile());
    let out = sb.run(&["profile", "export", "pin"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", text(&out.stderr));
    let expected = format!(
        "# Exported by rustain profile export\n\
         # Source: pin (user)\n\
         # Note: extends chain flattened; this file is self-contained.\n\
         {PIN_EXPECTED}"
    );
    let stdout = text(&out.stdout);
    assert!(
        stdout.contains("# Exported at: "),
        "header keeps the timestamp: {stdout}"
    );
    assert_eq!(normalise_export(&stdout), expected);
}

#[test]
fn ac7_show_toml_without_persona_config_is_byte_identical_to_base() {
    let sb = Sandbox::new();
    write_profile(&sb.profiles(), "pin", &pin_profile());
    let out = sb.run(&["profile", "show", "pin", "--toml"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", text(&out.stderr));
    // `show --toml` is headerless and printed with `println!`.
    assert_eq!(text(&out.stdout), format!("{PIN_EXPECTED}\n"));
}

const PERSONA_TABLE: &str =
    "[persona]\nadapter = \"coding\"\n\n[persona.config]\nfile = \"p.md\"\n";

fn persona_file_profile(name: &str) -> String {
    full_profile(
        name,
        &[(
            "persona",
            "[persona]\nadapter = \"coding\"\nconfig = { file = \"p.md\" }\n",
        )],
    )
}

#[test]
fn ac7_export_carries_the_persona_file_reference() {
    let sb = Sandbox::new();
    write_profile(&sb.profiles(), "withp", &persona_file_profile("withp"));
    let out = sb.run(&["profile", "export", "withp"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", text(&out.stderr));
    let stdout = text(&out.stdout);
    assert!(
        stdout.contains(PERSONA_TABLE),
        "export must carry the persona file reference, got:\n{stdout}"
    );
    // The reference only — not the markdown, and no other port grows a config.
    assert_eq!(stdout.matches(".config]").count(), 1, "{stdout}");
}

#[test]
fn ac7_show_toml_carries_the_persona_file_reference() {
    let sb = Sandbox::new();
    write_profile(&sb.profiles(), "withp", &persona_file_profile("withp"));
    let out = sb.run(&["profile", "show", "withp", "--toml"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", text(&out.stderr));
    let stdout = text(&out.stdout);
    assert!(
        stdout.contains(PERSONA_TABLE),
        "show --toml must carry the persona file reference, got:\n{stdout}"
    );
}

#[test]
fn ac7_import_of_an_export_resolves_to_the_same_persona_ref() {
    let sb = Sandbox::new();
    write_profile(&sb.profiles(), "withp", &persona_file_profile("withp"));
    let exported = sb.work.join("withp.export.toml");
    let out = sb.run(&[
        "profile",
        "export",
        "withp",
        "--output",
        exported.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", text(&out.stderr));
    let out = sb.run(&[
        "profile",
        "import",
        exported.to_str().unwrap(),
        "--name",
        "imported",
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout={} stderr={}",
        text(&out.stdout),
        text(&out.stderr)
    );
    let original = resolve(&sb.profiles(), "withp").unwrap();
    let imported = resolve(&sb.profiles(), "imported").unwrap();
    assert_eq!(persona_file(&original).as_deref(), Some("p.md"), "control");
    assert_eq!(
        persona_ref(&imported).adapter,
        persona_ref(&original).adapter
    );
    assert_eq!(
        persona_ref(&imported)._config,
        persona_ref(&original)._config
    );
}

// ═══════════════════════ Section B — needs the C-2 production API ═══════════════════════

mod new_api {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use rustain::domain::errors::AdapterCompositionError;
    use rustain::domain::models::project_context::{
        ContextFileType, ProjectContext, ProjectContextFile,
    };
    use rustain::domain::models::{AdapterRef, PortDimension, ProfileSelection};
    use rustain::infrastructure::composition::{ComposeContext, compose_persona};
    use rustain::infrastructure::runtime::agent_core::AgentCore;

    const CLAUDE_CANARY: &str = "C2-CLAUDE-MD-CANARY";
    const PA_TEXT: &str = "You are a personal assistant running in rustain. Help the user plan the day, keep track of tasks and notes, draft messages and answer questions.\nBe brief and concrete. Use only the tools you are given. Never claim to have read email, calendars or messages you were not shown.\nAsk before acting on anything the user did not request.";

    fn claude_md_context() -> ProjectContext {
        let content = format!("# Project rules\n{CLAUDE_CANARY}\n");
        ProjectContext {
            total_chars: content.len(),
            files: vec![ProjectContextFile {
                path: PathBuf::from("CLAUDE.md"),
                content,
                priority: 1,
                source_type: ContextFileType::ClaudeMd,
            }],
            truncated: false,
        }
    }

    fn reference(adapter: &str, file: Option<&str>) -> AdapterRef {
        AdapterRef {
            adapter: adapter.to_string(),
            _config: file.map(|f| toml::from_str(&format!("file = {f:?}")).unwrap()),
        }
    }

    fn personas_dir_with(file: &str, body: &str) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
        tmp
    }

    fn prompt(adapter: &str, file: Option<&str>, dir: &Path) -> String {
        let ctx = claude_md_context();
        compose_persona(&reference(adapter, file), &ctx, dir)
            .unwrap_or_else(|e| panic!("compose_persona({adapter}, {file:?}) failed: {e}"))
            .system_prompt(Path::new("/unused"))
    }

    #[test]
    fn coding_with_file_is_body_then_project_context() {
        let dir = personas_dir_with("devops.md", "BODY-LINE-1\nBODY-LINE-2\n");
        let expected = format!(
            "BODY-LINE-1\nBODY-LINE-2\n\n{}",
            claude_md_context().assembled_prompt()
        );
        assert_eq!(prompt("coding", Some("devops.md"), dir.path()), expected);
    }

    #[test]
    fn coding_with_nested_file_loads_the_subdirectory_file() {
        let dir = personas_dir_with("team/devops.md", "NESTED-BODY\n");
        let expected = format!("NESTED-BODY\n\n{}", claude_md_context().assembled_prompt());
        assert_eq!(
            prompt("coding", Some("team/devops.md"), dir.path()),
            expected
        );
    }

    #[test]
    fn minimal_with_file_is_the_body_only() {
        let dir = personas_dir_with("devops.md", "ONLY-BODY\n");
        let out = prompt("minimal", Some("devops.md"), dir.path());
        assert_eq!(out, "ONLY-BODY");
        assert!(!out.contains(CLAUDE_CANARY));
    }

    #[test]
    fn personal_assistant_without_file_is_the_embedded_text() {
        let dir = tempfile::tempdir().unwrap();
        let out = prompt("personal-assistant", None, dir.path());
        assert_eq!(out, PA_TEXT);
        assert!(!out.contains(CLAUDE_CANARY));
        assert!(!out.contains("FILE:"));
    }

    #[test]
    fn personal_assistant_with_file_is_the_file_body() {
        let dir = personas_dir_with("pa.md", "MY-PA-BODY\n");
        let out = prompt("personal-assistant", Some("pa.md"), dir.path());
        assert_eq!(out, "MY-PA-BODY");
        assert!(!out.contains(CLAUDE_CANARY));
    }

    #[test]
    fn byte_identity_pins_without_a_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            prompt("coding", None, dir.path()),
            claude_md_context().assembled_prompt()
        );
        assert_eq!(prompt("minimal", None, dir.path()), "");
    }

    #[test]
    fn missing_file_with_coding_fails_closed_naming_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = claude_md_context();
        let err = match compose_persona(&reference("coding", Some("absent.md")), &ctx, dir.path()) {
            Ok(_) => panic!("a missing persona file must be an error"),
            Err(e) => e,
        };
        let shown = err.to_string();
        let joined = dir.path().join("absent.md");
        assert!(
            matches!(
                &err,
                AdapterCompositionError::AdapterConstructionFailed {
                    port: PortDimension::Persona,
                    ..
                }
            ),
            "wrong variant: {err:?}"
        );
        assert!(
            shown.contains(&*joined.to_string_lossy()),
            "names the path: {shown}"
        );
    }

    fn compose_ctx(config_dir: &Path) -> ComposeContext {
        ComposeContext {
            workspace_path: config_dir.join("ws"),
            project_context: claude_md_context(),
            storage: Arc::new(rustain::adapters::noop::NoOpStorage)
                as Arc<dyn rustain::domain::ports::StoragePort>,
            skill_activator: Arc::new(rustain::adapters::skill_activation::SkillActivator::new()),
            mcp_servers: Vec::new(),
            include_builtin_tools: true,
            domain_tx: None,
            channel_turn_tx: None,
            tool_exposure: "static-full".into(),
            assembler: "passthrough".into(),
            skill_exposure: "l1-metadata".into(),
            skill_cache: Arc::new(
                rustain::infrastructure::skill_cache::SkillCache::new_in_memory(),
            ),
            sandbox: rustain::domain::models::sandbox::SandboxSession::off(),
            sandbox_slot: Arc::new(arc_swap::ArcSwap::from_pointee(Arc::new(
                rustain::adapters::sandbox::NoOpSandbox,
            )
                as Arc<dyn rustain::domain::ports::SandboxManager>)),
            memory_slot: Arc::new(arc_swap::ArcSwap::from_pointee(Arc::new(
                rustain::adapters::noop::NoOpMemory,
            )
                as Arc<dyn rustain::domain::ports::MemoryPort>)),
            sandbox_policy: Arc::new(tokio::sync::RwLock::new(
                rustain::domain::models::sandbox::SandboxPolicy::Permissive,
            )),
            memory_write_gate: Arc::new(tokio::sync::RwLock::new(())),
            peer_topic_store: Arc::new(rustain::adapters::rap::PeerTopicStore::new()),
            #[cfg(feature = "meta-search")]
            search_config: rustain::domain::models::SearchConfig::default(),
            #[cfg(feature = "meta-search")]
            meta_search_engine: None,
            a2a_peers: Vec::new(),
            config_dir: config_dir.to_path_buf(),
        }
    }

    fn selection(persona: AdapterRef) -> ProfileSelection {
        let mut s = ProfileSelection::default();
        for (dim, name) in [
            (PortDimension::Memory, "noop"),
            (PortDimension::Session, "basic"),
            (PortDimension::Tools, "builtin-only"),
            (PortDimension::Channels, "terminal"),
            (PortDimension::Scheduler, "none"),
            (PortDimension::Context, "default"),
        ] {
            s.dimensions.insert(dim, reference(name, None));
        }
        s.dimensions.insert(PortDimension::Persona, persona);
        s
    }

    #[tokio::test]
    async fn agent_core_compose_reads_the_file_from_config_dir_personas() {
        let config_dir = tempfile::tempdir().unwrap();
        let personas = config_dir.path().join("personas");
        std::fs::create_dir_all(&personas).unwrap();
        std::fs::write(personas.join("devops.md"), "TUI-BODY\n").unwrap();

        let ctx = compose_ctx(config_dir.path());
        let sel = selection(reference("coding", Some("devops.md")));
        let core = AgentCore::compose("p", &sel, &ctx).expect("compose");
        let got = core.persona.load().system_prompt(&ctx.workspace_path);

        let expected = format!("TUI-BODY\n\n{}", claude_md_context().assembled_prompt());
        assert_eq!(got, expected);
        assert_eq!(
            got,
            compose_persona(
                &reference("coding", Some("devops.md")),
                &ctx.project_context,
                &personas
            )
            .unwrap()
            .system_prompt(&ctx.workspace_path)
        );
    }

    #[tokio::test]
    async fn agent_core_compose_fails_closed_when_the_file_is_missing() {
        let config_dir = tempfile::tempdir().unwrap();
        let ctx = compose_ctx(config_dir.path());
        let sel = selection(reference("coding", Some("absent.md")));
        let err = match AgentCore::compose("p", &sel, &ctx) {
            Ok(_) => panic!("composition must fail when the persona file is missing"),
            Err(e) => e,
        };
        assert!(
            matches!(
                &err,
                AdapterCompositionError::AdapterConstructionFailed {
                    port: PortDimension::Persona,
                    ..
                }
            ),
            "wrong variant: {err:?}"
        );
    }
}
