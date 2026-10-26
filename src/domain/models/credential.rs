use std::collections::BTreeMap;

use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};

use super::secret::SecretString;

/// A stored authentication credential. Serialization is always masked.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Credential {
    ApiKey(SecretString),
    OAuth(OAuthCredential),
}

#[derive(Debug, Clone, Deserialize)]
pub struct OAuthCredential {
    pub access: SecretString,
    pub refresh: SecretString,
    #[serde(rename = "expires_at")]
    pub expires_at_ms: i64,
    pub identity: CredentialIdentity,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialIdentity {
    pub account_id: Option<String>,
    pub email: Option<String>,
    pub project_id: Option<String>,
}

impl CredentialIdentity {
    /// Stable upsert identity; missing identities never match each other.
    pub fn key(&self) -> Option<String> {
        self.account_id
            .as_ref()
            .map(|id| format!("account:{id}"))
            .or_else(|| self.email.as_ref().map(|email| format!("email:{email}")))
            .or_else(|| self.project_id.as_ref().map(|id| format!("project:{id}")))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialOrigin {
    Login,
    Stored,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CredentialId(pub u64);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredCredential {
    pub id: CredentialId,
    pub rev: u64,
    pub origin: CredentialOrigin,
    #[serde(flatten)]
    pub credential: Credential,
    pub last_validated: Option<chrono::DateTime<chrono::Utc>>,
    pub disabled_cause: Option<String>,
}

impl std::fmt::Display for StoredCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct AuthSnapshot {
    pub providers: BTreeMap<String, Vec<StoredCredential>>,
    pub unknown_rows: usize,
}

impl AuthSnapshot {
    pub fn rows(&self, provider: &str) -> &[StoredCredential] {
        self.providers
            .get(provider)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CasOutcome {
    Applied,
    Stale,
}

impl Credential {
    pub fn new_api_key(key: String) -> Self {
        Self::ApiKey(SecretString::new(key))
    }

    /// Expose only at disk-write and provider-construction boundaries.
    pub fn expose_api_key(&self) -> Option<&str> {
        match self {
            Self::ApiKey(s) => Some(s.expose_secret()),
            Self::OAuth(_) => None,
        }
    }
}

impl std::fmt::Display for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ApiKey(s) => write!(f, "{s}"),
            Self::OAuth(_) => write!(f, "OAuth(***)"),
        }
    }
}

impl Serialize for Credential {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        match self {
            Self::ApiKey(_) => {
                map.serialize_entry("type", "api_key")?;
                map.serialize_entry("api_key", "***")?;
            }
            Self::OAuth(oauth) => {
                map.serialize_entry("type", "oauth")?;
                map.serialize_entry("access", "***")?;
                map.serialize_entry("refresh", "***")?;
                map.serialize_entry("expires_at", &oauth.expires_at_ms)?;
                map.serialize_entry("identity", &oauth.identity)?;
            }
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for Credential {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum Tagged {
            ApiKey {
                api_key: SecretString,
            },
            Oauth {
                access: SecretString,
                refresh: SecretString,
                expires_at: i64,
                identity: CredentialIdentity,
            },
        }
        Ok(match Tagged::deserialize(deserializer)? {
            Tagged::ApiKey { api_key } => Self::ApiKey(api_key),
            Tagged::Oauth {
                access,
                refresh,
                expires_at,
                identity,
            } => Self::OAuth(OAuthCredential {
                access,
                refresh,
                expires_at_ms: expires_at,
                identity,
            }),
        })
    }
}

/// Whether the credential has been verified.
#[derive(Debug, Clone)]
pub enum AuthStatus {
    Authenticated,
    Invalid,
    Unknown,
}

/// Where the winning credential was discovered. Absence is `Option::None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthSource {
    Runtime,
    StoredOAuth,
    LoginKey,
    Env { var: String },
    StoredKey,
}

/// Supported authentication mechanisms for a provider.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum AuthMethod {
    ApiKey,
    // Epic 20 will add: OAuth { ... }
}

/// A fully-resolved credential; refresh tokens never leave the store/resolver.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum ResolvedAuth {
    ApiKey(SecretString),
    OAuth { access: SecretString },
}

impl ResolvedAuth {
    pub fn to_api_key(self) -> Option<SecretString> {
        match self {
            Self::ApiKey(s) => Some(s),
            Self::OAuth { .. } => None,
        }
    }

    pub fn bearer_secret(&self) -> &SecretString {
        match self {
            Self::ApiKey(s) | Self::OAuth { access: s } => s,
        }
    }
}

impl std::fmt::Display for ResolvedAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "***")
    }
}

impl Serialize for ResolvedAuth {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(2))?;
        match self {
            Self::ApiKey(_) => {
                map.serialize_entry("type", "api_key")?;
                map.serialize_entry("api_key", "***")?;
            }
            Self::OAuth { .. } => {
                map.serialize_entry("type", "oauth")?;
                map.serialize_entry("access", "***")?;
            }
        }
        map.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth_model_masks_both_tokens_and_preserves_identity_priority() {
        let identity = CredentialIdentity {
            account_id: Some("account".into()),
            email: Some("mail".into()),
            project_id: Some("project".into()),
        };
        assert_eq!(identity.key().as_deref(), Some("account:account"));
        let credential = Credential::OAuth(OAuthCredential {
            access: "ACCESS-CANARY".into(),
            refresh: "REFRESH-CANARY".into(),
            expires_at_ms: 123,
            identity,
        });
        assert!(credential.expose_api_key().is_none());
        let row = StoredCredential {
            id: CredentialId(1),
            rev: 1,
            origin: CredentialOrigin::Stored,
            credential,
            last_validated: None,
            disabled_cause: None,
        };
        let resolved = ResolvedAuth::OAuth {
            access: "ACCESS-CANARY".into(),
        };
        for output in [
            format!("{row:?} {row}"),
            serde_json::to_string(&row).unwrap(),
            format!("{resolved:?} {resolved}"),
            serde_json::to_string(&resolved).unwrap(),
        ] {
            assert!(!output.contains("ACCESS-CANARY"));
            assert!(!output.contains("REFRESH-CANARY"));
        }
        assert_eq!(resolved.bearer_secret().expose_secret(), "ACCESS-CANARY");
    }

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
