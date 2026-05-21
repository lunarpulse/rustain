//! Profile install infrastructure — URL parser + sidecar I/O (NO network calls).
//! Story 8.6b AC-9.

use std::path::Path;

use once_cell::sync::Lazy;
use regex::Regex;

static GH_USER_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[a-zA-Z0-9][a-zA-Z0-9-]{0,38}$").expect("GH_USER_RE compile"));
static GH_REPO_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[a-zA-Z0-9][a-zA-Z0-9-_.]{0,99}$").expect("GH_REPO_RE compile"));

/// Parsed `gh:` spec. Public-repo-only in v1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedGhSpec {
    pub user: String,
    pub repo: String,
    pub path: Option<String>,
    pub reference: Option<String>, // None in v1; reserved for future @branch / @tag / @sha
}

/// Return the raw.githubusercontent.com base URL, respecting RUSTAIN_GH_RAW_BASE env var.
pub fn raw_base_url() -> String {
    std::env::var("RUSTAIN_GH_RAW_BASE") // CONFORMANCE_EXCEPTION: infra utility for GitHub raw URL override
        .unwrap_or_else(|_| "https://raw.githubusercontent.com".to_string())
}

/// Parse a `gh:` spec like `gh:owner/repo` or `gh:owner/repo/path/to.toml`.
pub fn parse_gh_spec(spec: &str) -> Result<ParsedGhSpec, ProfileInstallError> {
    if !spec.starts_with("gh:") {
        return Err(ProfileInstallError::UnsupportedScheme(spec.to_string()));
    }

    let body = &spec[3..]; // strip "gh:"
    if body.is_empty() {
        return Err(ProfileInstallError::UnsupportedScheme(spec.to_string()));
    }

    // Split on '/' but reject '..' / '\' anywhere in the path
    if body.contains("..") || body.contains('\\') {
        return Err(ProfileInstallError::PathTraversal(spec.to_string()));
    }

    let segments: Vec<&str> = body.split('/').collect();

    // Must have at least user/repo
    if segments.len() < 2 {
        return Err(ProfileInstallError::Incomplete(spec.to_string()));
    }

    let user = segments[0].to_string();
    let repo = segments[1].to_string();

    if !GH_USER_RE.is_match(&user) {
        return Err(ProfileInstallError::InvalidSegment {
            spec: spec.to_string(),
            segment: format!("user '{}'", user),
            regex: "^[a-zA-Z0-9][a-zA-Z0-9-]{0,38}$",
        });
    }
    if !GH_REPO_RE.is_match(&repo) {
        return Err(ProfileInstallError::InvalidSegment {
            spec: spec.to_string(),
            segment: format!("repo '{}'", repo),
            regex: "^[a-zA-Z0-9][a-zA-Z0-9-_.]{0,99}$",
        });
    }

    let path = if segments.len() > 2 {
        let joined = segments[2..].join("/");
        // Re-check path segments for traversal
        if joined.contains("..") || joined.contains('\\') {
            return Err(ProfileInstallError::PathTraversal(spec.to_string()));
        }
        Some(joined)
    } else {
        None
    };

    Ok(ParsedGhSpec {
        user,
        repo,
        path,
        reference: None, // v1 does not support @ref
    })
}

/// Write the `.toml.source` sidecar with the spec verbatim. Atomic via tempfile + rename.
pub fn write_source_sidecar(dest_toml: &Path, spec: &str) -> Result<(), std::io::Error> {
    let tmp = dest_toml.with_extension("toml.source.tmp");
    std::fs::write(&tmp, spec)?;
    std::fs::rename(&tmp, dest_toml.with_extension("toml.source"))?;
    Ok(())
}

/// Read a `{toml_path}.toml.source` sidecar. 1024-byte cap. Returns None if missing/empty/oversized.
pub fn read_source_sidecar(toml_path: &Path) -> Option<String> {
    let sidecar_path = toml_path.with_extension("toml.source");
    let file = match std::fs::File::open(&sidecar_path) {
        Ok(f) => f,
        Err(_) => {
            tracing::warn!(
                target: "profile_install",
                "missing source sidecar for community profile: {:?}",
                sidecar_path
            );
            return None;
        }
    };
    // Reject oversized sidecars (> 1024 bytes)
    let metadata = match file.metadata() {
        Ok(m) => m,
        Err(_) => return None,
    };
    if metadata.len() > 1024 {
        tracing::warn!(
            target: "profile_install",
            "oversized source sidecar for community profile ({size} bytes): {:?}",
            sidecar_path,
            size = metadata.len()
        );
        return None;
    }
    use std::io::Read;
    let mut buf = String::new();
    let mut reader = std::io::BufReader::new(file);
    if reader.read_to_string(&mut buf).is_err() {
        return None;
    }
    let trimmed = buf.trim().to_string();
    if trimmed.is_empty() {
        tracing::warn!(
            target: "profile_install",
            "empty source sidecar for community profile: {:?}",
            sidecar_path
        );
        None
    } else {
        Some(trimmed)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProfileInstallError {
    #[error("only 'gh:' scheme supported in v1 (public GitHub repos)")]
    UnsupportedScheme(String),
    #[error("invalid '{segment}' in '{spec}': must match {regex}")]
    InvalidSegment {
        spec: String,
        segment: String,
        regex: &'static str,
    },
    #[error("path-traversal not allowed in '{0}'")]
    PathTraversal(String),
    #[error("missing user or repo in '{0}'")]
    Incomplete(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_gh_spec_simple_owner_repo() {
        let spec = "gh:owner/repo";
        let parsed = parse_gh_spec(spec).unwrap();
        assert_eq!(parsed.user, "owner");
        assert_eq!(parsed.repo, "repo");
        assert_eq!(parsed.path, None);
        assert_eq!(parsed.reference, None);
    }

    #[test]
    fn parse_gh_spec_with_path_suffix() {
        let spec = "gh:owner/repo/profiles/coding.toml";
        let parsed = parse_gh_spec(spec).unwrap();
        assert_eq!(parsed.user, "owner");
        assert_eq!(parsed.repo, "repo");
        assert_eq!(parsed.path.as_deref(), Some("profiles/coding.toml"));
    }

    #[test]
    fn parse_gh_spec_rejects_https_scheme() {
        let spec = "https://github.com/owner/repo";
        let err = parse_gh_spec(spec).unwrap_err();
        assert!(matches!(err, ProfileInstallError::UnsupportedScheme(_)));
    }

    #[test]
    fn parse_gh_spec_rejects_path_traversal_dotdot() {
        let spec = "gh:owner/repo/../etc/passwd";
        let err = parse_gh_spec(spec).unwrap_err();
        assert!(matches!(err, ProfileInstallError::PathTraversal(_)));
    }

    #[test]
    fn parse_gh_spec_rejects_invalid_segment_chars() {
        let spec = "gh:u@ser/repo";
        let err = parse_gh_spec(spec).unwrap_err();
        assert!(matches!(err, ProfileInstallError::InvalidSegment { .. }));
    }

    #[test]
    fn parse_gh_spec_rejects_incomplete_missing_repo() {
        let spec = "gh:user";
        let err = parse_gh_spec(spec).unwrap_err();
        assert!(matches!(err, ProfileInstallError::Incomplete(_)));
    }

    #[test]
    fn parse_gh_spec_rejects_empty_string() {
        let spec = "";
        let err = parse_gh_spec(spec).unwrap_err();
        assert!(matches!(err, ProfileInstallError::UnsupportedScheme(_)));
    }

    #[test]
    fn write_source_sidecar_round_trips_via_read() {
        let dir = tempfile::tempdir().unwrap();
        let toml_path = dir.path().join("foo.toml");
        std::fs::write(&toml_path, "[test]\n").unwrap();
        write_source_sidecar(&toml_path, "gh:owner/foo").unwrap();
        let origin = read_source_sidecar(&toml_path);
        assert_eq!(origin.as_deref(), Some("gh:owner/foo"));
    }

    #[test]
    fn read_source_sidecar_handles_oversize_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let toml_path = dir.path().join("big.toml");
        std::fs::write(&toml_path, "[test]\n").unwrap();
        let big_content = "x".repeat(2048);
        std::fs::write(toml_path.with_extension("toml.source"), &big_content).unwrap();
        let origin = read_source_sidecar(&toml_path);
        // Oversized sidecars (>1024 bytes) return None per AC-9 spec
        assert_eq!(origin, None);
    }

    #[test]
    fn read_source_sidecar_missing_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let toml_path = dir.path().join("nope.toml");
        let origin = read_source_sidecar(&toml_path);
        assert_eq!(origin, None);
    }
}
