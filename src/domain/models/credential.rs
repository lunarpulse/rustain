use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};

use super::secret::SecretString;

// ---------------------------------------------------------------------------
// Credential
// ---------------------------------------------------------------------------

/// A stored authentication credential.
///
/// Forward-compat scaffold: Epic 19 adds an OAuth variant.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Credential {
    ApiKey(SecretString),
}

impl Credential {
    /// Wrap a raw API key string into a [`Credential::ApiKey`].
    pub fn new_api_key(key: String) -> Self {
        Self::ApiKey(SecretString::new(key))
    }

    /// Expose the inner API-key value.
    ///
    /// Use **only** at disk-write and provider-construction boundaries.
    pub fn expose_api_key(&self) -> Option<&str> {
        match self {
            Self::ApiKey(s) => Some(s.expose_secret()),
        }
    }
}
impl std::fmt::Display for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ApiKey(s) => write!(f, "{s}"),
        }
    }
}

impl Serialize for Credential {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::ApiKey(_) => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("type", "api_key")?;
                map.serialize_entry("api_key", "***")?;
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for Credential {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        /// Helper for tag-based deserialization.
        #[derive(Deserialize)]
        #[serde(tag = "type")]
        enum Tagged {
            #[serde(rename = "api_key")]
            ApiKey { api_key: String },
        }

        struct CredentialVisitor;

        impl<'de> Visitor<'de> for CredentialVisitor {
            type Value = Credential;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(
                    r#"a tagged credential object, e.g. {"type":"api_key","api_key":"..."}"#,
                )
            }

            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                let tagged = Tagged::deserialize(de::value::MapAccessDeserializer::new(map))?;
                match tagged {
                    Tagged::ApiKey { api_key } => Ok(Credential::new_api_key(api_key)),
                }
            }
        }

        deserializer.deserialize_map(CredentialVisitor)
    }
}

// ---------------------------------------------------------------------------
// ProviderStatus / AuthStatus / AuthSource
// ---------------------------------------------------------------------------

/// Current authentication state of a provider.
#[derive(Debug, Clone)]
pub struct ProviderStatus {
    pub provider: String,
    pub status: AuthStatus,
    pub source: AuthSource,
    pub last_validated: Option<chrono::DateTime<chrono::Utc>>,
}

/// Whether the credential has been verified.
#[derive(Debug, Clone)]
pub enum AuthStatus {
    Authenticated,
    Invalid,
    Unknown,
}

/// Where the credential was discovered.
#[derive(Debug, Clone)]
pub enum AuthSource {
    Env,
    AuthJson,
    Config,
    None,
}

// ---------------------------------------------------------------------------
// AuthMethod
// ---------------------------------------------------------------------------

/// Supported authentication mechanisms for a provider.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum AuthMethod {
    ApiKey,
    // Epic 19 will add: OAuth { ... }
}

// ---------------------------------------------------------------------------
// ResolvedAuth
// ---------------------------------------------------------------------------

/// A fully-resolved credential ready for use in an API call.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum ResolvedAuth {
    ApiKey(SecretString),
    // Epic 19 will add: OAuth { access_token, ... }
}

impl ResolvedAuth {
    /// Extract the API key as a `SecretString` if this is the `ApiKey` variant.
    pub fn to_api_key(self) -> Option<SecretString> {
        match self {
            Self::ApiKey(s) => Some(s),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // P0-1a: Redaction canary — Credential Debug mask (delegates to SecretString)
    #[test]
    fn credential_debug_never_exposes_secret() {
        let cred = Credential::new_api_key("SECRET-CANARY-DEADBEEF".to_string());
        let debug = format!("{:?}", cred);
        assert!(
            !debug.contains("SECRET-CANARY-DEADBEEF"),
            "Debug leaked the secret: {debug}"
        );
        assert!(
            debug.contains("redacted") || debug.contains("***"),
            "Debug should show masked form: {debug}"
        );
    }

    // P0-1b: Redaction canary — Credential Display mask (delegates to SecretString)
    #[test]
    fn credential_display_never_exposes_secret() {
        let cred = Credential::new_api_key("SECRET-CANARY-DEADBEEF".to_string());
        let display = format!("{}", cred);
        assert!(
            !display.contains("SECRET-CANARY-DEADBEEF"),
            "Display leaked the secret"
        );
        assert!(display.contains("***"), "Display should show masked form");
    }

    // P0-1c: Redaction canary — Credential Serialize mask
    #[test]
    fn credential_serialize_never_exposes_secret() {
        let cred = Credential::new_api_key("SECRET-CANARY-DEADBEEF".to_string());
        let json = serde_json::to_string(&cred).unwrap();
        assert!(
            !json.contains("SECRET-CANARY-DEADBEEF"),
            "Serialize leaked the secret: {json}"
        );
        assert!(
            json.contains("***") || json.contains("redacted"),
            "Serialize should mask: {json}"
        );
        assert!(
            json.contains("api_key"),
            "Serialize should contain type tag: {json}"
        );
    }

    // P0-1d: Positive control — expose_api_key works
    #[test]
    fn credential_expose_api_key_returns_real_value() {
        let cred = Credential::new_api_key("my-real-secret".to_string());
        assert_eq!(cred.expose_api_key(), Some("my-real-secret"));
    }

    // Deserialize round-trip
    #[test]
    fn credential_deserialize_recovers_real_value() {
        let json = r#"{"type": "api_key", "api_key": "sk-real-secret-123"}"#;
        let cred: Credential = serde_json::from_str(json).unwrap();
        assert_eq!(cred.expose_api_key(), Some("sk-real-secret-123"));
    }

    // ResolvedAuth
    #[test]
    fn resolved_auth_api_key_converts() {
        let auth = ResolvedAuth::ApiKey(SecretString::new("key123".into()));
        assert_eq!(auth.to_api_key().unwrap().expose_secret(), "key123");
    }

    // P-1: ResolvedAuth Debug must never leak the key (canary).
    #[test]
    fn resolved_auth_debug_never_exposes_secret() {
        let auth = ResolvedAuth::ApiKey(SecretString::new("SECRET-CANARY-DEADBEEF".into()));
        let debug = format!("{:?}", auth);
        assert!(
            !debug.contains("SECRET-CANARY-DEADBEEF"),
            "ResolvedAuth Debug leaked the secret: {debug}"
        );
        assert!(
            debug.contains("redacted"),
            "ResolvedAuth Debug should show masked form: {debug}"
        );
    }
}
