use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use super::inherited::{Host, Journal};
use axum::{
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{Method, StatusCode},
    response::Response,
};
use rustain::{
    adapters::a2a::admission::A2aAdmissionPolicy,
    domain::models::{A2aPeerSource, A2aPeerSpec, RedactedUrl, ResponseMode},
};
use std::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// Snapshot of the actual POST headers and body before forwarding.
pub(crate) type CapturedPost = axum::http::Request<Vec<u8>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HttpCounts {
    pub(crate) get: usize,
    pub(crate) post: usize,
}

#[derive(Default)]
struct Counters {
    get: AtomicUsize,
    post: AtomicUsize,
    posts: Mutex<Vec<CapturedPost>>,
}

#[derive(Clone)]
struct Proxy {
    upstream: String,
    client: reqwest::Client,
    counters: Arc<Counters>,
}

/// Counts the actual HTTP boundary while forwarding unchanged requests to the
/// production signed-card, admission, policy, runtime and journal pipeline.
pub(crate) struct ObservedHost {
    host: Host,
    endpoint: String,
    counters: Arc<Counters>,
    cancel: CancellationToken,
    proxy: tokio::task::JoinHandle<Result<(), std::io::Error>>,
}

impl ObservedHost {
    pub(crate) async fn start(policy: A2aAdmissionPolicy, mode: ResponseMode) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("observer bind");
        let authority = listener
            .local_addr()
            .expect("observer authority")
            .to_string();
        let host = Host::start_at_authority(policy, mode, Some(authority.clone())).await;
        let endpoint = format!("http://{authority}/");
        let counters = Arc::new(Counters::default());
        let app = axum::Router::new().fallback(forward).with_state(Proxy {
            upstream: host.endpoint().trim_end_matches('/').to_owned(),
            client: reqwest::Client::new(),
            counters: counters.clone(),
        });
        let cancel = CancellationToken::new();
        let shutdown = cancel.clone();
        let proxy = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
        });
        Self {
            host,
            endpoint,
            counters,
            cancel,
            proxy,
        }
    }

    pub(crate) fn peer(&self, alias: &str) -> A2aPeerSpec {
        A2aPeerSpec::new(
            alias,
            RedactedUrl::from(self.endpoint.clone()),
            A2aPeerSource::Workspace,
        )
    }

    pub(crate) fn journal(&self) -> &Journal {
        &self.host.journal
    }

    pub(crate) fn counts(&self) -> HttpCounts {
        HttpCounts {
            get: self.counters.get.load(Ordering::SeqCst),
            post: self.counters.post.load(Ordering::SeqCst),
        }
    }

    pub(crate) fn posts(&self) -> Vec<CapturedPost> {
        self.counters.posts.lock().expect("captured POSTs").clone()
    }

    pub(crate) fn reset_counts(&self) {
        self.counters.get.store(0, Ordering::SeqCst);
        self.counters.post.store(0, Ordering::SeqCst);
        self.counters.posts.lock().expect("captured POSTs").clear();
    }

    pub(crate) async fn stop(self) {
        self.cancel.cancel();
        self.proxy
            .await
            .expect("observer task")
            .expect("observer shutdown");
        self.host.stop().await;
    }
}

async fn forward(
    State(proxy): State<Proxy>,
    request: Request,
) -> Result<Response, (StatusCode, String)> {
    if request.method() == Method::GET {
        proxy.counters.get.fetch_add(1, Ordering::SeqCst);
    }
    let (parts, body) = request.into_parts();
    let bytes = to_bytes(body, 4 * 1024 * 1024)
        .await
        .map_err(|error| (StatusCode::BAD_GATEWAY, error.to_string()))?;
    let upstream = format!("{}{}", proxy.upstream, parts.uri);
    if parts.method == Method::POST {
        proxy.counters.post.fetch_add(1, Ordering::SeqCst);
        let mut post = CapturedPost::new(bytes.to_vec());
        *post.method_mut() = Method::POST;
        *post.uri_mut() = parts.uri;
        *post.version_mut() = parts.version;
        *post.headers_mut() = parts.headers.clone();
        proxy
            .counters
            .posts
            .lock()
            .expect("captured POSTs")
            .push(post);
    }
    let response = proxy
        .client
        .request(parts.method, upstream)
        .headers(parts.headers)
        .body(bytes)
        .send()
        .await
        .map_err(|error| (StatusCode::BAD_GATEWAY, error.to_string()))?;
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .bytes()
        .await
        .map_err(|error| (StatusCode::BAD_GATEWAY, error.to_string()))?;
    let mut outgoing = Response::new(Body::from(bytes));
    *outgoing.status_mut() = status;
    *outgoing.headers_mut() = headers;
    Ok(outgoing)
}
