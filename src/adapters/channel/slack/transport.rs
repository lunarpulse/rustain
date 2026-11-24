//! Production Slack transport: `reqwest` for the two Web API calls and
//! `tokio-websockets` over `tokio-rustls` for the Socket Mode connection.
//!
//! One TLS stack: rustls with the ring provider, trusting the operating
//! system's roots (`rustls-native-certs`, which honors `SSL_CERT_FILE` /
//! `SSL_CERT_DIR` like the provider clients), with no certificate-verification
//! relaxation anywhere. The WebSocket URL Slack returns carries a one-time
//! ticket: it is validated (`wss`, a `*.slack.com` host), used once, and never
//! logged. Tokens live in [`Secret`], whose `Debug` is redacted and whose
//! storage is zeroized on drop.

use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::sync::OnceCell;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_websockets::{ClientBuilder, Limits, Message, WebSocketStream};
use zeroize::Zeroize;

use super::proto::MAX_FRAME_BYTES;
use super::{SlackError, SlackSocket, SlackTransport, SocketEvent};

const API_BASE: &str = "https://slack.com/api/";
const API_TIMEOUT: Duration = Duration::from_secs(15);
const WS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// Slack's own limit is far below this; the transport allows twice the parse bound.
const WS_MAX_PAYLOAD: usize = 2 * MAX_FRAME_BYTES;
const MAX_API_BODY_BYTES: usize = 64 * 1024;

/// A bearer token: never printed, zeroized when dropped.
struct Secret(String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// `*.slack.com` and nothing else.
fn is_slack_host(host: &str) -> bool {
    host.len() > ".slack.com".len() && host.ends_with(".slack.com")
}

pub struct SlackHttpTransport {
    http: reqwest::Client,
    api_base: String,
    app_token: Secret,
    bot_token: Secret,
    tls: OnceCell<Arc<rustls::ClientConfig>>,
    host_ok: fn(&str) -> bool,
}

impl std::fmt::Debug for SlackHttpTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlackHttpTransport")
            .field("app_token", &self.app_token)
            .field("bot_token", &self.bot_token)
            .finish_non_exhaustive()
    }
}

impl SlackHttpTransport {
    /// Build the production transport. Performs no network or trust-store I/O.
    pub fn new(app_token: String, bot_token: String) -> Result<Self, SlackError> {
        let app_token = app_token.trim().to_string();
        let bot_token = bot_token.trim().to_string();
        if app_token.is_empty() || bot_token.is_empty() {
            return Err(SlackError("empty Slack token".into()));
        }
        let http = reqwest::Client::builder()
            .timeout(API_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| SlackError("http client unavailable".into()))?;
        Ok(Self {
            http,
            api_base: API_BASE.to_string(),
            app_token: Secret(app_token),
            bot_token: Secret(bot_token),
            tls: OnceCell::new(),
            host_ok: is_slack_host,
        })
    }

    /// Fixture constructor: a local Web API origin, a caller-supplied trust
    /// configuration and a loopback-accepting WebSocket host check. Compiled for
    /// this crate's own tests only; the product has no endpoint override.
    #[cfg(test)]
    pub(crate) fn for_test(
        api_base: String,
        tls: Arc<rustls::ClientConfig>,
        host_ok: fn(&str) -> bool,
    ) -> Self {
        let mut transport =
            Self::new("xapp-test-sentinel".into(), "xoxb-test-sentinel".into()).expect("builds");
        transport.api_base = api_base;
        transport.tls = OnceCell::new_with(Some(tls));
        transport.host_ok = host_ok;
        transport
    }

    async fn tls_config(&self) -> Result<Arc<rustls::ClientConfig>, SlackError> {
        self.tls
            .get_or_try_init(|| async {
                tokio::task::spawn_blocking(native_tls_config)
                    .await
                    .map_err(|_| SlackError("trust store load failed".into()))?
            })
            .await
            .cloned()
    }

    async fn api_call(
        &self,
        method: &str,
        token: &Secret,
        body: Option<Value>,
    ) -> Result<Value, SlackError> {
        let url = format!("{}{method}", self.api_base);
        let mut request = self.http.post(url).bearer_auth(&token.0);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(describe_request_error)?;
        let status = response.status();
        let bytes = read_bounded(response).await?;
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(SlackError("rate_limited".into()));
        }
        if !status.is_success() {
            return Err(SlackError(format!("http_{}", status.as_u16())));
        }
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|_| SlackError("response was not JSON".into()))?;
        if value.get("ok").and_then(Value::as_bool) == Some(true) {
            Ok(value)
        } else {
            Err(SlackError(error_code(&value)))
        }
    }

    async fn open_url(&self) -> Result<String, SlackError> {
        let value = self
            .api_call("apps.connections.open", &self.app_token, None)
            .await?;
        value
            .get("url")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| SlackError("connections.open returned no url".into()))
    }
}

/// A Slack error code, reduced to a safe token (`[a-z0-9_]`, at most 64 bytes).
fn error_code(value: &Value) -> String {
    value
        .get("error")
        .and_then(Value::as_str)
        .filter(|code| {
            !code.is_empty()
                && code.len() <= 64
                && code
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        })
        .map_or_else(|| "unknown_error".to_string(), str::to_string)
}

fn describe_request_error(error: reqwest::Error) -> SlackError {
    let kind = if error.is_timeout() {
        "request timed out"
    } else if error.is_connect() {
        "connection failed"
    } else {
        "request failed"
    };
    SlackError(kind.into())
}

async fn read_bounded(mut response: reqwest::Response) -> Result<Vec<u8>, SlackError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(describe_request_error)? {
        if body.len() + chunk.len() > MAX_API_BODY_BYTES {
            return Err(SlackError("response too large".into()));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn native_tls_config() -> Result<Arc<rustls::ClientConfig>, SlackError> {
    let loaded = rustls_native_certs::load_native_certs();
    let mut roots = rustls::RootCertStore::empty();
    let (added, _ignored) = roots.add_parsable_certificates(loaded.certs);
    if added == 0 {
        return Err(SlackError("no usable system trust roots".into()));
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|_| SlackError("tls configuration failed".into()))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// Validate a Socket Mode URL: `wss`, an accepted host, no credentials.
fn parse_socket_url(
    raw: &str,
    host_ok: fn(&str) -> bool,
) -> Result<(String, u16, String), SlackError> {
    let reject = || SlackError("connections.open returned an unusable url".into());
    let url = url::Url::parse(raw).map_err(|_| reject())?;
    if url.scheme() != "wss" || !url.username().is_empty() || url.password().is_some() {
        return Err(reject());
    }
    let host = url.host_str().ok_or_else(reject)?;
    if !host_ok(host) {
        return Err(reject());
    }
    Ok((host.to_string(), url.port().unwrap_or(443), url.to_string()))
}

struct WsSocket {
    stream: WebSocketStream<TlsStream<TcpStream>>,
}

#[async_trait::async_trait]
impl SlackSocket for WsSocket {
    async fn next_event(&mut self) -> Result<SocketEvent, SlackError> {
        match self.stream.next().await {
            None => Ok(SocketEvent::Closed),
            Some(Err(_)) => Err(SlackError("websocket read failed".into())),
            Some(Ok(message)) if message.is_close() => Ok(SocketEvent::Closed),
            Some(Ok(message)) => Ok(message.as_text().map_or(SocketEvent::Activity, |text| {
                SocketEvent::Text(text.to_string())
            })),
        }
    }

    async fn send_text(&mut self, text: String) -> Result<(), SlackError> {
        self.stream
            .send(Message::text(text))
            .await
            .map_err(|_| SlackError("websocket write failed".into()))
    }
}

#[async_trait::async_trait]
impl SlackTransport for SlackHttpTransport {
    async fn connect(&self) -> Result<Box<dyn SlackSocket>, SlackError> {
        let raw_url = self.open_url().await?;
        let (host, port, url) = parse_socket_url(&raw_url, self.host_ok)?;
        let tls = self.tls_config().await?;
        let handshake = async {
            let tcp = TcpStream::connect((host.as_str(), port))
                .await
                .map_err(|_| SlackError("websocket connect failed".into()))?;
            let name = rustls::pki_types::ServerName::try_from(host.clone())
                .map_err(|_| SlackError("websocket host unusable".into()))?;
            let tls_stream = TlsConnector::from(tls)
                .connect(name, tcp)
                .await
                .map_err(|_| SlackError("websocket tls handshake failed".into()))?;
            let (stream, _response) = ClientBuilder::new()
                .uri(&url)
                .map_err(|_| SlackError("websocket url unusable".into()))?
                .limits(Limits::default().max_payload_len(Some(WS_MAX_PAYLOAD)))
                .connect_on(tls_stream)
                .await
                .map_err(|_| SlackError("websocket upgrade failed".into()))?;
            Ok::<_, SlackError>(WsSocket { stream })
        };
        let socket = tokio::time::timeout(WS_HANDSHAKE_TIMEOUT, handshake)
            .await
            .map_err(|_| SlackError("websocket handshake timed out".into()))??;
        Ok(Box::new(socket))
    }

    async fn post_message(&self, channel: &str, text: &str) -> Result<(), SlackError> {
        let body = json!({
            "channel": channel,
            "text": text,
            "mrkdwn": false,
            "parse": "none",
            "unfurl_links": false,
            "unfurl_media": false,
        });
        self.api_call("chat.postMessage", &self.bot_token, Some(body))
            .await
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const APP: &str = "xapp-test-sentinel";
    const BOT: &str = "xoxb-test-sentinel";

    fn config_trusting(roots: rustls::RootCertStore) -> Arc<rustls::ClientConfig> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        Arc::new(
            rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        )
    }

    fn loopback_ok(host: &str) -> bool {
        host == "127.0.0.1"
    }

    fn http_transport(api: &MockServer) -> SlackHttpTransport {
        SlackHttpTransport::for_test(
            format!("{}/", api.uri()),
            config_trusting(rustls::RootCertStore::empty()),
            loopback_ok,
        )
    }

    #[tokio::test]
    async fn connections_open_bootstraps_with_the_app_token_only() {
        let api = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/apps.connections.open"))
            .and(header("authorization", format!("Bearer {APP}").as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"ok": true, "url": "wss://wss-primary.slack.com/link/?ticket=t"}),
            ))
            .expect(1)
            .mount(&api)
            .await;
        let url = http_transport(&api).open_url().await.expect("bootstrap");
        assert_eq!(url, "wss://wss-primary.slack.com/link/?ticket=t");
    }

    #[tokio::test]
    async fn post_message_uses_the_bot_token_and_a_plain_text_body_for_the_admitted_dm() {
        let api = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat.postMessage"))
            .and(header("authorization", format!("Bearer {BOT}").as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
            .expect(1)
            .mount(&api)
            .await;
        http_transport(&api)
            .post_message("D0ALICE01", "hello <!channel> *world*")
            .await
            .expect("posted");
        let requests = api.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["channel"], "D0ALICE01");
        assert_eq!(body["text"], "hello <!channel> *world*");
        assert_eq!(body["mrkdwn"], false);
        assert_eq!(body["parse"], "none");
        assert_eq!(body["unfurl_links"], false);
        assert_eq!(body["unfurl_media"], false);
    }

    #[tokio::test]
    async fn api_failures_map_to_redacted_codes_and_never_carry_a_token() {
        let cases: Vec<(ResponseTemplate, &str)> = vec![
            (
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok": false, "error": "invalid_auth"})),
                "invalid_auth",
            ),
            (
                ResponseTemplate::new(200)
                    .set_body_json(json!({"ok": false, "error": "Bad Value!"})),
                "unknown_error",
            ),
            (ResponseTemplate::new(429), "rate_limited"),
            (ResponseTemplate::new(500), "http_500"),
            (
                ResponseTemplate::new(200).set_body_string("<html>"),
                "response was not JSON",
            ),
            (
                ResponseTemplate::new(200).set_body_string("x".repeat(MAX_API_BODY_BYTES + 1)),
                "response too large",
            ),
        ];
        for (template, expected) in cases {
            let api = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(template)
                .mount(&api)
                .await;
            let transport = http_transport(&api);
            let open = transport.open_url().await.expect_err("bootstrap fails");
            let post = transport
                .post_message("D0ALICE01", "x")
                .await
                .expect_err("post fails");
            for error in [open, post] {
                assert_eq!(error.0, expected);
                assert!(!error.0.contains("sentinel"));
            }
        }
    }

    #[tokio::test]
    async fn an_ok_bootstrap_without_a_url_is_an_error() {
        let api = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
            .mount(&api)
            .await;
        let error = http_transport(&api).open_url().await.expect_err("no url");
        assert!(error.0.contains("no url"));
    }

    #[test]
    fn socket_urls_must_be_wss_on_a_slack_host_without_credentials() {
        let ok = parse_socket_url("wss://wss-primary.slack.com/link/?ticket=t", is_slack_host);
        assert_eq!(
            ok.unwrap(),
            (
                "wss-primary.slack.com".to_string(),
                443,
                "wss://wss-primary.slack.com/link/?ticket=t".to_string()
            )
        );
        for bad in [
            "ws://wss-primary.slack.com/link/",
            "https://wss-primary.slack.com/link/",
            "wss://slack.com/link/",
            "wss://evil.example/link/",
            "wss://slack.com.evil.example/link/",
            "wss://notslack.com/link/",
            "wss://user:pw@wss-primary.slack.com/link/",
            "wss:///link/",
            "not a url",
        ] {
            let error = parse_socket_url(bad, is_slack_host).expect_err(bad);
            assert!(!error.0.contains("ticket"), "{bad}");
        }
    }

    #[test]
    fn tokens_are_required_trimmed_and_redacted_in_debug() {
        assert!(SlackHttpTransport::new("  ".into(), BOT.into()).is_err());
        assert!(SlackHttpTransport::new(APP.into(), "".into()).is_err());
        let transport = SlackHttpTransport::new(format!(" {APP} "), format!("{BOT}\n")).unwrap();
        assert_eq!(transport.app_token.0, APP);
        assert_eq!(transport.bot_token.0, BOT);
        let debug = format!("{transport:?}");
        assert!(!debug.contains("sentinel"), "{debug}");
        assert!(debug.contains("redacted"));
    }

    // ── the real WebSocket path over loopback TLS ───────────────────────────

    struct Loopback {
        addr: std::net::SocketAddr,
        roots: rustls::RootCertStore,
        server: tokio::task::JoinHandle<Vec<String>>,
    }

    /// A TLS WebSocket server that says hello, then records the text frames the
    /// client sends until it closes.
    async fn loopback_server() -> Loopback {
        use tokio_rustls::TlsAcceptor;
        let cert = rcgen::generate_simple_self_signed(vec![
            "localhost".to_owned(),
            "127.0.0.1".to_owned(),
        ])
        .unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert.cert.der().clone()).unwrap();
        let key =
            rustls::pki_types::PrivateKeyDer::try_from(cert.signing_key.serialize_der()).unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let server_config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert.cert.der().clone()], key)
            .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(server_config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let tls = acceptor.accept(tcp).await.unwrap();
            let (_request, mut ws) = tokio_websockets::ServerBuilder::new()
                .accept(tls)
                .await
                .unwrap();
            ws.send(Message::text(r#"{"type":"hello"}"#.to_string()))
                .await
                .unwrap();
            let mut received = Vec::new();
            while let Some(Ok(message)) = ws.next().await {
                if let Some(text) = message.as_text() {
                    received.push(text.to_string());
                }
                if message.is_close() {
                    break;
                }
            }
            received
        });
        Loopback {
            addr,
            roots,
            server,
        }
    }

    async fn bootstrap_to(api: &MockServer, addr: std::net::SocketAddr) {
        Mock::given(method("POST"))
            .and(path("/apps.connections.open"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "url": format!("wss://127.0.0.1:{}/link/?ticket=t", addr.port()),
            })))
            .mount(api)
            .await;
    }

    #[tokio::test]
    async fn connect_completes_tls_and_the_upgrade_then_reads_and_acknowledges_frames() {
        let api = MockServer::start().await;
        let loopback = loopback_server().await;
        bootstrap_to(&api, loopback.addr).await;
        let transport = SlackHttpTransport::for_test(
            format!("{}/", api.uri()),
            config_trusting(loopback.roots.clone()),
            loopback_ok,
        );
        let mut socket = transport.connect().await.expect("connects");
        assert_eq!(
            socket.next_event().await.unwrap(),
            SocketEvent::Text(r#"{"type":"hello"}"#.into())
        );
        socket
            .send_text(r#"{"envelope_id":"env-1"}"#.into())
            .await
            .unwrap();
        drop(socket);
        let received = tokio::time::timeout(Duration::from_secs(10), loopback.server)
            .await
            .expect("server finished")
            .unwrap();
        assert_eq!(received, [r#"{"envelope_id":"env-1"}"#]);
    }

    #[tokio::test]
    async fn an_untrusted_certificate_is_refused_not_waved_through() {
        let api = MockServer::start().await;
        let loopback = loopback_server().await;
        bootstrap_to(&api, loopback.addr).await;
        let transport = SlackHttpTransport::for_test(
            format!("{}/", api.uri()),
            config_trusting(rustls::RootCertStore::empty()),
            loopback_ok,
        );
        let error = transport.connect().await.err().expect("verification fails");
        assert_eq!(error.0, "websocket tls handshake failed");
        loopback.server.abort();
    }

    #[tokio::test]
    async fn a_non_slack_host_is_refused_before_any_connection() {
        let api = MockServer::start().await;
        let loopback = loopback_server().await;
        bootstrap_to(&api, loopback.addr).await;
        // Production host policy: the loopback address is not a Slack host.
        let mut transport = SlackHttpTransport::for_test(
            format!("{}/", api.uri()),
            config_trusting(loopback.roots.clone()),
            loopback_ok,
        );
        transport.host_ok = is_slack_host;
        let error = transport.connect().await.err().expect("refused");
        assert!(error.0.contains("unusable url"), "{}", error.0);
        loopback.server.abort();
    }
}
