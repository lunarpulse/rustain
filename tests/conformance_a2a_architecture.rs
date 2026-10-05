use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn source(relative: &str) -> String {
    std::fs::read_to_string(root().join(relative))
        .unwrap_or_else(|error| panic!("read {relative}: {error}"))
}

#[test]
fn a2a_domain_types_are_wire_and_transport_free() {
    let domain = source("src/domain/models/a2a_peer_spec.rs");
    for forbidden in [
        "reqwest",
        "axum",
        "JsonRpc",
        "serde_jcs",
        "ed25519_dalek",
        "base64::",
        "AgentCardView",
        "A2aClientAdapter",
    ] {
        assert!(
            !domain.contains(forbidden),
            "pure A2A domain model contains forbidden dependency {forbidden:?}"
        );
    }
    assert!(domain.contains("RedactedUrl"));
    assert!(domain.contains("fn trust_tier(&self) -> TrustTier"));
}

#[test]
fn a2a_feature_is_off_by_default_and_declares_the_documented_fallback() {
    let manifest: toml::Value = toml::from_str(&source("Cargo.toml")).unwrap();
    let features = manifest["features"].as_table().unwrap();
    let defaults: Vec<_> = features["default"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect();
    assert!(!defaults.contains(&"a2a"));

    let a2a: BTreeSet<_> = features["a2a"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect();
    assert_eq!(
        a2a,
        BTreeSet::from([
            "dep:axum",
            "dep:reqwest",
            "dep:rustls",
            "dep:rustls-pemfile",
            "dep:rustls-webpki",
            "dep:serde_jcs",
            "dep:subtle",
            "dep:tokio-rustls",
            "mcp",
        ]),
        "DF-17-4a-3 fallback must remain explicit until composite gating is separated. \
         Story 18.1b adds the server-side TLS stack (rustls/tokio-rustls/rustls-pemfile) \
         and the constant-time credential comparison (subtle) — all `a2a`-gated, none \
         reachable from a default build. Story 19.14 adds `rustls-webpki`, named ONLY to \
         downcast `rustls::CertificateError::Other` to `webpki::Error::CaUsedAsEndEntity`: \
         without it an anchor refusal cannot say a CA was served as a server certificate \
         and collapses to the generic mismatch form. It is the crate `rustls` already \
         depends on, declared in rustls's own form so cargo unifies on ONE copy."
    );
    for dependency in [
        "reqwest",
        "serde_jcs",
        "axum",
        "rustls",
        "tokio-rustls",
        "rustls-pemfile",
        "rustls-webpki",
        "subtle",
    ] {
        assert_eq!(
            manifest["dependencies"][dependency]["optional"].as_bool(),
            Some(true),
            "{dependency} must stay optional so a default build links none of it"
        );
    }
    assert!(
        !source("Cargo.toml").contains("a2a-lf"),
        "R11 fired the a2a-lf drop-trigger"
    );
}

#[test]
fn only_config_parsing_compiles_without_the_a2a_feature() {
    let module = source("src/adapters/a2a/mod.rs");

    // Two modules are deliberately ungated, and each has a production consumer
    // that would not compile otherwise.
    //
    // `config`: startup must reject configured peers loudly rather than silently
    // omit them (this test's original subject).
    //
    // `transparency`: Story 18.3 made `TransparencySink` the mandatory recorder for
    // every live verified peer frame, and the RAP path that holds it is ungated core
    // (ADR-18-3-01 D3 — when two adapters must meet, they meet at a domain port).
    // It is named ungated at `infrastructure/startup.rs:978` and
    // `adapters/daemon/mod.rs:486,646`. 18.3 widened the feature-off surface here and
    // left this list unchanged, so the list — not the module — was stale; it failed
    // on disk at `530dbd2` and is corrected by Story 18.3b rather than relabelled.
    for ungated in ["config", "transparency"] {
        assert!(
            module.contains(&format!("pub mod {ungated};")),
            "{ungated} must exist"
        );
        assert!(
            !module.contains(&format!("#[cfg(feature = \"a2a\")]\npub mod {ungated};")),
            "{ungated} must stay UNGATED — it has ungated production consumers, and gating it \
             would break the feature-off daemon build"
        );
    }

    for adapter in [
        "admission",
        "auth",
        "card",
        "card_cache",
        "client",
        "error",
        "exec",
        "jws",
        "projection",
        "provider",
        "server",
        "tls",
    ] {
        assert!(
            module.contains(&format!("#[cfg(feature = \"a2a\")]\npub mod {adapter};")),
            "{adapter} must be feature-gated"
        );
    }
}

#[test]
fn concrete_a2a_client_and_provider_are_constructed_only_at_the_egress_composition_boundary() {
    let src = root().join("src");
    let mut references = BTreeSet::new();
    visit_rs(&src, &mut |path, text| {
        let production = text.split("#[cfg(test)]").next().unwrap_or(text);
        if production.contains("A2aClientAdapter::new") || production.contains("A2aProvider::new") {
            references.insert(path.strip_prefix(&src).unwrap().to_path_buf());
        }
    });
    assert_eq!(
        references,
        BTreeSet::from([PathBuf::from("adapters/a2a/egress.rs")])
    );
}

#[test]
fn every_public_a2a_enum_is_non_exhaustive() {
    let files = [
        "src/adapters/a2a/admission.rs",
        "src/adapters/a2a/auth.rs",
        "src/adapters/a2a/config.rs",
        "src/adapters/a2a/error.rs",
        "src/adapters/a2a/exec.rs",
        "src/adapters/a2a/projection.rs",
        "src/adapters/a2a/transparency.rs",
        "src/domain/models/a2a_peer_spec.rs",
    ];
    let mut enums = BTreeSet::new();
    for file in files {
        let text = source(file);
        let lines: Vec<_> = text.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            let trimmed = line.trim_start();
            let Some(rest) = trimmed.strip_prefix("pub enum ") else {
                continue;
            };
            let name = rest.split_whitespace().next().unwrap();
            let attributes = lines[index.saturating_sub(4)..index].join("\n");
            assert!(
                attributes.contains("#[non_exhaustive]"),
                "public enum {name} in {file} must be #[non_exhaustive]"
            );
            enums.insert(name.to_owned());
        }
    }
    assert_eq!(
        enums,
        BTreeSet::from([
            "A2aAdmissionPolicy".to_owned(),
            "A2aConfigError".to_owned(),
            "A2aError".to_owned(),
            "A2aPeerSource".to_owned(),
            "A2aPeerSpecError".to_owned(),
            "A2aServerAuth".to_owned(),
            "AdmissionVerdict".to_owned(),
            // Story 19.14: the anchor refusal taxonomy. `AnchorFailure`'s
            // members are the sub-causes `UX-DR-TM-10` v0.6 separates, and a
            // fifth one is a real possibility (revocation, for instance) — so
            // it is `#[non_exhaustive]` for the same reason every enum above
            // is.
            "AnchorCause".to_owned(),
            "AnchorFailure".to_owned(),
            "AuthOutcome".to_owned(),
            "BindDecision".to_owned(),
            // Story 19.18c `A7`/`P7`: the capability gate's verb classes and
            // verdicts; a new verb or verdict must be decided, not defaulted.
            "CapabilityGatedVerb".to_owned(),
            "CapabilityVerdict".to_owned(),
            "Disclosure".to_owned(),
            "InboundOutcome".to_owned(),
            "PendingAuth".to_owned(),
            // Story 19.18d `A9`: one `server.peerKeys` value (alias string or
            // object); a new form must be decided, not defaulted.
            "PeerKeyBinding".to_owned(),
            "PinnedKeyAlgorithm".to_owned(),
            "SubmitterTrust".to_owned(),
            // Story 19.18 `AC12`: the thread-access setting; an unrecognised
            // value already folds to `Unknown` and fails closed.
            "ThreadReads".to_owned(),
            "TrustTier".to_owned(),
        ])
    );
}

/// K08a: policy authority is introduced only at the ingress funnels. Story
/// 19.18d `F8`: per-file counts — `rap()` and `bound_a2a()` in the principal
/// module, the pseudonymous arm of `SubmitterKey::policy_principal` in exec —
/// so a direct `Pinned { … }` in exec cannot stand in for a missing funnel.
#[test]
fn k08a_policy_principal_construction_funnel() {
    let src = root().join("src");
    let mut sites = Vec::new();
    let forbidden_impl = regex::Regex::new(
        r"impl(?:\s*<[^>]*>)?\s+(?:From\s*<[^>]*\bPeerId\s*>|Default)\s+for\s+(?:[\w:]+::)?PolicyPrincipal\b",
    )
    .unwrap();
    let construction =
        regex::Regex::new(r"\b(?:PolicyPrincipal|Self)::(?:Pseudonymous\s*\(|Pinned\s*\{)")
            .unwrap();
    let variant_import =
        regex::Regex::new(r"PolicyPrincipal\s*::\s*[{*]|PolicyPrincipal\s+as\s").unwrap();
    visit_rs(&src, &mut |path, text| {
        let production = without_test_items(text);
        let relative = path.strip_prefix(&src).unwrap();
        let joined = production
            .iter()
            .map(|(_, line)| *line)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !forbidden_impl.is_match(&joined),
            "implicit principal construction in {}",
            relative.display()
        );
        for (number, line) in production {
            let line = line.trim();
            if line.starts_with("//") {
                continue;
            }
            assert!(
                !line.starts_with("use ") || !variant_import.is_match(line),
                "aliased principal variants in {}:{number}",
                relative.display()
            );
            if construction.is_match(expression_side(line)) {
                sites.push((relative.to_path_buf(), number));
            }
        }
    });
    let mut per_file = BTreeMap::<PathBuf, usize>::new();
    for (path, _) in &sites {
        *per_file.entry(path.clone()).or_default() += 1;
    }
    assert_eq!(
        per_file,
        BTreeMap::from([
            (PathBuf::from("domain/models/policy_principal.rs"), 2),
            (PathBuf::from("adapters/a2a/exec.rs"), 1),
        ]),
        "principal construction sites: {sites:?}"
    );
}

/// Story 19.18d `K13` (`F7`, supplemental to the behavioural keystones) — the
/// one production derivation of an A2A credential pseudonym is
/// `SubmitterKey::ingress_principal`; item principals are built only by
/// `SubmitterKey::principal`'s mapping and the item model's constructors. A
/// site that derives a pseudonym ad hoc would silently mint
/// `A2aPseudonym(…)` where a bound caller's items live under `A2aBound(pin)`.
/// `M42` turns this RED.
#[test]
fn k13_one_production_pseudonym_derivation() {
    let src = root().join("src");
    let call = regex::Regex::new(r"\.pseudonymous_peer_id\(").unwrap();
    let construction = regex::Regex::new(r"ItemPrincipal::A2aPseudonym\(").unwrap();
    let declaration = regex::Regex::new(r"\bfn\s+(\w+)").unwrap();
    let mut calls = Vec::new();
    let mut constructions = BTreeMap::<PathBuf, usize>::new();
    visit_rs(&src, &mut |path, text| {
        let relative = path.strip_prefix(&src).unwrap().to_path_buf();
        let mut enclosing = String::new();
        for (number, line) in without_test_items(text) {
            let trimmed = line.trim();
            if let Some(captures) = declaration.captures(trimmed) {
                enclosing = captures[1].to_owned();
            }
            if trimmed.starts_with("//") || trimmed.starts_with('|') {
                continue;
            }
            if call.is_match(trimmed) {
                calls.push((relative.clone(), number, enclosing.clone()));
            }
            if construction.is_match(expression_side(trimmed)) {
                *constructions.entry(relative.clone()).or_default() += 1;
            }
        }
    });
    assert_eq!(
        calls.len(),
        1,
        "production pseudonym derivations: {calls:?}"
    );
    assert_eq!(
        (calls[0].0.as_path(), calls[0].2.as_str()),
        (Path::new("adapters/a2a/exec.rs"), "ingress_principal"),
        "production pseudonym derivations: {calls:?}"
    );
    assert_eq!(
        constructions,
        BTreeMap::from([
            // `SubmitterKey::principal`'s pseudonym arm.
            (PathBuf::from("adapters/a2a/exec.rs"), 1),
            // `ItemAddress::from_a2a_ingress` and the address wire decoder.
            (PathBuf::from("domain/models/recipient_item.rs"), 2),
        ]),
        "item-principal pseudonym constructions"
    );
}

/// Story 19.18d `AC13(d)` — the A2A ingress principal lives in the domain
/// model, outside the fixed inventory above; a new ingress kind must be
/// decided by every match, not defaulted.
#[test]
fn the_a2a_ingress_principal_is_non_exhaustive() {
    let text = source("src/domain/models/recipient_item.rs");
    let lines: Vec<_> = text.lines().collect();
    let index = lines
        .iter()
        .position(|line| {
            line.trim_start()
                .starts_with("pub enum A2aIngressPrincipal")
        })
        .expect("A2aIngressPrincipal is declared");
    assert!(
        lines[index.saturating_sub(4)..index]
            .join("\n")
            .contains("#[non_exhaustive]"),
        "A2aIngressPrincipal must be #[non_exhaustive]"
    );
}

/// Production lines of a rustfmt-formatted file, numbered from 1: every item
/// carrying `#[cfg(test)]` or `#[cfg(all(test, …))]` is dropped through its
/// closing line at the item's own indentation. Relies on `cargo fmt --check`
/// (CI); a misformatted test item leaks into the scan and fails loudly, and
/// the skip never runs past a dedent, so it cannot swallow enclosing code.
fn without_test_items(text: &str) -> Vec<(usize, &str)> {
    let test_cfg = regex::Regex::new(r"^#\[cfg\((?:test|all\((?:[^()]*,\s*)?test\s*[,)])").unwrap();
    let indent_of = |line: &str| line.len() - line.trim_start().len();
    let lines: Vec<&str> = text.lines().collect();
    let mut kept = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let trimmed = lines[index].trim_start();
        if !test_cfg.is_match(trimmed) {
            kept.push((index + 1, lines[index]));
            index += 1;
            continue;
        }
        // The item starts on the attribute line itself (`#[cfg(test)] use x;`)
        // or after any further attributes and comments.
        let inline_item = trimmed
            .find(']')
            .is_some_and(|end| !trimmed[end + 1..].trim().is_empty());
        if !inline_item {
            index += 1;
            while index < lines.len() {
                let next = lines[index].trim_start();
                if !(next.starts_with("#[") || next.starts_with("//")) {
                    break;
                }
                index += 1;
            }
        }
        let Some(start) = lines.get(index) else {
            break;
        };
        let indent = indent_of(start);
        let start = start.trim_end();
        index += 1;
        if start.ends_with(';') || (start.ends_with('}') && start.contains('{')) {
            continue;
        }
        while let Some(line) = lines.get(index) {
            let body = line.trim();
            if !body.is_empty() && indent_of(line) < indent {
                break;
            }
            index += 1;
            if indent_of(line) == indent
                && body.starts_with(['}', ']', ')'])
                && !body.ends_with('{')
            {
                break;
            }
        }
    }
    kept
}

/// The part of a line in expression position: right of the last `=>` in a
/// match arm, right of `=` in any `let` / `if let` / `while let` binding;
/// nothing for `matches!` (its principal argument is a pattern).
fn expression_side(line: &str) -> &str {
    if let Some((_, right)) = line.rsplit_once("=>") {
        return right;
    }
    if line.contains("matches!") {
        return "";
    }
    if line.starts_with("let ") || line.contains("if let ") || line.contains("while let ") {
        return line.split_once(" = ").map_or("", |(_, right)| right);
    }
    line
}

/// K08b: future principal and storage kinds require explicit policy decisions.
#[test]
fn k08b_policy_principal_enums_are_non_exhaustive() {
    let mut found = BTreeSet::new();
    for file in [
        "src/domain/models/policy_principal.rs",
        "src/domain/ports/consent_projection.rs",
    ] {
        let text = source(file);
        let lines: Vec<_> = text.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            let Some(rest) = line.trim_start().strip_prefix("pub enum ") else {
                continue;
            };
            let name = rest.split_whitespace().next().unwrap();
            if !matches!(
                name,
                "PolicyPrincipal" | "PinnedVia" | "PrincipalKind" | "ConsentSlot"
            ) {
                continue;
            }
            assert!(
                lines[index.saturating_sub(4)..index]
                    .join("\n")
                    .contains("#[non_exhaustive]"),
                "{name} in {file} must be #[non_exhaustive]"
            );
            found.insert(name.to_owned());
        }
    }
    assert_eq!(
        found,
        BTreeSet::from([
            "PolicyPrincipal".to_owned(),
            "PinnedVia".to_owned(),
            "PrincipalKind".to_owned(),
            "ConsentSlot".to_owned(),
        ])
    );
}

#[test]
fn signature_verification_remains_a_pure_raw_payload_function() {
    let jws = source("src/adapters/a2a/jws.rs");
    assert!(jws.contains(
        "pub fn verify_card(raw_bytes: &str, pinned: &PinnedKey) -> Result<(), A2aError>"
    ));
    assert!(!jws.contains("async fn verify_card"));
    assert!(!jws.contains("reqwest"));
    assert!(jws.contains("serde_json::from_str(raw_bytes)"));
    assert!(jws.contains("verify_strict"));
}

fn visit_rs(dir: &Path, visit: &mut impl FnMut(&Path, &str)) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            visit_rs(&path, visit);
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
            let text = std::fs::read_to_string(&path).unwrap();
            visit(&path, &text);
        }
    }
}

/// ADR-17-CC-05: `transparency.jsonl` is a **regenerable export**, never a
/// second source of truth.
///
/// Structural, not behavioural, because the failure mode is a *future* edit:
/// someone adds a read of the export file "just for the panel" and the product
/// quietly grows a second log to keep consistent — the one that drifts is the
/// one nobody is looking at. The only permitted mentions are the path helper
/// that mints it, the export shell that writes it, and documentation.
#[test]
fn no_code_path_reads_the_transparency_export() {
    const WRITER: &str = "src/infrastructure/transparency.rs";
    const PATH_OWNER: &str = "src/infrastructure/paths.rs";

    let mut mentions: Vec<String> = Vec::new();
    visit_rs(&root().join("src"), &mut |path, text| {
        let relative = path
            .strip_prefix(root())
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        if relative == WRITER || relative == PATH_OWNER {
            return;
        }
        for (index, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            // Doc and ordinary comments are how the invariant is explained;
            // they are not a read.
            if trimmed.starts_with("//") {
                continue;
            }
            if line.contains("transparency.jsonl") {
                mentions.push(format!("{relative}:{}", index + 1));
            }
        }
    });
    assert!(
        mentions.is_empty(),
        "`transparency.jsonl` may only be named by the export shell ({WRITER}) and the path \
         helper ({PATH_OWNER}). Every transparency fact is read from the room journal.\n\
         Offending sites:\n{}",
        mentions.join("\n")
    );
}

/// The export path is owned by `infrastructure::paths`, which declares itself
/// the single source of truth for paths. An inline `join(".rustain")` is how a
/// second, subtly different location gets created.
#[test]
fn the_transparency_export_path_is_minted_only_by_the_paths_module() {
    let paths = source("src/infrastructure/paths.rs");
    assert!(
        paths.contains("pub fn transparency_export_path("),
        "the export path helper must live in infrastructure::paths"
    );
    let writer = source("src/infrastructure/transparency.rs");
    assert!(
        writer.contains("paths::transparency_export_path"),
        "the export shell must resolve its path through the paths module"
    );
    assert!(
        !writer.contains("join(\".rustain\")"),
        "the export shell must not inline the workspace runtime directory"
    );
}
