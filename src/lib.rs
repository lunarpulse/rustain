// Pre-existing clippy suppressions — these issues predate Story 3-0 and are
// suppressed to meet AC9 clippy gate without introducing behavioral changes.
// Sync with main.rs — both files must carry identical suppressions.
#![allow(dead_code)] // TODO(epic-4): public API surface used by integration tests; audit per-module
#![allow(unused_imports)] // TODO(epic-4): re-exports consumed by integration tests; prune unused
#![allow(clippy::too_many_arguments)] // TODO(epic-4): large render fns — refactor into smaller units
#![allow(clippy::implicit_saturating_sub)] // TODO(epic-4): audit saturating_sub usage
#![allow(clippy::redundant_closure)] // TODO(epic-4): clean up trivial closures
#![allow(clippy::needless_return)] // TODO(epic-4): remove explicit returns
#![allow(clippy::derivable_impls)] // TODO(epic-4): derive Default where possible
#![allow(clippy::collapsible_if)] // TODO(epic-4): flatten nested ifs
#![allow(clippy::collapsible_else_if)] // TODO(epic-4): flatten nested else-ifs
#![allow(clippy::doc_lazy_continuation)] // TODO(epic-4): fix doc comment formatting
#![allow(clippy::wrong_self_convention)] // TODO(epic-4): audit is_*/has_* methods
#![allow(clippy::doc_overindented_list_items)] // AI-12.1: pure doc-list formatting, same class as doc_lazy_continuation
#![allow(clippy::field_reassign_with_default)] // AI-12.1: readable `let mut x = T::default(); x.f = …` in test setup
#![allow(clippy::large_enum_variant)] // AI-12.1: daemon protocol/handler enums — boxing changes wire/layout, deferred design call
#![allow(clippy::items_after_test_module)] // AI-12.1: harmless item ordering after `#[cfg(test)] mod tests`
#![allow(clippy::manual_async_fn)] // AI-12.1: explicit `impl Future` kept where lifetime/Send bounds are clearer
#![allow(clippy::let_and_return)] // AI-12.1: named-then-return kept for readability in a few sites
#![allow(clippy::let_unit_value)] // AI-12.1: `let _x = …;` binding unit results, intentional
#![allow(clippy::collapsible_match)] // AI-12.1: same family as collapsible_if/else_if (already allowed)
#![allow(clippy::empty_line_after_doc_comments)] // AI-12.1: doc formatting, same class as doc_lazy_continuation
#![allow(clippy::type_complexity)] // AI-12.1: complex callback/closure tuple types (cron scheduler) — same class as too_many_arguments

pub mod adapters;
pub mod domain;
pub mod infrastructure;

#[cfg(test)]
mod epic_numbering_hygiene {
    // Story 19.6 A6/A9 (AC2), CI-enforced. The OAuth epic has carried the number
    // 20 since the 2026-08-23 renumber; a stale OAuth-era mention of the old
    // number misattributes the OAuth epic to the journey-coverage epic that now
    // owns it.
    //
    // This guard lives in the LIB on purpose (A9): the CI Check job runs
    // `cargo test --lib` and only COMPILES `tests/` targets unless they are
    // hand-listed, so a `tests/comment_hygiene.rs` ratchet would be green in CI
    // forever. Precedent and reasoning: the lib-hosted whole-`src/domain` scan in
    // `src/domain/ports/capability_provider.rs`
    // (`a2a_wire_types_absent_from_entire_src_domain`). Pin the claim, never the
    // identifier. Two things turn this guard red, and only the first is a defect:
    // a stale mention of the old number came back (fix the comment), or the OAuth
    // epic legitimately grew a new mention — it is still backlog, so its own
    // stories will add some; then update the expected count below, and ⛔ never
    // delete a legitimate comment to restore the old count. ⚠ Note this comment
    // names neither number literally, for the reason in the next paragraph.
    //
    // The scan needles below are ASSEMBLED, not written literally, so that this
    // file stays clean under the same `grep -rn "Epic …" src` a reviewer or a
    // future ratchet would run — the guard must not violate its own invariant.

    /// Collects every line under `dir` containing `needle`.
    ///
    /// Read errors are returned, never swallowed: a guard that silently skips an
    /// unreadable directory or a non-UTF-8 file can be green while a stale
    /// mention hides in the part it could not read. The count assertion below
    /// only catches a scan that reached *nothing*; partial loss is invisible to
    /// it, so the scan itself has to be loud.
    fn collect_matching_lines(
        dir: &std::path::Path,
        needle: &str,
        out: &mut Vec<String>,
        scanned: &mut usize,
    ) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            // `symlink_metadata` does not follow links, so a symlinked directory
            // is skipped rather than recursed: no cycle can spin this guard, and
            // no target outside the crate can pollute the count.
            let meta = std::fs::symlink_metadata(&path)?;
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                collect_matching_lines(&path, needle, out, scanned)?;
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                *scanned += 1;
                let content = std::fs::read_to_string(&path)?;
                for (idx, line) in content.lines().enumerate() {
                    if line.contains(needle) {
                        out.push(format!("{}:{}: {}", path.display(), idx + 1, line.trim()));
                    }
                }
            }
        }
        Ok(())
    }

    #[test]
    fn oauth_epic_is_numbered_20_in_src_comments() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");

        // Needles assembled (see module comment): keeps this file itself clean
        // under the grep this test encodes.
        let stale_needle = format!("Epic 1{}", 9);
        let current_needle = format!("Epic 2{}", 0);

        let mut scanned = 0usize;
        let mut stale = Vec::new();
        collect_matching_lines(&src, &stale_needle, &mut stale, &mut scanned)
            .expect("scan of src/ must succeed; a read error would hide stale mentions");

        // Positive control, following the precedent's `assert!(!files.is_empty())`:
        // proves the scan actually reached the tree before either count is trusted.
        assert!(
            scanned > 100,
            "expected the scan to reach the whole crate; only {scanned} .rs files seen"
        );
        assert!(
            stale.is_empty(),
            "the old OAuth-era epic number in src/ no longer denotes the OAuth epic \
             (renumbered 2026-08-23); stale mentions:\n{}",
            stale.join("\n")
        );

        let mut current = Vec::new();
        collect_matching_lines(&src, &current_needle, &mut current, &mut scanned)
            .expect("scan of src/ must succeed; a read error would hide OAuth-epic mentions");
        assert_eq!(
            current.len(),
            10,
            "expected exactly 10 OAuth-epic mentions under the new number in src/ (Story \
             19.6 A6 rewrote ten comments); found:\n{}",
            current.join("\n")
        );
    }
}
