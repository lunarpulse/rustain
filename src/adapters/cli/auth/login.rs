//! `auth login <provider>` handler (Story 13.4a).
//!
//! Interactive masked key entry → validate via connectivity_probe → store via
//! `AuthStorePort`.  The handler NEVER touches the file directly — all
//! persistence goes through the port.

use std::sync::Arc;

use anyhow::Result;

use crate::adapters::cli::auth::providers;
use crate::domain::errors::ProviderError;
use crate::domain::models::ProviderConfig;
use crate::domain::models::credential::Credential;
use crate::domain::ports::{AuthStorePort, StreamingProvider};
use crate::infrastructure::provider_factory;
/// Provider metadata needed by the login flow.
///
/// Separated from the static [`providers::ProviderMeta`] so `auth login` can
/// handle providers that are configured in `config.toml` but not in the
/// built-in static table (e.g. an `openai-compatible` provider like `zai`).
#[derive(Debug, Clone)]
struct LoginProviderMeta {
    id: String,
    display_name: String,
    signup_url: String,
    requires_key: bool,
    api_key_env: String,
}

impl LoginProviderMeta {
    fn from_static(meta: &'static providers::ProviderMeta) -> Self {
        Self {
            id: meta.id.to_string(),
            display_name: meta.display_name.to_string(),
            signup_url: meta.signup_url.to_string(),
            requires_key: meta.requires_key,
            api_key_env: meta.api_key_env.to_string(),
        }
    }
}

/// Run the `auth login` flow.
///
/// # Errors
///
/// Returns `Err` for I/O failures, validation failures, and unknown providers.
/// The error message is already printed to stderr — the caller maps it to
/// `SubcommandExit` so `main.rs` does not double-print.
pub async fn run_auth_login(
    provider_id: String,
    json: bool,
    store: &Arc<dyn AuthStorePort>,
    app_config: &crate::domain::models::AppConfig,
) -> Result<()> {
    // 1. Resolve provider from the static table or config.toml.
    let (meta, validation_cfg) = resolve_provider(&provider_id, app_config)?;

    // AC8 — keyless provider (e.g. ollama): no key required.
    if !meta.requires_key {
        let msg = format!(
            "{} does not require an API key — no credentials to store.",
            meta.display_name
        );
        if json {
            let j = serde_json::json!({
                "provider": meta.id,
                "status": "keyless",
                "message": msg,
            });
            println!("{}", serde_json::to_string_pretty(&j)?);
        } else {
            println!("{msg}");
        }
        return Ok(());
    }

    // AC1/P-4 — refuse non-TTY BEFORE any interactive read (overwrite confirm + key
    // entry). Previously the overwrite-confirm read stdin before this guard ran.
    if !atty_stdin() {
        eprintln!(
            "Error: auth login requires an interactive terminal (stdin is not a TTY).\n\
             Set {} instead, or pipe through a TTY.",
            meta.api_key_env
        );
        anyhow::bail!("auth login requires a TTY");
    }

    // AC5 — overwrite warning: if a credential already exists, confirm.
    let existing = store.get(&provider_id).await?;
    let proceed = if existing.is_some() {
        eprintln!(
            "⚠  A credential for {} already exists. Overwrite?",
            meta.display_name
        );
        // Reuse yes/no prompting pattern from profile/prompt.rs.
        print!("[y/n] ");
        std::io::Write::flush(&mut std::io::stdout())?;
        let mut input = String::new();
        std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut input)?;
        input.trim().starts_with(['y', 'Y'])
    } else {
        true
    };

    // Read the key + validate ONLY when proceeding (skip the prompt on decline).
    // `execute_login` owns every outcome → store → output-string decision so the
    // validate-before-store / signup-URL / offline / redaction / decline gates are
    // unit-testable without a TTY or network (P-10..P-14).
    let (key, outcome) = if proceed {
        // AC1 — masked interactive key entry.
        let key = rpassword::prompt_password(format!("Enter {} API key: ", meta.display_name))
            .map_err(|e| anyhow::anyhow!("failed to read API key: {e}"))?;
        if key.trim().is_empty() {
            anyhow::bail!("API key cannot be empty.");
        }
        let key = key.trim().to_owned();

        // AC2 — validate before store.
        eprintln!("Validating key against {}…", meta.display_name);
        let outcome = validate_key(&provider_id, &validation_cfg, &key).await;
        (key, outcome)
    } else {
        (String::new(), Ok(())) // unused on the decline path
    };

    match execute_login(&meta, store, json, proceed, key, outcome).await {
        LoginOutcome::Stored { output, .. } => {
            println!("{output}");
            Ok(())
        }
        LoginOutcome::Rejected { message } => {
            eprintln!("{message}");
            anyhow::bail!("login rejected")
        }
        LoginOutcome::Declined { message } => {
            eprintln!("{message}");
            Ok(())
        }
    }
}
/// Resolve a provider for login.
///
/// First checks the static built-in table. If the id is not built-in, falls back
/// to any `[provider.<id>]` section in `config.toml`. This keeps `auth login` in
/// sync with `rustain doctor`, which probes every configured provider.
fn resolve_provider(
    provider_id: &str,
    app_config: &crate::domain::models::AppConfig,
) -> Result<(LoginProviderMeta, ProviderConfig)> {
    if let Some(meta) = providers::lookup(provider_id) {
        let cfg = ProviderConfig {
            provider_id: provider_id.to_owned(),
            model_id: String::new(),
            api_key_env: meta.api_key_env.to_owned(),
            enabled: true,
            kind: Some(provider_id.to_owned()),
            base_url: None,
            context_window: None,
            supports_tools: None,
            discover_models: false,
            model_filter: vec![],
            cache_ttl_seconds: 0,
        };
        return Ok((LoginProviderMeta::from_static(meta), cfg));
    }

    if let Some(cfg) = app_config.provider.get(provider_id) {
        let meta = LoginProviderMeta {
            id: provider_id.to_owned(),
            display_name: provider_id.to_owned(),
            signup_url: String::new(),
            requires_key: true,
            api_key_env: cfg.api_key_env.clone(),
        };
        return Ok((meta, cfg.clone()));
    }

    let mut valid: Vec<String> = providers::all_provider_ids()
        .map(|s| s.to_string())
        .collect();
    for id in app_config.provider.keys() {
        if !valid.contains(id) {
            valid.push(id.clone());
        }
    }
    eprintln!(
        "Error: unknown provider '{provider_id}'.\n\nValid providers: {}",
        valid.join(", ")
    );
    anyhow::bail!("unknown provider");
}

// ---------------------------------------------------------------------------
// Decision core (testable without a TTY / network / rpassword — P-10..P-14)
// ---------------------------------------------------------------------------

/// Outcome of the login decision core. Owns all store + output-string logic so the
/// validate/signup-URL/offline/redaction/decline gates are asserted against a mock
/// store, independent of the interactive I/O in [`run_auth_login`].
enum LoginOutcome {
    /// Credential stored. `output` is the success line (json or human) — never the key.
    Stored { validated: bool, output: String },
    /// Validation rejected the key (invalid / offline / error). `message` carries the
    /// actionable text — including the provider's signup URL for an invalid key (P-12).
    Rejected { message: String },
    /// User declined to overwrite an existing credential — nothing was stored (P-14).
    Declined { message: String },
}

/// Pure decision core: given the validation outcome (and whether the user chose to
/// proceed past the overwrite prompt), store the credential or produce a rejection.
///
/// `proceed == false` short-circuits to [`LoginOutcome::Declined`] WITHOUT touching
/// the store (`key` / `outcome` are ignored on that path). This is the single place
/// that calls `AuthStorePort::set_validated`, which is what lets the
/// validate-before-store / signup-URL / offline / redaction / decline gates assert
/// store behaviour against a mock.
async fn execute_login(
    meta: &LoginProviderMeta,
    store: &Arc<dyn AuthStorePort>,
    json: bool,
    proceed: bool,
    key: String,
    outcome: Result<(), ValidationOutcome>,
) -> LoginOutcome {
    if !proceed {
        return LoginOutcome::Declined {
            message: "Aborted — existing credential unchanged.".to_string(),
        };
    }

    let validated = match outcome {
        Ok(()) => true,
        Err(ValidationOutcome::Inconclusive) => {
            // Q1: endpoint unsupported → store unvalidated (last_validated = None, P-3).
            false
        }
        Err(ValidationOutcome::Invalid) => {
            return LoginOutcome::Rejected {
                message: format!(
                    "✗ Invalid API key for {}.\n  Get one at: {}",
                    meta.display_name, meta.signup_url
                ),
            };
        }
        Err(ValidationOutcome::Offline(detail)) => {
            return LoginOutcome::Rejected {
                message: format!("✗ Cannot validate — offline: {detail}\n  Key was NOT stored."),
            };
        }
        Err(ValidationOutcome::Error(e)) => {
            return LoginOutcome::Rejected {
                message: format!("✗ Validation error: {e}"),
            };
        }
    };

    // AC3 — store via port (P-3: record the validation outcome for last_validated).
    let cred = Credential::new_api_key(key);
    match store.set_validated(&meta.id, cred, validated).await {
        Ok(()) => {
            // AC6 — success output never echoes the key.
            let output = if json {
                serde_json::to_string_pretty(&serde_json::json!({
                    "provider": meta.id,
                    "status": "authenticated",
                    "validated": validated,
                }))
                .unwrap_or_default()
            } else if validated {
                format!("✓ {} credentials stored successfully.", meta.display_name)
            } else {
                format!(
                    "⚠  Could not confirm key via /models endpoint — {} credentials stored unvalidated.",
                    meta.display_name
                )
            };
            LoginOutcome::Stored { validated, output }
        }
        Err(e) => LoginOutcome::Rejected {
            message: format!("✗ Failed to store credential: {e}"),
        },
    }
}

// ---------------------------------------------------------------------------
// Validation helpers
// ---------------------------------------------------------------------------

enum ValidationOutcome {
    Invalid,
    Offline(String),
    /// Endpoint unsupported (404/405) — key might be fine.
    Inconclusive,
    Error(String),
}

/// Build a temporary provider and call `connectivity_probe` with the candidate key.
///
/// Uses [`provider_factory::build_provider_for_config_with_key`] so the candidate
/// key is passed straight to the adapter — never injected into the process
/// environment (DN-1: the old `std::env::set_var` path leaked the key via
/// `/proc/self/environ` and was unsafe under the multi-threaded tokio runtime).
async fn validate_key(
    provider_id: &str,
    cfg: &ProviderConfig,
    key: &str,
) -> Result<(), ValidationOutcome> {
    let provider: Arc<dyn StreamingProvider> =
        match provider_factory::build_provider_for_config_with_key(provider_id, cfg, key) {
            Ok(p) => p,
            Err(ProviderError::Other(msg)) if msg.contains("feature not enabled") => {
                // Provider feature not compiled in — can't validate, treat as inconclusive.
                return Err(ValidationOutcome::Inconclusive);
            }
            Err(e) => {
                return Err(ValidationOutcome::Error(format!(
                    "provider build failed: {e}"
                )));
            }
        };

    match provider.connectivity_probe().await {
        Ok(_outcome) => Ok(()),
        Err(ProviderError::AuthenticationFailed) => Err(ValidationOutcome::Invalid),
        Err(ProviderError::Offline(detail)) => Err(ValidationOutcome::Offline(detail)),
        Err(ProviderError::EndpointUnsupported(_)) => Err(ValidationOutcome::Inconclusive),
        Err(e) => Err(ValidationOutcome::Error(e.to_string())),
    }
}

/// Check if stdin is a TTY.
fn atty_stdin() -> bool {
    // Cross-platform via std (Rust 1.70+). The old hand-rolled `libc::isatty`
    // returned `true` on non-unix, letting Windows piped stdin through (P-6).
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::errors::AuthError;
    use crate::domain::models::credential::ProviderStatus;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// Shared observation handles for [`CountingStore`] — live outside the erased
    /// trait object so tests can read them after driving `execute_login`.
    #[derive(Default)]
    struct Counts {
        set_calls: AtomicUsize,
        last_validated: AtomicBool,
    }

    /// `AuthStorePort` double that counts `set`/`set_validated` calls and records
    /// the last `validated` flag — the seam the P-10..P-14 tests assert against.
    struct CountingStore {
        counts: Arc<Counts>,
    }
    impl CountingStore {
        /// Returns the observation handle + the store as a trait object.
        fn double() -> (Arc<Counts>, Arc<dyn AuthStorePort>) {
            let counts = Arc::new(Counts::default());
            let store: Arc<dyn AuthStorePort> = Arc::new(CountingStore {
                counts: Arc::clone(&counts),
            });
            (counts, store)
        }
    }
    #[async_trait]
    impl AuthStorePort for CountingStore {
        async fn get(&self, _provider: &str) -> Result<Option<Credential>, AuthError> {
            Ok(None)
        }
        async fn set(&self, _provider: &str, _cred: Credential) -> Result<(), AuthError> {
            self.counts.set_calls.fetch_add(1, Ordering::SeqCst);
            self.counts.last_validated.store(true, Ordering::SeqCst);
            Ok(())
        }
        async fn set_validated(
            &self,
            _provider: &str,
            _cred: Credential,
            validated: bool,
        ) -> Result<(), AuthError> {
            self.counts.set_calls.fetch_add(1, Ordering::SeqCst);
            self.counts
                .last_validated
                .store(validated, Ordering::SeqCst);
            Ok(())
        }
        async fn remove(&self, _provider: &str) -> Result<(), AuthError> {
            Ok(())
        }
        async fn list(&self) -> Result<Vec<ProviderStatus>, AuthError> {
            Ok(vec![])
        }
    }

    fn anthropic_meta() -> LoginProviderMeta {
        LoginProviderMeta::from_static(
            providers::lookup("anthropic").expect("anthropic is in the provider table"),
        )
    }

    // P-10: validate-before-store — a valid key is stored; an invalid key is NOT.
    #[tokio::test]
    async fn p10_valid_key_stored_invalid_key_not() {
        let (store, port) = CountingStore::double();
        let meta = anthropic_meta();

        // Valid (Ok) → stored, validated=true.
        let out = execute_login(&meta, &port, false, true, "sk-good".to_string(), Ok(())).await;
        assert!(
            matches!(
                out,
                LoginOutcome::Stored {
                    validated: true,
                    ..
                }
            ),
            "valid key should be stored"
        );
        assert_eq!(store.set_calls.load(Ordering::SeqCst), 1);
        assert!(
            store.last_validated.load(Ordering::SeqCst),
            "validated must be recorded as true"
        );

        // Invalid → Rejected, store untouched (still 1 call from the valid case).
        let out = execute_login(
            &meta,
            &port,
            false,
            true,
            "sk-bad".to_string(),
            Err(ValidationOutcome::Invalid),
        )
        .await;
        assert!(
            matches!(out, LoginOutcome::Rejected { .. }),
            "invalid → Rejected"
        );
        assert_eq!(
            store.set_calls.load(Ordering::SeqCst),
            1,
            "an invalid key must NEVER be written"
        );
    }

    // P-12: invalid-key rejection includes the provider's signup URL.
    #[tokio::test]
    async fn p12_invalid_rejection_includes_signup_url() {
        let (_counts, port) = CountingStore::double();
        let meta = anthropic_meta();
        let out = execute_login(
            &meta,
            &port,
            false,
            true,
            "sk-bad".to_string(),
            Err(ValidationOutcome::Invalid),
        )
        .await;
        match out {
            LoginOutcome::Rejected { message } => {
                assert!(
                    message.contains(&meta.signup_url),
                    "rejection must include the signup URL; got: {message}"
                );
            }
            _ => panic!("expected Rejected for an invalid key"),
        }
    }

    // P-13: offline during validation → nothing stored.
    #[tokio::test]
    async fn p13_offline_stores_nothing() {
        let (store, port) = CountingStore::double();
        let meta = anthropic_meta();
        let out = execute_login(
            &meta,
            &port,
            false,
            true,
            "sk-x".to_string(),
            Err(ValidationOutcome::Offline("no route to host".to_string())),
        )
        .await;
        assert!(
            matches!(out, LoginOutcome::Rejected { .. }),
            "offline → Rejected"
        );
        assert_eq!(
            store.set_calls.load(Ordering::SeqCst),
            0,
            "offline must NOT store"
        );
    }

    // P-3: inconclusive validation → stored with validated=false (last_validated null).
    #[tokio::test]
    async fn p3_inconclusive_stored_unvalidated() {
        let (store, port) = CountingStore::double();
        let meta = anthropic_meta();
        let out = execute_login(
            &meta,
            &port,
            false,
            true,
            "sk-maybe".to_string(),
            Err(ValidationOutcome::Inconclusive),
        )
        .await;
        match out {
            LoginOutcome::Stored { validated, output } => {
                assert!(!validated, "inconclusive must record validated=false");
                assert!(
                    output.contains("unvalidated"),
                    "human output should warn the key is unvalidated: {output}"
                );
            }
            _ => panic!("inconclusive → Stored"),
        }
        assert!(
            !store.last_validated.load(Ordering::SeqCst),
            "validated must be recorded as false"
        );
    }

    // P-14: overwrite decline → nothing stored; accept → stored (replace).
    #[tokio::test]
    async fn p14_decline_stores_nothing_accept_stores() {
        let (store, port) = CountingStore::double();
        let meta = anthropic_meta();

        // Decline (proceed=false) → Declined, store untouched.
        let out = execute_login(&meta, &port, false, false, String::new(), Ok(())).await;
        assert!(
            matches!(out, LoginOutcome::Declined { .. }),
            "decline → Declined"
        );
        assert_eq!(
            store.set_calls.load(Ordering::SeqCst),
            0,
            "decline must NOT store"
        );

        // Accept (proceed=true) → stored.
        let out = execute_login(&meta, &port, false, true, "sk-new".to_string(), Ok(())).await;
        assert!(
            matches!(out, LoginOutcome::Stored { .. }),
            "accept → Stored"
        );
        assert_eq!(store.set_calls.load(Ordering::SeqCst), 1);
    }

    // P-11: success output (human + json) never echoes the key (redaction canary).
    #[tokio::test]
    async fn p11_success_output_never_echoes_key() {
        let (_counts, port) = CountingStore::double();
        let meta = anthropic_meta();
        const SENTINEL: &str = "SECRET-CANARY-DEADBEEF";

        match execute_login(&meta, &port, false, true, SENTINEL.to_string(), Ok(())).await {
            LoginOutcome::Stored { output, .. } => assert!(
                !output.contains(SENTINEL),
                "human success output leaked the key: {output}"
            ),
            _ => panic!("expected Stored"),
        }
        match execute_login(&meta, &port, true, true, SENTINEL.to_string(), Ok(())).await {
            LoginOutcome::Stored { output, .. } => {
                assert!(
                    !output.contains(SENTINEL),
                    "json output leaked the key: {output}"
                );
                assert!(
                    output.contains("validated"),
                    "json output should carry the validated field: {output}"
                );
            }
            _ => panic!("expected Stored"),
        }
    }
    // -----------------------------------------------------------------------
    // Provider resolution
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_provider_static_table() {
        let cfg = crate::domain::models::AppConfig::default();
        let (meta, validation_cfg) = resolve_provider("anthropic", &cfg).unwrap();
        assert_eq!(meta.id, "anthropic");
        assert_eq!(validation_cfg.kind, Some("anthropic".to_string()));
    }

    #[test]
    fn resolve_provider_config_fallback() {
        let mut cfg = crate::domain::models::AppConfig::default();
        cfg.provider.insert(
            "zai".to_string(),
            crate::domain::models::ProviderConfig {
                provider_id: "zai".to_string(),
                model_id: "zai-model".to_string(),
                api_key_env: "ZAI_API_KEY".to_string(),
                enabled: true,
                kind: Some("openai-compatible".to_string()),
                base_url: Some("https://api.z.ai/v1".to_string().into()),
                context_window: None,
                supports_tools: None,
                discover_models: false,
                model_filter: vec![],
                cache_ttl_seconds: 3600,
            },
        );
        let (meta, validation_cfg) = resolve_provider("zai", &cfg).unwrap();
        assert_eq!(meta.id, "zai");
        assert_eq!(meta.api_key_env, "ZAI_API_KEY");
        assert!(meta.requires_key);
        assert_eq!(validation_cfg.kind, Some("openai-compatible".to_string()));
        assert_eq!(
            validation_cfg.base_url.as_ref().map(|u| u.expose_url()),
            Some("https://api.z.ai/v1")
        );
    }

    #[test]
    fn resolve_provider_unknown_errors() {
        let cfg = crate::domain::models::AppConfig::default();
        assert!(resolve_provider("not-a-provider", &cfg).is_err());
    }
}
