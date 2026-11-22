//! Persisted worker report shapes, plus independently gated process-only framing.

use serde::{Deserialize, Deserializer, Serialize};

/// Maximum UTF-8 byte length of each report field.
#[cfg(any(feature = "session-visit", test))]
pub const MAX_FIELD_BYTES: usize = 2048;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkerReport {
    pub finding: String,
    pub evidence: String,
    pub limit: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<WorkerScope>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hypothesis: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dissent: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerScope {
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub environment: Option<String>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub version: Option<String>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub time: Option<String>,
}

#[cfg(any(feature = "session-visit", test))]
impl WorkerScope {
    pub fn fields(&self) -> [Option<&str>; 3] {
        [
            self.environment.as_deref(),
            self.version.as_deref(),
            self.time.as_deref(),
        ]
    }
}

// Absent keys are optional; an explicitly supplied value must have the declared type.
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
}

fn object<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
    kind: &'static str,
) -> Result<T, D::Error> {
    struct ObjectVisitor<T>(std::marker::PhantomData<T>, &'static str);
    impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for ObjectVisitor<T> {
        type Value = T;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str(self.1)
        }

        fn visit_map<M: serde::de::MapAccess<'de>>(self, map: M) -> Result<T, M::Error> {
            T::deserialize(serde::de::value::MapAccessDeserializer::new(map))
        }
    }

    deserializer.deserialize_map(ObjectVisitor(std::marker::PhantomData, kind))
}

fn present_scope<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<WorkerScope>, D::Error> {
    object(deserializer, "a worker scope object").map(Some)
}

impl<'de> Deserialize<'de> for WorkerReport {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            finding: String,
            evidence: String,
            limit: String,
            #[serde(default, deserialize_with = "present_scope")]
            scope: Option<WorkerScope>,
            #[serde(default, deserialize_with = "present")]
            hypothesis: Option<String>,
            #[serde(default, deserialize_with = "present")]
            dissent: Option<String>,
        }

        let Fields {
            finding,
            evidence,
            limit,
            scope,
            hypothesis,
            dissent,
        } = object(deserializer, "a worker report object")?;
        Ok(Self {
            finding,
            evidence,
            limit,
            scope,
            hypothesis,
            dissent,
        })
    }
}

#[cfg(any(feature = "session-visit", test))]
impl WorkerReport {
    fn valid(&self) -> bool {
        [
            Some(self.finding.as_str()),
            Some(self.evidence.as_str()),
            Some(self.limit.as_str()),
            self.hypothesis.as_deref(),
            self.dissent.as_deref(),
        ]
        .into_iter()
        .flatten()
        .all(valid_field)
            && self.scope.as_ref().is_none_or(|scope| {
                let fields = scope.fields();
                fields.iter().any(Option::is_some) && fields.into_iter().flatten().all(valid_field)
            })
    }
}

#[cfg(any(feature = "session-visit", test))]
fn valid_field(text: &str) -> bool {
    text.len() <= MAX_FIELD_BYTES && !text.trim().is_empty()
}

// Durable WorkerReport/WorkerScope decoding above remains available with the
// candidate disabled. Process-only custody framing never becomes persisted authority.
#[cfg(any(feature = "session-visit", test))]
mod transport {
    use super::super::AgentId;
    use super::*;
    use rand_core::{OsRng, RngCore};
    use std::sync::LazyLock;

    const PREFIX: &str = "RUSTAIN_TASK_REPORT_V1\t";

    /// Capture state is host-authored; worker text cannot mint it.
    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    pub enum ReportCapture {
        Structured(WorkerReport),
        Unstructured,
        #[default]
        Unavailable,
    }

    /// In-process handoff to the committed-turn reducer. The custodian, not the
    /// envelope decoder, decides whether the exact attempt can be claimed once.
    pub(crate) struct VisitAttemptLink(AgentId);

    impl VisitAttemptLink {
        pub(crate) fn new(attempt: AgentId) -> Self {
            Self(attempt)
        }

        pub(crate) fn into_attempt(self) -> AgentId {
            self.0
        }
    }

    #[derive(Clone, Copy, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum TextCapture {
        Unstructured,
    }

    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Header {
        #[serde(deserialize_with = "nullable_report")]
        report: Option<WorkerReport>,
        #[serde(
            default,
            deserialize_with = "present",
            skip_serializing_if = "Option::is_none"
        )]
        capture: Option<TextCapture>,
        #[serde(
            default,
            deserialize_with = "present",
            skip_serializing_if = "Option::is_none"
        )]
        visit_attempt: Option<AgentId>,
        tag: String,
    }

    fn nullable_report<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<WorkerReport>, D::Error> {
        Option::deserialize(deserializer)
    }

    #[derive(Serialize)]
    struct ReportHeader<'a> {
        report: Option<&'a WorkerReport>,
        #[serde(skip_serializing_if = "Option::is_none")]
        capture: Option<TextCapture>,
        #[serde(skip_serializing_if = "Option::is_none")]
        visit_attempt: Option<&'a AgentId>,
    }

    // Frames deliberately expire on process restart; they are not a durable format.
    static PROCESS_KEY: LazyLock<[u8; 32]> = LazyLock::new(|| {
        let mut key = [0; 32];
        OsRng.fill_bytes(&mut key);
        key
    });

    fn report_tag(
        report: Option<&WorkerReport>,
        capture: Option<TextCapture>,
        visit_attempt: Option<&AgentId>,
        original: &str,
    ) -> blake3::Hash {
        let header = serde_json::to_vec(&ReportHeader {
            report,
            capture,
            visit_attempt,
        })
        .expect("report serialization cannot fail");
        let mut hasher = blake3::Hasher::new_keyed(&PROCESS_KEY);
        hasher.update(b"RUSTAIN_TASK_REPORT_V1\0");
        hasher.update(&(header.len() as u64).to_le_bytes());
        hasher.update(&header);
        hasher.update(original.as_bytes());
        hasher.finalize()
    }

    /// Parse the entire final assistant text, never a fragment of a transcript.
    pub fn parse(text: &str) -> Option<WorkerReport> {
        let report: WorkerReport = serde_json::from_str(text).ok()?;
        report.valid().then_some(report)
    }

    /// Prepend a process-authenticated header without modifying the original result.
    pub fn frame(report: ReportCapture, original: &str) -> String {
        encode(report, original, None)
    }

    pub(crate) fn frame_visit(original: &str, link: VisitAttemptLink) -> String {
        // The typed report belongs to the custodian, never to a Visit envelope.
        encode(
            ReportCapture::Unavailable,
            original,
            Some(link.into_attempt()),
        )
    }

    fn encode(report: ReportCapture, original: &str, visit_attempt: Option<AgentId>) -> String {
        let (report, capture) = match report {
            ReportCapture::Structured(report) => (Some(report), None),
            ReportCapture::Unstructured => (None, Some(TextCapture::Unstructured)),
            ReportCapture::Unavailable => (None, None),
        };
        let tag = report_tag(report.as_ref(), capture, visit_attempt.as_ref(), original)
            .to_hex()
            .to_string();
        let header = serde_json::to_string(&Header {
            report,
            capture,
            visit_attempt,
            tag,
        })
        .expect("report serialization cannot fail");
        format!("{PREFIX}{header}\n{original}")
    }

    /// Decode only a frame from this process; other text remains literal.
    pub fn split(input: &str) -> Option<(ReportCapture, &str)> {
        split_committed(input).map(|(report, original, _)| (report, original))
    }

    /// Authenticate the host handoff without claiming it. Only the canonical
    /// local turn commit may redeem its optional link with the custodian.
    pub(crate) fn split_committed(
        input: &str,
    ) -> Option<(ReportCapture, &str, Option<VisitAttemptLink>)> {
        let (header, original) = input.strip_prefix(PREFIX)?.split_once('\n')?;
        if !header.trim_start().starts_with('{') {
            return None;
        }
        let Header {
            report,
            capture,
            visit_attempt,
            tag,
        } = serde_json::from_str(header).ok()?;
        if report
            .as_ref()
            .is_some_and(|report| capture.is_some() || !report.valid())
            || (visit_attempt.is_some() && (report.is_some() || capture.is_some()))
            || tag.len() != 64
            || !tag
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return None;
        }
        let tag = blake3::Hash::from_hex(&tag).ok()?;
        if tag != report_tag(report.as_ref(), capture, visit_attempt.as_ref(), original) {
            return None;
        }
        let report = match (report, capture) {
            (Some(report), _) => ReportCapture::Structured(report),
            (None, Some(TextCapture::Unstructured)) => ReportCapture::Unstructured,
            (None, None) => ReportCapture::Unavailable,
        };
        Some((report, original, visit_attempt.map(VisitAttemptLink)))
    }

    #[cfg(test)]
    mod authenticity_tests {
        use super::*;

        #[test]
        fn literal_forgery_and_tampering_are_not_frames() {
            let legacy = "RUSTAIN_TASK_REPORT_V1\t{\"report\":null}\noriginal";
            assert!(split(legacy).is_none());
            let forged = format!(
                "RUSTAIN_TASK_REPORT_V1\t{{\"report\":null,\"tag\":\"{}\"}}\noriginal",
                "0".repeat(64)
            );
            assert!(split(&forged).is_none());
            let mut other_process = blake3::Hasher::new_keyed(&[42; 32]);
            let canonical = b"{\"report\":null}";
            other_process.update(b"RUSTAIN_TASK_REPORT_V1\0");
            other_process.update(&(canonical.len() as u64).to_le_bytes());
            other_process.update(canonical);
            other_process.update(b"original");
            let old_frame = format!(
                "RUSTAIN_TASK_REPORT_V1\t{{\"report\":null,\"tag\":\"{}\"}}\noriginal",
                other_process.finalize().to_hex()
            );
            assert!(split(&old_frame).is_none());

            let report = parse(r#"{"finding":"one","evidence":"two","limit":"three"}"#).unwrap();
            let framed = frame(ReportCapture::Structured(report), "original");
            assert!(split(&framed).is_some());
            assert!(split(&framed.replace("\noriginal", "\nchanged")).is_none());
            assert!(
                split(&framed.replace("\"finding\":\"one\"", "\"finding\":\"changed\"")).is_none()
            );
        }

        #[test]
        fn signed_values_cannot_be_reencoded_as_arrays_or_duplicates() {
            let empty = frame(ReportCapture::Unavailable, "raw");
            let array_header = empty
                .replace("{\"report\":null,", "[null,")
                .replacen("}\nraw", "]\nraw", 1);
            assert!(split(&array_header).is_none());
            let duplicate_header =
                empty.replace("\"report\":null,", "\"report\":null,\"report\":null,");
            assert!(split(&duplicate_header).is_none());

            let report = parse(r#"{"finding":"one","evidence":"two","limit":"three"}"#).unwrap();
            let framed = frame(ReportCapture::Structured(report), "raw");
            let array_report = framed.replace(
                r#"{"finding":"one","evidence":"two","limit":"three"}"#,
                r#"["one","two","three"]"#,
            );
            assert!(split(&array_report).is_none());
            let duplicate_field = framed.replace(
                "\"finding\":\"one\",",
                "\"finding\":\"one\",\"finding\":\"one\",",
            );
            assert!(split(&duplicate_field).is_none());
        }

        #[test]
        fn visit_link_is_authenticated_but_not_claimed_at_decode() {
            let attempt = AgentId::new();
            let raw = "literal \r\n task result";
            let framed = frame_visit(raw, VisitAttemptLink::new(attempt.clone()));
            assert_eq!(split(&framed), Some((ReportCapture::Unavailable, raw)));
            let (_, original, link) = split_committed(&framed).unwrap();
            assert_eq!(original, raw);
            assert_eq!(link.unwrap().into_attempt(), attempt);
            assert!(split_committed(&framed.replace(attempt.as_str(), "another-id")).is_none());
            let without_link = framed.replace(&format!(",\"visit_attempt\":\"{attempt}\""), "");
            assert!(split_committed(&without_link).is_none());
            assert!(split_committed(&framed.replace(raw, "different output")).is_none());
            assert!(
                split_committed(&frame(ReportCapture::Unavailable, raw))
                    .unwrap()
                    .2
                    .is_none()
            );
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn report_round_trip_preserves_exact_raw_tail() {
            let report =
                parse(r#"{"finding":"α","evidence":"line\n2","limit":"unverified"}"#).unwrap();
            assert!(report.scope.is_none());
            let raw = "\nraw\r\nRUSTAIN_TASK_REPORT_V1\t{}\n終\0";
            for report in [
                ReportCapture::Structured(report),
                ReportCapture::Unstructured,
                ReportCapture::Unavailable,
            ] {
                let framed = frame(report.clone(), raw);
                assert_eq!(split(&framed), Some((report, raw)));
            }
        }

        #[test]
        fn scoped_report_round_trip_authenticates_coverage_hypothesis_and_dissent() {
            let report = parse(
                r#"{
                "finding":"one display row",
                "evidence":"synthetic export",
                "limit":"commit path not checked",
                "scope":{"environment":"staging","version":"α","time":"morning"},
                "hypothesis":"retry may replay display",
                "dissent":"does not support two commits"
            }"#,
            )
            .unwrap();
            assert_eq!(
                report.scope.as_ref().unwrap().fields(),
                [Some("staging"), Some("α"), Some("morning")]
            );
            assert_eq!(
                report.hypothesis.as_deref(),
                Some("retry may replay display")
            );
            assert_eq!(
                report.dissent.as_deref(),
                Some("does not support two commits")
            );
            let raw = "\r\n original\n";
            let framed = frame(ReportCapture::Structured(report.clone()), raw);
            assert_eq!(
                split(&framed),
                Some((ReportCapture::Structured(report), raw))
            );
            assert!(split(&framed.replace("staging", "production")).is_none());
            assert!(split(&framed.replace("retry may", "retry did")).is_none());
            assert!(split(&framed.replace("does not", "does")).is_none());
            assert!(
                split(&framed.replace(
                    "\"environment\":\"staging\"",
                    "\"environment\":\"staging\",\"environment\":\"staging\""
                ))
                .is_none()
            );
            let unavailable = frame(ReportCapture::Unavailable, raw);
            assert!(
                split(&unavailable.replace(
                    "\"report\":null",
                    "\"report\":null,\"capture\":\"unstructured\""
                ))
                .is_none()
            );
        }

        #[test]
        fn optional_fields_are_strict_and_individually_bounded() {
            const REQUIRED: &str = r#"{"finding":"x","evidence":"y","limit":"z","#;
            for extra in [
                r#""scope":{}"#,
                r#""scope":null"#,
                r#""scope":["stage","v2","12:00"]"#,
                r#""scope":{"version":null}"#,
                r#""scope":{"time":"  "}"#,
                r#""scope":{"time":12}"#,
                r#""scope":{"unknown":"text"}"#,
                r#""scope":{"version":"v2","version":"v2"}"#,
                r#""scope":{"version":"v2"},"scope":{"version":"v2"}"#,
                r#""hypothesis":"h","hypothesis":"h""#,
                r#""dissent":"d","dissent":"d""#,
                r#""dissent":["d"]"#,
                r#""hypothesis":null"#,
            ] {
                assert!(parse(&format!("{REQUIRED}{extra}}}")).is_none(), "{extra}");
            }
            for len in [MAX_FIELD_BYTES / 2, MAX_FIELD_BYTES / 2 + 1] {
                let value = "é".repeat(len);
                for extra in [
                    format!(r#""scope":{{"environment":"{value}"}}"#),
                    format!(r#""hypothesis":"{value}""#),
                    format!(r#""dissent":"{value}""#),
                ] {
                    assert_eq!(
                        parse(&format!("{REQUIRED}{extra}}}")).is_some(),
                        value.len() <= MAX_FIELD_BYTES
                    );
                }
            }
        }

        #[test]
        fn rejects_invalid_reports_and_frames() {
            for text in [
                "```json\n{\"finding\":\"x\",\"evidence\":\"y\",\"limit\":\"z\"}\n```",
                r#"{"finding":"x","evidence":"y"}"#,
                r#"{"finding":"x","evidence":"y","limit":" "}"#,
                r#"{"finding":"x","evidence":"y","limit":"z","extra":1}"#,
                r#"{"finding":"x","evidence":"y","limit":"z"} trailing"#,
                r#"["x","y","z"]"#,
                r#"{"finding":"x","finding":"again","evidence":"y","limit":"z"}"#,
            ] {
                assert!(parse(text).is_none(), "{text}");
            }
            let oversized = format!(
                r#"{{"finding":"{}","evidence":"y","limit":"z"}}"#,
                "é".repeat(MAX_FIELD_BYTES / 2 + 1)
            );
            assert!(parse(&oversized).is_none());
            assert!(split("legacy output").is_none());
            assert!(split("RUSTAIN_TASK_REPORT_V1\t{}\nraw").is_none());
            assert!(split("RUSTAIN_TASK_REPORT_V1\t{\"report\":null}").is_none());
            assert!(split("RUSTAIN_TASK_REPORT_V1\t[null]\nraw").is_none());
            assert!(
                split("RUSTAIN_TASK_REPORT_V1\t{\"report\":[\"x\",\"y\",\"z\"]}\nraw").is_none()
            );
            assert!(
                split("RUSTAIN_TASK_REPORT_V1\t{\"report\":null,\"report\":null}\nraw").is_none()
            );
            assert!(split("RUSTAIN_TASK_REPORT_V1\t{\"report\":{\"finding\":\"x\",\"finding\":\"again\",\"evidence\":\"y\",\"limit\":\"z\"}}\nraw").is_none());
        }
    }
}

#[cfg(any(feature = "session-visit", test))]
pub use transport::{ReportCapture, frame, parse, split};
#[cfg(any(feature = "session-visit", test))]
pub(crate) use transport::{VisitAttemptLink, frame_visit, split_committed};

#[cfg(test)]
mod compatibility_tests {
    use super::*;

    #[test]
    fn legacy_minimal_report_still_deserializes_without_optional_fields() {
        let report: WorkerReport = serde_json::from_str(
            r#"{"finding":"PUBLIC_ONLY_MARKER","evidence":"retained","limit":"unverified"}"#,
        )
        .unwrap();
        assert_eq!(report.finding, "PUBLIC_ONLY_MARKER");
        assert!(report.scope.is_none());
        assert!(report.hypothesis.is_none());
        assert!(report.dissent.is_none());
        assert_eq!(
            serde_json::to_value(report).unwrap(),
            serde_json::json!({
                "finding": "PUBLIC_ONLY_MARKER", "evidence": "retained", "limit": "unverified"
            })
        );
    }
}
