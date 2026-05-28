use serde::{Deserialize, Serialize};

/// W3C Trace Context propagation primitive — minimal in-house struct.
/// `traceparent` header shape: 00-<trace_id 32-hex>-<parent_id 16-hex>-<flags 2-hex>.
/// Codex precedent at codex-rs/core/src/codex_delegate.rs. Forward-compat seam
/// for LangSmith / Langfuse / OpenTelemetry integration without schema migration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceContext {
    pub trace_id: String,
    pub span_id: String,
    pub flags: u8,
}

impl TraceContext {
    pub fn new(trace_id: String, span_id: String, flags: u8) -> Result<Self, &'static str> {
        if trace_id.len() != 32 || !trace_id.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err("trace_id must be exactly 32 lowercase hex characters");
        }
        if span_id.len() != 16 || !span_id.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err("span_id must be exactly 16 lowercase hex characters");
        }
        Ok(Self {
            trace_id,
            span_id,
            flags,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_trace_context() {
        let ctx = TraceContext::new(
            "0af7651916cd43dd8448eb211c80319c".into(),
            "b7ad6b7169203331".into(),
            1,
        )
        .unwrap();
        assert_eq!(ctx.trace_id.len(), 32);
        assert_eq!(ctx.span_id.len(), 16);
    }

    #[test]
    fn reject_short_trace_id() {
        assert!(TraceContext::new("abc".into(), "b7ad6b7169203331".into(), 0).is_err());
    }

    #[test]
    fn reject_non_hex_trace_id() {
        assert!(
            TraceContext::new(
                "gggggggggggggggggggggggggggggggg".into(),
                "b7ad6b7169203331".into(),
                0,
            )
            .is_err()
        );
    }

    #[test]
    fn reject_short_span_id() {
        assert!(
            TraceContext::new("0af7651916cd43dd8448eb211c80319c".into(), "abc".into(), 0,).is_err()
        );
    }
}
