//! The embedded `iroh-relay` server behind `rustain relay serve` (Story
//! 18.4c-b, FR159).
//!
//! # The one composition, and why it is here rather than in the CLI
//!
//! [`spawn_relay`] is the **only** place rustain names `iroh_relay::server`
//! types. It is the front door for every keystone that needs a live relay:
//! constructing a `ServerConfig`/`RelayConfig`/`TlsConfig` inline in a test would
//! prove `iroh-relay` works, ⛔ not that rustain composes it.
//!
//! # Three `RelayConfig` types are in scope in this tree
//!
//! `crate::domain::models::relay` has the operator posture value types,
//! `iroh::RelayMap` has a client-side one, and `iroh_relay::server` has the
//! server one. The imports below alias the server types, exactly as iroh's own
//! downstream composition does, so a reader can never be in doubt which is meant.
//!
//! # Two landmines that are not obvious from the API
//!
//! 1. **A rustls `CryptoProvider` must be installed or the TLS path panics at
//!    runtime.** `iroh-relay`'s `server` feature enables `rustls/ring`, while
//!    rustain's own `rustls` carries the default `aws-lc-rs`; with two providers
//!    linked and none installed, `rustls::ServerConfig::builder()` panics. It is
//!    not a compile error, and it only fires on the TLS path. `a2a` is not a
//!    default feature, so a `relay-server`-only build has no other installer in
//!    the process — [`crate::adapters::pem_tls`] does it for both.
//! 2. **`Server::shutdown` consumes `self`.** The shutdown path must therefore
//!    OWN the `Server`: ⛔ not an `Arc<Server>`, ⛔ not a clone. An
//!    implementation that parks the server behind a shared handle and shuts it
//!    down from a signal task does not compile. [`serve`] takes it by value.
//!
//! # ⛔ Certificates are operator-supplied, and only operator-supplied
//!
//! `CertConfig` offers exactly two variants and there is no self-signed one.
//! This cut ships `Manual` only: `CertConfig::LetsEncrypt` performs outbound ACME
//! to a third party **by construction**, and FR159-a says the set of relay hosts
//! this process contacts is exactly the set the operator configured — an ACME
//! directory is not in that set. `DF-18-4c-b-ACME` carries the automatic-renewal
//! path. ⚠ Note what that does and does not buy: `tokio-rustls-acme` is compiled
//! and linked unconditionally with the `server` feature, so the honest claim is
//! that **no code path here constructs it**, ⛔ not that the ACME client is
//! absent from the binary.

use iroh_relay::server::{
    CertConfig, QuicConfig as RelayServerQuicConfig, RelayConfig as RelayServerConfig, Server,
    ServerConfig as RelayServerServerConfig, TlsConfig as RelayServerTlsConfig,
};

use crate::adapters::cli::relay::serve::ServePlan;

/// A relay that could not be composed or could not start.
#[derive(Debug)]
pub enum RelayServerError {
    /// The PEM pair the operator named could not become a server config.
    Certificate(crate::adapters::pem_tls::PemTlsError),
    /// `iroh-relay` refused to start, usually because a socket was taken.
    Spawn(String),
}

impl std::fmt::Display for RelayServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Certificate(error) => write!(f, "{error}"),
            Self::Spawn(reason) => write!(f, "{reason}"),
        }
    }
}

impl std::error::Error for RelayServerError {}

/// How the serve loop ended.
///
/// ⚑ The distinction is the whole point: systemd's `Restart=on-failure` fires on
/// a non-zero exit, so a relay that terminated on its own must not look like a
/// clean stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayExit {
    /// The operator stopped it: `SIGINT` (Ctrl-C), measured to exit `0`.
    ///
    /// ⚠ **`SIGTERM` is NOT handled and this variant is not reached for it** —
    /// `tokio::signal::ctrl_c()` is `SIGINT` only, so `systemctl stop` kills the
    /// process by default disposition instead of running the graceful path. That
    /// is correct for the shipped unit, because systemd does not apply
    /// `Restart=on-failure` to a stop it requested itself — but ⛔ do not write
    /// that this variant covers `SIGTERM`, and ⛔ do not conclude the graceful
    /// shutdown below runs on `systemctl stop`. It does not.
    Cancelled,
    /// The relay's own supervisor finished first — an internal failure, or no
    /// services were enabled. ⚠ `PT-13` never mentions this case, and it matters
    /// *precisely because* rustain-side supervision defers: a relay that dies on
    /// its own must exit non-zero, and ⛔ must not hang waiting for a Ctrl-C
    /// that will never come.
    RelayStopped,
}

impl RelayExit {
    /// The process exit code this ending deserves.
    #[must_use]
    pub fn code(self) -> i32 {
        match self {
            Self::Cancelled => 0,
            Self::RelayStopped => 1,
        }
    }
}

/// Compose the `ServerConfig` this plan describes.
///
/// ⛔ `ServerConfig` is `#[non_exhaustive]`, so a struct literal — including a
/// `..Default::default()` functional update — does not compile downstream
/// (E0639). `::default()` plus field assignment is the only shape available, and
/// `RelayConfig::new` / `TlsConfig::new` have no `Default` at all.
pub fn build_server_config(
    plan: &ServePlan,
) -> Result<RelayServerServerConfig, RelayServerError> {
    let mut config = RelayServerServerConfig::default();
    let mut relay = RelayServerConfig::new(plan.http_addr);

    if let Some(tls) = plan.tls.as_ref() {
        let server_config =
            crate::adapters::pem_tls::load_server_tls_config(&tls.cert, &tls.key)
                .map_err(RelayServerError::Certificate)?;
        relay.tls = Some(RelayServerTlsConfig::new(
            tls.https_addr,
            // ⛔ `CertConfig` is `#[non_exhaustive]` as an enum, so a `match` on
            // it needs a `_` arm — but its VARIANTS are not, so this one is
            // constructible downstream.
            CertConfig::Manual { server_config },
        ));
        // ⚑ Address discovery is served whenever TLS is, ⛔ never left dead: the
        // client's `RelayMap` fills in the default discovery port for every URL
        // it is given, so a relay with this unset degrades public-address
        // discovery — hole-punching with it — for every peer that configures it.
        // The server config is inherited from `RelayConfig::tls` above.
        config.quic = Some(RelayServerQuicConfig::new(tls.quic_addr));
    } else {
        // ⚑ Mechanical, ⛔ not a choice: `QuicConfig` inherits
        // `RelayConfig::tls`, and spawning without one fails
        // `QuicSpawnError::TlsNotConfigured`.
        config.quic = None;
    }

    // ⚑ Left at `iroh-relay`'s own default, `AllowAll`, in this cut: the relay
    // carries traffic for anyone who has its URL, and the operator copy says so.
    // ⛔ Not designed closed — `access` stays reachable from here, and
    // `DF-18-4c-b-RELAY-ACCESS` records that when it opens it reuses the
    // domain's `PinnedKey` / `peer_admission` vocabulary rather than minting a
    // third allowlist encoding.
    config.relay = Some(relay);
    Ok(config)
}

/// Start the relay this plan describes.
///
/// The production entry, and the front door for keystones. ⛔ The forbidden
/// bypass is an inline `ServerConfig` in a test.
pub async fn spawn_relay(plan: &ServePlan) -> Result<Server, RelayServerError> {
    let config = build_server_config(plan)?;
    Server::spawn(config)
        .await
        .map_err(|error| RelayServerError::Spawn(error.to_string()))
}

/// Run until the operator stops it or the relay stops itself.
///
/// ⚑ `server` is taken **by value** because `Server::shutdown(self)` consumes
/// it. The structure is upstream's own: own the server here, select on the
/// shutdown signal against `join()`, then shut down.
///
/// ⚠ On the `join()` arm the supervisor has already been awaited to completion,
/// so `shutdown()` must **not** be called — awaiting the same handle twice
/// panics. Dropping the `Server` stops it, which is all that is left to do.
pub async fn serve<S>(mut server: Server, shutdown: S) -> RelayExit
where
    S: Future<Output = ()>,
{
    enum First {
        Cancelled,
        Stopped,
    }
    let first = {
        let shutdown = std::pin::pin!(shutdown);
        tokio::select! {
            () = shutdown => First::Cancelled,
            _ = server.join() => First::Stopped,
        }
    };
    match first {
        First::Cancelled => {
            // Awaited to completion, exactly once, before returning: NFR24's
            // < 5 s budget is a failure deadline, ⛔ not the property.
            if let Err(error) = server.shutdown().await {
                tracing::warn!(%error, "the relay did not stop cleanly");
            }
            RelayExit::Cancelled
        }
        First::Stopped => {
            drop(server);
            RelayExit::RelayStopped
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relay_that_stops_itself_exits_non_zero() {
        // ⚑ THE MUTANT THIS PAIR EXISTS FOR: return zero on the `join()` arm and
        // a dead relay looks like a clean stop, so `Restart=on-failure` never
        // fires and the host silently stops carrying traffic.
        assert_eq!(RelayExit::RelayStopped.code(), 1);
        assert_eq!(RelayExit::Cancelled.code(), 0);
    }
}
