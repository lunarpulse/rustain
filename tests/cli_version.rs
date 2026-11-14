//! fix-1 (FR109): `rustain --version` through the real binary and clap, checked against
//! the surfaces that speak each protocol and against `Cargo.toml`.
//!
//! Field labels and sets are the contract; values are not pinned (AD-8). The recorded
//! owner-visible examples are compared by hand in the story, not here.

use rustain::infrastructure::build_info;
use std::collections::BTreeSet;
use std::process::{Command, Stdio};

/// stdout of `rustain --version` run with a scratch `HOME`, no `TERM`, null stdin.
fn binary_version_output() -> String {
    let home = tempfile::tempdir().expect("scratch HOME");
    let out = Command::new(env!("CARGO_BIN_EXE_rustain"))
        .arg("--version")
        .env("HOME", home.path())
        .env_remove("TERM")
        .stdin(Stdio::null())
        .output()
        .expect("rustain --version runs");
    assert!(
        out.status.success(),
        "--version exits 0, got {:?}; stderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("UTF-8 stdout")
}

fn value_of<'a>(text: &'a str, label: &str) -> &'a str {
    text.lines()
        .find_map(|l| l.strip_prefix(&format!("{label}: ")))
        .unwrap_or_else(|| panic!("no `{label}:` line in {text}"))
}

fn name_set(text: &str, label: &str) -> BTreeSet<String> {
    match value_of(text, label) {
        "none" => BTreeSet::new(),
        list => list.split(", ").map(str::to_string).collect(),
    }
}

/// K1: the program name appears once, the binary's stdout is exactly the shared block,
/// the labels are exact and ordered, and `rustc:` is a real three-component version.
#[test]
fn k1_binary_version_has_one_name_and_the_block_labels() {
    let text = binary_version_output();
    let lines: Vec<&str> = text.lines().collect();
    let first = lines[0];
    assert!(
        first.starts_with(&format!("rustain {} (", env!("CARGO_PKG_VERSION"))),
        "first line must start with `rustain <version> (`: {first:?}"
    );
    assert!(
        !first.starts_with("rustain rustain"),
        "the program name must appear once: {first:?}"
    );
    let labels: Vec<&str> = lines[1..]
        .iter()
        .map(|l| l.split_once(": ").map_or(*l, |(label, _)| label))
        .collect();
    assert_eq!(
        labels,
        [
            "build",
            "commit",
            "rustc",
            "protocols",
            "features",
            "not built"
        ],
        "line labels and order: {text}"
    );
    let rustc = value_of(&text, "rustc");
    assert_ne!(rustc, "unknown");
    let numeric = rustc.split('-').next().unwrap_or_default();
    let parts: Vec<&str> = numeric.split('.').collect();
    assert!(
        parts.len() == 3 && parts.iter().all(|p| p.parse::<u64>().is_ok()),
        "rustc value must be a three-component compiler version, got {rustc:?}"
    );
    assert_eq!(
        text,
        format!("{}\n", build_info::version_block()),
        "binary stdout must equal the shared block"
    );
}

/// What Cargo makes of a manifest's `[features]` and optional dependencies.
struct EffectiveFeatures {
    /// Explicit keys plus the implicit feature of every optional dependency that no
    /// `dep:name` reference suppresses; `default` excluded.
    all: BTreeSet<String>,
    /// Everything switched on when default features are on, following aliases
    /// (`a = ["b"]`) and the `name/feat` form's dependency activation.
    default_reachable: BTreeSet<String>,
}

/// Optional dependency names of every dependency table (plain, build, target-specific).
fn optional_dependencies(manifest: &toml::Value) -> BTreeSet<String> {
    let mut tables = Vec::new();
    for key in ["dependencies", "build-dependencies"] {
        tables.extend(manifest.get(key).and_then(toml::Value::as_table));
    }
    if let Some(targets) = manifest.get("target").and_then(toml::Value::as_table) {
        for target in targets.values() {
            for key in ["dependencies", "build-dependencies"] {
                tables.extend(target.get(key).and_then(toml::Value::as_table));
            }
        }
    }
    tables
        .into_iter()
        .flat_map(|table| table.iter())
        .filter(|(_, dep)| dep.get("optional").and_then(toml::Value::as_bool) == Some(true))
        .map(|(name, _)| name.clone())
        .collect()
}

fn effective_features(manifest: &toml::Value) -> EffectiveFeatures {
    let table = manifest
        .get("features")
        .and_then(toml::Value::as_table)
        .expect("[features] table");
    let members = |name: &str| -> Vec<String> {
        table
            .get(name)
            .and_then(toml::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|v| v.as_str().expect("feature entry is a string").to_string())
                    .collect()
            })
            .unwrap_or_default()
    };
    let suppressed: BTreeSet<String> = table
        .keys()
        .flat_map(|k| members(k))
        .filter_map(|m| m.strip_prefix("dep:").map(str::to_string))
        .collect();
    let mut all: BTreeSet<String> = table.keys().filter(|k| *k != "default").cloned().collect();
    let implicit: BTreeSet<String> = optional_dependencies(manifest)
        .into_iter()
        .filter(|dep| !suppressed.contains(dep) && !table.contains_key(dep))
        .collect();
    all.extend(implicit.iter().cloned());

    let mut reachable = BTreeSet::new();
    let mut pending = members("default");
    while let Some(item) = pending.pop() {
        if item.starts_with("dep:") {
            continue;
        }
        // `dep/feat` and `dep?/feat` name a dependency's feature; only the strong
        // form (no `?`) also switches the dependency's own implicit feature on.
        let name = match item.split_once('/') {
            Some((dep, _)) if dep.ends_with('?') => continue,
            Some((dep, _)) => dep.to_string(),
            None => item,
        };
        if reachable.insert(name.clone()) && table.contains_key(&name) {
            pending.extend(members(&name));
        }
    }
    EffectiveFeatures {
        all,
        default_reachable: reachable,
    }
}

fn manifest_effective_features() -> EffectiveFeatures {
    let manifest = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
        .expect("read Cargo.toml");
    effective_features(&toml::from_str(&manifest).expect("parse Cargo.toml"))
}

/// K2: the table in `build_info` and the effective Cargo feature set (explicit keys plus
/// unsuppressed optional-dependency features, minus `default`) name the same features,
/// each once; no test-only feature is reachable from `default`, aliases included.
#[test]
fn k2_feature_table_equals_cargo_toml() {
    let effective = manifest_effective_features();
    let listed: Vec<&str> = build_info::features().iter().map(|f| f.name).collect();
    let listed_set: BTreeSet<String> = listed.iter().map(|s| s.to_string()).collect();
    assert_eq!(listed.len(), listed_set.len(), "a feature is listed twice");
    assert_eq!(
        listed_set, effective.all,
        "build_info::features() and the effective Cargo feature set must name the same set"
    );
    for feature in build_info::features().iter().filter(|f| f.test_only) {
        assert!(
            !effective.default_reachable.contains(feature.name),
            "test-only feature {} must not be reachable from default",
            feature.name
        );
    }
}

/// K2 unit cases on synthetic manifests: the guard's own logic, independent of the
/// real `Cargo.toml`.
#[test]
fn k2_effective_feature_rules() {
    let effective = |text: &str| effective_features(&toml::from_str(text).expect("toml"));
    let base = "[dependencies]\nfoo = { version = \"1\", optional = true }\nbar = \"1\"\n\
                [target.'cfg(unix)'.dependencies]\nbaz = { version = \"1\", optional = true }\n";
    // An optional dependency nobody references with `dep:` is an implicit feature,
    // also when declared under a target table.
    let e = effective(&format!("{base}[features]\ndefault = []\nx = []\n"));
    assert_eq!(
        e.all,
        ["baz", "foo", "x"].map(String::from).into_iter().collect()
    );
    // A `dep:` reference suppresses it; a same-named explicit key is not doubled.
    let e = effective(&format!(
        "{base}[features]\nx = [\"dep:foo\", \"dep:baz\"]\n"
    ));
    assert_eq!(e.all, ["x"].map(String::from).into_iter().collect());
    // Aliases are followed from `default`; `dep:` and `?/` forms switch no feature on.
    let e = effective(&format!(
        "{base}[features]\ndefault = [\"a\", \"foo?/z\", \"dep:baz\"]\na = [\"b\"]\nb = [\"c/y\"]\nc = []\n"
    ));
    assert_eq!(
        e.default_reachable,
        ["a", "b", "c"].map(String::from).into_iter().collect()
    );
}

/// Features the lane builds, written in the test crate, independent of the library's table.
fn lane_cfg() -> [(&'static str, bool); 8] {
    [
        ("mcp", cfg!(feature = "mcp")),
        ("a2a", cfg!(feature = "a2a")),
        ("p2p", cfg!(feature = "p2p")),
        ("relay-server", cfg!(feature = "relay-server")),
        ("self-update", cfg!(feature = "self-update")),
        (
            "test-instrumentation",
            cfg!(feature = "test-instrumentation"),
        ),
        ("p2p-test-utils", cfg!(feature = "p2p-test-utils")),
        ("test-fake-mcp", cfg!(feature = "test-fake-mcp")),
    ]
}

/// K3: built features are on `features:`, absent non-test features on `not built:`,
/// absent test-only features on neither, and the lines are disjoint; the test crate's
/// own `cfg!` controls agree in whichever polarity this lane builds.
#[test]
fn k3_features_and_not_built_lines_match_the_lane() {
    let text = binary_version_output();
    let built = name_set(&text, "features");
    let not_built = name_set(&text, "not built");
    assert!(built.is_disjoint(&not_built), "{built:?} vs {not_built:?}");
    for feature in build_info::features() {
        let name = feature.name.to_string();
        assert_eq!(built.contains(&name), feature.built, "{name} on features:");
        assert_eq!(
            not_built.contains(&name),
            !feature.built && !feature.test_only,
            "{name} on not built:"
        );
    }
    for (name, on) in lane_cfg() {
        let name = name.to_string();
        assert_eq!(
            built.contains(&name),
            on,
            "lane control {name} on features:"
        );
        let test_only = build_info::features()
            .iter()
            .any(|f| f.name == name && f.test_only);
        assert_eq!(
            not_built.contains(&name),
            !on && !test_only,
            "lane control {name} on not built:"
        );
    }
}

/// K4: the protocols line lists the four names in order; `agent-skills` carries no
/// version; `acp` carries the constant `initialize` answers with; `mcp` and `a2a`
/// carry their handshake constants iff the feature is built.
#[test]
fn k4_protocols_line_is_truthful() {
    let text = binary_version_output();
    let mut expected = vec![
        "agent-skills".to_string(),
        format!(
            "acp {}",
            rustain::adapters::acp::agent::ACP_PROTOCOL_VERSION
        ),
    ];
    #[cfg(feature = "mcp")]
    expected.push(format!(
        "mcp {}",
        rustain::adapters::mcp::client::offered_protocol_version()
    ));
    #[cfg(not(feature = "mcp"))]
    expected.push("mcp not built".to_string());
    #[cfg(feature = "a2a")]
    expected.push(format!(
        "a2a {}",
        rustain::adapters::a2a::jsonrpc::A2A_PROTOCOL_VERSION
    ));
    #[cfg(not(feature = "a2a"))]
    expected.push("a2a not built".to_string());
    assert_eq!(value_of(&text, "protocols"), expected.join(", "));
}

/// K5: the served A2A card agrees with the block (`protocolVersion` and `version`).
/// Nearest test-visible seam to the HTTP handler; the HTTP path itself is checked on
/// the real asset by `release-smoke.sh`.
#[cfg(feature = "a2a")]
#[tokio::test]
async fn k5_served_card_agrees_with_the_version_block() {
    use rustain::adapters::a2a::card::ServedAgentCard;
    use rustain::domain::models::capability_registry::CapabilityRegistry;

    let text = binary_version_output();
    let a2a_version = value_of(&text, "protocols")
        .split(", ")
        .find_map(|p| p.strip_prefix("a2a "))
        .expect("a2a version on the protocols line")
        .to_string();
    let card =
        ServedAgentCard::from_registry(&CapabilityRegistry::new(None), "http://127.0.0.1:1").await;
    let json = serde_json::to_value(&card).expect("card serializes");
    let interfaces = json["supportedInterfaces"]
        .as_array()
        .expect("supportedInterfaces array");
    assert!(!interfaces.is_empty(), "the card advertises an interface");
    assert_eq!(interfaces[0]["protocolVersion"], a2a_version.as_str());
    let first = text.lines().next().expect("first line");
    let package_version = first
        .strip_prefix("rustain ")
        .and_then(|rest| rest.split(' ').next())
        .expect("package version on the first line");
    assert_eq!(json["version"], package_version);
}

/// The compiler version Cargo itself selects, resolved independently of `build.rs` and of
/// any toolchain layout: `cargo rustc -- --version` on a dependency-free throwaway crate
/// makes the very `cargo` that compiled this test pick its `rustc` (honouring `RUSTC`,
/// `build.rustc` in the repository's config, and the repository's toolchain file) and run
/// it. Cargo does not export `RUSTC` to test processes, so asking Cargo is the oracle that
/// does not assume where the compiler lives.
fn cargo_selected_rustc_version() -> String {
    let probe = tempfile::tempdir().expect("scratch crate");
    std::fs::create_dir(probe.path().join("src")).expect("mkdir src");
    std::fs::write(probe.path().join("src/lib.rs"), "").expect("write lib.rs");
    // rustup resolves a `rustc` proxy from the invoking directory: give the scratch
    // crate the repository's toolchain pin so it resolves what the repository builds with.
    for pin in ["rust-toolchain.toml", "rust-toolchain"] {
        let from = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(pin);
        if from.exists() {
            std::fs::copy(&from, probe.path().join(pin)).expect("copy toolchain pin");
        }
    }
    std::fs::write(
        probe.path().join("Cargo.toml"),
        "[package]\nname = \"rustc-oracle\"\nversion = \"0.0.0\"\nedition = \"2021\"\n[workspace]\n",
    )
    .expect("write Cargo.toml");
    let out = Command::new(env!("CARGO"))
        .args(["rustc", "--lib", "--manifest-path"])
        .arg(probe.path().join("Cargo.toml"))
        .args(["--", "--version"])
        // The repository's own config (`build.rustc`) must apply; only the target
        // directory and wrappers are isolated.
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("CARGO_TARGET_DIR", probe.path().join("target"))
        .env("RUSTC_WRAPPER", "")
        .env("RUSTC_WORKSPACE_WRAPPER", "")
        .stdin(Stdio::null())
        .output()
        .expect("spawn `cargo rustc` (the oracle for the embedded compiler)");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success(),
        "compiler oracle `cargo rustc -- --version` ({}) failed: {text}",
        env!("CARGO")
    );
    text.lines()
        .find_map(|l| l.strip_prefix("rustc "))
        .and_then(|rest| rest.split_whitespace().next())
        .map(str::to_string)
        .unwrap_or_else(|| {
            panic!("compiler oracle could not find a `rustc <version>` line in: {text}")
        })
}

/// K8: the binary's `rustc:` is the compiler Cargo selected.
#[test]
fn k8_rustc_line_is_the_compiler_cargo_selected() {
    assert_eq!(
        value_of(&binary_version_output(), "rustc"),
        cargo_selected_rustc_version()
    );
}
