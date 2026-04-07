/// Returns a rich multi-line version string with build metadata.
/// Sourced from compile-time environment variables set by `build.rs`.
// Covers: FR109
pub fn version_string() -> String {
    let version = env!("CARGO_PKG_VERSION");
    let target = option_env!("TARGET").unwrap_or("unknown");
    let build_date = option_env!("BUILD_DATE").unwrap_or("unknown");
    let git_hash = option_env!("GIT_HASH").unwrap_or("unknown");
    let rust_version = env!("CARGO_PKG_RUST_VERSION");

    format!(
        "rustain {version} ({target})\nbuild: {build_date}\ncommit: {git_hash}\nrust: {rust_version}\nprotocols: agent-skills n/a, mcp n/a, a2a n/a"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version_string_non_empty() {
        let v = version_string();
        assert!(!v.is_empty());
    }

    #[test]
    fn test_version_string_contains_version() {
        let v = version_string();
        assert!(
            v.contains(env!("CARGO_PKG_VERSION")),
            "version_string should contain CARGO_PKG_VERSION"
        );
    }

    #[test]
    fn test_version_string_contains_rustain() {
        let v = version_string();
        assert!(v.starts_with("rustain "), "should start with 'rustain '");
    }
}
