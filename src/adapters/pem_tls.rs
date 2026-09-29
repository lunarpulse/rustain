//! Operator-supplied PEM material → a `rustls::ServerConfig`, for every server
//! listener rustain binds.
//!
//! # Why this is its own module (Story 18.4c-b, ruling A7b(2))
//!
//! This loader was `a2a::tls::load_tls_material` and was `a2a`-gated.
//! `relay serve --cert/--key` needs exactly the same thing — `rustls_pemfile`
//! parsing, the empty-block refusals, the crypto-provider install, and
//! `ServerConfig::builder().with_no_client_auth().with_single_cert(..)`. It was
//! **lifted here** rather than duplicated: ⛔ two PEM loaders is two error
//! vocabularies for one operator mistake, and the second copy is the one that
//! forgets the provider install below. `a2a::tls::load_tls_material` keeps its
//! signature and delegates here.
//!
//! # The crypto-provider install is not optional
//!
//! `iroh-relay`'s `server` feature enables `rustls/ring`, while rustain's own
//! `rustls` dependency carries the default `aws-lc-rs`. With two providers
//! linked and none installed, `rustls::ServerConfig::builder()` **panics at
//! runtime** — not a compile error, and only on the TLS path. `a2a` is not a
//! default feature, so a `relay-server`-only build has no other installer in
//! the process: ⛔ do not assume something else installed one.

#![cfg(any(feature = "a2a", feature = "relay-server"))]

use std::path::Path;

/// A PEM pair that could not be turned into a server configuration.
///
/// One message vocabulary for both callers. Each message names the offending
/// path, because "invalid certificate" without a path is a support ticket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PemTlsError(String);

impl PemTlsError {
    /// The operator-facing reason.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PemTlsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PemTlsError {}

/// Load a PEM certificate bundle: every `CERTIFICATE` block in one file.
///
/// # Why this is shared (Story 19.14, `A23`)
///
/// Story 19.14's A2A **client** needs exactly this — read, `rustls_pemfile::certs`,
/// refuse a file with no `CERTIFICATE` block — to turn an operator's `caCert` into
/// trust anchors. ⛔ A second loader would be a second error vocabulary for one
/// operator mistake, which is the reason this module exists; the three messages
/// below are already shipped by `relay serve` and the A2A server, so ⛔ do not
/// reword them.
///
/// Blocking file I/O — call from `spawn_blocking` or from a non-async startup
/// path, never from inside a request handler.
pub fn load_certificate_bundle(
    cert_path: &Path,
) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>, PemTlsError> {
    let cert_pem = read_pem(cert_path)?;
    let certs = rustls_pemfile::certs(&mut cert_pem.as_slice())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            PemTlsError(format!(
                "parsing certificate chain {}: {error}",
                cert_path.display()
            ))
        })?;
    if certs.is_empty() {
        return Err(PemTlsError(format!(
            "certificate file {} contains no CERTIFICATE block",
            cert_path.display()
        )));
    }
    Ok(certs)
}

fn read_pem(path: &Path) -> Result<Vec<u8>, PemTlsError> {
    std::fs::read(path).map_err(|error| PemTlsError(format!("reading {}: {error}", path.display())))
}

/// Load a PEM certificate chain and key into a `rustls::ServerConfig`.
///
/// Blocking file I/O — call from `spawn_blocking` or from a non-async startup
/// path, never from inside a request handler.
pub fn load_server_tls_config(
    cert_path: &Path,
    key_path: &Path,
) -> Result<rustls::ServerConfig, PemTlsError> {
    let certs = load_certificate_bundle(cert_path)?;

    let key_pem = read_pem(key_path)?;

    let key = rustls_pemfile::private_key(&mut key_pem.as_slice())
        .map_err(|error| {
            PemTlsError(format!(
                "parsing private key {}: {error}",
                key_path.display()
            ))
        })?
        .ok_or_else(|| {
            PemTlsError(format!(
                "private key file {} contains no PRIVATE KEY block",
                key_path.display()
            ))
        })?;

    install_ring_crypto_provider();

    rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|error| PemTlsError(format!("building rustls server config: {error}")))
}

/// Make `ring` the process default, best-effort.
///
/// An `Err` means another provider was already installed, which is safe —
/// `ServerConfig::builder()` works with whichever provider is installed — but
/// ⚠ it may not be `ring`: rustain's own `rustls` carries the default
/// `aws-lc-rs`, so another component may have installed that first. The claim
/// this function can make is only that *a* provider exists after it returns.
/// Named as its own function because two features now depend on it and a
/// future third must not have to rediscover the panic.
pub fn install_ring_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_is_a_typed_error_naming_the_path() {
        let error = load_server_tls_config(
            Path::new("/nonexistent/cert.pem"),
            Path::new("/nonexistent/key.pem"),
        )
        .expect_err("missing files must not load");
        assert!(error.reason().contains("/nonexistent/cert.pem"), "{error}");
    }

    #[test]
    fn a_pem_file_without_a_certificate_block_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cert = dir.path().join("cert.pem");
        let key = dir.path().join("key.pem");
        std::fs::write(&cert, b"not a pem file\n").expect("write cert");
        std::fs::write(&key, b"not a pem file\n").expect("write key");
        let error = load_server_tls_config(&cert, &key).expect_err("garbage must not load");
        assert!(error.reason().contains("no CERTIFICATE block"), "{error}");
    }
}
