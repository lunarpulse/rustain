//! Subprocess harness for tests that drive the real `rustain` binary against a
//! local Anthropic `/v1/messages` stub (Story C-2 AC4; reused by C-3).
//!
//! Isolation rules (they are the reason this module exists):
//! * the child runs with `env_clear()` plus an explicit minimal env, so the
//!   developer's `RUSTAIN_PROFILE`, API keys and proxies never leak in;
//! * the test process never mutates its own environment (`std::env::set_var`);
//! * every spawned child is wrapped in [`ChildGuard`], so a failing assertion
//!   cannot leak a daemon;
//! * every wait is a condition poll with a deadline; there is no fixed sleep
//!   standing in for synchronisation (`POLL` below is only the poll cadence).
//!
//! Pieces: [`Stub`] (scriptable, recording, holdable Anthropic server), [`Env`]
//! (short-path temp dirs + the child environment), [`Proc`] (a guarded child
//! with line-captured stdout/stderr and an optional open stdin), and the
//! wrappers [`DaemonProc`], [`PlainAttach`] and [`AcpProc`].

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

use super::process::ChildGuard;

/// Poll cadence of every deadline wait below (not a synchronisation delay).
const POLL: Duration = Duration::from_millis(10);

/// The API key every child gets (`ANTHROPIC_API_KEY`).
pub const STUB_API_KEY: &str = "sk-ant-c2-stub";

/// Text the stub streams when no reply was scripted for a request index.
pub const DEFAULT_REPLY: &str = "C2-STUB-REPLY";

/// Poll `probe` until it yields `Some`, or `None` once `deadline` has passed.
pub async fn poll_until_opt<T>(
    deadline: Duration,
    mut probe: impl FnMut() -> Option<T>,
) -> Option<T> {
    let end = Instant::now() + deadline;
    loop {
        if let Some(found) = probe() {
            return Some(found);
        }
        if Instant::now() >= end {
            return None;
        }
        tokio::time::sleep(POLL).await;
    }
}

/// [`poll_until_opt`] that panics naming `what` on timeout.
pub async fn poll_until<T>(deadline: Duration, what: &str, probe: impl FnMut() -> Option<T>) -> T {
    match poll_until_opt(deadline, probe).await {
        Some(found) => found,
        None => panic!("timed out after {deadline:?} waiting for {what}"),
    }
}

// ───────────────────────────── Anthropic stub ─────────────────────────────

fn sse_event(value: Value) -> String {
    format!(
        "event: {}\ndata: {value}\n\n",
        value["type"].as_str().expect("SSE event type")
    )
}

/// One scripted `/v1/messages` answer (a complete Anthropic SSE stream),
/// optionally held until the test releases it.
#[derive(Clone)]
pub struct Reply {
    sse: String,
    hold: Option<Arc<Notify>>,
}

/// Releases a [`Reply`] that was built with [`Reply::held`].
pub struct Release(Arc<Notify>);

impl Release {
    /// Let the held response go. Safe to call before the stub is waiting: the
    /// permit is stored.
    pub fn release(&self) {
        self.0.notify_one();
    }
}

impl Reply {
    /// A plain assistant text reply that ends the turn.
    pub fn text(text: &str) -> Self {
        let sse = [
            sse_event(json!({"type":"message_start","message":{"id":"msg_c2","type":"message","role":"assistant","model":"c2-stub","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":0}}})),
            sse_event(json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})),
            sse_event(json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}})),
            sse_event(json!({"type":"content_block_stop","index":0})),
            sse_event(json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}})),
            sse_event(json!({"type":"message_stop"})),
        ]
        .concat();
        Self { sse, hold: None }
    }

    /// A reply that asks for one tool call (`stop_reason: tool_use`).
    pub fn tool_use(id: &str, name: &str, input: &Value) -> Self {
        let sse = [
            sse_event(json!({"type":"message_start","message":{"id":"msg_c2","type":"message","role":"assistant","model":"c2-stub","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":0}}})),
            sse_event(json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":id,"name":name,"input":{}}})),
            sse_event(json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":input.to_string()}})),
            sse_event(json!({"type":"content_block_stop","index":0})),
            sse_event(json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":20}})),
            sse_event(json!({"type":"message_stop"})),
        ]
        .concat();
        Self { sse, hold: None }
    }

    /// Hold this reply: the stub records the request at once but sends no
    /// response byte until the returned [`Release`] fires.
    pub fn held(mut self) -> (Self, Release) {
        let gate = Arc::new(Notify::new());
        self.hold = Some(gate.clone());
        (self, Release(gate))
    }
}

struct StubState {
    requests: Mutex<Vec<Value>>,
    scripts: Mutex<HashMap<usize, Reply>>,
    probes: AtomicUsize,
}

/// A local Anthropic server for child processes.
///
/// * `POST /v1/messages` with `"stream": true` is a **model turn**: the body is
///   recorded (in arrival order) and the reply scripted for that index (or
///   [`DEFAULT_REPLY`]) is streamed. Non-streaming POSTs are connectivity
///   probes: answered with an empty message, counted in [`Stub::probes`], and
///   never recorded, so "zero requests" means "no model turn".
/// * `GET /v1/models` answers one model; `GET /api.json` (models.dev) answers
///   `{}`; anything else is 404.
pub struct Stub {
    url: String,
    state: Arc<StubState>,
    task: tokio::task::JoinHandle<()>,
}

impl Stub {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind Anthropic stub");
        let url = format!("http://{}", listener.local_addr().expect("stub address"));
        let state = Arc::new(StubState {
            requests: Mutex::new(Vec::new()),
            scripts: Mutex::new(HashMap::new()),
            probes: AtomicUsize::new(0),
        });
        let shared = state.clone();
        let task = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                tokio::spawn(serve_connection(socket, shared.clone()));
            }
        });
        Self { url, state, task }
    }

    /// Base URL for `ANTHROPIC_BASE_URL` / `RUSTAIN_MODELS_DEV_URL`.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Script the reply for the model turn with this zero-based arrival index.
    pub fn script(&self, index: usize, reply: Reply) {
        self.state.scripts.lock().unwrap().insert(index, reply);
    }

    /// Every recorded model-turn request body, in arrival order.
    pub fn requests(&self) -> Vec<Value> {
        self.state.requests.lock().unwrap().clone()
    }

    /// Number of non-streaming `/v1/messages` probes seen.
    pub fn probes(&self) -> usize {
        self.state.probes.load(Ordering::SeqCst)
    }

    /// Wait until at least `n` model-turn requests were recorded; returns all
    /// recorded so far. Panics on timeout.
    pub async fn wait_for_requests(&self, n: usize, deadline: Duration) -> Vec<Value> {
        poll_until(deadline, &format!("{n} stub request(s)"), || {
            let seen = self.requests();
            (seen.len() >= n).then_some(seen)
        })
        .await
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The request's `system` prompt as text: the string, or every text block of the
/// array form concatenated; empty when the field is absent.
pub fn system_text(request: &Value) -> String {
    match request.get("system") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect(),
        Some(other) => panic!("unexpected `system` shape in request: {other}"),
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Decode a complete chunked body, or `None` while it is still incomplete.
fn decode_chunked(raw: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut pos = 0;
    loop {
        let line_end = find(&raw[pos..], b"\r\n")?;
        let size_text = std::str::from_utf8(&raw[pos..pos + line_end]).ok()?;
        let size = usize::from_str_radix(size_text.split(';').next()?.trim(), 16).ok()?;
        pos += line_end + 2;
        if size == 0 {
            return Some(out);
        }
        if raw.len() < pos + size + 2 {
            return None;
        }
        out.extend_from_slice(&raw[pos..pos + size]);
        pos += size + 2;
    }
}

/// Read one HTTP/1.1 request: `(method, path, body)`.
async fn read_request(socket: &mut TcpStream) -> Option<(String, String, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        if let Some(end) = find(&buf, b"\r\n\r\n") {
            break end;
        }
        let n = socket.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next()?.split_whitespace();
    let method = request_line.next()?.to_owned();
    let path = request_line.next()?.to_owned();
    let mut length = 0usize;
    let mut chunked = false;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim();
            if name == "content-length" {
                length = value.parse().ok()?;
            } else if name == "transfer-encoding" && value.to_ascii_lowercase().contains("chunked")
            {
                chunked = true;
            }
        }
    }
    let mut body = buf[head_end + 4..].to_vec();
    if chunked {
        loop {
            if let Some(decoded) = decode_chunked(&body) {
                return Some((method, path, decoded));
            }
            let n = socket.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            body.extend_from_slice(&chunk[..n]);
        }
    }
    while body.len() < length {
        let n = socket.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(length);
    Some((method, path, body))
}

async fn respond(socket: &mut TcpStream, status: &str, content_type: &str, body: &str) {
    let head = format!(
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    let _ = socket.write_all(head.as_bytes()).await;
    let _ = socket.write_all(body.as_bytes()).await;
    let _ = socket.shutdown().await;
}

async fn serve_connection(mut socket: TcpStream, state: Arc<StubState>) {
    let Some((method, path, body)) = read_request(&mut socket).await else {
        return;
    };
    let route = path.split('?').next().unwrap_or("");
    match (method.as_str(), route) {
        ("POST", "/v1/messages") => {
            let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            if request.get("stream").and_then(Value::as_bool) != Some(true) {
                state.probes.fetch_add(1, Ordering::SeqCst);
                let probe = json!({"id":"probe","type":"message","role":"assistant","content":[],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":0}});
                respond(
                    &mut socket,
                    "200 OK",
                    "application/json",
                    &probe.to_string(),
                )
                .await;
                return;
            }
            let reply = {
                let mut requests = state.requests.lock().unwrap();
                let index = requests.len();
                requests.push(request);
                state
                    .scripts
                    .lock()
                    .unwrap()
                    .remove(&index)
                    .unwrap_or_else(|| Reply::text(DEFAULT_REPLY))
            };
            if let Some(gate) = &reply.hold {
                gate.notified().await;
            }
            respond(&mut socket, "200 OK", "text/event-stream", &reply.sse).await;
        }
        ("GET", "/v1/models") => {
            let models = json!({"data":[{"id":"claude-sonnet-4-6","type":"model","display_name":"C2 stub"}],"has_more":false});
            respond(
                &mut socket,
                "200 OK",
                "application/json",
                &models.to_string(),
            )
            .await;
        }
        ("GET", "/api.json") => respond(&mut socket, "200 OK", "application/json", "{}").await,
        _ => respond(&mut socket, "404 Not Found", "application/json", "{}").await,
    }
}

// ───────────────────────────── environment ─────────────────────────────

/// Isolated directories plus the child environment for one test.
///
/// The root lives directly under `/tmp` with a short prefix so the daemon's
/// Unix socket path (`<data>/daemons/<hash>.sock`) stays below the 108-byte
/// `sun_path` limit. It is canonicalised so paths in child diagnostics match.
pub struct Env {
    _root: tempfile::TempDir,
    pub workspace: PathBuf,
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub home: PathBuf,
    pub log_path: PathBuf,
    stub_url: String,
}

impl Env {
    pub fn new(stub: &Stub) -> Self {
        let root = tempfile::Builder::new()
            .prefix("c2-")
            .tempdir_in("/tmp")
            .expect("temp root under /tmp");
        let base = root.path().canonicalize().expect("canonical temp root");
        let make = |name: &str| {
            let dir = base.join(name);
            std::fs::create_dir_all(&dir).expect("create env dir");
            dir
        };
        Self {
            workspace: make("ws"),
            config_dir: make("cfg"),
            data_dir: make("data"),
            home: make("home"),
            log_path: base.join("product.log"),
            _root: root,
            stub_url: stub.url().to_owned(),
        }
    }

    /// `<config_dir>/personas`
    pub fn personas_dir(&self) -> PathBuf {
        self.config_dir.join("personas")
    }

    /// `<config_dir>/personas/<file>`
    pub fn persona_path(&self, file: &str) -> PathBuf {
        self.personas_dir().join(file)
    }

    /// Write `<config_dir>/personas/<file>` with exactly `body`.
    pub fn write_persona(&self, file: &str, body: &str) -> PathBuf {
        let path = self.persona_path(file);
        std::fs::create_dir_all(path.parent().unwrap()).expect("create personas dir");
        std::fs::write(&path, body).expect("write persona file");
        path
    }

    /// Write `<config_dir>/profiles/<name>.toml`.
    pub fn write_profile(&self, name: &str, toml: &str) {
        let dir = self.config_dir.join("profiles");
        std::fs::create_dir_all(&dir).expect("create profiles dir");
        std::fs::write(dir.join(format!("{name}.toml")), toml).expect("write profile");
    }

    /// The workspace-scoped daemon PID file (`<ws>/.rustain/daemon.pid`).
    pub fn pid_path(&self) -> PathBuf {
        self.workspace.join(".rustain").join("daemon.pid")
    }

    fn apply(&self, command: &mut Command) {
        command
            .env_clear()
            .current_dir(&self.workspace)
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_DATA_HOME", self.home.join(".local/share"))
            .env("TERM", "xterm-256color")
            .env("NO_COLOR", "1")
            .env("NO_PROXY", "*")
            .env("no_proxy", "*")
            .env("RUSTAIN_CONFIG_DIR", &self.config_dir)
            .env("RUSTAIN_DATA_DIR", &self.data_dir)
            .env("RUSTAIN_LOG_PATH", &self.log_path)
            .env("ANTHROPIC_API_KEY", STUB_API_KEY)
            .env("ANTHROPIC_BASE_URL", &self.stub_url)
            .env("RUSTAIN_MODELS_DEV_URL", &self.stub_url);
    }

    /// A `rustain` invocation with the isolated environment, cwd = workspace,
    /// stdin null and stdout/stderr piped (override before spawning if needed).
    pub fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rustain"));
        command.args(args);
        self.apply(&mut command);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    /// Spawn `rustain <args>` as a guarded [`Proc`]; `stdin_open` pipes stdin
    /// and keeps it open until [`Proc::close_stdin`].
    pub fn spawn(&self, args: &[&str], stdin_open: bool) -> Proc {
        let mut command = self.command(args);
        if stdin_open {
            command.stdin(Stdio::piped());
        }
        Proc::new(command.spawn().expect("spawn rustain"))
    }

    /// Run `rustain <args>` to completion within `deadline`.
    pub async fn run(&self, args: &[&str], deadline: Duration) -> Finished {
        let mut proc = self.spawn(args, false);
        proc.finish(deadline).await
    }

    pub fn spawn_daemon_foreground(&self, profile: &str) -> DaemonProc {
        DaemonProc {
            proc: self.spawn(
                &["--profile", profile, "daemon", "start", "--foreground"],
                false,
            ),
        }
    }

    /// `rustain daemon stop`.
    pub async fn daemon_stop(&self) -> Finished {
        self.run(&["daemon", "stop"], Duration::from_secs(20)).await
    }
}

/// `[persona]` block text for a profile named `name`: the six non-persona
/// dimensions are those of the built-in `coding` profile (on `base`).
pub fn profile_toml(name: &str, persona_adapter: &str, persona_file: Option<&str>) -> String {
    let config = persona_file
        .map(|file| format!("config = {{ file = {file:?} }}\n"))
        .unwrap_or_default();
    format!(
        "name = {name:?}\n\
         [persona]\nadapter = {persona_adapter:?}\n{config}\
         [memory]\nadapter = \"project-scoped\"\n\
         [session]\nadapter = \"workspace\"\n\
         [tools]\nadapter = \"composite\"\n\
         [channels]\nadapter = \"terminal\"\n\
         [scheduler]\nadapter = \"none\"\n\
         [context]\nadapter = \"default\"\n"
    )
}

// ───────────────────────────── processes ─────────────────────────────

#[derive(Default)]
struct BufState {
    lines: Vec<String>,
    closed: bool,
}

/// A child pipe drained on a thread into complete lines; cloneable view.
#[derive(Clone, Default)]
pub struct LineBuf(Arc<Mutex<BufState>>);

impl LineBuf {
    fn pump<R: std::io::Read + Send + 'static>(reader: R) -> Self {
        let buf = Self::default();
        let sink = buf.clone();
        std::thread::spawn(move || {
            let mut reader = std::io::BufReader::new(reader);
            let mut raw = Vec::new();
            loop {
                raw.clear();
                match reader.read_until(b'\n', &mut raw) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let line = String::from_utf8_lossy(&raw)
                            .trim_end_matches(['\n', '\r'])
                            .to_owned();
                        sink.0.lock().unwrap().lines.push(line);
                    }
                }
            }
            sink.0.lock().unwrap().closed = true;
        });
        buf
    }

    /// An already-closed empty buffer (for a pipe that was not requested).
    fn empty() -> Self {
        let buf = Self::default();
        buf.0.lock().unwrap().closed = true;
        buf
    }

    pub fn lines(&self) -> Vec<String> {
        self.0.lock().unwrap().lines.clone()
    }

    pub fn text(&self) -> String {
        self.lines().join("\n")
    }

    pub fn is_closed(&self) -> bool {
        self.0.lock().unwrap().closed
    }

    /// First line (from the start of the capture) satisfying `pred`, waiting up
    /// to `deadline`; `None` on timeout.
    pub async fn wait_for_line(
        &self,
        pred: impl Fn(&str) -> bool,
        deadline: Duration,
    ) -> Option<String> {
        poll_until_opt(deadline, || self.lines().into_iter().find(|l| pred(l))).await
    }
}

/// What a finished child left behind.
#[derive(Debug)]
pub struct Finished {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

impl Finished {
    pub fn code(&self) -> Option<i32> {
        self.status.code()
    }
}

/// A guarded child with line-captured output and optionally open stdin.
pub struct Proc {
    pub child: ChildGuard,
    pub stdin: Option<ChildStdin>,
    pub stdout: LineBuf,
    pub stderr: LineBuf,
}

impl Proc {
    pub fn new(mut child: Child) -> Self {
        let stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .map_or_else(LineBuf::empty, LineBuf::pump);
        let stderr = child
            .stderr
            .take()
            .map_or_else(LineBuf::empty, LineBuf::pump);
        Self {
            child: ChildGuard::new(child),
            stdin,
            stdout,
            stderr,
        }
    }

    pub fn id(&self) -> u32 {
        self.child.id()
    }

    /// Exit status if the child has already exited.
    pub fn try_exit(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().expect("poll child exit")
    }

    /// Captured output so far, for failure messages.
    pub fn diagnostics(&self) -> String {
        format!(
            "--- stdout ---\n{}\n--- stderr ---\n{}",
            self.stdout.text(),
            self.stderr.text()
        )
    }

    /// Wait for exit; `None` on timeout (the child is left running).
    pub async fn wait_exit_opt(&mut self, deadline: Duration) -> Option<ExitStatus> {
        let end = Instant::now() + deadline;
        loop {
            if let Some(status) = self.try_exit() {
                return Some(status);
            }
            if Instant::now() >= end {
                return None;
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Wait for exit; panics with the captured output on timeout.
    pub async fn wait_exit(&mut self, deadline: Duration) -> ExitStatus {
        match self.wait_exit_opt(deadline).await {
            Some(status) => status,
            None => panic!(
                "child {} did not exit within {deadline:?}\n{}",
                self.id(),
                self.diagnostics()
            ),
        }
    }

    /// Wait for exit and for both pipes to reach EOF, then return everything.
    pub async fn finish(&mut self, deadline: Duration) -> Finished {
        let status = self.wait_exit(deadline).await;
        let (out, err) = (self.stdout.clone(), self.stderr.clone());
        poll_until(Duration::from_secs(10), "child pipes to reach EOF", || {
            (out.is_closed() && err.is_closed()).then_some(())
        })
        .await;
        Finished {
            status,
            stdout: out.text(),
            stderr: err.text(),
        }
    }

    /// Write one line to the child's stdin and flush; stdin stays open.
    pub fn send_line(&mut self, line: &str) -> std::io::Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| std::io::Error::other("stdin is closed or was never piped"))?;
        stdin.write_all(line.as_bytes())?;
        stdin.write_all(b"\n")?;
        stdin.flush()
    }

    /// Close stdin (EOF for the child).
    pub fn close_stdin(&mut self) {
        self.stdin = None;
    }
}

/// How a foreground daemon's startup resolved.
pub enum Settled {
    /// The PID file (the readiness marker) carries the daemon's own PID.
    Ready,
    /// The process exited first.
    Exited(ExitStatus),
}

/// `rustain --profile <p> daemon start --foreground`.
pub struct DaemonProc {
    pub proc: Proc,
}

impl DaemonProc {
    fn pid_matches(&self, env: &Env) -> bool {
        let Ok(text) = std::fs::read_to_string(env.pid_path()) else {
            return false;
        };
        text.lines()
            .filter_map(|line| line.strip_prefix("pid = "))
            .any(|pid| pid.trim().parse::<u32>().ok() == Some(self.proc.id()))
    }

    /// Wait until the daemon is ready or has exited. Panics on timeout.
    pub async fn settle(&mut self, env: &Env, deadline: Duration) -> Settled {
        let end = Instant::now() + deadline;
        loop {
            if let Some(status) = self.proc.try_exit() {
                return Settled::Exited(status);
            }
            if self.pid_matches(env) {
                return Settled::Ready;
            }
            assert!(
                Instant::now() < end,
                "daemon neither became ready nor exited within {deadline:?}\n{}",
                self.proc.diagnostics()
            );
            tokio::time::sleep(POLL).await;
        }
    }

    /// Wait for readiness; panics if the daemon exits instead.
    pub async fn wait_ready(&mut self, env: &Env, deadline: Duration) {
        match self.settle(env, deadline).await {
            Settled::Ready => {}
            Settled::Exited(status) => panic!(
                "daemon exited ({status}) before becoming ready\n{}",
                self.proc.diagnostics()
            ),
        }
    }
}

/// Spawn a foreground daemon and wait until it is ready.
pub async fn spawn_daemon_ready(env: &Env, profile: &str, deadline: Duration) -> DaemonProc {
    let mut daemon = env.spawn_daemon_foreground(profile);
    daemon.wait_ready(env, deadline).await;
    daemon
}

/// `rustain daemon attach --plain` with stdin held open (EOF detaches at once,
/// `attach_client.rs`), so the reply can still stream back after the line.
pub struct PlainAttach {
    pub proc: Proc,
}

impl PlainAttach {
    pub fn spawn(env: &Env) -> Self {
        Self {
            proc: env.spawn(&["daemon", "attach", "--plain"], true),
        }
    }

    /// Send one input line; stdin stays open.
    pub fn send_line(&mut self, line: &str) {
        self.proc.send_line(line).expect("write attach stdin");
    }

    /// First stdout line satisfying `pred`; `None` on timeout.
    pub async fn wait_for_line(
        &self,
        pred: impl Fn(&str) -> bool,
        deadline: Duration,
    ) -> Option<String> {
        self.proc.stdout.wait_for_line(pred, deadline).await
    }

    /// Detach: close stdin.
    pub fn close_stdin(&mut self) {
        self.proc.close_stdin();
    }
}

/// `rustain [args] acp`: JSON-RPC, one object per line, over the child's stdio.
pub struct AcpProc {
    pub proc: Proc,
    next_id: u64,
}

impl AcpProc {
    /// `extra` are global flags placed before the `acp` subcommand.
    pub fn spawn(env: &Env, extra: &[&str]) -> Self {
        let mut args: Vec<&str> = extra.to_vec();
        args.push("acp");
        Self {
            proc: env.spawn(&args, true),
            next_id: 1,
        }
    }

    /// Send a JSON-RPC request; returns its id. An `Err` (broken pipe) means the
    /// child is already gone.
    pub fn request(&mut self, method: &str, params: Value) -> std::io::Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        let message = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        self.proc.send_line(&message.to_string())?;
        Ok(id)
    }

    /// The response (`result` or `error`) to request `id`; panics on timeout.
    pub async fn wait_response(&self, id: u64, deadline: Duration) -> Value {
        let line = self
            .proc
            .stdout
            .wait_for_line(
                |line| {
                    serde_json::from_str::<Value>(line).is_ok_and(|v| {
                        v.get("id") == Some(&json!(id))
                            && v.get("method").is_none()
                            && (v.get("result").is_some() || v.get("error").is_some())
                    })
                },
                deadline,
            )
            .await
            .unwrap_or_else(|| {
                panic!(
                    "no ACP response to request {id} within {deadline:?}\n{}",
                    self.proc.diagnostics()
                )
            });
        serde_json::from_str(&line).expect("ACP response is JSON")
    }

    pub async fn initialize(&mut self, deadline: Duration) -> Value {
        let id = self
            .request(
                "initialize",
                json!({"protocolVersion":1,"clientCapabilities":{}}),
            )
            .expect("write initialize");
        self.wait_response(id, deadline).await
    }

    /// `session/new` with no MCP servers; returns the session id.
    pub async fn new_session(&mut self, cwd: &Path, deadline: Duration) -> String {
        let id = self
            .request("session/new", json!({"cwd": cwd, "mcpServers": []}))
            .expect("write session/new");
        let response = self.wait_response(id, deadline).await;
        response["result"]["sessionId"]
            .as_str()
            .unwrap_or_else(|| panic!("session/new returned no sessionId: {response}"))
            .to_owned()
    }

    /// `session/prompt` with one text block; returns the final response.
    pub async fn prompt(&mut self, session_id: &str, text: &str, deadline: Duration) -> Value {
        let id = self
            .request(
                "session/prompt",
                json!({"sessionId": session_id, "prompt": [{"type":"text","text":text}]}),
            )
            .expect("write session/prompt");
        self.wait_response(id, deadline).await
    }
}
