fn main() {
    // Git commit hash (short) — check exit status to avoid empty string
    let hash = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                String::from_utf8(o.stdout).ok()
            } else {
                None
            }
        })
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=GIT_HASH={}", hash.trim());

    // Build date (UTC ISO-8601) — portable, no external `date` command
    let build_date = {
        use std::time::SystemTime;
        match SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
            Ok(dur) => {
                let secs = dur.as_secs();
                // Manual UTC formatting (avoids chrono dependency in build script)
                let days = secs / 86400;
                let time_of_day = secs % 86400;
                let hours = time_of_day / 3600;
                let minutes = (time_of_day % 3600) / 60;
                let seconds = time_of_day % 60;
                // Days since epoch to Y-M-D (simplified civil calendar)
                let (year, month, day) = days_to_date(days);
                format!(
                    "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
                    year, month, day, hours, minutes, seconds
                )
            }
            Err(_) => "unknown".to_string(),
        }
    };
    println!("cargo:rustc-env=BUILD_DATE={}", build_date);

    // Target triple (set by Cargo during cross-compilation)
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=TARGET={}", target);

    // Re-run if HEAD changes (branch checkout)
    println!("cargo:rerun-if-changed=.git/HEAD");
    // Also track the ref file that HEAD points to (new commits on current branch)
    if let Ok(head_contents) = std::fs::read_to_string(".git/HEAD")
        && let Some(ref_path) = head_contents.trim().strip_prefix("ref: ")
    {
        println!("cargo:rerun-if-changed=.git/{}", ref_path);
    }
}

/// Convert days since Unix epoch to (year, month, day).
fn days_to_date(days: u64) -> (u64, u64, u64) {
    // Algorithm from Howard Hinnant's chrono-compatible date library
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}
