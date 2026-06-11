use std::fs;
use std::path::Path;

fn walk_rs_files(dir: &Path, files: &mut Vec<std::path::PathBuf>) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.is_dir() {
                walk_rs_files(&path, files);
            } else if path.extension().map_or(false, |ext| ext == "rs") {
                files.push(path);
            }
        }
    }
}

#[test]
fn test_no_std_sync_in_subagent_async_modules() {
    let dirs = ["src/adapters/subagent", "src/infrastructure/subagent"];

    let forbidden = [
        "std::sync::RwLock",
        "std::sync::Mutex",
        "parking_lot::RwLock",
        "parking_lot::Mutex",
    ];

    let bare_types = ["RwLock", "Mutex"];

    for dir in &dirs {
        if !Path::new(dir).exists() {
            continue;
        }
        let mut files = Vec::new();
        walk_rs_files(Path::new(dir), &mut files);
        for path in files {
            let content = fs::read_to_string(&path).unwrap();
            let mut use_imports = std::collections::HashSet::new();
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("use ") {
                    for bad in &forbidden {
                        if trimmed.contains(bad) {
                            use_imports.insert(bad.to_string());
                        }
                    }
                }
            }
            for (line_no, line) in content.lines().enumerate() {
                let trimmed = line.trim();
                if trimmed.starts_with("// ")
                    || trimmed.starts_with("///")
                    || trimmed.starts_with("//!")
                {
                    continue;
                }
                if trimmed.contains("CONFORMANCE_EXCEPTION_STD_SYNC_LOCK") {
                    continue;
                }
                for bad in &forbidden {
                    if trimmed.contains(bad) {
                        panic!(
                            "Forbidden sync lock '{}' found at {}:{}\n  {}",
                            bad,
                            path.display(),
                            line_no + 1,
                            line
                        );
                    }
                }
                if !trimmed.starts_with("use ") {
                    for bare in &bare_types {
                        if use_imports.contains(&bare.to_string())
                            && (trimmed.contains(&format!("{}::", bare))
                                || trimmed.contains(&format!("{}(", bare))
                                || trimmed.contains(&format!("{}::new", bare)))
                        {
                            panic!(
                                "Bare '{}' used after 'use' import of forbidden sync type at {}:{}\n  {}",
                                bare,
                                path.display(),
                                line_no + 1,
                                line
                            );
                        }
                    }
                }
            }
        }
    }
}

/// AC-10-2-8: Background subprocess tier tokens must NOT appear in foreground
/// subagent modules. This grep ratchets against Story 10.9 code leaking into
/// the in-process tier.
#[test]
fn test_no_background_tier_tokens_in_foreground_subagent_modules() {
    let dirs = ["src/adapters/subagent", "src/infrastructure/subagent"];
    let forbidden = [
        "UnixListener",
        "UnixStream",
        "sun_path",
        "jsonrpc",
        ".sock",
        "pid_file",
        "sysv_signal",
        "named_pipe",
    ];

    for dir in &dirs {
        if !Path::new(dir).exists() {
            continue;
        }
        let mut files = Vec::new();
        walk_rs_files(Path::new(dir), &mut files);
        for path in files {
            let content = fs::read_to_string(&path).unwrap();
            for (line_no, line) in content.lines().enumerate() {
                let trimmed = line.trim();
                if trimmed.starts_with("// ")
                    || trimmed.starts_with("///")
                    || trimmed.starts_with("//!")
                {
                    continue;
                }
                for token in &forbidden {
                    if trimmed.contains(token) {
                        panic!(
                            "Forbidden background-tier token '{}' found at {}:{}\n  {}. \
                             Unix socket / JSON-RPC / PID file code belongs in Story 10.9, not the foreground tier.",
                            token,
                            path.display(),
                            line_no + 1,
                            line
                        );
                    }
                }
            }
        }
    }
}
