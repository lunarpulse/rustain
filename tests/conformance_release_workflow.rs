//! Release-workflow ratchets that stay inside the standalone `rustain` checkout.
//!
//! Story 19.5 (A11): nothing read `release.yml`, so the next person to touch the
//! `--features` line could delete the provide column with every lane green. These
//! assertions are the ratchet. ⛔ They read no path outside `rustain/` — `_bmad-output/`
//! is a different repository and is absent from the CI checkout (A8).

const WORKFLOW: &str = include_str!("../.github/workflows/release.yml");
/// The build command, matched as a line prefix: the step tees its output for
/// the failure annotation, which must not loosen the feature-list pin.
const RELEASE_BUILD: &str = "cargo build --release --target ${{ matrix.target }} --features self-update,relay-server,a2a,p2p ";
/// NFR9's byte bound. ⚑ AMENDED 2026-08-31 by Story 9.9 (owner ruling,
/// measurement-driven): 36,700,160 → 39,845,888. The Streamable HTTP client
/// transport measured 38,154,880 B on the shipped feature set — the transport
/// itself is 1,152,296 B and would have fit; the TLS backend that makes
/// `https://` MCP URLs work is the other 2,740,848 B. ⛔ This constant is the
/// ratchet: move it only with a fresh BYTE measurement recorded in `prd.md`
/// NFR9 — never with a crate count, which is precisely the inference Story
/// 9.9's own ruling A3 got wrong (the crates were in the tree but LTO-stripped
/// until something called them).
const NFR9_BOUND_BYTES: &str = "39845888";
const SIZE_GUARD: &str = "if [ \"$SIZE\" -gt \"39845888\" ]; then";
const SMOKE_STEP: &str = "      - name: Smoke required release features\n        if: matrix.target == 'x86_64-unknown-linux-gnu'\n        shell: bash\n        run: .github/scripts/release-smoke.sh \"${ASSET}\"\n";
const SMOKE_SCRIPT: &str = ".github/scripts/release-smoke.sh";

#[test]
fn release_build_ships_a2a_and_p2p_with_the_measured_size_bound() {
    assert!(
        WORKFLOW
            .lines()
            .any(|line| line.trim_start().starts_with(RELEASE_BUILD)),
        "release build must name self-update, relay-server, a2a, and p2p explicitly"
    );
    assert_eq!(
        WORKFLOW.matches(NFR9_BOUND_BYTES).count(),
        1,
        "the NFR9 byte bound must appear exactly once in release.yml"
    );
    // Counting the constant is not enough on its own: it would still pass with the
    // number parked in a comment and the comparison changed. Pin the comparison.
    assert!(
        WORKFLOW.lines().any(|line| line.trim() == SIZE_GUARD),
        "the size guard must compare against the NFR9 byte bound itself"
    );
}

#[test]
fn the_linux_leg_smokes_the_release_asset_through_an_in_repo_script() {
    assert!(
        WORKFLOW.contains(SMOKE_STEP),
        "the Linux leg must invoke {SMOKE_SCRIPT} on the staged asset (AC2)"
    );

    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(SMOKE_SCRIPT);
    let metadata = std::fs::metadata(&script)
        .unwrap_or_else(|e| panic!("{} must exist in the checkout: {e}", script.display()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert!(
            metadata.permissions().mode() & 0o111 != 0,
            "{} must be executable — the release step runs it directly",
            script.display()
        );
    }
    #[cfg(not(unix))]
    let _ = metadata;
}

/// The `sign` job discloses *expected minus present* targets in the release body
/// (AC6). An expected target no matrix leg builds makes that disclosure fire on
/// every release, which is how a real missing macOS asset gets lost in the noise.
#[test]
fn the_expected_target_set_is_exactly_the_build_matrix() {
    let matrix: Vec<&str> = WORKFLOW
        .lines()
        .filter_map(|line| line.trim().strip_prefix("- target: "))
        .collect();
    assert_eq!(matrix.len(), 3, "matrix targets changed: {matrix:?}");

    let mut expected: Vec<&str> = WORKFLOW
        .lines()
        .skip_while(|line| line.trim() != "expected_targets=(")
        .skip(1)
        .take_while(|line| line.trim() != ")")
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect();
    assert!(
        !expected.is_empty(),
        "the sign job must enumerate the expected release targets"
    );

    let mut matrix = matrix;
    matrix.sort_unstable();
    expected.sort_unstable();
    assert_eq!(
        expected, matrix,
        "the sign job's expected targets must be exactly the targets the matrix builds"
    );
}

// Story 18.7 K2: only crates whose own non-generic code is on the hot path.
const HOT_CRATES: &[&str] = &[
    "tokio",
    "tokio-util",
    "tokio-stream",
    "mio",
    "futures-util",
    "futures-core",
    "futures-channel",
    "bytes",
    "ratatui-core",
    "ratatui-widgets",
    "ratatui-crossterm",
    "crossterm",
    "kasuari",
    "compact_str",
    "unicode-width",
    "unicode-segmentation",
    "unicode-truncate",
    "lru",
    "pulldown-cmark",
    "serde_json",
    "memchr",
    "itoa",
    "zmij",
    "hyper",
    "hyper-util",
    "http",
    "httparse",
    "rustls",
    "tokio-rustls",
    "ring",
    "regex",
    "regex-automata",
    "aho-corasick",
    "globset",
    "hashbrown",
    "sha2",
    "blake3",
    "curve25519-dalek",
    "ed25519-dalek",
];

#[test]
fn k2_release_keeps_hot_code_fast_and_cold_dependencies_small() {
    let manifest: toml::Value = include_str!("../Cargo.toml")
        .parse()
        .expect("valid manifest");
    let release = &manifest["profile"]["release"];
    assert_eq!(release["opt-level"].as_integer(), Some(3));
    let packages = release["package"].as_table().expect("per-package profile");
    assert_eq!(packages["*"]["opt-level"].as_str(), Some("z"));
    let mut actual = std::collections::BTreeSet::new();
    for (name, settings) in packages {
        assert_eq!(settings.as_table().expect("package settings").len(), 1);
        if name != "*" {
            assert_eq!(settings["opt-level"].as_integer(), Some(3), "{name}");
            actual.insert(name.as_str());
        }
    }
    assert_eq!(actual, HOT_CRATES.iter().copied().collect());
}

#[test]
fn k1_tls_dependencies_select_only_ring_and_preserve_native_trust() {
    let manifest: toml::Value = include_str!("../Cargo.toml")
        .parse()
        .expect("valid manifest");
    let dependencies = &manifest["dependencies"];
    for (name, expected) in [
        (
            "reqwest",
            &[
                "stream",
                "json",
                "rustls-tls-native-roots",
                "charset",
                "system-proxy",
            ][..],
        ),
        ("rustls", &["ring", "std", "tls12", "logging"][..]),
        ("tokio-rustls", &["ring", "tls12", "logging"][..]),
        (
            "rmcp",
            &[
                "client",
                "transport-child-process",
                "transport-streamable-http-client-reqwest",
                "reqwest-tls-no-provider",
            ][..],
        ),
    ] {
        let dependency = &dependencies[name];
        assert_eq!(
            dependency["default-features"].as_bool(),
            Some(false),
            "{name}"
        );
        let actual: std::collections::BTreeSet<_> = dependency["features"]
            .as_array()
            .expect("dependency features")
            .iter()
            .map(|feature| feature.as_str().expect("feature name"))
            .collect();
        assert_eq!(actual, expected.iter().copied().collect(), "{name}");
    }
    assert!(
        manifest["features"]["mcp"]
            .as_array()
            .expect("mcp feature")
            .iter()
            .any(|feature| feature.as_str() == Some("dep:rustls"))
    );
}

const RATCHET_SCRIPT: &str = ".github/scripts/release-size-guard.sh";

#[test]
#[cfg(unix)]
fn k3_size_guard_enforces_the_reserved_margin_and_nfr9_boundaries() {
    use std::os::unix::fs::PermissionsExt;
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(RATCHET_SCRIPT);
    assert_ne!(
        std::fs::metadata(&script)
            .expect("size guard")
            .permissions()
            .mode()
            & 0o111,
        0
    );
    let source = std::fs::read_to_string(&script).expect("size guard source");
    let value = |name: &str| -> u64 {
        source
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name}=")))
            .unwrap_or_else(|| panic!("missing {name}"))
            .parse()
            .expect("byte constant")
    };
    let ceiling = value("NFR9_CEILING_BYTES");
    let margin = value("RESERVED_HEADROOM_BYTES");
    assert_eq!(ceiling.to_string(), NFR9_BOUND_BYTES);
    assert!(margin >= 524_288 && margin < ceiling && margin % 65_536 == 0);
    let dir = tempfile::tempdir().expect("sparse file directory");
    let binary = dir.path().join("binary with spaces");
    let file = std::fs::File::create(&binary).expect("sparse binary");
    for (bytes, code, diagnostic) in [
        (ceiling - margin, 0, ""),
        (ceiling - margin + 1, 1, "RESERVED_HEADROOM_BYTES"),
        (ceiling + 1, 1, "NFR9"),
    ] {
        file.set_len(bytes).expect("sparse binary length");
        let output = std::process::Command::new(&script)
            .arg(&binary)
            .output()
            .expect("run guard");
        assert_eq!(output.status.code(), Some(code), "{bytes}: {output:?}");
        if !diagnostic.is_empty() {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(text.contains(diagnostic), "{bytes}: {text}");
        }
    }
    // A symlink is measured by its target, not by its own path length.
    let link = dir.path().join("link to binary");
    std::os::unix::fs::symlink(&binary, &link).expect("symlink to sparse binary");
    let output = std::process::Command::new(&script)
        .arg(&link)
        .output()
        .expect("run guard on symlink");
    assert_eq!(output.status.code(), Some(1), "symlink: {output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("NFR9"));
}

const TLS_STACK_SCRIPT: &str = ".github/scripts/single-tls-stack.sh";

#[test]
#[cfg(unix)]
fn k5_single_tls_stack_check_rejects_missing_ring_or_a_second_stack() {
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(TLS_STACK_SCRIPT);
    let run = |tree: &str| {
        use std::io::Write as _;
        let mut child = std::process::Command::new(&script)
            .arg("fixture")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("run TLS stack check");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(tree.as_bytes())
            .expect("write tree");
        let output = child.wait_with_output().expect("TLS stack check output");
        (
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };
    // `openssl-probe` only locates certificate files; it is not a TLS stack.
    let single =
        "ring v0.17.14\nrustls v0.23.37\nrustls-native-certs v0.8.4\nopenssl-probe v0.2.1\n";
    assert_eq!(run(single).0, Some(0));
    let (code, stderr) = run("rustls v0.23.37\nring-compat v0.8.0\n");
    assert_eq!(code, Some(1));
    assert!(
        stderr.contains("ring missing") && stderr.contains("fixture"),
        "{stderr}"
    );
    for second in [
        "aws-lc-rs v1.17.3",
        "aws-lc-sys v0.43.0",
        "openssl-sys v0.9.111",
        "native-tls v0.2.14",
        "hyper-tls v0.6.0",
    ] {
        let (code, stderr) = run(&format!("{single}{second}\n"));
        assert_eq!(code, Some(1), "{second}");
        assert!(
            stderr.contains("second TLS stack") && stderr.contains(second),
            "{second}: {stderr}"
        );
    }
}

#[test]
fn k4_existing_release_builds_run_the_ratchet_before_capture_or_upload() {
    const RELEASE_RATCHET: &str = "      - name: Size ratchet\n        if: matrix.target == 'x86_64-unknown-linux-gnu'\n        run: .github/scripts/release-size-guard.sh \"${ASSET}\"\n";
    assert!(WORKFLOW.contains(RELEASE_RATCHET));
    assert!(
        WORKFLOW
            .find("      - name: Size guard (≤ 38 MiB)")
            .unwrap()
            < WORKFLOW.find(RELEASE_RATCHET).unwrap()
    );
    assert!(
        WORKFLOW.find(RELEASE_RATCHET).unwrap()
            < WORKFLOW.find("      - name: Upload artifact").unwrap()
    );
    let ci = include_str!("../.github/workflows/ci.yml");
    let capture = ci
        .split_once("  tui-conflict-flag:\n")
        .expect("capture job")
        .1;
    const BUILD_AND_GUARD: &str = "        run: cargo build --locked --release --features self-update,relay-server,a2a,p2p\n      - name: Size ratchet\n        run: .github/scripts/release-size-guard.sh target/release/rustain\n";
    assert!(capture.contains(BUILD_AND_GUARD));
    assert_eq!(
        capture.matches("cargo build ").count(),
        1,
        "reuse the existing release build"
    );
    assert!(capture.contains("      - name: Single TLS stack (rustls + ring)\n"));
    assert!(capture.contains("cargo tree --locked -e normal --prefix none --format '{p}'"));
    assert!(capture.contains("for features in '' self-update,relay-server,a2a,p2p; do"));
    assert!(capture.contains("| .github/scripts/single-tls-stack.sh \"${features:-default}\"\n"));
    assert!(
        capture.find(BUILD_AND_GUARD).unwrap()
            < capture
                .find("      - name: Execute exact conflict viewports without reruns")
                .unwrap()
    );
}
