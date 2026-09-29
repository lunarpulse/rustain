//! Cached signed AgentCard.
//!
//! Story 18.1b, AC7b (R2). `GET /.well-known/agent-card.json` is the only
//! surface a non-loopback deployment exposes **before** authentication, and
//! building its answer costs a registry snapshot, a skill projection, a sort,
//! a JCS canonicalization, a base64url encode, an Ed25519 signature and a
//! second canonicalization. Unauthenticated GETs are exactly the requests that
//! must stay cheap, so the signed bytes are built once per catalogue generation
//! and handed out afterwards.
//!
//! Two properties this type is responsible for:
//!
//! * **Exactly one signature per generation**, even under N concurrent misses.
//!   The rebuild is serialized by a `tokio::sync::Mutex` and re-checks the cache
//!   after acquiring it, so concurrent card GETs collapse onto one build rather
//!   than stampeding.
//! * **Invalidation on registry delta**, keyed on
//!   [`CapabilityRegistry::generation`] — a relaxed atomic load, which is
//!   cheaper than the work it guards. Hashing a snapshot would not be: taking
//!   the snapshot is most of the cost.

use std::sync::Arc;

use crate::adapters::rap::AgentSigner;
use crate::domain::models::capability_registry::CapabilityRegistry;

use super::auth::A2aServerAuth;
use super::card::ServedAgentCard;
use super::error::A2aError;
use super::jws::sign_card;

#[derive(Clone)]
struct CachedCard {
    generation: u64,
    signed: Arc<str>,
}

pub struct SignedCardCache {
    current: tokio::sync::RwLock<Option<CachedCard>>,
    rebuild: tokio::sync::Mutex<()>,
    /// Ed25519 signatures this cache has performed.
    ///
    /// AC7b's caching keystone is a deterministic counter, not a timing test:
    /// "the second GET was faster" is a measurement, "the second GET signed
    /// nothing" is a proof. Per-instance rather than a process-global static so
    /// concurrently-running tests cannot inflate each other's count.
    #[cfg(any(test, feature = "test-instrumentation"))]
    signatures: std::sync::atomic::AtomicU64,
}

impl Default for SignedCardCache {
    fn default() -> Self {
        Self::new()
    }
}

impl SignedCardCache {
    #[must_use]
    pub fn new() -> Self {
        Self {
            current: tokio::sync::RwLock::new(None),
            rebuild: tokio::sync::Mutex::new(()),
            #[cfg(any(test, feature = "test-instrumentation"))]
            signatures: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// The signed card for the registry's current generation, building it only
    /// if the cache is cold or stale.
    pub async fn signed(
        &self,
        registry: &CapabilityRegistry,
        signer: &AgentSigner,
        endpoint_url: &str,
        auth: Option<&A2aServerAuth>,
    ) -> Result<Arc<str>, A2aError> {
        self.signed_for_sampled_generation(
            registry,
            signer,
            endpoint_url,
            auth,
            registry.generation(),
        )
        .await
    }

    /// Complete a card lookup after the caller sampled `sampled_generation`.
    ///
    /// Keeping the initial sample explicit makes the stale-waiter rule visible:
    /// after waiting for `rebuild`, only a newly loaded generation can decide
    /// whether another builder already produced the requested card.
    async fn signed_for_sampled_generation(
        &self,
        registry: &CapabilityRegistry,
        signer: &AgentSigner,
        endpoint_url: &str,
        auth: Option<&A2aServerAuth>,
        sampled_generation: u64,
    ) -> Result<Arc<str>, A2aError> {
        if let Some(cached) = self.current.read().await.as_ref()
            && cached.generation == sampled_generation
        {
            return Ok(cached.signed.clone());
        }

        let _rebuild = self.rebuild.lock().await;
        // Re-read under the rebuild lock: a waiter may have sampled an older
        // generation while another builder published the current card.
        let generation = registry.generation();
        if let Some(cached) = self.current.read().await.as_ref()
            && cached.generation == generation
        {
            return Ok(cached.signed.clone());
        }

        let card = ServedAgentCard::from_registry(registry, endpoint_url)
            .await
            .with_declared_auth(auth)
            .with_ownership(signer.identity().peer_id.to_string())
            .with_signature_budget_reserved();

        #[cfg(any(test, feature = "test-instrumentation"))]
        self.signatures
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let signed: Arc<str> = sign_card(&card, signer)?.into();

        *self.current.write().await = Some(CachedCard {
            generation,
            signed: signed.clone(),
        });
        Ok(signed)
    }

    /// Signatures performed by this cache.
    #[cfg(any(test, feature = "test-instrumentation"))]
    pub fn signature_count(&self) -> u64 {
        self.signatures.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::adapters::rap::IdentityKeyStore;
    use crate::domain::models::TrustTier;
    use crate::domain::models::capability_id::CapabilityId;
    use crate::domain::models::capability_registry::RegisteredCapability;

    fn test_capability() -> RegisteredCapability {
        RegisteredCapability {
            id: CapabilityId {
                protocol: "skill".into(),
                server: String::new(),
                tool: "cache-test".into(),
            },
            protocol: "skill".into(),
            provider_id: "test".into(),
            name: "cache-test".into(),
            description: "cache test capability".into(),
            input_schema: serde_json::json!({}),
            parallel_safe: true,
            trust: TrustTier::Verified,
        }
    }

    #[tokio::test]
    async fn a_stale_waiter_rechecks_the_fresh_cached_generation() {
        let registry = Arc::new(CapabilityRegistry::new(None));
        let _registration = registry
            .register(test_capability())
            .await
            .expect("register test capability");
        let fresh_generation = registry.generation();
        assert!(fresh_generation > 0, "registration advances the generation");

        let cache = SignedCardCache::new();
        let expected: Arc<str> = Arc::from("already-signed");
        *cache.current.write().await = Some(CachedCard {
            generation: fresh_generation,
            signed: expected.clone(),
        });

        let key_dir = tempfile::tempdir().expect("temporary signing-key directory");
        let signer = IdentityKeyStore::new(key_dir.path())
            .load_or_generate()
            .expect("test signing identity");
        let actual = cache
            .signed_for_sampled_generation(
                &registry,
                &signer,
                "http://127.0.0.1:8080",
                None,
                fresh_generation - 1,
            )
            .await
            .expect("fresh cache entry is reused");

        assert_eq!(actual.as_ref(), expected.as_ref());
        assert_eq!(
            cache.signature_count(),
            0,
            "a waiter with a stale sample must not re-sign the current generation"
        );
    }
}
