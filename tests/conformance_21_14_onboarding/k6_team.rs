//! K6 — team onboarding defaults and persistent hints (AC11, AC12, D16, D17, D18). Every config
//! assertion goes through the production layer assembly (`load_from_explicit_layers`: the same
//! `layered_figment` startup uses) with every layer built by the test — scratch workspace and
//! user-global files, a fixed environment layer, `-c` pairs through the production parser — so
//! no host config directory, working-directory file or ambient `RUSTAIN_*` variable can reach it
//! and nothing mutates the process environment (D22). Hints go through the production call sites.
use super::support::*;
use clap::Parser;
use rustain::adapters::cli::commands::Cli;
use rustain::adapters::cli::config_cmd::LayerDescriptor;
use rustain::adapters::profile_resolver::noop::NoopProfileResolver;
use rustain::adapters::tui::handlers::config::{apply_markdown_config, apply_onboarding_config};
use rustain::adapters::tui::keymap::Key;
use rustain::domain::models::{AppConfig, OnboardingConfig, ReasoningDisplay};
use rustain::infrastructure::config::{
    ACTIVE_PROFILE_PRIORITY, BUILT_IN_DEFAULTS_PRIORITY, CLI_FLAGS_PRIORITY, ENV_VARS_PRIORITY,
    KNOWN_TOP_LEVEL_FIELDS, LOCAL_OVERRIDE_PRIORITY, USER_GLOBAL_CONFIG_PRIORITY,
    WORKSPACE_CONFIG_PRIORITY, load_from_explicit_layers, parse_config_overrides, team_defaults,
};
use serde_json::{Value, json};

/// Load with every layer explicit: `workspace` / `user` are file bodies written into a scratch
/// directory (`None`: no such layer), `env` is the fixed environment layer, `overrides` are
/// `-c KEY=VALUE` pairs.
fn load_in(
    user: Option<&str>,
    workspace: Option<&str>,
    env: Value,
    overrides: &[&str],
) -> AppConfig {
    let dir = tempfile::tempdir().expect("scratch dir");
    let file = |name: &str, body: Option<&str>| {
        body.map(|body| {
            let path = dir.path().join(name);
            std::fs::write(&path, body).expect("write the layer file");
            path
        })
    };
    let (user, workspace) = (file("user.toml", user), file("workspace.toml", workspace));
    let layer = |kind, path: &Option<std::path::PathBuf>, priority| LayerDescriptor {
        kind,
        path: path.clone(),
        exists: path.is_some(),
        priority,
    };
    let layers = [
        layer("CLI flags", &None, CLI_FLAGS_PRIORITY),
        layer("env", &None, ENV_VARS_PRIORITY),
        layer("local", &None, LOCAL_OVERRIDE_PRIORITY),
        layer("workspace", &workspace, WORKSPACE_CONFIG_PRIORITY),
        layer("user-global", &user, USER_GLOBAL_CONFIG_PRIORITY),
        layer("profile", &None, ACTIVE_PROFILE_PRIORITY),
        layer("defaults", &None, BUILT_IN_DEFAULTS_PRIORITY),
    ];
    let cli = Cli::parse_from(["rustain"]);
    let pairs: Vec<String> = overrides.iter().map(ToString::to_string).collect();
    let parsed = parse_config_overrides(&pairs).expect("overrides parse");
    load_from_explicit_layers(&cli, &layers, &NoopProfileResolver, Some(&parsed), &env)
        .expect("config loads")
}

/// The workspace layer and `-c` pairs only (no user-global file, an empty environment layer).
fn load(workspace: Option<&str>, overrides: &[&str]) -> AppConfig {
    load_in(None, workspace, json!({}), overrides)
}

fn view(config: &AppConfig) -> (bool, ReasoningDisplay, bool, bool) {
    (
        config.default_plan_mode,
        config.markdown.reasoning(),
        config.onboarding.offer_tour,
        config.onboarding.persistent_hints,
    )
}

#[test]
fn the_team_switch_supplies_the_four_defaults_from_any_source() {
    let team = (true, ReasoningDisplay::Expanded, true, true);
    assert_eq!(
        view(&load(Some("[onboarding]\nteam = true"), &[])),
        team,
        "workspace file"
    );
    assert_eq!(
        view(&load_in(
            Some("[onboarding]\nteam = true"),
            None,
            json!({}),
            &[]
        )),
        team,
        "user-global file"
    );
    assert_eq!(
        view(&load_in(
            None,
            None,
            json!({"onboarding": {"team": true}}),
            &[]
        )),
        team,
        "the environment layer (RUSTAIN_ONBOARDING__TEAM), supplied as a fixed layer"
    );
    assert_eq!(
        view(&load(None, &["onboarding.team=true"])),
        team,
        "-c onboarding.team=true"
    );
}

#[test]
fn explicit_user_values_beat_team_defaults() {
    let workspace =
        "[onboarding]\nteam = true\noffer_tour = false\n\n[markdown]\nreasoning = \"folded\"\n";
    let config = load(Some(workspace), &["default_plan_mode=false"]);
    assert_eq!(
        view(&config),
        (false, ReasoningDisplay::Folded, false, true),
        "every explicit key wins at key level; the rest of the team layer stays"
    );
    // Each key alone, from the workspace file (the same layer as the team switch).
    for (body, expected) in [
        (
            "[onboarding]\nteam = true\n\n[markdown]\nreasoning = \"folded\"",
            (true, ReasoningDisplay::Folded, true, true),
        ),
        (
            "default_plan_mode = false\n\n[onboarding]\nteam = true",
            (false, ReasoningDisplay::Expanded, true, true),
        ),
        (
            "[onboarding]\nteam = true\npersistent_hints = false",
            (true, ReasoningDisplay::Expanded, true, false),
        ),
    ] {
        assert_eq!(view(&load(Some(body), &[])), expected, "{body}");
    }
}

#[test]
fn without_the_team_switch_the_defaults_are_byte_for_byte_todays() {
    for config in [
        load(None, &[]),
        load(Some("[onboarding]\nteam = false"), &[]),
        load(None, &["onboarding.team=false"]),
    ] {
        assert_eq!(
            serde_json::to_value(&config).unwrap(),
            serde_json::to_value(AppConfig::default()).unwrap(),
            "no layer leaks without `team`"
        );
    }
    assert_eq!(
        view(&AppConfig::default()),
        (false, ReasoningDisplay::Folded, false, false)
    );
}

#[test]
fn the_team_layer_is_a_closed_list_that_names_no_retired_key() {
    assert_eq!(
        team_defaults(),
        serde_json::json!({
            "default_plan_mode": true,
            "markdown": { "reasoning": "expanded" },
            "onboarding": { "offer_tour": true, "persistent_hints": true },
        })
    );
    let layer = team_defaults();
    let top: Vec<&String> = layer.as_object().unwrap().keys().collect();
    assert_eq!(top, ["default_plan_mode", "markdown", "onboarding"]);
    assert!(
        top.iter()
            .all(|key| KNOWN_TOP_LEVEL_FIELDS.contains(&key.as_str())),
        "every team key is a known key"
    );
    assert!(!top.iter().any(|key| key.as_str() == "layout"));
}

#[test]
fn a_team_file_with_the_retired_layout_keys_loads_and_onboarding_is_known() {
    let config = load(
        Some("[onboarding]\nteam = true\n\n[layout]\ndensity_mode = \"focus\"\n"),
        &[],
    );
    assert_eq!(
        view(&config),
        (true, ReasoningDisplay::Expanded, true, true)
    );
    assert!(KNOWN_TOP_LEVEL_FIELDS.contains(&"onboarding"));
    assert!(parse_config_overrides(&["onboarding.team=true".to_owned()]).is_ok());
    assert!(parse_config_overrides(&["layout.density_mode=focus".to_owned()]).is_ok());
}

#[test]
fn the_team_defaults_never_loosen_a_permission() {
    let tight = load(Some("[onboarding]\nteam = true"), &[]);
    let base = AppConfig::default();
    let (mut tight_json, mut base_json) = (
        serde_json::to_value(&tight).unwrap(),
        serde_json::to_value(&base).unwrap(),
    );
    // Remove the four defaults the team layer owns; everything else (every permission, sandbox,
    // tool and budget setting) must be exactly today's.
    for json in [&mut tight_json, &mut base_json] {
        let root = json.as_object_mut().unwrap();
        root.remove("default_plan_mode");
        root.remove("markdown");
        root.remove("onboarding");
    }
    assert_eq!(tight_json, base_json, "the layer touches nothing else");
}

#[test]
fn reload_applies_persistent_hints_and_the_reasoning_default_through_the_handlers() {
    let mut f = plain(80, 24);
    let config = load(Some("[onboarding]\nteam = true"), &[]);
    apply_markdown_config(&mut f.h.state, &config.markdown);
    apply_onboarding_config(&mut f.h.state, &config.onboarding);
    assert_eq!(f.h.state.reasoning_display, ReasoningDisplay::Expanded);
    assert!(f.h.state.hints_persistent);
    assert_eq!(f.h.state.hint_fade_sessions(), u32::MAX);
    // A reload that no longer carries the switch restores the fade.
    let config = load(None, &[]);
    apply_markdown_config(&mut f.h.state, &config.markdown);
    apply_onboarding_config(&mut f.h.state, &config.onboarding);
    assert_eq!(f.h.state.reasoning_display, ReasoningDisplay::Folded);
    assert!(!f.h.state.hints_persistent);
    assert_eq!(
        f.h.state.hint_fade_sessions(),
        f.h.state.theme.timing.status_hint_fade_sessions
    );
}

/// The hint the presentation reducer recomputes after a frame closes (one of the production
/// call sites), at `session_count`.
fn hint_after_a_frame_closes(session_count: u32, persistent: bool) -> Option<String> {
    let mut f = plain(80, 24);
    apply_onboarding_config(
        &mut f.h.state,
        &OnboardingConfig {
            persistent_hints: persistent,
            ..OnboardingConfig::default()
        },
    );
    f.h.state.session_count = session_count;
    press_all(&mut f, &[Key::Ctrl('x'), Key::Char('?')]);
    press(&mut f, Key::Esc);
    assert!(f.h.state.modal_stack.is_empty());
    f.h.state.current_hint.clone()
}

#[test]
fn persistent_hints_show_at_any_session_count_and_the_fade_is_unchanged_otherwise() {
    assert!(
        hint_after_a_frame_closes(5, false).is_some(),
        "the fade threshold is inclusive"
    );
    for count in [6, 100, u32::MAX] {
        assert!(
            hint_after_a_frame_closes(count, false).is_none(),
            "the existing fade at {count} sessions"
        );
        assert!(
            hint_after_a_frame_closes(count, true).is_some(),
            "persistent hints at {count} sessions"
        );
    }
}

/// Every production hint call site reads the one accessor: the theme threshold is named only by
/// the accessor, the theme and the two test rigs that set or pass their own threshold.
#[test]
fn every_hint_call_site_uses_the_accessor() {
    fn scan(dir: &std::path::Path, hits: &mut Vec<(String, usize)>) {
        for entry in std::fs::read_dir(dir).expect("read src") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                scan(&path, hits);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let text = std::fs::read_to_string(&path).expect("read source");
                let count = text.matches("timing.status_hint_fade_sessions").count();
                if count > 0 {
                    hits.push((path.display().to_string(), count));
                }
            }
        }
    }
    let mut hits = Vec::new();
    scan(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut hits,
    );
    hits.sort();
    let names: Vec<(&str, usize)> = hits
        .iter()
        .map(|(path, count)| (path.rsplit("/src/").next().expect("path under src"), *count))
        .collect();
    assert_eq!(
        names,
        [
            ("adapters/tui/state.rs", 2),
            ("infrastructure/runtime/attach_keys.rs", 1),
            (
                "infrastructure/runtime/attach_loop/inspect_hint_tests.rs",
                1
            ),
        ],
        "a call site passing the raw theme threshold would bypass persistent hints"
    );
}
