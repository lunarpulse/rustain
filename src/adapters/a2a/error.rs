#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum A2aError {
    #[error("invalid AgentCard JSON: {0}")]
    InvalidJson(#[from] serde_json::Error),
    #[error("malformed AgentCard: required field {field} is missing or empty")]
    MalformedCard { field: String },
    #[error("AgentCard has no signatures")]
    MissingSignatures,
    #[error("invalid JWS protected header")]
    InvalidProtectedHeader,
    #[error("unsupported AgentCard signature algorithm {algorithm:?}")]
    UnsupportedAlgorithm { algorithm: String },
    #[error("AgentCard signature key id does not match configured pin")]
    KeyIdMismatch {
        expected: String,
        actual: Option<String>,
    },
    #[error("invalid pinned Ed25519 public key")]
    InvalidPinnedKey,
    #[error("invalid AgentCard signature encoding")]
    InvalidSignatureEncoding,
    #[error("AgentCard JCS canonicalization failed: {0}")]
    Canonicalization(String),
    #[error("AgentCard signature verification failed")]
    BadSignature,
    #[error("failed to build A2A HTTP client: {0}")]
    ClientBuild(String),
    #[error("unsafe A2A URL: {reason}")]
    UnsafeUrl { reason: String },
    #[error("A2A request failed: {0}")]
    Request(String),
    #[error("A2A peer returned HTTP {status}")]
    HttpStatus { status: u16 },
    #[error("A2A redirect is invalid: {0}")]
    InvalidRedirect(String),
    #[error("A2A redirect limit exceeded")]
    TooManyRedirects,
    #[error("AgentCard response has non-JSON content type {content_type:?}")]
    UnexpectedContentType { content_type: Option<String> },
    #[error("AgentCard response exceeds {max_bytes} byte limit")]
    BodyTooLarge { max_bytes: usize },
    #[error("AgentCard response is not UTF-8")]
    InvalidUtf8,
    #[error("A2A card exposes no reachable JSON-RPC endpoint: {reason}")]
    NoJsonRpcEndpoint { reason: String },
    #[error(
        "unrecognized A2A task-state spelling {raw:?} — the JSON-RPC binding is \
         lowercase-hyphen; refuse never mis-parse"
    )]
    UnknownTaskState { raw: String },
    #[error("A2A JSON-RPC error {code}: {message}")]
    JsonRpc { code: i64, message: String },
    #[error("malformed A2A JSON-RPC response: {reason}")]
    MalformedResponse { reason: String },
    #[error("A2A JSON-RPC response id {actual:?} does not correlate with request id {expected}")]
    CorrelationMismatch { expected: u64, actual: String },
    #[error("A2A server configuration is invalid: {0}")]
    Config(String),
}
