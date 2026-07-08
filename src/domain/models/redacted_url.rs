//! `RedactedUrl` — a newtype that strips userinfo from `Debug`/`Display`
//! while preserving the full URL for serialization and the connect path.
//!
//! Story 14.0, AC4.  Surgical redaction: `Debug`/`Display` strip only the
//! `user:pass@` userinfo and **keep** host/scheme/path visible (diagnosability).
//! `Serialize` emits the **real** URL (URLs round-trip through user-owned config).

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A URL that redacts userinfo in `Debug`/`Display` but preserves the full
/// value for serialization and the connect path.
#[derive(Clone, PartialEq, Eq)]
pub struct RedactedUrl(String);

impl RedactedUrl {
    /// Wrap a raw URL string.
    pub fn new(url: String) -> Self {
        Self(url)
    }

    /// The full URL including any credentials — for the connect/transport path.
    pub fn expose_url(&self) -> &str {
        self.0.as_str()
    }

    /// Strip userinfo from the URL for safe display.
    fn stripped(&self) -> std::borrow::Cow<'_, str> {
        match url::Url::parse(&self.0) {
            Ok(parsed) => {
                if parsed.username().is_empty() && parsed.password().is_none() {
                    std::borrow::Cow::Borrowed(&self.0)
                } else {
                    let mut sanitized = parsed;
                    let _ = sanitized.set_username("");
                    let _ = sanitized.set_password(None);
                    std::borrow::Cow::Owned(sanitized.to_string())
                }
            }
            Err(_) => std::borrow::Cow::Owned(strip_userinfo_raw(&self.0)),
        }
    }
}
/// Best-effort userinfo strip for URLs that `url::Url::parse` rejects.
///
/// Reached only when parse fails (illegal scheme, control char in the authority,
/// etc.). Drops the `user:pass@` segment from the authority while preserving
/// scheme/host/path — so Debug/Display never emit raw credentials even for
/// unparseable inputs (P-2, code review 2026-06-20).
fn strip_userinfo_raw(raw: &str) -> String {
    let Some(scheme_end) = raw.find("://") else {
        return raw.to_string();
    };
    let after_scheme = scheme_end + 3;
    let rest = &raw[after_scheme..];
    let auth_end = rest.find('/').unwrap_or(rest.len());
    let authority = &rest[..auth_end];
    match authority.rfind('@') {
        Some(at) => {
            let mut out = String::with_capacity(raw.len());
            out.push_str(&raw[..after_scheme]);
            out.push_str(&authority[at + 1..]);
            out.push_str(&rest[auth_end..]);
            out
        }
        None => raw.to_string(),
    }
}

impl std::fmt::Debug for RedactedUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RedactedUrl({})", self.stripped())
    }
}

impl std::fmt::Display for RedactedUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.stripped())
    }
}

impl Serialize for RedactedUrl {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Emit the REAL URL — config files round-trip.
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for RedactedUrl {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(RedactedUrl::new(s))
    }
}

impl From<String> for RedactedUrl {
    fn from(s: String) -> Self {
        Self::new(s)
    }
}

impl From<&str> for RedactedUrl {
    fn from(s: &str) -> Self {
        Self::new(s.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL_WITH_CREDS: &str = "https://alice:hunter2@host.example.com/sse";
    const URL_NO_CREDS: &str = "https://host.example.com/sse";

    #[test]
    fn debug_strips_userinfo() {
        let u = RedactedUrl::new(URL_WITH_CREDS.into());
        let debug = format!("{:?}", u);
        assert!(!debug.contains("alice"), "Debug leaked username: {debug}");
        assert!(!debug.contains("hunter2"), "Debug leaked password: {debug}");
        assert!(
            debug.contains("host.example.com"),
            "Debug should show host: {debug}"
        );
    }

    #[test]
    fn display_strips_userinfo() {
        let u = RedactedUrl::new(URL_WITH_CREDS.into());
        let display = format!("{}", u);
        assert!(
            !display.contains("alice"),
            "Display leaked username: {display}"
        );
        assert!(
            !display.contains("hunter2"),
            "Display leaked password: {display}"
        );
        assert!(
            display.contains("host.example.com"),
            "Display should show host: {display}"
        );
    }
    #[test]
    fn debug_display_strip_userinfo_on_unparseable_url() {
        // `url::Url::parse` rejects the illegal scheme `ht!tp`; stripped() must
        // still drop creds via the raw-string fallback (P-2 regression guard).
        let u = RedactedUrl::new("ht!tp://alice:hunter2@host.example.com/sse".into());
        let debug = format!("{:?}", u);
        let display = format!("{}", u);
        assert!(
            !debug.contains("alice"),
            "Debug leaked username on parse err: {debug}"
        );
        assert!(
            !debug.contains("hunter2"),
            "Debug leaked password on parse err: {debug}"
        );
        assert!(
            !display.contains("alice"),
            "Display leaked username on parse err: {display}"
        );
        assert!(
            !display.contains("hunter2"),
            "Display leaked password on parse err: {display}"
        );
        assert!(
            debug.contains("host.example.com"),
            "Debug should keep host: {debug}"
        );
    }

    #[test]
    fn expose_url_returns_full_url() {
        let u = RedactedUrl::new(URL_WITH_CREDS.into());
        assert_eq!(u.expose_url(), URL_WITH_CREDS);
    }

    #[test]
    fn serialize_emits_real_url() {
        let u = RedactedUrl::new(URL_WITH_CREDS.into());
        let json = serde_json::to_string(&u).unwrap();
        assert!(
            json.contains("alice"),
            "Serialize should emit real URL: {json}"
        );
        assert!(
            json.contains("hunter2"),
            "Serialize should emit real URL: {json}"
        );
    }

    #[test]
    fn deserialize_wraps_raw_string() {
        let u: RedactedUrl = serde_json::from_str(&format!("\"{}\"", URL_WITH_CREDS)).unwrap();
        assert_eq!(u.expose_url(), URL_WITH_CREDS);
    }

    #[test]
    fn no_creds_url_unchanged() {
        let u = RedactedUrl::new(URL_NO_CREDS.into());
        let debug = format!("{:?}", u);
        assert!(debug.contains("host.example.com"));
        let display = format!("{}", u);
        assert_eq!(display, URL_NO_CREDS);
    }

    #[test]
    fn json_round_trip_preserves_full_url() {
        let u = RedactedUrl::new(URL_WITH_CREDS.into());
        let json = serde_json::to_string(&u).unwrap();
        let u2: RedactedUrl = serde_json::from_str(&json).unwrap();
        assert_eq!(u2.expose_url(), URL_WITH_CREDS);
    }

    #[test]
    fn debug_json_do_not_contain_creds() {
        let u = RedactedUrl::new(URL_WITH_CREDS.into());
        let debug = format!("{:?}", u);
        let display = format!("{}", u);
        let debug_json = serde_json::to_string(&debug).unwrap();
        // Debug and Display should never contain creds
        assert!(!debug.contains("alice"));
        assert!(!debug.contains("hunter2"));
        assert!(!display.contains("alice"));
        assert!(!display.contains("hunter2"));
        assert!(!debug_json.contains("alice"));
        assert!(!debug_json.contains("hunter2"));
        // But host IS shown
        assert!(debug.contains("host.example.com"));
        assert!(display.contains("host.example.com"));
    }
}
