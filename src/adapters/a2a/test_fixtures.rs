//! In-crate A2A peer fixtures for Story 19.14's keystones.
//!
//! # Why a hand-written server rather than `wiremock`
//!
//! Three of this story's ACs need things `wiremock` cannot express: a **real TLS
//! handshake** against a locally generated certificate (`AC3`), an
//! **accepted-TCP-connection count** that must stay at zero to prove no client
//! was built (`AC4(c′)`), and a **certificate rotation** whose second handshake
//! is genuinely re-validated.
//!
//! ⚠ The rotation trap this fixture exists to avoid: `reqwest` pools connections
//! and rustls caches TLS 1.3 sessions, so re-using one `ServerConfig` lets the
//! post-rotation RPC *resume* without any certificate check. [`PeerFixture::rotate`]
//! therefore builds a **fresh** `ServerConfig` and every response carries
//! `Connection: close`. ⛔ Never change the client's pool or resumption settings
//! to work around this — that would test a client the product does not ship.
//!
//! ⚠ Every generated certificate gets a **distinct** Distinguished Name (`A25`).
//! With `rcgen`'s default shared DN a "different CA" fixture fails as
//! `BadSignature` instead of `UnknownIssuer`, and the positive control fails too.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

// ── certificates ────────────────────────────────────────────────────────────

/// A locally generated certificate authority.
pub(crate) struct TestCa {
    issuer: rcgen::Issuer<'static, rcgen::KeyPair>,
    /// PEM of the CA certificate — this is what an operator puts in `caCert`.
    pub anchor_pem: String,
}

/// A server certificate plus its key, both PEM.
pub(crate) struct TestLeaf {
    pub cert_pem: String,
    pub key_pem: String,
}

/// When a certificate is valid.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Validity {
    Current,
    /// `notAfter` in the past — rustls answers `ExpiredContext`, ⛔ never
    /// `Expired`, which is why the classifier matches variants instead of `==`.
    Expired,
}

fn distinguished_name(common_name: &str) -> rcgen::DistinguishedName {
    let mut name = rcgen::DistinguishedName::new();
    name.push(rcgen::DnType::CommonName, common_name);
    name
}

fn apply_validity(params: &mut rcgen::CertificateParams, validity: Validity) {
    match validity {
        Validity::Current => {
            params.not_before = rcgen::date_time_ymd(2020, 1, 1);
            params.not_after = rcgen::date_time_ymd(2100, 1, 1);
        }
        Validity::Expired => {
            params.not_before = rcgen::date_time_ymd(2020, 1, 1);
            params.not_after = rcgen::date_time_ymd(2021, 1, 1);
        }
    }
}

/// A CA whose subject is `common_name` — ⛔ give every fixture a different one.
pub(crate) fn test_ca(common_name: &str, validity: Validity) -> TestCa {
    let mut params = rcgen::CertificateParams::default();
    params.distinguished_name = distinguished_name(common_name);
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::CrlSign,
        rcgen::KeyUsagePurpose::DigitalSignature,
    ];
    apply_validity(&mut params, validity);
    let key = rcgen::KeyPair::generate().expect("generate CA key");
    let certificate = params.self_signed(&key).expect("self-sign CA");
    TestCa {
        anchor_pem: certificate.pem(),
        issuer: rcgen::Issuer::new(params, key),
    }
}

/// A server leaf for `host`, issued by `ca`.
pub(crate) fn leaf_issued_by(
    ca: &TestCa,
    common_name: &str,
    host: &str,
    validity: Validity,
) -> TestLeaf {
    let mut params =
        rcgen::CertificateParams::new(vec![host.to_owned()]).expect("leaf SAN parameters");
    params.distinguished_name = distinguished_name(common_name);
    params.is_ca = rcgen::IsCa::ExplicitNoCa;
    params.use_authority_key_identifier_extension = true;
    params.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    apply_validity(&mut params, validity);
    let key = rcgen::KeyPair::generate().expect("generate leaf key");
    let certificate = params.signed_by(&key, &ca.issuer).expect("sign leaf");
    TestLeaf {
        cert_pem: certificate.pem(),
        key_pem: key.serialize_pem(),
    }
}

/// A self-signed server certificate, pinned as its own anchor.
///
/// `is_ca` is the whole point of the pair of `AC3(h)` rows: a non-CA self-signed
/// leaf validates, and the `CA:TRUE` shape `openssl req -x509` produces by
/// default is refused as `CaUsedAsEndEntity`.
pub(crate) fn self_signed_leaf(common_name: &str, host: &str, is_ca: rcgen::IsCa) -> TestLeaf {
    let mut params =
        rcgen::CertificateParams::new(vec![host.to_owned()]).expect("leaf SAN parameters");
    params.distinguished_name = distinguished_name(common_name);
    params.is_ca = is_ca;
    params.key_usages = vec![
        rcgen::KeyUsagePurpose::DigitalSignature,
        rcgen::KeyUsagePurpose::KeyCertSign,
    ];
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    apply_validity(&mut params, Validity::Current);
    let key = rcgen::KeyPair::generate().expect("generate leaf key");
    let certificate = params.self_signed(&key).expect("self-sign leaf");
    TestLeaf {
        cert_pem: certificate.pem(),
        key_pem: key.serialize_pem(),
    }
}

/// Write `pem` into `dir` under `name` and return the path — an operator's
/// `caCert` is a path, so a fixture anchor has to be one too.
pub(crate) fn write_pem(dir: &std::path::Path, name: &str, pem: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, pem).expect("write PEM fixture");
    path
}

// ── the peer fixture ────────────────────────────────────────────────────────

/// One request a fixture peer received.
#[derive(Debug, Clone)]
pub(crate) struct RecordedRequest {
    pub method: String,
    pub path: String,
    /// The `x-api-key` header, if the client sent one.
    pub api_key: Option<String>,
    pub body: String,
}

/// How a fixture answers `POST`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum RpcAnswer {
    /// A JSON-RPC result whose task is already terminal, so one round trip is a
    /// complete send.
    Completed,
    /// A bare HTTP status — 401/403 is the credential verdict under test.
    Status(u16),
    /// A correlated JSON-RPC error object with this code (Story 19.16f: the
    /// codes the real server cannot be driven to answer for a served verb,
    /// e.g. `-32601` from an older build).
    JsonRpcError(i64),
}

struct FixtureState {
    accepted: AtomicUsize,
    requests: Mutex<Vec<RecordedRequest>>,
    tls: Mutex<Option<Arc<rustls::ServerConfig>>>,
    card_endpoint: Mutex<String>,
    answer: Mutex<RpcAnswer>,
    cancel: CancellationToken,
}

/// A loopback A2A peer: serves an AgentCard, answers JSON-RPC, and records what
/// it was actually sent.
pub(crate) struct PeerFixture {
    /// `http://localhost:<port>` or `https://localhost:<port>`.
    pub origin: String,
    state: Arc<FixtureState>,
    _tls_dir: Option<tempfile::TempDir>,
}

impl Drop for PeerFixture {
    fn drop(&mut self) {
        self.state.cancel.cancel();
    }
}

impl PeerFixture {
    /// A plaintext loopback peer. `http` to a loopback authority is exactly what
    /// the shipped `validate_url` permits, so no TLS is needed to exercise the
    /// credential half.
    pub(crate) async fn plaintext() -> Self {
        Self::start(None).await
    }

    /// A TLS peer serving `leaf`.
    pub(crate) async fn tls(leaf: &TestLeaf) -> Self {
        Self::start(Some(leaf)).await
    }

    async fn start(leaf: Option<&TestLeaf>) -> Self {
        let (tls, tls_dir) = match leaf {
            None => (None, None),
            Some(leaf) => {
                let dir = tempfile::tempdir().expect("tls fixture dir");
                (Some(server_config(dir.path(), leaf)), Some(dir))
            }
        };
        let scheme = if tls.is_some() { "https" } else { "http" };
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture listener");
        let port = listener.local_addr().expect("fixture port").port();
        // `localhost`, not `127.0.0.1`: the leaf's SAN is a DNS name, and a
        // certificate's name check is what `AnchorFailure::WrongName` is about.
        let origin = format!("{scheme}://localhost:{port}");
        let state = Arc::new(FixtureState {
            accepted: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
            tls: Mutex::new(tls),
            card_endpoint: Mutex::new(format!("{origin}/")),
            answer: Mutex::new(RpcAnswer::Completed),
            cancel: CancellationToken::new(),
        });
        tokio::spawn(accept_loop(listener, state.clone()));
        Self {
            origin,
            state,
            _tls_dir: tls_dir,
        }
    }

    /// Point the served AgentCard's JSON-RPC endpoint somewhere else — the
    /// cross-origin shape `A29` closes.
    pub(crate) async fn serve_endpoint(&self, endpoint: &str) {
        *self.state.card_endpoint.lock().await = endpoint.to_owned();
    }

    pub(crate) async fn answer_with(&self, answer: RpcAnswer) {
        *self.state.answer.lock().await = answer;
    }

    /// Swap in a **fresh** `ServerConfig` for `leaf`.
    ///
    /// ⚠ Fresh, not mutated: a reused `ServerConfig` keeps its TLS 1.3 session
    /// cache, and a resumed handshake performs no certificate validation at all.
    pub(crate) async fn rotate(&self, leaf: &TestLeaf) {
        let dir = tempfile::tempdir().expect("rotation dir");
        *self.state.tls.lock().await = Some(server_config(dir.path(), leaf));
    }

    /// TCP connections this fixture accepted.
    ///
    /// The load-failure keystone asserts this stays **zero**: a fallback client
    /// would reach the socket before failing, so "no client was built" is only
    /// provable from the server's side.
    pub(crate) fn accepted_connections(&self) -> usize {
        self.state.accepted.load(Ordering::SeqCst)
    }

    pub(crate) async fn requests(&self) -> Vec<RecordedRequest> {
        self.state.requests.lock().await.clone()
    }

    pub(crate) async fn api_keys_seen(&self) -> Vec<String> {
        self.requests()
            .await
            .into_iter()
            .filter_map(|request| request.api_key)
            .collect()
    }

    pub(crate) async fn posts(&self) -> Vec<RecordedRequest> {
        self.requests()
            .await
            .into_iter()
            .filter(|request| request.method == "POST")
            .collect()
    }
}

/// A port that is bound and then released: a connect refusal with **no**
/// `rustls::Error` anywhere in its chain.
pub(crate) async fn unreachable_origin(scheme: &str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind throwaway listener");
    let port = listener.local_addr().expect("throwaway port").port();
    drop(listener);
    format!("{scheme}://localhost:{port}")
}

fn server_config(dir: &std::path::Path, leaf: &TestLeaf) -> Arc<rustls::ServerConfig> {
    let cert = write_pem(dir, "server.pem", &leaf.cert_pem);
    let key = write_pem(dir, "server.key", &leaf.key_pem);
    // The production loader, which also installs a crypto provider.
    Arc::new(
        crate::adapters::pem_tls::load_server_tls_config(&cert, &key)
            .expect("fixture server config"),
    )
}

async fn accept_loop(listener: TcpListener, state: Arc<FixtureState>) {
    loop {
        let accepted = tokio::select! {
            _ = state.cancel.cancelled() => return,
            accepted = listener.accept() => accepted,
        };
        let Ok((stream, _)) = accepted else { continue };
        state.accepted.fetch_add(1, Ordering::SeqCst);
        let state = state.clone();
        tokio::spawn(async move {
            let tls = state.tls.lock().await.clone();
            match tls {
                Some(config) => {
                    let acceptor = tokio_rustls::TlsAcceptor::from(config);
                    // A client that refuses our certificate aborts here; that is
                    // the anchor-failure path, and the accepted count above has
                    // already recorded that it reached the socket.
                    if let Ok(stream) = acceptor.accept(stream).await {
                        let _ = handle(stream, &state).await;
                    }
                }
                None => {
                    let _ = handle(stream, &state).await;
                }
            }
        });
    }
}

async fn handle<S>(mut stream: S, state: &FixtureState) -> std::io::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(index) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break index;
        }
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(());
        }
        buffer.extend_from_slice(&chunk[..read]);
    };

    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default().to_owned();
    let mut headers: HashMap<String, String> = HashMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }

    let content_length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let body_start = head_end + 4;
    while buffer.len() < body_start + content_length {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let body_end = buffer.len().min(body_start + content_length);
    let body = String::from_utf8_lossy(&buffer[body_start..body_end]).into_owned();

    let mut fields = request_line.split_whitespace();
    let method = fields.next().unwrap_or_default().to_owned();
    let path = fields.next().unwrap_or_default().to_owned();

    state.requests.lock().await.push(RecordedRequest {
        method: method.clone(),
        path: path.clone(),
        api_key: headers.get("x-api-key").cloned(),
        body: body.clone(),
    });

    let response = if method == "GET" && path == "/.well-known/agent-card.json" {
        let card = serde_json::json!({
            "name": "Fixture Peer",
            "url": state.card_endpoint.lock().await.clone(),
            "preferredTransport": "JSONRPC",
            "skills": [{ "id": "scan", "name": "Scan" }],
        });
        http_response(200, "application/json", &card.to_string())
    } else if method == "POST" {
        match *state.answer.lock().await {
            RpcAnswer::Completed => {
                http_response(200, "application/json", &completed_task(&body).to_string())
            }
            // ⛔ A body is served and the client must discard it: `A20` forbids
            // reading or rendering a non-success body, and `AC4(f)` greps for
            // this sentinel.
            RpcAnswer::Status(status) => http_response(
                status,
                "application/json",
                r#"{"detail":"FIXTURE-SERVER-TEXT"}"#,
            ),
            RpcAnswer::JsonRpcError(code) => http_response(
                200,
                "application/json",
                &serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": request_id(&body),
                    "error": { "code": code, "message": "fixture" },
                })
                .to_string(),
            ),
        }
    } else {
        http_response(404, "text/plain", "")
    };

    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

/// A terminal task, correlated with the request's own JSON-RPC id.
fn request_id(request_body: &str) -> u64 {
    serde_json::from_str::<serde_json::Value>(request_body)
        .ok()
        .and_then(|value| value.get("id").and_then(serde_json::Value::as_u64))
        .unwrap_or(1)
}

fn completed_task(request_body: &str) -> serde_json::Value {
    let id = request_id(request_body);
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "kind": "task",
            "id": "fixture-task",
            "contextId": "fixture-context",
            "status": { "state": "completed", "timestamp": "2026-09-15T00:00:00+00:00" },
            "artifacts": [{ "parts": [{ "kind": "text", "text": "fixture reply" }] }],
        },
    })
}

fn http_response(status: u16, content_type: &str, body: &str) -> String {
    let reason = match status {
        200 => "OK",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Error",
    };
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
}
