//! JSON-RPC 2.0 request/response/error layer for the A2A binding.
//!
//! Ruling 12 (Story 17.4b): there is no JSON-RPC infrastructure in this repo —
//! MCP's lives in `rmcp` (closed enums, stdio-only), ACP's in
//! `agent-client-protocol` (stdio, newline-delimited), and RAP is not JSON-RPC.
//! This is the hand-rolled request/response/error layer over `reqwest` POST.
//!
//! Hard rules from the Task 0b spike (each a way to break an agent that would
//! otherwise answer):
//! - **Never send an `A2A-Version` header.** Omitting it works on every measured
//!   agent; sending `1.0` gets `-32603` from the v1.0 agent.
//! - **POST only to the resolved `url`, never the origin.** The origin returns a
//!   plain `{"detail":"Not Found"}` 404 a naive client mis-reads as transport
//!   failure.

use serde::{Deserialize, Serialize};

use super::error::A2aError;

/// Standard JSON-RPC method-not-found (captured by the spike).
pub const CODE_METHOD_NOT_FOUND: i64 = -32601;
/// Internal error; the v1.0 agent returns this for an unsupported `A2A-Version`.
pub const CODE_INTERNAL_ERROR: i64 = -32603;
/// A2A "Task not found" — `tasks/get`/`tasks/cancel` on an unknown id.
pub const CODE_TASK_NOT_FOUND: i64 = -32001;

/// A JSON-RPC 2.0 request. `id` is a monotonic correlation key that the response
/// must echo. `params` is pre-built A2A payload JSON.
#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: String,
    pub params: serde_json::Value,
}

impl JsonRpcRequest {
    /// Build a 2.0 request with the given correlation id.
    pub fn new(id: u64, method: impl Into<String>, params: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            method: method.into(),
            params,
        }
    }
}

/// The `error` object of a JSON-RPC response.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct JsonRpcErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(default)]
    pub data: Option<serde_json::Value>,
}

/// Raw wire response, deserialized before result/error demux and id correlation.
#[derive(Debug, Deserialize)]
struct JsonRpcResponseRaw {
    #[serde(default)]
    id: serde_json::Value,
    #[serde(default)]
    result: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<JsonRpcErrorObject>,
}

/// A classified JSON-RPC error code. Open (`Other`) — the code space is not a
/// closed enum (ADR-17-4a-01 R11 spirit); classification names the codes the
/// spike captured without pretending the set is exhaustive.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonRpcErrorKind {
    MethodNotFound,
    InternalError,
    TaskNotFound,
    Other(i64),
}

impl JsonRpcErrorKind {
    pub const fn classify(code: i64) -> Self {
        match code {
            CODE_METHOD_NOT_FOUND => Self::MethodNotFound,
            CODE_INTERNAL_ERROR => Self::InternalError,
            CODE_TASK_NOT_FOUND => Self::TaskNotFound,
            other => Self::Other(other),
        }
    }
}

/// Demux a raw JSON-RPC response body against the request `id`. Returns the
/// `result` value on success; maps an `error` object to a typed
/// [`A2aError::JsonRpc`]; refuses a response whose `id` does not correlate
/// (AC4: correlation-id round-trip), or that carries neither result nor error.
pub fn parse_response(raw: &str, expected_id: u64) -> Result<serde_json::Value, A2aError> {
    let parsed: JsonRpcResponseRaw =
        serde_json::from_str(raw).map_err(|error| A2aError::MalformedResponse {
            reason: format!("not JSON-RPC: {error}"),
        })?;

    // Correlation-id round-trip. JSON-RPC ids may be numbers or strings; accept a
    // numeric id equal to expected, or its string form, and refuse anything else.
    let correlates = match &parsed.id {
        serde_json::Value::Number(number) => number.as_u64() == Some(expected_id),
        serde_json::Value::String(text) => text == &expected_id.to_string(),
        _ => false,
    };
    if !correlates {
        return Err(A2aError::CorrelationMismatch {
            expected: expected_id,
            actual: parsed.id.to_string(),
        });
    }

    match (parsed.result, parsed.error) {
        (_, Some(error)) => Err(A2aError::JsonRpc {
            code: error.code,
            message: error.message,
        }),
        (Some(result), None) => Ok(result),
        (None, None) => Err(A2aError::MalformedResponse {
            reason: "response has neither result nor error".to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_serializes_as_json_rpc_2_0_without_version_header_concern() {
        let request = JsonRpcRequest::new(7, "message/send", serde_json::json!({"foo": "bar"}));
        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["id"], 7);
        assert_eq!(value["method"], "message/send");
        assert_eq!(value["params"]["foo"], "bar");
    }

    #[test]
    fn result_is_returned_when_id_correlates() {
        let raw = r#"{"jsonrpc":"2.0","id":7,"result":{"status":{"state":"working"}}}"#;
        let result = parse_response(raw, 7).unwrap();
        assert_eq!(result["status"]["state"], "working");
    }

    #[test]
    fn string_id_correlates_with_numeric_request_id() {
        let raw = r#"{"jsonrpc":"2.0","id":"7","result":{"ok":true}}"#;
        assert!(parse_response(raw, 7).is_ok());
    }

    #[test]
    fn mismatched_id_is_refused() {
        let raw = r#"{"jsonrpc":"2.0","id":9,"result":{"ok":true}}"#;
        let error = parse_response(raw, 7).expect_err("id 9 must not correlate with request 7");
        assert!(matches!(
            error,
            A2aError::CorrelationMismatch { expected: 7, .. }
        ));
    }

    #[test]
    fn error_object_maps_to_typed_json_rpc_error() {
        let raw = r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32001,"message":"Task not found"}}"#;
        let error = parse_response(raw, 3).expect_err("error object must surface");
        match error {
            A2aError::JsonRpc { code, ref message } => {
                assert_eq!(code, CODE_TASK_NOT_FOUND);
                assert_eq!(message, "Task not found");
                assert_eq!(
                    JsonRpcErrorKind::classify(code),
                    JsonRpcErrorKind::TaskNotFound
                );
            }
            other => panic!("expected JsonRpc error, got {other:?}"),
        }
    }

    #[test]
    fn version_not_supported_classifies_as_internal_error() {
        assert_eq!(
            JsonRpcErrorKind::classify(CODE_INTERNAL_ERROR),
            JsonRpcErrorKind::InternalError
        );
        assert_eq!(
            JsonRpcErrorKind::classify(-32601),
            JsonRpcErrorKind::MethodNotFound
        );
        assert_eq!(
            JsonRpcErrorKind::classify(-40000),
            JsonRpcErrorKind::Other(-40000)
        );
    }

    #[test]
    fn neither_result_nor_error_is_malformed() {
        let raw = r#"{"jsonrpc":"2.0","id":1}"#;
        let error = parse_response(raw, 1).expect_err("empty response is malformed");
        assert!(matches!(error, A2aError::MalformedResponse { .. }));
    }

    #[test]
    fn non_json_body_is_malformed_not_a_panic() {
        // The origin-404 case: `{"detail":"Not Found"}` is valid JSON but has no
        // id → correlation mismatch; a truly non-JSON body is MalformedResponse.
        let error = parse_response("Not Found", 1).expect_err("non-JSON must error");
        assert!(matches!(error, A2aError::MalformedResponse { .. }));
    }
}
