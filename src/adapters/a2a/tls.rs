//! TLS termination for the non-loopback A2A listener.
//!
//! Story 18.1b, AC3b. `rustls::ServerConfig` + `tokio_rustls::TlsAcceptor`,
//! adapted onto [`axum::serve::Listener`] so the *same* router, the same
//! outermost request deadline, and the same handlers serve both the plaintext
//! loopback socket and the TLS one. A second serving path would be a second set
//! of middleware to forget.
//!
//! After a handshake succeeds, the stream owns one of 128 established-connection
//! permits until hyper drops it. This bounds completed TLS connections that idle
//! before sending an HTTP request, rather than allowing them to consume file
//! descriptors indefinitely.

use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, mpsc};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

use super::auth::A2aTlsMaterial;
use super::error::A2aError;

/// How long a single TLS handshake may take before it is abandoned.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How many handshakes may be in flight at once.
///
/// A TLS handshake is the cheapest way for an unauthenticated caller to make us
/// do asymmetric crypto, and this listener is reachable off-host by definition.
/// The bound is deliberately small: legitimate A2A peers are few and long-lived.
const MAX_CONCURRENT_HANDSHAKES: usize = 64;

/// Depth of the accepted-connection queue handed to axum.
const READY_QUEUE: usize = 32;

/// Maximum completed TLS connections admitted to axum at once.
///
/// The permit is retained for the lifetime of the connection, including while
/// it is idle before its first HTTP request.
const MAX_ESTABLISHED_CONNECTIONS: usize = 128;

/// Initial delay after a retryable listener accept failure.
const ACCEPT_BACKOFF_INITIAL: Duration = Duration::from_millis(5);

/// Maximum delay after persistent listener accept failures.
const ACCEPT_BACKOFF_MAX: Duration = Duration::from_secs(1);

#[derive(Debug)]
struct AcceptBackoff {
    next: Duration,
}

impl AcceptBackoff {
    const fn new() -> Self {
        Self {
            next: ACCEPT_BACKOFF_INITIAL,
        }
    }

    fn next_delay(&mut self) -> Duration {
        let delay = self.next;
        self.next = self
            .next
            .checked_mul(2)
            .unwrap_or(ACCEPT_BACKOFF_MAX)
            .min(ACCEPT_BACKOFF_MAX);
        delay
    }

    fn reset(&mut self) {
        self.next = ACCEPT_BACKOFF_INITIAL;
    }
}

/// Whether retrying `TcpListener::accept` cannot repair the listener.
fn accept_error_is_fatal(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::AddrInUse
            | io::ErrorKind::AddrNotAvailable
            | io::ErrorKind::InvalidData
            | io::ErrorKind::InvalidInput
            | io::ErrorKind::NotConnected
            | io::ErrorKind::PermissionDenied
            | io::ErrorKind::Unsupported
    )
}

/// Wait for a TCP connection without leaving a detached pump behind after the
/// `TlsListener` receiver has been dropped.
async fn accept_or_receiver_closed<T>(
    listener: &TcpListener,
    ready: &mpsc::Sender<T>,
) -> Option<io::Result<(TcpStream, SocketAddr)>> {
    tokio::select! {
        biased;
        _ = ready.closed() => None,
        accepted = listener.accept() => Some(accepted),
    }
}

async fn run_accept_pump(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    ready: mpsc::Sender<(EstablishedTlsStream, SocketAddr)>,
    handshake_permits: Arc<tokio::sync::Semaphore>,
    established_permits: Arc<tokio::sync::Semaphore>,
) {
    let mut backoff = AcceptBackoff::new();

    loop {
        let Some(accepted) = accept_or_receiver_closed(&listener, &ready).await else {
            tracing::debug!("A2A TLS listener: receiver dropped; stopping accept pump");
            break;
        };

        let (stream, peer) = match accepted {
            Ok(accepted) => {
                backoff.reset();
                accepted
            }
            Err(error) if accept_error_is_fatal(&error) => {
                tracing::error!(%error, "A2A TLS listener: unrecoverable accept failure");
                break;
            }
            Err(error) => {
                let delay = backoff.next_delay();
                tracing::warn!(
                    %error,
                    ?delay,
                    "A2A TLS listener: accept failed; retrying"
                );
                tokio::time::sleep(delay).await;
                continue;
            }
        };

        let handshake_permit = tokio::select! {
            biased;
            _ = ready.closed() => break,
            permit = handshake_permits.clone().acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => break,
            },
        };
        let acceptor = acceptor.clone();
        let ready = ready.clone();
        let established_permits = established_permits.clone();

        tokio::spawn(async move {
            let _handshake_permit = handshake_permit;
            let handshake = tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await;
            match handshake {
                Ok(Ok(tls)) => {
                    let established_permit = match established_permits.try_acquire_owned() {
                        Ok(permit) => permit,
                        Err(_) => {
                            tracing::debug!(
                                "A2A TLS listener: established connection limit reached"
                            );
                            return;
                        }
                    };
                    if ready
                        .send((EstablishedTlsStream::new(tls, established_permit), peer))
                        .await
                        .is_err()
                    {
                        tracing::debug!("A2A TLS listener: receiver dropped after handshake");
                    }
                }
                Ok(Err(error)) => {
                    tracing::debug!(%error, "A2A TLS handshake failed");
                }
                Err(_) => {
                    tracing::debug!("A2A TLS handshake timed out");
                }
            }
        });
    }
}

/// Load a PEM certificate chain and private key into a `rustls::ServerConfig`.
///
/// Blocking file I/O — call from `spawn_blocking` or from a non-async startup
/// path, never from inside a request handler.
pub fn load_tls_material(cert_path: &Path, key_path: &Path) -> Result<A2aTlsMaterial, A2aError> {
    let read = |path: &Path| -> Result<Vec<u8>, A2aError> {
        std::fs::read(path)
            .map_err(|error| A2aError::Config(format!("reading {}: {error}", path.display())))
    };

    let cert_pem = read(cert_path)?;
    let certs = rustls_pemfile::certs(&mut cert_pem.as_slice())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            A2aError::Config(format!(
                "parsing certificate chain {}: {error}",
                cert_path.display()
            ))
        })?;
    if certs.is_empty() {
        return Err(A2aError::Config(format!(
            "certificate file {} contains no CERTIFICATE block",
            cert_path.display()
        )));
    }

    let key_pem = read(key_path)?;
    let key = rustls_pemfile::private_key(&mut key_pem.as_slice())
        .map_err(|error| {
            A2aError::Config(format!(
                "parsing private key {}: {error}",
                key_path.display()
            ))
        })?
        .ok_or_else(|| {
            A2aError::Config(format!(
                "private key file {} contains no PRIVATE KEY block",
                key_path.display()
            ))
        })?;

    // The process may host several rustls users (reqwest already links one), so
    // installing the default provider is best-effort: an `Err` means somebody
    // installed one first, which is fine — it is the same `ring` provider.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|error| A2aError::Config(format!("building rustls server config: {error}")))?;

    Ok(A2aTlsMaterial {
        config: Arc::new(config),
    })
}

/// A completed TLS transport that retains its post-handshake admission permit.
///
/// This type is public only because [`axum::serve::Listener`] exposes its I/O
/// associated type. Callers receive it through [`TlsListener`].
pub struct EstablishedTlsStream {
    stream: TlsStream<TcpStream>,
    _admission_permit: OwnedSemaphorePermit,
}

impl EstablishedTlsStream {
    fn new(stream: TlsStream<TcpStream>, admission_permit: OwnedSemaphorePermit) -> Self {
        Self {
            stream,
            _admission_permit: admission_permit,
        }
    }
}

impl AsyncRead for EstablishedTlsStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        Pin::new(&mut this.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for EstablishedTlsStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        Pin::new(&mut this.stream).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        Pin::new(&mut this.stream).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        Pin::new(&mut this.stream).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        Pin::new(&mut this.stream).poll_shutdown(cx)
    }
}

/// A [`TcpListener`] whose accepted connections have already completed their TLS
/// handshake.
///
/// Handshakes run on spawned tasks rather than inline, because
/// [`axum::serve::Listener::accept`] yields one connection at a time: a peer
/// that opens a socket and then sends nothing would otherwise hold the accept
/// loop hostage for the whole handshake timeout — a one-connection denial of
/// service against every other peer.
pub struct TlsListener {
    local_addr: SocketAddr,
    ready: mpsc::Receiver<(EstablishedTlsStream, SocketAddr)>,
}

impl TlsListener {
    /// Wrap `listener`, spawning the accept-and-handshake pump.
    ///
    /// The pump selects receiver closure alongside `accept`, so dropping the
    /// listener stops it even if no subsequent handshake completes.
    pub fn new(listener: TcpListener, material: &A2aTlsMaterial) -> io::Result<Self> {
        let local_addr = listener.local_addr()?;
        let acceptor = TlsAcceptor::from(material.config.clone());
        let (tx, ready) = mpsc::channel(READY_QUEUE);
        let handshake_permits = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_HANDSHAKES));
        let established_permits =
            Arc::new(tokio::sync::Semaphore::new(MAX_ESTABLISHED_CONNECTIONS));

        tokio::spawn(run_accept_pump(
            listener,
            acceptor,
            tx,
            handshake_permits,
            established_permits,
        ));

        Ok(Self { local_addr, ready })
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = EstablishedTlsStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.ready.recv().await {
                Some(accepted) => return accepted,
                // The pump is gone. `Listener::accept` has no failure channel,
                // so park forever rather than fabricate a connection; graceful
                // shutdown drives termination.
                None => std::future::pending().await,
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        Ok(self.local_addr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_certificate_file_is_a_typed_config_error_not_a_panic() {
        let error = load_tls_material(
            Path::new("/nonexistent/cert.pem"),
            Path::new("/nonexistent/key.pem"),
        )
        .expect_err("missing files must not load");
        assert!(matches!(error, A2aError::Config(_)), "{error:?}");
    }

    #[test]
    fn a_pem_file_without_a_certificate_block_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cert = dir.path().join("cert.pem");
        let key = dir.path().join("key.pem");
        std::fs::write(&cert, b"not a pem file\n").expect("write cert");
        std::fs::write(&key, b"not a pem file\n").expect("write key");
        let error = load_tls_material(&cert, &key).expect_err("garbage must not load");
        let A2aError::Config(reason) = error else {
            panic!("expected a config error");
        };
        assert!(reason.contains("no CERTIFICATE block"), "{reason}");
    }

    #[test]
    fn accept_error_backoff_is_bounded_and_resets_after_success() {
        let mut backoff = AcceptBackoff::new();

        for millis in [5, 10, 20, 40, 80, 160, 320, 640, 1_000, 1_000] {
            assert_eq!(backoff.next_delay(), Duration::from_millis(millis));
        }

        backoff.reset();
        assert_eq!(backoff.next_delay(), ACCEPT_BACKOFF_INITIAL);
    }

    #[test]
    fn invalid_accept_errors_are_not_retried() {
        assert!(accept_error_is_fatal(&io::Error::from(
            io::ErrorKind::InvalidInput
        )));
        assert!(!accept_error_is_fatal(&io::Error::from(
            io::ErrorKind::Other
        )));
    }

    #[tokio::test]
    async fn pump_accept_loop_exits_when_ready_receiver_closes() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let (ready, receiver) = mpsc::channel::<()>(1);
        drop(receiver);

        assert!(
            accept_or_receiver_closed(&listener, &ready).await.is_none(),
            "closed receiver must stop the accept pump before another accept"
        );
    }
}
