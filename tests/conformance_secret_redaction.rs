//! Conformance test: allowlist for `expose_secret()` and `expose_url()` call sites.
//!
//! Story 14.0 AC5 — exact file-set equality + pinned count + per-sink positive
//! control + matcher self-test.  Excludes `#[cfg(test)]` blocks and `tests/`.

use std::collections::BTreeSet;
use std::path::Path;

/// Collect all `.rs` files under a directory, recursively.
fn collect_rs_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_rs_files(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }
}

/// Scan `src/` for lines matching `pattern`, excluding `#[cfg(test)]` blocks and
/// test modules.  Returns `(file_set, total_count)`.
fn scan_src_for_pattern(pattern: &str) -> (BTreeSet<String>, usize) {
    let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut file_set = BTreeSet::new();
    let mut total = 0usize;

    let mut rs_files = Vec::new();
    collect_rs_files(&src_dir, &mut rs_files);

    for path in &rs_files {
        let content = std::fs::read_to_string(path).unwrap();

        let mut in_test_block = false;
        let mut brace_depth: i32 = 0;
        let mut test_brace_start: i32 = 0;

        for line in content.lines() {
            let trimmed = line.trim();

            if trimmed == "#[cfg(test)]" {
                in_test_block = true;
                continue;
            }

            if in_test_block && brace_depth == test_brace_start {
                if trimmed.contains('{') {
                    test_brace_start = brace_depth;
                    for ch in trimmed.chars() {
                        match ch {
                            '{' => brace_depth += 1,
                            '}' => brace_depth -= 1,
                            _ => {}
                        }
                    }
                    if brace_depth <= test_brace_start {
                        in_test_block = false;
                    }
                    continue;
                }
            }

            if in_test_block && brace_depth > test_brace_start {
                for ch in trimmed.chars() {
                    match ch {
                        '{' => brace_depth += 1,
                        '}' => brace_depth -= 1,
                        _ => {}
                    }
                }
                if brace_depth <= test_brace_start {
                    in_test_block = false;
                }
                continue;
            }

            if trimmed.contains(pattern) {
                let rel = path
                    .strip_prefix(&src_dir)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                file_set.insert(rel);
                total += 1;
            }

            for ch in trimmed.chars() {
                match ch {
                    '{' => brace_depth += 1,
                    '}' => brace_depth -= 1,
                    _ => {}
                }
            }
        }
    }

    (file_set, total)
}

// ---------------------------------------------------------------------------
// expose_secret()
// ---------------------------------------------------------------------------

#[test]
fn expose_secret_file_set_is_exactly_the_allowlist() {
    let (files, count) = scan_src_for_pattern(".expose_secret()");

    let expected: BTreeSet<String> = [
        "adapters/auth_store.rs",
        "adapters/anthropic/mod.rs",
        "adapters/openai/mod.rs",
        "domain/models/credential.rs",
        "domain/models/secret.rs",
        "infrastructure/provider_factory.rs",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();

    assert_eq!(
        files, expected,
        "expose_secret() file set mismatch.\n  Got:      {files:?}\n  Expected: {expected:?}"
    );

    // Pinned count — a new call site fails CI; zero also fails.
    assert!(
        count >= 1,
        "expose_secret() count is 0 — the matcher is broken"
    );
    // Exact pinned count of non-test expose_secret() calls.
    assert_eq!(
        count, 13,
        "expose_secret() pinned count changed: got {count}, expected 13. \
         If you added a legitimate call site, update the allowlist AND this count."
    );
}

#[test]
fn expose_secret_per_sink_positive_control() {
    let (files, _) = scan_src_for_pattern(".expose_secret()");

    // Per-sink positive control — each of the 6 allowlisted files is asserted present.
    assert!(
        files.contains("adapters/auth_store.rs"),
        "auth_store.rs must call expose_secret()"
    );
    assert!(
        files.contains("adapters/anthropic/mod.rs"),
        "anthropic/mod.rs must call expose_secret()"
    );
    assert!(
        files.contains("adapters/openai/mod.rs"),
        "openai/mod.rs must call expose_secret()"
    );
    assert!(
        files.contains("domain/models/credential.rs"),
        "credential.rs must call expose_secret() (AC2 delegation)"
    );
    assert!(
        files.contains("domain/models/secret.rs"),
        "secret.rs must call expose_secret() (expose_secret_string helper)"
    );
    assert!(
        files.contains("infrastructure/provider_factory.rs"),
        "provider_factory.rs must call expose_secret() (resolve_api_key boundary)"
    );
}

#[test]
fn expose_secret_matcher_self_test() {
    // A known-positive fixture string is asserted to match the scanner's pattern.
    let fixture = r#"let val = secret.expose_secret();"#;
    assert!(
        fixture.contains(".expose_secret()"),
        "Matcher self-test failed: the pattern '.expose_secret()' does not match the fixture"
    );
}

// ---------------------------------------------------------------------------
// expose_url()
// ---------------------------------------------------------------------------

#[test]
fn expose_url_file_set_is_exactly_the_allowlist() {
    let (files, count) = scan_src_for_pattern(".expose_url()");

    let expected: BTreeSet<String> = ["infrastructure/provider_factory.rs"]
        .iter()
        .map(|s| s.to_string())
        .collect();

    assert_eq!(
        files, expected,
        "expose_url() file set mismatch.\n  Got:      {files:?}\n  Expected: {expected:?}"
    );

    assert!(
        count >= 1,
        "expose_url() count is 0 — the matcher is broken"
    );
    assert_eq!(
        count, 5,
        "expose_url() pinned count changed: got {count}, expected 5. \
         If you added a legitimate call site, update the allowlist AND this count."
    );
}

#[test]
fn expose_url_per_sink_positive_control() {
    let (files, _) = scan_src_for_pattern(".expose_url()");

    assert!(
        files.contains("infrastructure/provider_factory.rs"),
        "provider_factory.rs must call expose_url()"
    );
}

#[test]
fn expose_url_matcher_self_test() {
    let fixture = r#"let val = url.expose_url();"#;
    assert!(
        fixture.contains(".expose_url()"),
        "Matcher self-test failed"
    );
}
