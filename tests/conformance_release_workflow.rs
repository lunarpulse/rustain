//! Release-workflow ratchets that stay inside the standalone `rustain` checkout.
//!
//! Story 19.5 (A11): nothing read `release.yml`, so the next person to touch the
//! `--features` line could delete the provide column with every lane green. These
//! assertions are the ratchet. ⛔ They read no path outside `rustain/` — `_bmad-output/`
//! is a different repository and is absent from the CI checkout (A8).

const WORKFLOW: &str = include_str!("../.github/workflows/release.yml");
/// The release-candidate tier: one shipped-set release build feeds the size
/// ratchet, the TLS check, the smoke and the Act 1, Act 2 and Conflict captures.
const PROOFS: &str = include_str!("../.github/workflows/nightly-proofs.yml");
/// The build command, matched as a line prefix: the step tees its output for
/// the failure annotation, which must not loosen the feature-list pin.
const RELEASE_BUILD: &str = "cargo build --release --target ${{ matrix.target }} --features self-update,relay-server,a2a,p2p,telegram,cron,slack ";
/// NFR9's 40 MiB byte bound follows the C-2b measured fit test and D-25.
/// Recalibrate only with a fresh byte measurement and the owner-approved rule.
const NFR9_BOUND_BYTES: &str = "41943040";
const SIZE_GUARD: &str = "if [ \"$SIZE\" -gt \"41943040\" ]; then";
const SMOKE_STEP: &str = "      - name: Smoke required release features\n        if: matrix.target == 'x86_64-unknown-linux-gnu'\n        shell: bash\n        run: .github/scripts/release-smoke.sh \"${ASSET}\"\n";
const SMOKE_SCRIPT: &str = ".github/scripts/release-smoke.sh";

#[test]
fn release_build_ships_a2a_and_p2p_with_the_measured_size_bound() {
    assert!(
        WORKFLOW
            .lines()
            .any(|line| line.trim_start().starts_with(RELEASE_BUILD)),
        "release build must name the shipped six-feature set explicitly"
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

/// fix-1 K9: the features the smoke requires of `--version` are exactly the features
/// `release.yml` builds with (a removed entry would let a release lose a feature).
#[test]
fn k9_smoke_required_features_equal_the_release_build_features() {
    let build_line = WORKFLOW
        .lines()
        .find(|line| line.trim_start().starts_with(RELEASE_BUILD))
        .expect("release build line");
    let built: std::collections::BTreeSet<&str> = build_line
        .split("--features ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .expect("--features value")
        .split(',')
        .collect();
    let script = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(SMOKE_SCRIPT),
    )
    .expect("read release-smoke.sh");
    let required: std::collections::BTreeSet<&str> = script
        .lines()
        .find_map(|l| l.strip_prefix("REQUIRED_FEATURES=\""))
        .and_then(|rest| rest.strip_suffix('"'))
        .expect("REQUIRED_FEATURES= line")
        .split_whitespace()
        .collect();
    assert_eq!(required, built);
}

/// What one run of the smoke script against a stub produced.
#[cfg(unix)]
struct SmokeRun {
    code: Option<i32>,
    stderr: String,
    calls: Vec<String>,
    attempts: usize,
}

/// True only when the run failed because the OS refused to exec the stub (ETXTBSY: a
/// sibling test's fork briefly holds the freshly written stub's write descriptor) and for
/// no other reason. The script exits 1 and its stderr is exactly the two lines observed
/// from real bash (`release-smoke.sh` at fix-1): either the `--version` exec
/// (`<script>: <stub>: /usr/bin/env: bad interpreter: Text file busy` then
/// `release smoke: <stub> --version failed`) or the server-start exec
/// (`release smoke: A2A server exited before serving its AgentCard` then the shell's
/// `env: ‘<stub>’: Text file busy`). The stub path must be this run's own; the phrase
/// anywhere else, an extra line, or any other status is a genuine failure.
#[cfg(unix)]
fn is_exec_busy_failure(
    code: Option<i32>,
    stderr: &str,
    script: &std::path::Path,
    stub: &std::path::Path,
) -> bool {
    if code != Some(1) {
        return false;
    }
    let (script, stub) = (script.display().to_string(), stub.display().to_string());
    let lines: Vec<&str> = stderr.lines().collect();
    let &[first, second] = lines.as_slice() else {
        return false;
    };
    // Shape A: the `--version` exec.
    if let Some(rest) = first.strip_prefix(&format!("{script}: ")) {
        // bash may or may not add `line N: `.
        let rest = match rest.strip_prefix("line ").and_then(|r| r.split_once(": ")) {
            Some((n, tail)) if n.chars().all(|c| c.is_ascii_digit()) => tail,
            _ => rest,
        };
        return rest == format!("{stub}: /usr/bin/env: bad interpreter: Text file busy")
            && second == format!("release smoke: {stub} --version failed");
    }
    // Shape B: the server-start exec (the script dumps the server log, `env`'s message).
    first == "release smoke: A2A server exited before serving its AgentCard"
        && ["‘{}’", "'{}'", "\"{}\""]
            .iter()
            .any(|q| second == format!("env: {}: Text file busy", q.replace("{}", &stub)))
}

/// One run of the smoke script against `stub` in a scratch HOME.
#[cfg(unix)]
fn run_smoke_once(stub: &std::path::Path, home: &std::path::Path) -> (Option<i32>, String) {
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(SMOKE_SCRIPT);
    let out = std::process::Command::new("bash")
        .arg(&script)
        .arg(stub)
        .env("HOME", home)
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run release-smoke.sh");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Runs the smoke script against a stub whose text is `body(sentinel)`; the stub should
/// append its arguments to the sentinel file.
///
/// The script executes the stub by design, so there is no seam to avoid exec-ing a file
/// this test just wrote. The exec failure ETXTBSY (see [`is_exec_busy_failure`]) is
/// retried, at most `ATTEMPTS` times and without sleeping; every other outcome is final
/// at once. Exhausting the attempts fails with the last attempt's stderr.
#[cfg(unix)]
fn run_smoke(body: impl Fn(&std::path::Path) -> String) -> SmokeRun {
    use std::os::unix::fs::PermissionsExt;
    const ATTEMPTS: usize = 5;
    let dir = tempfile::tempdir().expect("tempdir");
    let sentinel = dir.path().join("sentinel.txt");
    let stub = dir.path().join("rustain-stub");
    std::fs::write(&stub, body(&sentinel)).expect("write stub");
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).expect("chmod stub");
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(SMOKE_SCRIPT);
    for attempt in 1..=ATTEMPTS {
        let _ = std::fs::remove_file(&sentinel);
        let (code, stderr) = run_smoke_once(&stub, dir.path());
        if is_exec_busy_failure(code, &stderr, &script, &stub) {
            if attempt == ATTEMPTS {
                panic!(
                    "release-smoke.sh could not exec the stub after {ATTEMPTS} attempts \
                     (ETXTBSY every time); last stderr:\n{stderr}"
                );
            }
            continue;
        }
        let calls = std::fs::read_to_string(&sentinel)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect();
        return SmokeRun {
            code,
            stderr,
            calls,
            attempts: attempt,
        };
    }
    unreachable!("the loop returns or panics on its last attempt")
}

/// A stub `<binary>` that records its arguments and prints `block` on `--version`.
/// Returns (exit code, stderr, recorded invocations).
#[cfg(unix)]
fn run_smoke_against_stub(block: &str) -> (Option<i32>, String, Vec<String>) {
    let run = run_smoke(|sentinel| {
        format!(
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" >>'{}'\nif [[ \"${{1:-}}\" == \"--version\" ]]; then\ncat <<'BLOCK'\n{block}\nBLOCK\nfi\n",
            sentinel.display()
        )
    });
    (run.code, run.stderr, run.calls)
}

#[cfg(unix)]
const STUB_PROTOCOLS: &str = "protocols: agent-skills, acp 1, mcp 2025-11-25, a2a 0.3";

#[cfg(unix)]
fn stub_block(features: &str, protocols: &str) -> String {
    format!(
        "rustain 9.9.9 (stub)\nbuild: x\ncommit: y\nrustc: 1.0.0\n{protocols}\nfeatures: {features}\nnot built: gmail"
    )
}

/// fix-1 K9b: the real script refuses a block lacking a required feature (or listing
/// an unbuilt protocol) before it launches any server, and crosses the check when the
/// block is complete.
#[test]
#[cfg(unix)]
fn k9b_smoke_checks_version_before_starting_a_server() {
    let (code, stderr, calls) = run_smoke_against_stub(&stub_block(
        "a2a, cron, mcp, relay-server, self-update, slack, telegram",
        STUB_PROTOCOLS,
    ));
    assert_eq!(code, Some(1), "stderr: {stderr}");
    assert!(
        stderr.contains("p2p"),
        "message names the missing feature: {stderr}"
    );
    assert_eq!(
        calls,
        ["--version"],
        "no server may start on a failed check"
    );

    let (code, stderr, calls) = run_smoke_against_stub(&stub_block(
        "a2a, cron, mcp, p2p, relay-server, self-update, slack, telegram",
        "protocols: agent-skills, acp 1, mcp 2025-11-25, a2a not built",
    ));
    assert_eq!(code, Some(1), "stderr: {stderr}");
    assert!(stderr.contains("not built"), "{stderr}");
    assert_eq!(calls, ["--version"]);

    // Positive control: a complete block crosses the check and the script launches
    // the server (which the stub then fails to be; that later failure is not asserted).
    let (_, _, calls) = run_smoke_against_stub(&stub_block(
        "a2a, cron, mcp, p2p, relay-server, self-update, slack, telegram",
        STUB_PROTOCOLS,
    ));
    assert_eq!(calls.first().map(String::as_str), Some("--version"));
    assert!(
        calls.iter().any(|c| c.starts_with("--serve-a2a=")),
        "a complete block must reach the server start: {calls:?}"
    );
}

/// The retry classifier accepts exactly the two exec-failure shapes real bash produced
/// for this stub and nothing else.
#[test]
#[cfg(unix)]
fn k9b_busy_retry_matches_only_the_stubs_own_exec_failure() {
    use std::path::Path;
    let (script, stub) = (Path::new("/r/release-smoke.sh"), Path::new("/t/stub"));
    let a = "/r/release-smoke.sh: /t/stub: /usr/bin/env: bad interpreter: Text file busy\n\
             release smoke: /t/stub --version failed\n";
    let b = "release smoke: A2A server exited before serving its AgentCard\n\
             env: ‘/t/stub’: Text file busy\n";
    assert!(is_exec_busy_failure(Some(1), a, script, stub), "shape A");
    assert!(is_exec_busy_failure(Some(1), b, script, stub), "shape B");
    let a_line = a.replace("release-smoke.sh: ", "release-smoke.sh: line 28: ");
    assert!(is_exec_busy_failure(Some(1), &a_line, script, stub));
    // Everything else is a genuine failure and final at once.
    assert!(!is_exec_busy_failure(Some(126), a, script, stub), "status");
    assert!(!is_exec_busy_failure(None, a, script, stub), "signal");
    assert!(!is_exec_busy_failure(
        Some(1),
        &format!("{a}extra\n"),
        script,
        stub
    ));
    assert!(!is_exec_busy_failure(
        Some(1),
        &format!("noise\n{b}"),
        script,
        stub
    ));
    assert!(
        !is_exec_busy_failure(Some(1), b, script, Path::new("/t/other")),
        "path"
    );
    assert!(!is_exec_busy_failure(
        Some(1),
        "Text file busy\nrelease smoke: /t/stub --version failed\n",
        script,
        stub
    ));
    assert!(!is_exec_busy_failure(Some(1), "", script, stub));
}

/// The accepted shapes are the ones real bash prints: hold the stub open for writing
/// (deterministic ETXTBSY) and classify the script's actual output, then release it.
#[test]
#[cfg(unix)]
fn k9b_busy_classifier_recognises_a_real_etxtbsy_run() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("tempdir");
    let stub = dir.path().join("rustain-stub");
    std::fs::write(&stub, "#!/usr/bin/env bash\nexit 0\n").expect("write stub");
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(SMOKE_SCRIPT);
    let held = std::fs::OpenOptions::new()
        .append(true)
        .open(&stub)
        .expect("hold the stub open for writing");
    let (code, stderr) = run_smoke_once(&stub, dir.path());
    assert!(
        is_exec_busy_failure(code, &stderr, &script, &stub),
        "a real ETXTBSY run must be classified as busy: {code:?} {stderr:?}"
    );
    drop(held);
    let (code, stderr) = run_smoke_once(&stub, dir.path());
    assert!(
        !is_exec_busy_failure(code, &stderr, &script, &stub),
        "once released the stub execs: {code:?} {stderr:?}"
    );
}

/// Positive control: a stub that itself prints `Text file busy` from its own logic (and
/// fails) is a genuine failure: run exactly once, its stderr returned intact.
#[test]
#[cfg(unix)]
fn k9b_text_file_busy_from_the_stub_is_not_retried() {
    let run = run_smoke(|sentinel| {
        format!(
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" >>'{}'\necho 'Text file busy' >&2\nexit 7\n",
            sentinel.display()
        )
    });
    assert_eq!(run.attempts, 1, "must not be retried");
    assert_eq!(run.code, Some(1));
    assert!(run.stderr.contains("Text file busy"), "{}", run.stderr);
    assert_eq!(run.calls, ["--version"]);
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
        ("teloxide", &["macros", "rustls-native-roots"][..]),
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
fn k3_size_guard_warns_on_the_reserved_margin_and_fails_above_nfr9() {
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
    // The reserve only warns (owner decision 2026-11-22); the NFR9 ceiling fails.
    for (bytes, code, warns) in [
        (ceiling - margin, 0, false),
        (ceiling - margin + 1, 0, true),
        (ceiling, 0, true),
        (ceiling + 1, 1, false),
    ] {
        file.set_len(bytes).expect("sparse binary length");
        let output = std::process::Command::new(&script)
            .arg(&binary)
            .output()
            .expect("run guard");
        assert_eq!(output.status.code(), Some(code), "{bytes}: {output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let warning = stdout
            .lines()
            .any(|line| line.starts_with("::warning ") && line.contains("RESERVED_HEADROOM_BYTES"));
        assert_eq!(warning, warns, "{bytes}: {stdout}");
        assert_eq!(stderr.contains("NFR9"), code == 1, "{bytes}: {stderr}");
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
    // The Slack feature adds a WebSocket client over the same rustls + ring stack:
    // that tree passes, and the same tree with a second stack beside it fails.
    let slack = format!("{single}tokio-websockets v0.13.0\ntokio-rustls v0.26.4\n");
    assert_eq!(run(&slack).0, Some(0), "slack single-stack tree");
    let (code, stderr) = run(&format!("{slack}aws-lc-rs v1.17.3\n"));
    assert_eq!(code, Some(1));
    assert!(stderr.contains("second TLS stack"), "{stderr}");
}

#[test]
fn k4_existing_release_builds_run_the_ratchet_before_capture_or_upload() {
    const RELEASE_RATCHET: &str = "      - name: Size ratchet\n        if: matrix.target == 'x86_64-unknown-linux-gnu'\n        run: .github/scripts/release-size-guard.sh \"${ASSET}\"\n";
    assert!(WORKFLOW.contains(RELEASE_RATCHET));
    assert!(WORKFLOW.find(RELEASE_BUILD).unwrap() < WORKFLOW.find(RELEASE_RATCHET).unwrap());
    assert!(
        WORKFLOW
            .find("      - name: Size guard (≤ 40 MiB)")
            .unwrap()
            < WORKFLOW.find(RELEASE_RATCHET).unwrap()
    );
    assert!(
        WORKFLOW.find(RELEASE_RATCHET).unwrap()
            < WORKFLOW
                .find("        uses: actions/upload-artifact@v4")
                .unwrap()
    );
    let ci = include_str!("../.github/workflows/ci.yml");
    assert!(
        !ci.lines()
            .any(|line| line.contains("cargo build") && line.contains("--release")),
        "a per-push release build skips the ratchet; release builds live in nightly-proofs.yml"
    );
    const BUILD_AND_GUARD: &str = concat!(
        "      - name: Build shipped release set capture binary\n",
        "        run: cargo build --locked --release --features self-update,relay-server,a2a,p2p,telegram,cron,slack\n",
        "      - name: Size ratchet\n",
        "        run: .github/scripts/release-size-guard.sh target/release/rustain\n",
    );
    const TLS_GUARD: &str = concat!(
        "      - name: Single TLS stack (rustls + ring)\n",
        "        shell: bash\n",
        "        run: |\n",
        "          for features in '' self-update,relay-server,a2a,p2p,telegram,cron,slack; do\n",
        "            args=()\n",
        "            if [ -n \"$features\" ]; then args=(--features \"$features\"); fi\n",
        "            cargo tree --locked -e normal --prefix none --format '{p}' \"${args[@]}\" \\\n",
        "              | .github/scripts/single-tls-stack.sh \"${features:-default}\"\n",
        "          done\n",
    );
    const SMOKE: &str = "      - name: Smoke required release features\n        shell: bash\n        run: .github/scripts/release-smoke.sh target/release/rustain\n";
    const SHARE: &str = "      - name: Share the shipped release binary\n        uses: actions/upload-artifact@v4\n        with:\n          name: shipped-release-binary\n";
    const FETCH: &str = "        uses: actions/download-artifact@v4\n        with:\n          name: shipped-release-binary\n          path: target/release\n";
    const NATIVE_RUN: &str = concat!(
        "          env -u RUSTAIN_TUI_CLIENT_BINARY -u RUSTAIN_CONFLICT_CAPTURE_ONLY pytest \\\n",
        "            'tests_tui/test_story_19_21b_conflict_flag.py::test_conflict_flag_capture[80x24]' \\\n",
        "            -p no:rerunfailures -n 0 --timeout=420 --junitxml=conflict-native-results.xml -v\n",
    );
    assert_eq!(
        PROOFS.matches("cargo build ").count(),
        1,
        "one shipped-set release build feeds every capture"
    );
    let build = job(PROOFS, "shipped-release");
    let positions = [BUILD_AND_GUARD, TLS_GUARD, SMOKE, SHARE].map(|step| {
        build
            .find(step)
            .unwrap_or_else(|| panic!("missing exact shipped-release step: {step}"))
    });
    assert!(
        positions.is_sorted(),
        "the shipped build must check size, TLS and the smoke before it is shared"
    );
    for id in ["tui-act1", "tui-act2", "tui-conflict-flag"] {
        let capture = job(PROOFS, id);
        assert!(
            capture.contains("    needs: shipped-release\n") && capture.contains(FETCH),
            "{id} must capture the shared, ratcheted shipped binary"
        );
        assert!(!capture.contains("cargo build"), "{id} must not rebuild");
    }
    let capture = job(PROOFS, "tui-conflict-flag");
    let positions = [
        FETCH,
        "      - name: Verify existing ephemeral TLS fixture tool\n        run: openssl version\n",
        "      - name: Execute exact conflict 80x24 capture without reruns\n",
        NATIVE_RUN,
        "      - name: Execute exact three current conflict receipt controls\n",
        "      - name: Retain allowlisted conflict capture and integrity evidence\n",
    ]
    .map(|step| {
        capture
            .find(step)
            .unwrap_or_else(|| panic!("missing exact Conflict workflow step: {step}"))
    });
    assert!(
        positions.is_sorted(),
        "Conflict must fetch the shipped binary, check OpenSSL, then capture, verify and publish"
    );
    assert!(
        positions[4]
            < capture
                .find("        uses: actions/upload-artifact@v4")
                .unwrap(),
        "Conflict upload must follow the current receipt controls"
    );
}

/// The text of one job in a workflow: from its `  <id>:` header to the next
/// two-space-indented line (the next job id or a comment above it).
fn job<'a>(workflow: &'a str, id: &str) -> &'a str {
    let body = workflow
        .split_once(&format!("\n  {id}:\n"))
        .unwrap_or_else(|| panic!("missing workflow job: {id}"))
        .1;
    let end = body
        .match_indices("\n  ")
        .find(|(at, _)| !body[at + 3..].starts_with(' '))
        .map_or(body.len(), |(at, _)| at);
    &body[..end]
}

/// rel-4: the sign job publishes `dist/install.sh` as `install.sh`, only when its
/// `DEFAULT_VERSION` is the tag, and lists it in the signed `SHA256SUMS`.
#[test]
fn k10_release_publishes_the_install_script_in_the_signed_manifest() {
    const STAGE: &str = "      - name: Stage install script\n        shell: bash\n        run: |\n          # dist/install.sh installs its DEFAULT_VERSION by default; publish it only for that tag.\n          grep -qxF \"DEFAULT_VERSION=${GITHUB_REF_NAME#v}\" dist/install.sh\n          cp dist/install.sh release/install.sh\n";
    const SUMS: &str = "          sha256sum rustain-* install.sh > SHA256SUMS\n";
    let stage = WORKFLOW.find(STAGE).expect("stage step");
    let sums = WORKFLOW.find(SUMS).expect("SHA256SUMS lists install.sh");
    let sign = WORKFLOW
        .find("      - name: Sign SHA256SUMS (prehashed ED)")
        .expect("sign step");
    assert!(stage < sums && sums < sign);
    assert_eq!(
        WORKFLOW.matches("sha256sum ").count(),
        1,
        "one manifest command"
    );
}
#[test]
fn k11_every_release_composition_site_names_the_shipped_set() {
    use std::collections::BTreeSet;

    fn value<'a>(text: &'a str, anchor: &str, delimiter: &str) -> &'a str {
        let line = text
            .lines()
            .find(|line| line.contains(anchor))
            .unwrap_or_else(|| panic!("missing release composition anchor: {anchor}"));
        line.split_once(delimiter)
            .unwrap_or_else(|| panic!("missing feature delimiter at {anchor}"))
            .1
            .split_whitespace()
            .next()
            .expect("feature value")
            .trim_matches('"')
    }
    fn set(value: &str) -> BTreeSet<&str> {
        value.split(',').collect()
    }
    fn mismatch<'a>(
        sites: &[(&'a str, BTreeSet<&str>)],
        expected: &BTreeSet<&str>,
    ) -> Vec<&'a str> {
        sites
            .iter()
            .filter_map(|(name, actual)| (actual != expected).then_some(*name))
            .collect()
    }

    let build = job(PROOFS, "shipped-release");
    let smoke = include_str!("../.github/scripts/release-smoke.sh");
    let install = include_str!("../docs/install-macos.md");
    let manual = include_str!("../RELEASING.md");
    let release = set(value(
        WORKFLOW,
        "cargo build --release --target ${{ matrix.target }} --features",
        "--features ",
    ));
    let smoke_features = smoke
        .lines()
        .find_map(|line| line.strip_prefix("REQUIRED_FEATURES=\""))
        .and_then(|line| line.strip_suffix('"'))
        .expect("REQUIRED_FEATURES anchor")
        .split_whitespace()
        .collect();
    let tls = build
        .lines()
        .find_map(|line| line.trim().strip_prefix("for features in '' "))
        .and_then(|line| line.strip_suffix("; do"))
        .expect("TLS loop anchor");
    let sites = vec![
        ("release.yml", release.clone()),
        (
            "nightly-proofs shipped-release",
            set(value(
                build,
                "cargo build --locked --release --features",
                "--features ",
            )),
        ),
        ("TLS loop", set(tls)),
        ("release-smoke.sh", smoke_features),
        (
            "install-macos.md",
            set(value(
                install,
                "cargo install --locked --git",
                "--features ",
            )),
        ),
        (
            "RELEASING.md",
            set(value(
                manual,
                "cargo build --release --target \"$TARGET\"",
                "--features ",
            )),
        ),
    ];
    let absolute: BTreeSet<_> = [
        "self-update",
        "relay-server",
        "a2a",
        "p2p",
        "telegram",
        "cron",
        "slack",
    ]
    .into_iter()
    .collect();
    assert_eq!(
        release, absolute,
        "release build differs from the shipped set"
    );
    assert_eq!(mismatch(&sites, &release), Vec::<&str>::new());
    let altered = build.replace(
        "cargo build --locked --release --features self-update,relay-server,a2a,p2p,telegram,cron,slack",
        "cargo build --locked --release --features self-update,relay-server,a2a,p2p,telegram,cron",
    );
    assert_ne!(altered, build, "drift fixture must change the build line");
    let mut altered_sites = sites.clone();
    altered_sites[1].1 = set(value(
        &altered,
        "cargo build --locked --release --features",
        "--features ",
    ));
    let without_slack: BTreeSet<_> = absolute.iter().copied().filter(|f| *f != "slack").collect();
    assert_eq!(
        altered_sites[1].1, without_slack,
        "fixture removes exactly slack"
    );
    assert_eq!(
        mismatch(&altered_sites, &release),
        vec!["nightly-proofs shipped-release"]
    );
}
