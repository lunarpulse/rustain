//! Loopback-only A2A HTTP server.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::adapters::rap::{AgentSigner, IdentityKeyStore};
use crate::domain::models::capability_registry::CapabilityRegistry;
use crate::domain::models::{AppConfig, RapTaskState};

use super::card::ServedAgentCard;
use super::client::is_json_content_type;
use super::jsonrpc::{
    CODE_INVALID_PARAMS, CODE_INVALID_REQUEST, CODE_METHOD_NOT_FOUND, CODE_PARSE_ERROR,
    CODE_TASK_NOT_FOUND, JsonRpcErrorResponse, JsonRpcInboundRequest, JsonRpcResponse,
};
use super::jws::sign_card;
use super::task::rap_to_wire;

const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const ACCEPTANCE_DISABLED: &str = "task acceptance not enabled in this build";

#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindDecision {
    Bind,
    RefuseWithReason(String),
}

/// Effect-free bind decision. DNS is deliberately not consulted: this cut only
/// accepts the literal loopback set plus the exact `localhost` hostname.
pub fn evaluate_bind_safety(addr: &str) -> BindDecision {
    let parsed = match url::Url::parse(&format!("http://{addr}")) {
        Ok(parsed)
            if parsed.port().is_some()
                && parsed.path() == "/"
                && parsed.query().is_none()
                && parsed.fragment().is_none()
                && parsed.username().is_empty()
                && parsed.password().is_none() =>
        {
            parsed
        }
        _ => {
            return BindDecision::RefuseWithReason(
                "A2A bind must be a loopback host and explicit port".to_owned(),
            );
        }
    };
    let is_loopback = match parsed.host() {
        Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    };
    if is_loopback {
        BindDecision::Bind
    } else {
        BindDecision::RefuseWithReason(
            "non-loopback A2A serving requires Story 18-1b TLS, authentication, and signed-identity admission"
                .to_owned(),
        )
    }
}

#[derive(Clone)]
struct ServerState {
    registry: Arc<CapabilityRegistry>,
    signer: Arc<AgentSigner>,
    endpoint_url: Arc<str>,
}

/// Serve an already-bound listener. Tests enter through this production
/// spawn-and-serve core so they exercise real HTTP framing without owning
/// process-global Ctrl-C handling.
///
/// Re-checks loopback on the bound socket. [`evaluate_bind_safety`] is an
/// effect-free decision over a *string*; this is the only place that can see
/// the address the kernel actually gave us, so it is where the loopback-only
/// invariant is enforced against hostname resolution and against any caller
/// that hands us a listener it bound itself.
pub async fn serve(
    listener: tokio::net::TcpListener,
    registry: Arc<CapabilityRegistry>,
    signer: AgentSigner,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let bound = listener.local_addr()?;
    anyhow::ensure!(
        bound.ip().to_canonical().is_loopback(),
        "refusing to serve A2A on non-loopback address {bound}: \
         non-loopback serving requires Story 18-1b TLS, authentication, and signed-identity admission"
    );
    let endpoint_url: Arc<str> = format!("http://{bound}").into();
    let state = ServerState {
        registry,
        signer: Arc::new(signer),
        endpoint_url,
    };
    let router = Router::new()
        .route("/.well-known/agent-card.json", get(serve_agent_card))
        .route("/", post(handle_jsonrpc))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
        // Outermost: `Router::layer` wraps what is already there, so this
        // deadline covers body streaming and extractor work. A timeout applied
        // inside the handler starts after `Bytes` has been collected and is
        // therefore useless against a slow-body client.
        .layer(axum::middleware::from_fn(enforce_request_deadline))
        .with_state(state);
    axum::serve(listener, router)
        .with_graceful_shutdown(cancel.cancelled_owned())
        .await?;
    Ok(())
}

async fn enforce_request_deadline(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    match tokio::time::timeout(REQUEST_TIMEOUT, next.run(request)).await {
        Ok(response) => response,
        Err(_) => json_response(
            &JsonRpcErrorResponse::new(
                serde_json::Value::Null,
                super::jsonrpc::CODE_INTERNAL_ERROR,
                "A2A request timed out",
            ),
            StatusCode::REQUEST_TIMEOUT,
        ),
    }
}

async fn serve_agent_card(State(state): State<ServerState>) -> Response {
    let card = ServedAgentCard::from_registry(&state.registry, &state.endpoint_url)
        .await
        .with_ownership(state.signer.identity().peer_id.to_string());
    match sign_card(&card, &state.signer) {
        Ok(raw) => (StatusCode::OK, [(CONTENT_TYPE, "application/json")], raw).into_response(),
        Err(error) => {
            tracing::error!(%error, "failed to sign served AgentCard");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn handle_jsonrpc(
    State(_state): State<ServerState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok());
    if !content_type.is_some_and(is_json_content_type) {
        return json_response(
            &JsonRpcErrorResponse::new(
                serde_json::Value::Null,
                CODE_INVALID_REQUEST,
                "Content-Type must be application/json or application/*+json",
            ),
            StatusCode::OK,
        );
    }

    match dispatch_jsonrpc(&body) {
        Ok(response) => json_response(&response, StatusCode::OK),
        Err(response) => json_response(&response, StatusCode::OK),
    }
}

fn dispatch_jsonrpc(body: &[u8]) -> Result<JsonRpcResponse, JsonRpcErrorResponse> {
    let value: serde_json::Value = serde_json::from_slice(body).map_err(|_| {
        JsonRpcErrorResponse::new(serde_json::Value::Null, CODE_PARSE_ERROR, "Parse error")
    })?;
    let request: JsonRpcInboundRequest = serde_json::from_value(value).map_err(|_| {
        JsonRpcErrorResponse::new(
            serde_json::Value::Null,
            CODE_INVALID_REQUEST,
            "Invalid Request",
        )
    })?;
    if request.jsonrpc != "2.0"
        || request.method.trim().is_empty()
        || !matches!(
            request.id,
            serde_json::Value::Number(_) | serde_json::Value::String(_)
        )
    {
        return Err(JsonRpcErrorResponse::new(
            serde_json::Value::Null,
            CODE_INVALID_REQUEST,
            "Invalid Request",
        ));
    }

    let id = request.id;
    let params = request.params.unwrap_or(serde_json::Value::Null);
    match request.method.as_str() {
        "message/send" => {
            if !is_well_formed_message(params.get("message")) {
                return Err(JsonRpcErrorResponse::new(
                    id,
                    CODE_INVALID_PARAMS,
                    "Invalid params",
                ));
            }
            let task_id = params
                .pointer("/message/messageId")
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| format!("rejected-{}", jsonrpc_id_text(&id)));
            Ok(JsonRpcResponse::new(
                id,
                serde_json::json!({
                    "id": task_id,
                    "status": {
                        "state": rap_to_wire(RapTaskState::Rejected),
                        "message": {
                            "role": "agent",
                            "parts": [{
                                "kind": "text",
                                "text": ACCEPTANCE_DISABLED,
                            }],
                        },
                    },
                }),
            ))
        }
        "tasks/get" | "tasks/cancel" => {
            if params
                .get("id")
                .and_then(serde_json::Value::as_str)
                .is_none_or(str::is_empty)
            {
                Err(JsonRpcErrorResponse::new(
                    id,
                    CODE_INVALID_PARAMS,
                    "Invalid params",
                ))
            } else {
                Err(JsonRpcErrorResponse::new(
                    id,
                    CODE_TASK_NOT_FOUND,
                    "Task not found",
                ))
            }
        }
        _ => Err(JsonRpcErrorResponse::new(
            id,
            CODE_METHOD_NOT_FOUND,
            "Method not found",
        )),
    }
}

fn jsonrpc_id_text(id: &serde_json::Value) -> String {
    match id {
        serde_json::Value::String(value) => value.clone(),
        serde_json::Value::Number(value) => value.to_string(),
        _ => "unknown".to_owned(),
    }
}

/// A2A `message/send` requires a message with a role and at least one part.
/// Without this, `{"message":{}}` would receive a fabricated `rejected` task
/// instead of `-32602`, and a client could not tell a malformed payload apart
/// from the acceptance-disabled policy verdict.
fn is_well_formed_message(message: Option<&serde_json::Value>) -> bool {
    let Some(message) = message.and_then(serde_json::Value::as_object) else {
        return false;
    };
    let role_ok = message
        .get("role")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|role| !role.trim().is_empty());
    let parts_ok = message
        .get("parts")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|parts| !parts.is_empty() && parts.iter().all(serde_json::Value::is_object));
    role_ok && parts_ok
}

fn json_response(value: &impl Serialize, status: StatusCode) -> Response {
    match serde_json::to_vec(value) {
        Ok(body) => (status, [(CONTENT_TYPE, "application/json")], body).into_response(),
        Err(error) => {
            tracing::error!(%error, "failed to serialize A2A JSON response");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Run the standalone A2A server front door.
pub async fn run(addr: String, app_config: AppConfig, workspace: PathBuf) -> anyhow::Result<()> {
    match evaluate_bind_safety(&addr) {
        BindDecision::Bind => {}
        BindDecision::RefuseWithReason(reason) => anyhow::bail!(reason),
    }

    let registry = Arc::new(CapabilityRegistry::new(None));
    // `SkillRegistry::discover` is blocking filesystem I/O and documents that
    // tokio callers MUST offload it (skill_registry.rs:79-81), as
    // event_loop.rs:813 and cli/catalog/mod.rs:119 already do.
    let discovered = {
        let home = dirs::home_dir();
        let disabled = app_config.skills.disabled.clone();
        tokio::task::spawn_blocking(move || {
            crate::adapters::skill_registry::SkillRegistry::discover(
                &workspace,
                home.as_deref(),
                &disabled,
            )
        })
        .await?
    };
    let activator = Arc::new(
        crate::adapters::skill_activation::SkillActivator::with_registry(Arc::new(
            tokio::sync::RwLock::new(discovered),
        )),
    );
    let provider = crate::adapters::skill_provider::SkillsProvider::new(activator);
    let _registrations = registry
        .discover_and_register_all(&provider, "skill")
        .await?;
    let signer =
        IdentityKeyStore::new(crate::infrastructure::paths::data_dir()?).load_or_generate()?;
    // `evaluate_bind_safety` cleared the *string*. Resolve it here, in the
    // effect shell, and bind a concrete address we have checked — binding the
    // raw hostname would let resolution disagree with the decision. `serve`
    // re-checks `local_addr` as the last line of defence.
    let candidates: Vec<std::net::SocketAddr> = tokio::net::lookup_host(&addr).await?.collect();
    anyhow::ensure!(
        !candidates.is_empty(),
        "A2A bind address {addr} resolved to no socket address"
    );
    let bind_addr = candidates
        .iter()
        .copied()
        .find(|candidate| candidate.ip().to_canonical().is_loopback())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "A2A bind address {addr} resolves only to non-loopback addresses ({candidates:?}); \
                 non-loopback serving requires Story 18-1b TLS, authentication, and signed-identity admission"
            )
        })?;
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    let cancel = CancellationToken::new();
    let server = serve(listener, registry, signer, cancel.child_token());
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => result,
        signal = tokio::signal::ctrl_c() => {
            signal?;
            cancel.cancel();
            server.await
        }
    }
}
