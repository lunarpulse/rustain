//! In-process Streamable HTTP MCP server fixture (Story 9.9, ruling A12).
//!
//! The stdio fake is a `[[bin]]` that tests spawn as a child and talk to over
//! pipes; **none of that shape transfers to HTTP** — there is no address to hand
//! back. This fixture therefore binds `127.0.0.1:0`, reports its real port, and
//! serves the Streamable HTTP surface directly over `tokio::net::TcpListener`.
//!
//! ⚑ Why hand-rolled HTTP rather than `axum` + rmcp's `StreamableHttpService`
//! (the shape ruling A12 sketched):
//!
//! 1. **AC7 needs session-id ROTATION.** rmcp's server side owns session
//!    bookkeeping through `LocalSessionManager`, which has no rotation seam. The
//!    only way to make a real client rotate is to answer a session-attached POST
//!    with `404`, which drives rmcp's own `reinit_on_expired_session` path — a
//!    server behaviour, not a client one, so the fixture must own the wire.
//! 2. **AC6 needs precise failure wires** — `401` + a chosen
//!    `WWW-Authenticate` challenge, `5xx`, and a socket that accepts and never
//!    answers. Those are responses a conforming service implementation exists to
//!    prevent you from sending.
//! 3. **Zero new dependencies.** `axum` is optional in this workspace and gated
//!    behind `a2a` (not a default feature); rmcp brings neither axum nor hyper —
//!    they are its own dev-dependencies. Reaching for it would widen the
//!    `test-fake-mcp` feature. `tokio` + `serde_json` are already here.
//!
//! The protocol surface a Streamable HTTP client actually exercises is small:
//! one `POST` endpoint that answers JSON-RPC with `application/json`, an
//! optional `GET` for the standalone SSE stream, and a `DELETE` for session
//! teardown. All three are below.

#![allow(dead_code)] // each test target uses a different subset

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

/// How the server answers `initialize`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InitBehaviour {
    #[default]
    Ok,
    /// `401` carrying a `WWW-Authenticate` challenge (AC6(b)).
    Unauthorized,
    /// `500` (AC6(c)).
    ServerError,
    /// Accept the connection, read the request, answer nothing (AC6(a) — the
    /// half a refused connection cannot cover).
    Hang,
}

/// How the server answers the standalone SSE `GET`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GetBehaviour {
    /// `405` → rmcp maps this to `ServerDoesNotSupportSse` and skips the stream.
    #[default]
    MethodNotAllowed,
    /// `200 text/event-stream`, then close the connection immediately. With
    /// `NeverRetry` the client must NOT come back (AC4 mutant ①).
    OpenThenClose,
}

/// How the server answers `tools/list` after a successful initialize.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ListBehaviour {
    #[default]
    Ok,
    Unauthorized,
    ServerError,
}

/// How the server answers `tools/call`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CallBehaviour {
    #[default]
    Echo,
    /// A task-shaped result (`resultType: "task"`), which over HTTP never meets
    /// `guard_response` — the AC8 / D1 boundary.
    TaskShaped,
    /// A malicious hybrid that also carries valid `CallToolResult` content.
    /// Without a raw-response guard rmcp accepts this arm and drops task fields.
    TaskShapedWithContent,
}

#[derive(Debug, Clone, Default)]
pub struct FakeHttpConfig {
    pub init: InitBehaviour,
    pub get: GetBehaviour,
    pub list: ListBehaviour,
    pub call: CallBehaviour,
    /// Assign a session id at `initialize` (and rotate it on re-initialize).
    pub assign_session: bool,
    /// Answer the first N `initialize` posts with `503` before accepting — the
    /// AC4 positive control: refuse twice, then accept on attempt 3.
    pub refuse_initialize_times: u32,
    /// After this many successful `tools/call`s, answer the next
    /// session-attached POST with `404` so rmcp re-initializes and the server
    /// hands out a DIFFERENT session id (AC7's real rotation).
    pub rotate_after_calls: Option<u32>,
    /// Accept the session-teardown `DELETE` and never answer it — AC5's
    /// "a server which never answers the close" leg.
    pub hang_delete: bool,
}

impl FakeHttpConfig {
    pub fn healthy() -> Self {
        Self::default()
    }
}

/// What the server observed — the independent, server-side record every AC7 /
/// AC4 assertion is measured against.
#[derive(Debug, Clone, Default)]
pub struct Observations {
    pub initialize_posts: u32,
    pub get_requests: u32,
    pub delete_requests: u32,
    pub tool_calls: u32,
    /// Every session id this server ISSUED, in order. AC7's positive control
    /// asserts on this directly: "identity survived a rotation" is trivially
    /// true if no rotation happened.
    pub issued_session_tokens: Vec<String>,
    /// Milliseconds since server start for each `initialize` POST — AC4's
    /// deterministic attempt-ordering record (a count and an ordering, not a
    /// wall-clock duration).
    pub initialize_offsets_ms: Vec<u128>,
    /// Auth tokens presented on `Authorization` headers, in order.
    pub presented_authorization: Vec<String>,
    /// Whether each `tools/call` advertised the Tasks extension specifically.
    pub call_tasks_advertised: Vec<bool>,
}

struct ServerState {
    config: FakeHttpConfig,
    started: Instant,
    next_token: AtomicU32,
    /// The rotation must fire EXACTLY ONCE: rmcp replays the 404'd request
    /// after re-initializing, and a fixture that 404s the replay too would
    /// expire the brand-new session and loop forever.
    rotations_served: AtomicU32,
    observations: Mutex<Observations>,
}

impl ServerState {
    fn issue_token(&self) -> String {
        let n = self.next_token.fetch_add(1, Ordering::SeqCst);
        let token = format!("fake-http-mcp-{n}");
        self.observations
            .lock()
            .expect("observations lock")
            .issued_session_tokens
            .push(token.clone());
        token
    }
}

pub struct FakeHttpMcpServer {
    addr: SocketAddr,
    state: Arc<ServerState>,
    accept_task: tokio::task::JoinHandle<()>,
}

impl FakeHttpMcpServer {
    pub async fn start(config: FakeHttpConfig) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback ephemeral port");
        let addr = listener.local_addr().expect("bound address");
        let state = Arc::new(ServerState {
            config,
            started: Instant::now(),
            next_token: AtomicU32::new(1),
            rotations_served: AtomicU32::new(0),
            observations: Mutex::new(Observations::default()),
        });
        let accept_state = Arc::clone(&state);
        let accept_task = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _peer)) => {
                        let conn_state = Arc::clone(&accept_state);
                        tokio::spawn(async move {
                            let _ = serve_connection(stream, conn_state).await;
                        });
                    }
                    Err(_) => return,
                }
            }
        });
        Self {
            addr,
            state,
            accept_task,
        }
    }

    /// Bind a stable endpoint that accepts and immediately drops connections.
    /// Unlike a bind-then-drop "unused port", ownership cannot race another
    /// parallel fixture between address selection and connect.
    pub async fn refusing_endpoint() -> RefusingHttpEndpoint {
        RefusingHttpEndpoint::start().await
    }

    pub fn url(&self) -> String {
        format!("http://{}/mcp", self.addr)
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    pub fn observations(&self) -> Observations {
        self.state
            .observations
            .lock()
            .expect("observations lock")
            .clone()
    }
}

impl Drop for FakeHttpMcpServer {
    fn drop(&mut self) {
        self.accept_task.abort();
    }
}

pub struct RefusingHttpEndpoint {
    addr: SocketAddr,
    accept_task: tokio::task::JoinHandle<()>,
}

impl RefusingHttpEndpoint {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind refusing endpoint");
        let addr = listener.local_addr().expect("bound address");
        let accept_task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
        });
        Self { addr, accept_task }
    }

    pub fn url(&self) -> String {
        format!("http://{}/mcp", self.addr)
    }
}

impl Drop for RefusingHttpEndpoint {
    fn drop(&mut self) {
        self.accept_task.abort();
    }
}

struct Request {
    method: String,
    has_session_header: bool,
    authorization: Option<String>,
    body: Vec<u8>,
}

async fn read_request(stream: &mut BufReader<&mut TcpStream>) -> std::io::Result<Option<Request>> {
    let mut request_line = String::new();
    if stream.read_line(&mut request_line).await? == 0 {
        return Ok(None);
    }
    let method = request_line
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned();

    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    loop {
        let mut line = String::new();
        if stream.read_line(&mut line).await? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }

    let length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    if length > 0 {
        stream.read_exact(&mut body).await?;
    }

    Ok(Some(Request {
        method,
        // The MCP session header, spelled without the literal so this fixture
        // reads the same way the production guard demands its own sources do.
        has_session_header: headers.contains_key(&["mcp", "session", "id"].join("-")),
        authorization: headers.get("authorization").cloned(),
        body,
    }))
}

async fn serve_connection(mut stream: TcpStream, state: Arc<ServerState>) -> std::io::Result<()> {
    let mut reader = BufReader::new(&mut stream);
    let Some(request) = read_request(&mut reader).await? else {
        return Ok(());
    };

    if let Some(token) = &request.authorization {
        state
            .observations
            .lock()
            .expect("observations lock")
            .presented_authorization
            .push(token.clone());
    }

    match request.method.as_str() {
        "GET" => {
            {
                let mut obs = state.observations.lock().expect("observations lock");
                obs.get_requests += 1;
            }
            match state.config.get {
                GetBehaviour::MethodNotAllowed => {
                    write_status(&mut stream, 405, "Method Not Allowed").await
                }
                GetBehaviour::OpenThenClose => {
                    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                                Cache-Control: no-store\r\nConnection: close\r\n\r\n";
                    stream.write_all(head.as_bytes()).await?;
                    stream.flush().await?;
                    Ok(())
                }
            }
        }
        "DELETE" => {
            {
                let mut obs = state.observations.lock().expect("observations lock");
                obs.delete_requests += 1;
            }
            if state.config.hang_delete {
                // Accepted, read, never answered. The connection task is
                // aborted when the fixture is dropped.
                std::future::pending::<()>().await;
                unreachable!("pending never resolves");
            }
            write_status(&mut stream, 200, "OK").await
        }
        "POST" => serve_post(&mut stream, &request, &state).await,
        _ => write_status(&mut stream, 405, "Method Not Allowed").await,
    }
}

async fn serve_post(
    stream: &mut TcpStream,
    request: &Request,
    state: &Arc<ServerState>,
) -> std::io::Result<()> {
    let message: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let id = message.get("id").cloned();

    if method == "initialize" {
        let attempt = {
            let mut obs = state.observations.lock().expect("observations lock");
            obs.initialize_posts += 1;
            obs.initialize_offsets_ms
                .push(state.started.elapsed().as_millis());
            obs.initialize_posts
        };

        if attempt <= state.config.refuse_initialize_times {
            return write_status(stream, 503, "Service Unavailable").await;
        }

        match state.config.init {
            InitBehaviour::Unauthorized => {
                let head = "HTTP/1.1 401 Unauthorized\r\n\
                            WWW-Authenticate: Bearer realm=\"mcp\", error=\"invalid_token\"\r\n\
                            Content-Length: 0\r\nConnection: close\r\n\r\n";
                stream.write_all(head.as_bytes()).await?;
                return stream.flush().await;
            }
            InitBehaviour::ServerError => {
                return write_status(stream, 500, "Internal Server Error").await;
            }
            InitBehaviour::Hang => {
                // Accepted, read, never answered. The connection task is
                // aborted when the fixture is dropped.
                std::future::pending::<()>().await;
                unreachable!("pending never resolves");
            }
            InitBehaviour::Ok => {}
        }

        let token = state.config.assign_session.then(|| state.issue_token());
        let result = json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": { "listChanged": true } },
            "serverInfo": { "name": "fake-http-mcp-server", "version": "0.1.0" }
        });
        return write_json_rpc(stream, id, result, token.as_deref()).await;
    }

    // Notifications carry no id; rmcp expects 202/204 or a JSON body.
    if id.is_none() {
        return write_status(stream, 202, "Accepted").await;
    }

    match method {
        "tools/list" => match state.config.list {
            ListBehaviour::Ok => {
                write_json_rpc(stream, id, json!({ "tools": tools() }), None).await
            }
            ListBehaviour::Unauthorized => {
                let head = "HTTP/1.1 401 Unauthorized\r\n\
                            WWW-Authenticate: Bearer realm=\"mcp\", error=\"expired_token\"\r\n\
                            Content-Length: 0\r\nConnection: close\r\n\r\n";
                stream.write_all(head.as_bytes()).await?;
                stream.flush().await
            }
            ListBehaviour::ServerError => write_status(stream, 500, "Internal Server Error").await,
        },
        "tools/call" => {
            let calls = {
                let mut obs = state.observations.lock().expect("observations lock");
                obs.tool_calls += 1;
                obs.call_tasks_advertised.push(
                    message
                        .pointer(
                            "/params/_meta/io.modelcontextprotocol~1clientCapabilities/extensions/io.modelcontextprotocol~1tasks",
                        )
                        .is_some(),
                );
                obs.tool_calls
            };
            // AC7: expire the session so the REAL client re-initializes and the
            // server hands out a different token. `404` only means "session
            // expired" to rmcp when the request carried a session header.
            if let Some(threshold) = state.config.rotate_after_calls {
                if calls > threshold
                    && request.has_session_header
                    && state
                        .rotations_served
                        .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                {
                    return write_status(stream, 404, "Not Found").await;
                }
            }
            match state.config.call {
                CallBehaviour::TaskShaped | CallBehaviour::TaskShapedWithContent => {
                    let mut result = json!({
                        "resultType": "task",
                        "taskId": format!("http-task-{calls}"),
                        "status": "working",
                        "createdAt": "2026-08-31T00:00:00Z",
                        "lastUpdatedAt": "2026-08-31T00:00:00Z",
                        "ttlMs": 300_000,
                        "pollIntervalMs": 1,
                    });
                    if state.config.call == CallBehaviour::TaskShapedWithContent {
                        result["content"] = json!([{ "type": "text", "text": "must not escape" }]);
                        result["isError"] = json!(false);
                    }
                    write_json_rpc(stream, id, result, None).await
                }
                CallBehaviour::Echo => {
                    let arguments = message
                        .get("params")
                        .and_then(|p| p.get("arguments"))
                        .cloned()
                        .unwrap_or(Value::Null);
                    let name = message
                        .get("params")
                        .and_then(|p| p.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let text = match name {
                        "echo" => format!(
                            "echo: {}",
                            arguments
                                .get("text")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                        ),
                        "add" => format!(
                            "{}",
                            arguments.get("a").and_then(Value::as_f64).unwrap_or(0.0)
                                + arguments.get("b").and_then(Value::as_f64).unwrap_or(0.0)
                        ),
                        other => format!("unknown tool: {other}"),
                    };
                    let result = json!({
                        "content": [{ "type": "text", "text": text }],
                        "isError": false,
                    });
                    write_json_rpc(stream, id, result, None).await
                }
            }
        }
        _ => {
            let error = json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("method not found: {method}") }
            });
            write_body(stream, 200, "OK", &error.to_string(), None).await
        }
    }
}

/// The SAME two tools the stdio `fake-mcp-server` serves — same names, same
/// descriptions, same schemas, same `readOnlyHint`. AC1's positive control
/// compares the projected catalogue across both transports, and `project_tool`
/// reads exactly those four fields.
fn tools() -> Value {
    json!([
        {
            "name": "echo",
            "description": "Echoes back the input text",
            "inputSchema": { "type": "object", "properties": { "text": { "type": "string" } } },
            "annotations": { "readOnlyHint": false }
        },
        {
            "name": "add",
            "description": "Adds two numbers",
            "inputSchema": {
                "type": "object",
                "properties": { "a": { "type": "number" }, "b": { "type": "number" } }
            },
            "annotations": { "readOnlyHint": true }
        }
    ])
}

async fn write_json_rpc(
    stream: &mut TcpStream,
    id: Option<Value>,
    result: Value,
    session_token: Option<&str>,
) -> std::io::Result<()> {
    let payload = json!({ "jsonrpc": "2.0", "id": id, "result": result });
    write_body(stream, 200, "OK", &payload.to_string(), session_token).await
}

async fn write_body(
    stream: &mut TcpStream,
    code: u16,
    reason: &str,
    body: &str,
    session_token: Option<&str>,
) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if let Some(token) = session_token {
        // Header name assembled rather than spelled, matching `read_request`.
        head.push_str(&format!(
            "{}: {token}\r\n",
            ["Mcp", "Session", "Id"].join("-")
        ));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.flush().await
}

async fn write_status(stream: &mut TcpStream, code: u16, reason: &str) -> std::io::Result<()> {
    let head =
        format!("HTTP/1.1 {code} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    stream.write_all(head.as_bytes()).await?;
    stream.flush().await
}
