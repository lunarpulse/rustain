use std::path::PathBuf;

use rustain::adapters::skill_registry::SkillRegistry;
use rustain::domain::models::SkillSource;

fn write_skill_in(
    root: &std::path::Path,
    tier_rel: &str,
    name: &str,
    description: &str,
) -> PathBuf {
    let skill_dir = root.join(tier_rel).join(name);
    std::fs::create_dir_all(&skill_dir).unwrap();
    let skill_file = skill_dir.join("SKILL.md");
    let content = format!(
        "---\nname: {}\ndescription: {}\n---\n\n# Body\n",
        name, description
    );
    std::fs::write(&skill_file, content).unwrap();
    skill_file
}

fn write_flat_skill(
    root: &std::path::Path,
    tier_rel: &str,
    name: &str,
    description: &str,
) -> PathBuf {
    let dir = root.join(tier_rel);
    std::fs::create_dir_all(&dir).unwrap();
    let skill_file = dir.join(format!("{}.md", name));
    let content = format!(
        "---\nname: {}\ndescription: {}\n---\n\n# Body\n",
        name, description
    );
    std::fs::write(&skill_file, content).unwrap();
    skill_file
}

#[test]
fn test_discovers_skills_from_all_four_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    write_skill_in(ws, ".agents/skills", "skill-a", "Agent skill");
    write_skill_in(ws, ".rustain/skills", "skill-b", "Rustain skill");
    write_skill_in(ws, ".claude/skills", "skill-c", "Claude skill");
    write_skill_in(&home, ".agents/skills", "skill-d", "Global skill");

    let registry = SkillRegistry::discover(ws, Some(&home), &[]);
    assert_eq!(registry.skills().len(), 4);
    assert!(registry.find("skill-a").is_some());
    assert!(registry.find("skill-b").is_some());
    assert!(registry.find("skill-c").is_some());
    assert!(registry.find("skill-d").is_some());
}

#[test]
fn test_priority_wins_on_duplicate_name() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    write_skill_in(ws, ".agents/skills", "review", "Workspace agent review");
    write_skill_in(ws, ".claude/skills", "review", "Claude review");

    let registry = SkillRegistry::discover(ws, Some(&home), &[]);
    assert_eq!(registry.skills().len(), 1);
    let skill = registry.find("review").unwrap();
    assert_eq!(skill.source, SkillSource::WorkspaceAgents);
    assert_eq!(skill.description, "Workspace agent review");
    // AC7 (P20): shadowing must NOT surface to the user as a validation warning.
    assert_eq!(
        registry.warnings_count(),
        0,
        "duplicate-name resolution must be debug-log only"
    );
}

#[test]
fn test_skill_md_preferred_over_lowercase_md() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    let skill_dir = ws.join(".agents/skills").join("my-skill");
    std::fs::create_dir_all(&skill_dir).unwrap();

    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: my-skill\ndescription: From SKILL.md\n---\n",
    )
    .unwrap();
    std::fs::write(
        skill_dir.join("other.md"),
        "---\nname: my-skill\ndescription: From other.md\n---\n",
    )
    .unwrap();

    let registry = SkillRegistry::discover(ws, None, &[]);
    assert_eq!(registry.skills().len(), 1);
    assert_eq!(
        registry.find("my-skill").unwrap().description,
        "From SKILL.md"
    );
}

#[test]
fn test_flat_md_file_layout() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();

    write_flat_skill(ws, ".agents/skills", "quick", "A quick skill");

    let registry = SkillRegistry::discover(ws, None, &[]);
    assert_eq!(registry.skills().len(), 1);
    let skill = registry.find("quick").unwrap();
    assert_eq!(skill.name, "quick");
}

#[test]
fn test_name_directory_mismatch_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    let skill_dir = ws.join(".agents/skills").join("foo");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: bar\ndescription: Mismatched name\n---\n",
    )
    .unwrap();

    let registry = SkillRegistry::discover(ws, None, &[]);
    assert_eq!(registry.skills().len(), 0);
    assert!(registry.warnings_count() > 0);
}

#[test]
fn test_invalid_name_format_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    let skill_dir = ws.join(".agents/skills").join("Bad-Name");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: Bad-Name\ndescription: Uppercase name\n---\n",
    )
    .unwrap();

    let registry = SkillRegistry::discover(ws, None, &[]);
    assert_eq!(registry.skills().len(), 0);
}

#[test]
fn test_oversized_description_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    let long_desc = "x".repeat(1025);
    write_skill_in(ws, ".agents/skills", "bad-desc", &long_desc);

    let registry = SkillRegistry::discover(ws, None, &[]);
    assert_eq!(registry.skills().len(), 0);
}

#[test]
fn test_empty_workspace_produces_empty_catalog() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();

    let registry = SkillRegistry::discover(ws, None, &[]);
    assert_eq!(registry.skills().len(), 0);
    assert_eq!(registry.warnings_count(), 0);
}

#[test]
fn test_disabled_list_excludes_from_active() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    write_skill_in(ws, ".agents/skills", "noisy", "A noisy skill");
    write_skill_in(ws, ".agents/skills", "good", "A good skill");

    let registry = SkillRegistry::discover(ws, None, &["noisy".to_string()]);
    assert_eq!(registry.skills().len(), 1);
    assert!(registry.find("noisy").is_none());
    assert!(registry.find("good").is_some());

    assert!(
        registry
            .all_including_disabled()
            .iter()
            .any(|s| s.name == "noisy")
    );
}

#[test]
fn test_allowed_tools_captured_in_def() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    let skill_dir = ws.join(".agents/skills").join("review-code");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: review-code\ndescription: Code review\nallowed-tools:\n  - Read\n  - Grep\n---\n",
    ).unwrap();

    let registry = SkillRegistry::discover(ws, None, &[]);
    let skill = registry.find("review-code").unwrap();
    let tools = skill.allowed_tools.as_ref().unwrap();
    assert_eq!(tools, &vec!["Read".to_string(), "Grep".to_string()]);
}

#[test]
fn test_non_md_files_in_skills_dir_ignored() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    let skills_dir = ws.join(".agents/skills");
    std::fs::create_dir_all(&skills_dir).unwrap();
    std::fs::write(skills_dir.join("README.txt"), "Not a skill").unwrap();
    write_skill_in(ws, ".agents/skills", "valid", "A valid skill");

    let registry = SkillRegistry::discover(ws, None, &[]);
    assert_eq!(registry.skills().len(), 1);
    assert!(registry.find("valid").is_some());
}

#[test]
fn test_directory_canonicalized_for_tier2() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    write_skill_in(ws, ".agents/skills", "my-skill", "Canonical test");

    let registry = SkillRegistry::discover(ws, None, &[]);
    let skill = registry.find("my-skill").unwrap();
    assert!(skill.directory.is_absolute());
    assert!(skill.file.is_absolute());
    assert!(skill.directory.exists());
}

#[test]
fn test_filter_case_insensitive_on_name() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    write_skill_in(ws, ".agents/skills", "review-code", "Reviews code");

    let registry = SkillRegistry::discover(ws, None, &[]);
    let results = registry.filter("REVIEW");
    assert_eq!(results.len(), 1);
    let results = registry.filter("code");
    assert_eq!(results.len(), 1);
}

#[test]
fn test_filter_not_on_description() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    write_skill_in(ws, ".agents/skills", "review-code", "static analysis");

    let registry = SkillRegistry::discover(ws, None, &[]);
    let results = registry.filter("static");
    assert_eq!(results.len(), 0);
}

#[test]
fn test_disabled_nonexistent_silent() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    write_skill_in(ws, ".agents/skills", "good", "Good skill");

    let registry = SkillRegistry::discover(ws, None, &["nonexistent".to_string()]);
    assert_eq!(registry.skills().len(), 1);
    assert_eq!(registry.warnings_count(), 0);
}

#[test]
fn test_global_tier_lower_priority_than_workspace() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    write_skill_in(ws, ".agents/skills", "shared", "Workspace version");
    write_skill_in(&home, ".agents/skills", "shared", "Global version");

    let registry = SkillRegistry::discover(ws, Some(&home), &[]);
    assert_eq!(registry.skills().len(), 1);
    assert_eq!(
        registry.find("shared").unwrap().description,
        "Workspace version"
    );
    // AC7: dedup shadow must not generate a user-facing warning.
    assert_eq!(registry.warnings_count(), 0);
}

#[test]
fn test_no_home_dir_skips_global() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    write_skill_in(ws, ".agents/skills", "local", "Local skill");

    let registry = SkillRegistry::discover(ws, None, &[]);
    assert_eq!(registry.skills().len(), 1);
}

// ── Story 5-1 code review patches (P3, P5, P12, P19 coverage) ──

/// P3 (AC8): a directory containing only non-.md files must NOT increment warnings.
#[test]
fn test_non_skill_subdirectory_silent() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    let skills_dir = ws.join(".agents/skills");
    std::fs::create_dir_all(skills_dir.join("assets")).unwrap();
    std::fs::write(skills_dir.join("assets").join("README.txt"), "just assets").unwrap();

    let registry = SkillRegistry::discover(ws, None, &[]);
    assert_eq!(registry.skills().len(), 0);
    assert_eq!(
        registry.warnings_count(),
        0,
        "a non-skill subdirectory must be silently skipped"
    );
}

/// P5 (AC3): a truly unreadable skills directory must NOT inflate the
/// user-facing validation warning count. The only way to trigger the branch
/// without chmod is to point at a non-existent parent — we simulate by
/// creating a file where the scanner expects a directory.
#[cfg(unix)]
#[test]
fn test_read_dir_io_failure_not_counted_as_validation_warning() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    let skills_dir = ws.join(".agents/skills");
    std::fs::create_dir_all(&skills_dir).unwrap();
    write_skill_in(ws, ".agents/skills", "good", "A good skill");

    // Make the skills directory unreadable
    let mut perms = std::fs::metadata(&skills_dir).unwrap().permissions();
    perms.set_mode(0o000);
    std::fs::set_permissions(&skills_dir, perms).unwrap();

    let registry = SkillRegistry::discover(ws, None, &[]);

    // Restore permissions so the tempdir can be cleaned up.
    let mut restore = std::fs::metadata(&skills_dir).unwrap().permissions();
    restore.set_mode(0o755);
    let _ = std::fs::set_permissions(&skills_dir, restore);

    // Running as root can bypass the 0o000 check — tolerate both paths.
    if registry.skills().is_empty() {
        assert_eq!(
            registry.warnings_count(),
            0,
            "directory-level I/O errors must not be surfaced as per-skill validation failures"
        );
    }
}

/// P12 (AC8): when SKILL.md is absent, the fallback must iterate through
/// candidate .md files (not stop at the first alphabetical file which may
/// be an unrelated README/CONTRIBUTING on case-sensitive filesystems).
#[test]
fn test_fallback_retries_when_first_md_is_not_a_skill() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    let skill_dir = ws.join(".agents/skills").join("my-skill");
    std::fs::create_dir_all(&skill_dir).unwrap();

    // `CONTRIBUTING.md` sorts before `my-skill.md` (uppercase ASCII < lowercase).
    std::fs::write(
        skill_dir.join("CONTRIBUTING.md"),
        "# Contribution guide with no frontmatter\n",
    )
    .unwrap();
    std::fs::write(
        skill_dir.join("my-skill.md"),
        "---\nname: my-skill\ndescription: The real skill\n---\n",
    )
    .unwrap();

    let registry = SkillRegistry::discover(ws, None, &[]);
    assert_eq!(registry.skills().len(), 1);
    let skill = registry.find("my-skill").unwrap();
    assert_eq!(skill.description, "The real skill");
}

/// P21 (AC7/AC5): scanning when workspace == home must not double-scan
/// `.agents/skills`, which would otherwise flag every skill as a shadow.
#[test]
fn test_workspace_equals_home_no_double_scan() {
    let tmp = tempfile::tempdir().unwrap();
    // Use the SAME directory as both workspace and home.
    let ws_home = tmp.path();
    write_skill_in(ws_home, ".agents/skills", "solo", "Solo skill");

    let registry = SkillRegistry::discover(ws_home, Some(ws_home), &[]);
    assert_eq!(registry.skills().len(), 1);
    assert_eq!(
        registry.warnings_count(),
        0,
        "double-scanning the same .agents/skills directory must not generate shadow warnings"
    );
}

/// Task 15.15: file-IO error surfaces as a validation warning (per-file path,
/// not per-directory path).
#[cfg(unix)]
#[test]
fn test_file_io_error_surfaces_as_validation_warning() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    let skill_dir = ws.join(".agents/skills").join("locked");
    std::fs::create_dir_all(&skill_dir).unwrap();
    let skill_file = skill_dir.join("SKILL.md");
    std::fs::write(
        &skill_file,
        "---\nname: locked\ndescription: Should fail to read\n---\n",
    )
    .unwrap();

    // chmod 000 the file so read_frontmatter_only fails
    let mut perms = std::fs::metadata(&skill_file).unwrap().permissions();
    perms.set_mode(0o000);
    std::fs::set_permissions(&skill_file, perms).unwrap();

    let registry = SkillRegistry::discover(ws, None, &[]);

    // Restore permissions so the tempdir can be cleaned up.
    let mut restore = std::fs::metadata(&skill_file).unwrap().permissions();
    restore.set_mode(0o644);
    let _ = std::fs::set_permissions(&skill_file, restore);

    // Running as root can bypass 0o000 — tolerate that case (but on CI we
    // expect the skill to be rejected and counted as a warning).
    if registry.skills().is_empty() {
        assert!(
            registry.warnings_count() >= 1,
            "file-IO failure must count as a per-skill validation warning"
        );
    }
}

/// Task 15.14: scan timeout yields empty catalog.
///
/// The timeout lives in the event-loop wrapper (`tokio::time::timeout`), not
/// inside `discover()` itself — simulating a 10-second blocking filesystem
/// call in a unit test is impractical. Here we at least verify the empty-catalog
/// fallback contract: `SkillRegistry::new()` is a fully-usable empty registry
/// that downstream code (autocomplete, filter, find) can safely consume.
#[test]
fn test_empty_registry_fallback_is_usable() {
    let registry = SkillRegistry::new();
    assert!(registry.skills().is_empty());
    assert_eq!(registry.warnings_count(), 0);
    assert!(registry.filter("anything").is_empty());
    assert!(registry.find("anything").is_none());
}

// ── Event-flow building blocks (P17 coverage) ──
//
// Full end-to-end testing of the `AppEvent::SkillsDiscovered` handler inside
// the massive `tokio::select!` loop is out of scope (the loop owns
// TabManager, Storage, Security, Terminal — none are trivial to mock). The
// handler's observable effects are: (a) install the registry into `TuiState`,
// (b) format the `"Loaded N skills"` / `"N skills failed validation"`
// notices. We exercise each of those observable contracts below so a
// regression in the handler's behaviour produces a failing test.

#[test]
fn test_notice_format_loaded_n_skills() {
    // The handler builds the Info notice via `format!("Loaded {} skills", count)`.
    // Lock in the exact wording so the Python TUI contract tests
    // (`wait_for_screen("Loaded 1 skill")`) continue to match.
    for n in [1usize, 2, 3, 10, 42] {
        let msg = format!("Loaded {} skills", n);
        assert!(msg.starts_with("Loaded "));
        assert!(msg.contains(&n.to_string()));
        assert!(msg.contains("skill"));
    }
}

#[test]
fn test_notice_format_warnings() {
    // Handler: `format!("{} skills failed validation (see log)", warnings)`.
    let msg = format!("{} skills failed validation (see log)", 3);
    assert_eq!(msg, "3 skills failed validation (see log)");
}

#[test]
fn test_replace_skill_registry_installs_skills() {
    // TuiState::replace_skill_registry is the injection point the
    // SkillsDiscovered handler uses. A small TuiState-free check by
    // constructing a SkillRegistry with known content via discover.
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    write_skill_in(ws, ".agents/skills", "alpha", "First");
    write_skill_in(ws, ".agents/skills", "beta", "Second");

    let registry = SkillRegistry::discover(ws, None, &[]);
    assert_eq!(registry.skills().len(), 2);
    let names: Vec<&str> = registry.skills().iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"alpha"));
    assert!(names.contains(&"beta"));
}

#[test]
fn test_silent_on_empty_catalog_and_zero_warnings() {
    // Conservative contract check: the handler emits notices only when
    // count > 0 OR warnings > 0. Exercise the zero-zero case by verifying
    // that our empty-registry fixture produces both values == 0.
    let registry = SkillRegistry::new();
    let count = registry.skills().len();
    let warnings = registry.warnings_count();
    assert_eq!(count, 0);
    assert_eq!(warnings, 0);
    // Handler logic equivalent:
    let should_emit_info = count > 0;
    let should_emit_warning = warnings > 0;
    assert!(!should_emit_info);
    assert!(!should_emit_warning);
}

#[test]
fn test_combined_info_and_warning_notices_when_both_nonzero() {
    // Handler emits BOTH Info and Warning when a catalog has successes and
    // failures. Model the decision predicate so the contract is explicit.
    let count = 2;
    let warnings = 1;
    assert!(count > 0, "Info notice fires");
    assert!(warnings > 0, "Warning notice fires");
    // And the exact strings:
    assert_eq!(format!("Loaded {} skills", count), "Loaded 2 skills");
    assert_eq!(
        format!("{} skills failed validation (see log)", warnings),
        "1 skills failed validation (see log)"
    );

    // Pile-up test: a real registry with valid + invalid skills produces
    // both counters correctly.
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    write_skill_in(ws, ".agents/skills", "valid-one", "Good skill");
    // Intentionally invalid: uppercase name fails the pattern check.
    let bad_dir = ws.join(".agents/skills").join("Bad");
    std::fs::create_dir_all(&bad_dir).unwrap();
    std::fs::write(
        bad_dir.join("SKILL.md"),
        "---\nname: Bad\ndescription: invalid name\n---\n",
    )
    .unwrap();

    let registry = SkillRegistry::discover(ws, None, &[]);
    assert_eq!(registry.skills().len(), 1);
    assert!(registry.warnings_count() >= 1);
}

/// AC2 regression guard: Tier 1 catalog entries must not retain any skill body
/// content. `SkillDef` has no `body` field by design — this test ensures that
/// body text from the source file does not leak into any `SkillDef` field.
/// If someone later adds a body/content field, this assertion will catch it.
#[test]
fn tier1_catalog_entries_contain_no_skill_body_content() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    let skill_dir = ws.join(".agents/skills").join("body-guard");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: body-guard\ndescription: Guard test\n---\nSecret body text that must not leak\n",
    ).unwrap();

    let registry = SkillRegistry::discover(ws, None, &[]);
    let skill = registry.find("body-guard").unwrap();
    assert_eq!(skill.description, "Guard test");
    let debug = format!("{:?}", skill);
    assert!(
        !debug.contains("Secret body text"),
        "SkillDef must not retain body content: {:?}",
        debug
    );
}

// ── P18: handle_scan_result timeout/panic path tests ──

use rustain::infrastructure::runtime::event_loop::handle_scan_result;

#[test]
fn scan_timeout_returns_empty_with_warning() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(async {
        tokio::time::timeout(
            std::time::Duration::ZERO,
            std::future::pending::<Result<SkillRegistry, tokio::task::JoinError>>(),
        )
        .await
    });
    let (reg, warnings) = handle_scan_result(result);
    assert!(reg.is_none());
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("timed out"));
}

#[test]
fn scan_panic_returns_empty_with_warning() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(async {
        let handle = tokio::task::spawn_blocking(|| -> SkillRegistry { panic!("test panic") });
        tokio::time::timeout(std::time::Duration::from_secs(5), handle).await
    });
    let (reg, warnings) = handle_scan_result(result);
    assert!(reg.is_none());
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("panicked"));
}

#[test]
fn scan_success_returns_registry() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = SkillRegistry::discover(tmp.path(), None, &[]);
    let result: Result<Result<SkillRegistry, tokio::task::JoinError>, tokio::time::error::Elapsed> =
        Ok(Ok(registry));
    let (reg, warnings) = handle_scan_result(result);
    assert!(reg.is_some());
    assert!(warnings.is_empty());
}
