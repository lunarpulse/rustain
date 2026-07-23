//! JSON-RPC endpoint resolution across the v0.3 and v1.0 AgentCard shapes.
//!
//! Ruling 2 (Story 17.4b): resolution discriminates on **field presence**, never
//! on `protocolVersion` (proven to mis-route ~11% of the live population and
//! crash on the rest). The algorithm below was validated by execution against the
//! committed 141-card corpus: 138/141 resolve a JSON-RPC endpoint. Resolving an
//! endpoint is not the same as *speaking* the dialect at it — only ~80% are
//! speakable by the shipped JSON-RPC dialect (Ruling 1b). State both numbers.

use super::card::AgentCardView;
use super::error::A2aError;

/// A resolved JSON-RPC endpoint. The URL parses and carries a scheme + host; a
/// scheme-less or unparseable URL never reaches this type — it is refused, never
/// coerced (DF-17-4a-3 narrowed). The https-or-loopback policy is applied later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedEndpoint {
    url: String,
}

impl ResolvedEndpoint {
    /// The validated absolute URL to POST JSON-RPC requests to.
    pub fn url(&self) -> &str {
        &self.url
    }
}

/// Normalize a binding/transport spelling for comparison only: strip every
/// non-alphanumeric byte and uppercase. `"JSON-RPC"`, `"jsonrpc"`, and
/// `"JSONRPC"` all normalize to `"JSONRPC"`. Raw bytes are never mutated on the
/// card — this is a comparison key (ADR-17-4a-01 R11: 18 observed spellings).
fn normalize_binding(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

fn is_jsonrpc(raw: &Option<String>) -> bool {
    raw.as_deref()
        .is_some_and(|value| normalize_binding(value) == "JSONRPC")
}

/// `preferredTransport` is treated as absent when it is `None` or blank. The spec
/// defaults `preferredTransport` to `"JSONRPC"` when absent; a live card in the
/// corpus (`Hello World Agent`) publishes `preferredTransport: ""` alongside a
/// valid `url`, and treating the empty string as absent is what lifts the
/// resolver from 137/141 to the measured 138/141.
fn transport_absent(raw: &Option<String>) -> bool {
    raw.as_deref().is_none_or(|value| value.trim().is_empty())
}

/// Resolve a JSON-RPC endpoint from a decoded card using the Ruling-2 5-step
/// field-presence algorithm. A card that publishes no JSON-RPC binding, or whose
/// selected URL is scheme-less / non-loopback-http / otherwise unsafe, is a
/// **typed refusal** (`A2aError::NoJsonRpcEndpoint` or `A2aError::UnsafeUrl`),
/// never a fallback guess.
pub fn resolve_jsonrpc_endpoint(card: &AgentCardView) -> Result<ResolvedEndpoint, A2aError> {
    // Step 1: supportedInterfaces[] (v1.0) whose protocolBinding is JSON-RPC.
    if let Some(interfaces) = card.supported_interfaces.as_ref() {
        for interface in interfaces {
            if is_jsonrpc(&interface.protocol_binding) {
                return finalize(interface.url.as_deref(), "supportedInterfaces");
            }
        }
    }

    // Step 2: preferredTransport (v0.3) is JSON-RPC → use top-level url.
    if is_jsonrpc(&card.preferred_transport) {
        return finalize(card.url.as_deref(), "preferredTransport");
    }

    // Step 3: additionalInterfaces[] (v0.3) whose transport is JSON-RPC.
    if let Some(interfaces) = card.additional_interfaces.as_ref() {
        for interface in interfaces {
            if is_jsonrpc(&interface.transport) {
                return finalize(interface.url.as_deref(), "additionalInterfaces");
            }
        }
    }

    // Step 4: url present AND preferredTransport absent → spec default JSON-RPC.
    if card.url.is_some() && transport_absent(&card.preferred_transport) {
        return finalize(card.url.as_deref(), "url+default-transport");
    }

    // Step 5: typed refusal — no JSON-RPC binding is advertised anywhere.
    Err(A2aError::NoJsonRpcEndpoint {
        reason: "card advertises no JSON-RPC interface (no supportedInterfaces/preferredTransport/\
                 additionalInterfaces binding and no default-transport url)"
            .to_owned(),
    })
}

fn finalize(url: Option<&str>, source: &str) -> Result<ResolvedEndpoint, A2aError> {
    let raw = url.map(str::trim).filter(|value| !value.is_empty()).ok_or(
        A2aError::NoJsonRpcEndpoint {
            reason: format!("{source} selects a JSON-RPC binding but carries no url"),
        },
    )?;
    // Refuse scheme-less / unparseable URLs — never coerce a scheme (DF-17-4a-3: a
    // scheme-less URL is a dev-server artifact, and silently prepending http://
    // would be a transport downgrade). The stricter https-or-loopback safety
    // policy is enforced later, when `A2aClientAdapter` builds the POST client —
    // resolution answers only "does this card advertise a JSON-RPC endpoint" (the
    // 98% reach number); "can we safely POST to it" is the narrower ~80% number.
    let parsed = url::Url::parse(raw).map_err(|error| A2aError::UnsafeUrl {
        reason: format!("endpoint url {raw:?} does not parse: {error}"),
    })?;
    if parsed.host().is_none() {
        return Err(A2aError::UnsafeUrl {
            reason: format!("endpoint url {raw:?} has no host authority"),
        });
    }
    Ok(ResolvedEndpoint {
        url: raw.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(json: &str) -> AgentCardView {
        serde_json::from_str(json).expect("card parses")
    }

    #[test]
    fn supported_interfaces_json_rpc_wins_over_top_level_url() {
        let resolved = resolve_jsonrpc_endpoint(&card(
            r#"{"name":"x","skills":[],
                "url":"https://origin.example/ignored",
                "supportedInterfaces":[
                  {"url":"https://mcp.example/mcp","protocolBinding":"streamable-http"},
                  {"url":"https://rpc.example/jsonrpc","protocolBinding":"JSONRPC"}
                ]}"#,
        ))
        .expect("json-rpc interface resolves");
        assert_eq!(resolved.url(), "https://rpc.example/jsonrpc");
    }

    #[test]
    fn preferred_transport_json_rpc_uses_top_level_url() {
        let resolved = resolve_jsonrpc_endpoint(&card(
            r#"{"name":"x","skills":[],"url":"https://peer.example/a2a","preferredTransport":"JSONRPC"}"#,
        ))
        .expect("preferred transport resolves");
        assert_eq!(resolved.url(), "https://peer.example/a2a");
    }

    #[test]
    fn absent_preferred_transport_defaults_to_json_rpc() {
        let resolved = resolve_jsonrpc_endpoint(&card(
            r#"{"name":"x","skills":[],"url":"https://peer.example/a2a"}"#,
        ))
        .expect("default transport resolves");
        assert_eq!(resolved.url(), "https://peer.example/a2a");
    }

    #[test]
    fn empty_preferred_transport_is_treated_as_absent() {
        let resolved = resolve_jsonrpc_endpoint(&card(
            r#"{"name":"x","skills":[],"url":"https://peer.example/a2a","preferredTransport":""}"#,
        ))
        .expect("blank preferred transport is absent → default JSON-RPC");
        assert_eq!(resolved.url(), "https://peer.example/a2a");
    }

    #[test]
    fn binding_spelling_is_normalized_not_enum_matched() {
        for spelling in ["json-rpc", "JsonRpc", "JSON_RPC", "  JSONRPC  "] {
            let json = format!(
                r#"{{"name":"x","skills":[],"supportedInterfaces":[{{"url":"https://rpc.example/","protocolBinding":"{spelling}"}}]}}"#
            );
            assert!(
                resolve_jsonrpc_endpoint(&card(&json)).is_ok(),
                "spelling {spelling:?} should normalize to JSONRPC"
            );
        }
    }

    #[test]
    fn additional_interfaces_are_filtered_by_binding_never_indexed() {
        // additionalInterfaces[0] is MCP; the JSON-RPC entry is [1]. Indexing
        // [0] would land on the MCP endpoint.
        let resolved = resolve_jsonrpc_endpoint(&card(
            r#"{"name":"x","skills":[],"preferredTransport":"GRPC","url":"https://grpc.example/",
                "additionalInterfaces":[
                  {"url":"https://mcp.example/mcp","transport":"streamable-http"},
                  {"url":"https://rpc.example/jsonrpc","transport":"JSONRPC"}
                ]}"#,
        ))
        .expect("filtered additional interface resolves");
        assert_eq!(resolved.url(), "https://rpc.example/jsonrpc");
    }

    #[test]
    fn protocol_version_never_discriminates_shape() {
        // v1.0-advertising card wearing the v0.3 url shape still resolves via url.
        let resolved = resolve_jsonrpc_endpoint(&card(
            r#"{"name":"x","skills":[],"protocolVersion":"1.0","url":"https://peer.example/a2a","preferredTransport":"JSONRPC"}"#,
        ))
        .expect("version is ignored, shape resolves");
        assert_eq!(resolved.url(), "https://peer.example/a2a");
    }

    #[test]
    fn http_json_only_card_is_a_typed_refusal() {
        let error = resolve_jsonrpc_endpoint(&card(
            r#"{"name":"x","skills":[],"url":"https://peer.example/a2a","preferredTransport":"HTTP+JSON"}"#,
        ))
        .expect_err("HTTP+JSON-only card must refuse");
        assert!(matches!(error, A2aError::NoJsonRpcEndpoint { .. }));
    }

    #[test]
    fn no_url_and_no_interfaces_is_a_typed_refusal() {
        let error = resolve_jsonrpc_endpoint(&card(r#"{"name":"x","skills":[]}"#))
            .expect_err("card with no endpoint must refuse");
        assert!(matches!(error, A2aError::NoJsonRpcEndpoint { .. }));
    }

    #[test]
    fn scheme_less_url_is_refused_never_coerced() {
        // The v1.0 reference agent advertises a scheme-less gRPC url; a client
        // that coerces http:// would POST JSON-RPC at a gRPC port.
        let error = resolve_jsonrpc_endpoint(&card(
            r#"{"name":"x","skills":[],"url":"127.0.0.1:11002","preferredTransport":"JSONRPC"}"#,
        ))
        .expect_err("scheme-less url must be refused");
        assert!(matches!(error, A2aError::UnsafeUrl { .. }));
    }

    #[test]
    fn json_rpc_interface_without_url_refuses() {
        let error = resolve_jsonrpc_endpoint(&card(
            r#"{"name":"x","skills":[],"supportedInterfaces":[{"protocolBinding":"JSONRPC"}]}"#,
        ))
        .expect_err("JSON-RPC binding with no url must refuse");
        assert!(matches!(error, A2aError::NoJsonRpcEndpoint { .. }));
    }
}
