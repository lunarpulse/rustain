//! K11a — the S1 cutover gate (AC4, AC8): a scoped static check over the
//! production code of BOTH hosts. It is a ratchet, not behavioral proof (K5/K6
//! drive the real producers). The deleted S1 mechanisms must not come back, and
//! `StatusState::Flash` may exist only where the S1 allowlist says (the two
//! sidebar delete-confirmation sites); slice S2 empties the allowlist.
//!
//! Gate hygiene (21-11 K7 pattern): the matched function/site is unique, the
//! number of scanned files equals the number found, a stale exemption (listed
//! but no longer present) fails, and an in-memory positive control proves the
//! scanner sees what it bans.

use std::path::{Path, PathBuf};

/// Production part of a source: everything before its trailing test module.
fn production_part(source: &str) -> &str {
    source
        .find("\n#[cfg(test)]\nmod ")
        .map_or(source, |at| &source[..at])
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Tokens deleted by S1 (AC4): they must not appear in production code.
const DELETED: &[&str] = &[
    "status_before_flash",
    "queued_notifications",
    "QueuedNotification",
    "notify_or_queue",
    "auto_switch_to_monitor_on_error",
    "apply_warning_notice",
    "store_background_notice",
    "notification_hold_ms",
    "flash_copy",
    "drain_queued_notifications_for_test",
];

/// `StatusState::Flash` constructions allowed by the S1 allowlist:
/// (file suffix, expected count, why). S2 deletes the variant and this list.
const FLASH_ALLOWLIST: &[(&str, usize, &str)] = &[
    (
        "src/infrastructure/runtime/event_loop.rs",
        2,
        "the two sidebar delete-confirmation prompts (single + bulk), moved by S2",
    ),
    (
        "src/domain/models/notice.rs",
        usize::MAX, // definition, `display_text`, `is_active`: counted by presence only
        "the variant definition itself",
    ),
];

fn flash_count(source: &str) -> usize {
    source.matches("StatusState::Flash").count()
}

/// Scan `files` (path, source) and return violation lines.
fn scan(files: &[(String, String)]) -> Vec<String> {
    let mut violations = Vec::new();
    for (path, source) in files {
        let production = production_part(source);
        for token in DELETED {
            if production.contains(token) {
                violations.push(format!("{path}: deleted S1 mechanism `{token}` is back"));
            }
        }
        let flashes = flash_count(production);
        if flashes == 0 {
            continue;
        }
        match FLASH_ALLOWLIST
            .iter()
            .find(|(suffix, _, _)| path.ends_with(suffix))
        {
            None => violations.push(format!(
                "{path}: {flashes} `StatusState::Flash` outside the S1 allowlist"
            )),
            Some((_, expected, _)) if *expected != usize::MAX && *expected != flashes => {
                violations.push(format!(
                    "{path}: allowlist expects {expected} Flash sites, found {flashes}"
                ));
            }
            Some(_) => {}
        }
    }
    violations
}

fn production_sources() -> Vec<(String, String)> {
    let mut paths = Vec::new();
    rust_files(Path::new("src"), &mut paths);
    paths.sort();
    paths
        .into_iter()
        // `*_tests.rs` files are `#[cfg(test)]` modules: not production code.
        .filter(|p| {
            !p.file_name()
                .is_some_and(|n| n.to_string_lossy().ends_with("_tests.rs"))
        })
        .map(|p| {
            let source = std::fs::read_to_string(&p).unwrap();
            (p.to_string_lossy().replace('\\', "/"), source)
        })
        .collect()
}

#[test]
fn k11a_production_code_of_both_hosts_carries_no_deleted_s1_mechanism() {
    let files = production_sources();
    // checked == total: both hosts' roots are in the scan, nothing was skipped.
    for must in [
        "src/infrastructure/runtime/event_loop.rs",
        "src/infrastructure/runtime/attach_loop.rs",
        "src/infrastructure/runtime/attach_keys.rs",
        "src/adapters/tui/state.rs",
        "src/adapters/tui/notifications.rs",
    ] {
        assert_eq!(
            files.iter().filter(|(p, _)| p == must).count(),
            1,
            "{must} must be scanned exactly once"
        );
    }
    let violations = scan(&files);
    assert!(violations.is_empty(), "{violations:#?}");
}

#[test]
fn k11a_no_stale_flash_exemption() {
    let files = production_sources();
    for (suffix, expected, why) in FLASH_ALLOWLIST {
        let (path, source) = files
            .iter()
            .find(|(p, _)| p.ends_with(suffix))
            .unwrap_or_else(|| panic!("allowlisted file {suffix} no longer exists ({why})"));
        let flashes = flash_count(production_part(source));
        assert!(
            flashes > 0,
            "stale exemption: {path} has no `StatusState::Flash` left ({why})"
        );
        if *expected != usize::MAX {
            assert_eq!(flashes, *expected, "{path}: {why}");
        }
    }
}

#[test]
fn k11a_attached_host_has_no_flash_and_no_notice_transcript_rows() {
    let files = production_sources();
    for suffix in ["attach_loop.rs", "attach_keys.rs", "attach_interrupt.rs"] {
        let (path, source) = files.iter().find(|(p, _)| p.ends_with(suffix)).unwrap();
        assert_eq!(
            flash_count(production_part(source)),
            0,
            "{path}: attached flashes are store notices (AC6)"
        );
    }
}

#[test]
fn k11a_positive_control_the_scanner_sees_what_it_bans() {
    let bad = vec![
        (
            "src/x/new_file.rs".to_owned(),
            "fn f(s: &mut S) { s.status = StatusState::Flash { message: m, remaining_ms: 1 }; }"
                .to_owned(),
        ),
        (
            "src/y/old_queue.rs".to_owned(),
            "fn g() { notify_or_queue(state, kind); }".to_owned(),
        ),
        (
            "src/infrastructure/runtime/event_loop.rs".to_owned(),
            "StatusState::Flash StatusState::Flash StatusState::Flash".to_owned(),
        ),
    ];
    let violations = scan(&bad);
    assert_eq!(violations.len(), 3, "{violations:#?}");
    // And the same code inside a trailing test module is not production.
    let in_tests = vec![(
        "src/z.rs".to_owned(),
        "fn ok() {}\n#[cfg(test)]\nmod tests { fn t() { let _ = StatusState::Flash; } }".to_owned(),
    )];
    assert!(scan(&in_tests).is_empty());
}
