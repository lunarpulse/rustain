//! Transparency projection — the decision core behind every A2A disclosure
//! surface (Story 18.2, FR92/FR95, NFR62/63/67).
//!
//! # One journal, one fold
//!
//! `transparency.jsonl`, the `Ctrl+X, L` panel, `/team log` and
//! `rustain team log` all render the output of [`fold_transparency`] over the
//! canonical room journal. There is deliberately **no** second append-only
//! transparency log (ADR-17-CC-05): a file that logs independently of the
//! journal is a second source of truth to keep consistent, and the one that
//! drifts is the one nobody is looking at. The export is regenerated whole,
//! never appended, and nothing in the product ever reads it back.
//!
//! # Growth stance
//!
//! `transparency.jsonl` is the shape of `DF-CR-11-4a-2` — an unbounded
//! sidecar. Because it is *rendered whole* from the journal rather than
//! appended to, it inherits the journal's growth instead of compounding it:
//! there is exactly one thing to compact, and it is the journal. Journal
//! compaction is out of scope and un-owned — filed as
//! `DF-18-2-JOURNAL-GROWTH`, trigger *"first room journal exceeding N MB or
//! first operator report of a slow `/team log`"*.
//!
//! # Retraction forward-compatibility (NFR64, 18.3)
//!
//! If a transparency record is ever retracted, the retraction is an
//! **appended event**, never an edit or a delete of an existing line. The
//! journal is append-only; a surface that erased history would be the exact
//! thing an audit log exists to prevent. This paragraph reserves the shape so
//! 18.3 does not have to reopen a durable file format; no `Retracted` variant
//! ships here, because an unused variant proves nothing.
//!
//! # Structural replay scope (AC7, NFR63)
//!
//! What this substrate provides is **append-only, crash-safe, and structurally
//! replayable**: `flock`-serialized single-line appends, `fsync` before
//! acknowledgement, torn-tail repair, and a contiguous `seq` re-derived from
//! the durable tail on every append. It does not authenticate payloads or prove
//! their origin — there is no hash chain over `JournalEntry` and no signature
//! over a room record. See [`STRUCTURAL_REPLAY_CLAIM`] and
//! [`validate_replay_structure`].

use std::path::PathBuf;

use crate::domain::models::{
    Direction, JournalEntry, JournalRecord, RejectReason, RoomEvent, node_journal,
};

/// Longest disclosable free-text field rendered by any transparency surface.
///
/// Peer-influenced text reaches disk, a chat transcript and — via
/// `rustain team log` — a raw `println!`. Bounding it is not cosmetic: an
/// unbounded attacker-controlled identifier is the class 18.1b's review found
/// repeatedly.
pub const MAX_SUMMARY_BYTES: usize = 512;

/// Longest peer-supplied task/context identifier this instance will carry into
/// a transparency record. Mirrors the inbound `MAX_TASK_ID_BYTES` bound; the
/// outbound side had no equivalent before Story 18.2.
pub const MAX_PEER_ID_BYTES: usize = 256;

/// Appended when [`sanitize_disclosable`] drops bytes. Silent truncation is a
/// lie with good intentions (ADR R23) — the cut must be visible in the output.
pub const TRUNCATION_MARKER: &str = "…[truncated]";

/// The scoped structural-replay claim for every transparency surface (AC7,
/// NFR63).
///
/// It intentionally says nothing about payload or origin authentication: no
/// substrate in the product signs or hash-chains a `JournalEntry` today.
pub const STRUCTURAL_REPLAY_CLAIM: &str =
    "append-only, crash-safe, structurally replayable (not payload- or provenance-authenticated)";

/// Attribution honesty note (`DF-18-1-MTLS`). A peer identity is only as
/// trustworthy as the scheme that produced it: under a shared API key,
/// "peer X did this" means "someone holding X's key did this".
pub const ATTRIBUTION_CAVEAT: &str =
    "peer attribution is only as strong as the credential scheme that produced it";

/// What kind of decision a row records. Drives the glyph, never the colour
/// alone (monochrome rule).
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransparencyKind {
    /// A task crossed the boundary and was admitted.
    Accepted,
    /// A task was refused, or a delegation came back refused.
    Rejected,
    /// Admitted but parked on a human decision.
    AwaitingApproval,
    /// The peer asked for a task's status (first observation only).
    StatusQueried,
    /// Content was actually handed back to the peer.
    Disclosed,
    /// A record this build does not understand. Rendered, never dropped
    /// (UX-DR-ROOM-01).
    Unknown,
}

impl TransparencyKind {
    /// Monochrome-safe glyph. Always paired with [`Self::label`].
    #[must_use]
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Accepted => "✓",
            Self::Rejected => "✗",
            Self::AwaitingApproval => "⏸",
            Self::StatusQueried => "?",
            Self::Disclosed => "⇢",
            _ => "·",
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Rejected => "refused",
            Self::AwaitingApproval => "awaiting-approval",
            Self::StatusQueried => "status-query",
            Self::Disclosed => "disclosed",
            _ => "unknown",
        }
    }
}

/// One rendered transparency row. Every surface renders exactly this.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransparencyRow {
    /// Journal sequence — the total order, and the row's stable identity.
    pub seq: u64,
    /// Wall-clock unix milliseconds, or `None` for a line journaled before
    /// Story 18.2. Renders as `—`, **never** as epoch zero.
    pub recorded_at_ms: Option<i64>,
    /// First appended retraction timestamp for this row, if any.
    pub retracted_at_ms: Option<i64>,
    pub direction: Direction,
    pub kind: TransparencyKind,
    /// The remote principal, or `—` when the record carries none.
    pub peer: String,
    /// A2A task correlation id when recoverable.
    pub task: Option<String>,
    /// Human-readable, already sanitized and length-capped.
    pub summary: String,
}

impl TransparencyRow {
    /// Timestamp rendered for humans, or the explicit unknown marker.
    #[must_use]
    pub fn timestamp_label(&self) -> String {
        match self.recorded_at_ms {
            Some(ms) => format_unix_millis(ms),
            None => "—".to_owned(),
        }
    }

    /// One line, monochrome-safe, for the chat notice and the CLI.
    #[must_use]
    pub fn one_line(&self) -> String {
        let task = self.task.as_deref().unwrap_or("—");
        let retracted = self.retracted_at_ms.map_or_else(String::new, |ms| {
            format!(" [retracted {}]", format_unix_millis(ms))
        });
        format!(
            "{} {} {} {} · peer {} · task {} · {}{}",
            self.direction.glyph(),
            self.direction.label(),
            self.kind.glyph(),
            self.kind.label(),
            self.peer,
            task,
            self.summary,
            retracted
        )
    }
}

/// Strip C0/C1 control bytes and cap length, visibly.
///
/// # Why both halves, and why here
///
/// Peer-controlled text reaches three sinks: the room journal (disk), the chat
/// transcript (ratatui, which renders escapes as inert glyphs) and — new in
/// Story 18.2 — `rustain team log`'s `println!`, which is a genuine
/// terminal-escape-injection sink. Sanitizing at each render surface was
/// rejected: a fourth surface arrives in 18.3 and will be forgotten.
/// Sanitizing on write alone was also rejected: Story 18.1b already journaled
/// unsanitized records and those bytes are on disk today. So: strip on write
/// **and** on read, cap on both.
///
/// Legitimate content survives unchanged — RTL marks, combining marks, emoji
/// and CJK are all above U+009F and untouched.
#[must_use]
pub fn sanitize_disclosable(text: &str, max_bytes: usize) -> String {
    let mut out = String::with_capacity(text.len().min(max_bytes));
    let mut budget = max_bytes;
    let mut truncated = false;
    for ch in text.chars() {
        // C0 (incl. ESC, CR, LF, TAB), DEL, and C1 (U+0080..=U+009F). Newlines
        // go too: a transparency row is one line by contract, and a smuggled
        // newline is how a peer forges a second row.
        if ch.is_control() || ('\u{80}'..='\u{9f}').contains(&ch) {
            continue;
        }
        let width = ch.len_utf8();
        if width > budget {
            truncated = true;
            break;
        }
        budget -= width;
        out.push(ch);
    }
    if truncated {
        out.push_str(TRUNCATION_MARKER);
    }
    out
}

/// Project one journal line into a transparency row.
///
/// *Optional-finding* decision core (Story 18.0 idiom): `None` means "this
/// line is not a disclosable A2A interaction", which is the overwhelming
/// majority of a room journal. Effect-free, sync, no I/O, no clock.
///
/// Folds over [`JournalEntry`], **not** over `OrchestrationRoom`: the room
/// projection discards `AdmissionDeferred` entirely, so a room-backed viewer
/// would be blind to every pending-approval and status-query record.
#[must_use]
pub fn transparency_row(entry: &JournalEntry) -> Option<TransparencyRow> {
    let JournalRecord::Room(event) = &entry.record else {
        return None;
    };
    let recorded_at_ms = entry.has_timestamp().then_some(entry.recorded_at_ms);
    let (kind, direction, peer, task, summary) = match event {
        RoomEvent::RemoteEnvelopeAccepted {
            peer,
            node,
            direction,
            task,
            ..
        } => {
            let task = task
                .clone()
                .or_else(|| legacy_task_id_from_node(node.as_str()));
            (
                TransparencyKind::Accepted,
                *direction,
                peer.as_str().to_owned(),
                task,
                "task accepted and executing".to_owned(),
            )
        }
        RoomEvent::RemoteEnvelopeRejected {
            peer,
            reason,
            direction,
            task,
        } => (
            TransparencyKind::Rejected,
            *direction,
            peer.as_str().to_owned(),
            task.clone(),
            reject_reason_summary(reason),
        ),
        RoomEvent::AdmissionDeferred { spoke, gate, .. } => {
            let (kind, task) = classify_a2a_gate(gate)?;
            let summary = match kind {
                TransparencyKind::StatusQueried => {
                    "peer queried task status (first observation)".to_owned()
                }
                _ => "task parked awaiting operator approval".to_owned(),
            };
            // Host-local admission records are inbound by construction: the
            // gate only exists because a peer asked this instance for
            // something.
            (kind, Direction::Inbound, spoke.clone(), Some(task), summary)
        }
        RoomEvent::PeerDisclosure {
            peer,
            node: _,
            task,
            disclosed_bytes,
        } => (
            TransparencyKind::Disclosed,
            Direction::Outbound,
            peer.as_ref()
                .map(|peer| peer.as_str().to_owned())
                .unwrap_or_else(|| "unknown-peer".to_owned()),
            task.clone(),
            format!("disclosed result to peer ({disclosed_bytes} bytes)"),
        ),
        // Retractions mutate the prior projected row in `fold_transparency`;
        // they never create a second visible row.
        RoomEvent::AutoResponseRetracted { .. } => return None,
        RoomEvent::Unrecognized => (
            TransparencyKind::Unknown,
            Direction::Unknown,
            "—".to_owned(),
            None,
            "unrecognised record — written by a newer build".to_owned(),
        ),
        _ => return None,
    };
    Some(TransparencyRow {
        seq: entry.seq,
        recorded_at_ms,
        direction,
        retracted_at_ms: None,
        kind,
        // The peer id is host-derived (a SHA-256 pseudonym), but sanitizing it
        // costs nothing and keeps the invariant "no row field is unsanitized".
        peer: sanitize_disclosable(&peer, MAX_PEER_ID_BYTES),
        task: task.map(|id| sanitize_disclosable(&id, MAX_PEER_ID_BYTES)),
        summary: sanitize_disclosable(&summary, MAX_SUMMARY_BYTES),
    })
}

/// Fold an ordered journal into the transparency rows every surface renders.
///
/// *Projection-fold* decision core: effect-free, sync, no I/O. The adapter
/// streams entries in; this function never opens a file.
#[must_use]
pub fn fold_transparency<'a>(
    entries: impl IntoIterator<Item = &'a JournalEntry>,
) -> Vec<TransparencyRow> {
    let mut rows: Vec<TransparencyRow> = Vec::new();
    let mut positions = std::collections::HashMap::<u64, usize>::new();
    for entry in entries {
        if let JournalRecord::Room(RoomEvent::AutoResponseRetracted {
            target_seq,
            retracted_at_ms,
        }) = &entry.record
        {
            if let Some(index) = positions.get(target_seq).copied()
                && rows[index].retracted_at_ms.is_none()
            {
                rows[index].retracted_at_ms = if *retracted_at_ms == 0 {
                    entry.has_timestamp().then_some(entry.recorded_at_ms)
                } else {
                    Some(*retracted_at_ms)
                };
            }
            continue;
        }
        if let Some(row) = transparency_row(entry) {
            // AC5 retractions name PeerDisclosure rows. Never turn an
            // admission, refusal, or other projection into a retraction target.
            if row.kind == TransparencyKind::Disclosed {
                positions.insert(row.seq, rows.len());
            }
            rows.push(row);
        }
    }
    rows
}
/// Row filter shared by every transparency renderer.
///
/// Grammar: `direction=inbound|outbound|unknown`,
/// `kind=accepted|refused|awaiting-approval|status-query|disclosed|unknown`,
/// `peer=<substring>`, or a bare substring matched against the whole row.
/// `direction` and `kind` may appear once; repeated peer and bare-text terms
/// are ANDed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TransparencyFilter {
    terms: Vec<TransparencyFilterTerm>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum TransparencyFilterTerm {
    Direction(Direction),
    Kind(TransparencyKind),
    Peer(String),
    Text(String),
}

impl TransparencyFilter {
    /// Parse a non-empty transparency filter.
    ///
    /// # Errors
    ///
    /// Returns an operator-facing error for an empty, malformed, or unknown
    /// term. Refusing an invalid filter is safer than silently returning every
    /// row.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let mut terms = Vec::new();
        let mut saw_direction = false;
        let mut saw_kind = false;
        for term in spec.split_whitespace() {
            let Some((key, value)) = term.split_once('=') else {
                terms.push(TransparencyFilterTerm::Text(term.to_ascii_lowercase()));
                continue;
            };
            match key {
                "direction" => {
                    if saw_direction {
                        return Err("direction filter may appear only once".to_owned());
                    }
                    saw_direction = true;
                    terms.push(TransparencyFilterTerm::Direction(match value {
                        "inbound" => Direction::Inbound,
                        "outbound" => Direction::Outbound,
                        "unknown" => Direction::Unknown,
                        _ => {
                            return Err(format!(
                                "unknown direction `{value}` — valid: inbound, outbound, unknown"
                            ));
                        }
                    }));
                }
                "kind" => {
                    if saw_kind {
                        return Err("kind filter may appear only once".to_owned());
                    }
                    saw_kind = true;
                    terms.push(TransparencyFilterTerm::Kind(match value {
                        "accepted" => TransparencyKind::Accepted,
                        "refused" => TransparencyKind::Rejected,
                        "awaiting-approval" => TransparencyKind::AwaitingApproval,
                        "status-query" => TransparencyKind::StatusQueried,
                        "disclosed" => TransparencyKind::Disclosed,
                        "unknown" => TransparencyKind::Unknown,
                        _ => {
                            return Err(format!(
                                "unknown kind `{value}` — valid: accepted, refused, \
                                 awaiting-approval, status-query, disclosed, unknown"
                            ));
                        }
                    }));
                }
                "peer" if !value.is_empty() => {
                    terms.push(TransparencyFilterTerm::Peer(value.to_ascii_lowercase()));
                }
                "peer" => return Err("peer filter must not be empty".to_owned()),
                _ => {
                    return Err(format!(
                        "unknown filter key `{key}` — valid: direction, kind, peer, or a bare \
                         substring"
                    ));
                }
            }
        }
        if terms.is_empty() {
            return Err("filter must contain at least one term".to_owned());
        }
        Ok(Self { terms })
    }

    #[must_use]
    pub fn matches(&self, row: &TransparencyRow) -> bool {
        self.terms.iter().all(|term| match term {
            TransparencyFilterTerm::Direction(want) => *want == row.direction,
            TransparencyFilterTerm::Kind(want) => *want == row.kind,
            TransparencyFilterTerm::Peer(want) => row.peer.to_ascii_lowercase().contains(want),
            TransparencyFilterTerm::Text(want) => {
                row.one_line().to_ascii_lowercase().contains(want)
            }
        })
    }
}

/// Render the rows as the `transparency.jsonl` export body.
///
/// One JSON object per line, `seq`-ordered. Regenerated whole on every export
/// — never appended — which is what makes a re-export byte-identical to the
/// previous good one for an unchanged journal.
#[must_use]
pub fn render_export(rows: &[TransparencyRow]) -> String {
    let mut out = String::new();
    for row in rows {
        let value = serde_json::json!({
            "seq": row.seq,
            "recordedAtMs": row.recorded_at_ms,
            "direction": row.direction.label(),
            "kind": row.kind.label(),
            "peer": row.peer,
            "task": row.task,
            "summary": row.summary,
            "structural_replay": STRUCTURAL_REPLAY_CLAIM,
        });
        out.push_str(&value.to_string());
        out.push('\n');
    }
    out
}

/// One point-in-time projection of the transparency journal.
#[derive(Clone, Debug, Default)]
pub struct TransparencyReport {
    /// Rows in journal order.
    pub rows: Vec<TransparencyRow>,
    /// Structural divergences in the source journal.
    pub findings: Vec<StructuralDivergence>,
}

impl TransparencyReport {
    /// `true` only when the source journal is structurally replayable.
    #[must_use]
    pub fn is_structurally_consistent(&self) -> bool {
        self.findings.is_empty()
    }

    /// Operator-facing structural-divergence summary, if any.
    #[must_use]
    pub fn structural_divergence_report(&self) -> Option<String> {
        (!self.findings.is_empty()).then(|| {
            let detail = self
                .findings
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ");
            format!(
                "source journal has structural divergences; output is not structurally \
                 replayable: {detail}"
            )
        })
    }
}

/// What structural replay validation found in the source journal (AC7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StructuralDivergence {
    /// The journal's schema version is not one this build can structurally
    /// replay.
    UnsupportedSchema { seq: u64, found: u32 },
    /// `seq` is not contiguous from 1.
    SequenceGap { expected: u64, found: u64 },
    /// A persisted nonlegacy timestamp descends while `seq` advances.
    TimestampDescent {
        seq: u64,
        recorded_at_ms: i64,
        previous_seq: u64,
        previous_recorded_at_ms: i64,
    },
}

impl std::fmt::Display for StructuralDivergence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedSchema { seq, found } => write!(
                f,
                "entry {seq} declares schema version {found}, this build reads {}",
                node_journal::NODE_JOURNAL_SCHEMA_VERSION
            ),
            Self::SequenceGap { expected, found } => {
                write!(
                    f,
                    "sequence discontinuity: expected {expected}, found {found}"
                )
            }
            Self::TimestampDescent {
                seq,
                recorded_at_ms,
                previous_seq,
                previous_recorded_at_ms,
            } => write!(
                f,
                "persisted timestamp descends: seq {previous_seq}@{previous_recorded_at_ms}ms \
                 precedes seq {seq}@{recorded_at_ms}ms"
            ),
        }
    }
}

/// Renderer-facing result of replacing `transparency.jsonl`.
#[derive(Clone, Debug)]
pub struct TransparencyExport {
    pub path: PathBuf,
    /// Actual unfiltered row count in the snapshot written to disk.
    pub rows: usize,
    pub findings: Vec<StructuralDivergence>,
}

/// Validate only the durable stream's replay structure (AC7).
///
/// This detects unsupported schema versions, sequence discontinuities, and a
/// descent in persisted nonlegacy timestamps. It does not establish payload
/// preservation, protect against suffix deletion or rollback, validate
/// recomputed metadata, or authenticate content provenance.
///
/// An atomic `JournalRecord::Batch` line is flattened into N entries that share
/// one `seq`, so repeats of the immediately preceding `seq` are legal. Equal
/// timestamps and legacy timestamp zero are legal; only a descending
/// nonlegacy timestamp is a structural divergence.
#[must_use]
pub fn validate_replay_structure(entries: &[JournalEntry]) -> Vec<StructuralDivergence> {
    let mut findings = Vec::new();
    let mut expected_seq = 1u64;
    let mut previous_nonlegacy_timestamp: Option<(u64, i64)> = None;
    for entry in entries {
        if entry.schema_version != node_journal::NODE_JOURNAL_SCHEMA_VERSION {
            findings.push(StructuralDivergence::UnsupportedSchema {
                seq: entry.seq,
                found: entry.schema_version,
            });
        }
        // `expected_seq - 1` is the previous line's `seq`: a flattened batch
        // repeats it legitimately.
        let repeats_batch_line = expected_seq > 1 && entry.seq == expected_seq - 1;
        if entry.seq != expected_seq && !repeats_batch_line {
            findings.push(StructuralDivergence::SequenceGap {
                expected: expected_seq,
                found: entry.seq,
            });
        }
        expected_seq = entry.seq.saturating_add(1);
        if entry.has_timestamp()
            && let Some((previous_seq, previous_recorded_at_ms)) = previous_nonlegacy_timestamp
            && entry.recorded_at_ms < previous_recorded_at_ms
        {
            findings.push(StructuralDivergence::TimestampDescent {
                seq: entry.seq,
                recorded_at_ms: entry.recorded_at_ms,
                previous_seq,
                previous_recorded_at_ms,
            });
        }
        if entry.has_timestamp() {
            previous_nonlegacy_timestamp = Some((entry.seq, entry.recorded_at_ms));
        }
    }
    findings
}

/// `a2a-inbound-approval:{task}` / `a2a-status-query:{task}` gate strings are
/// the shipped stringly-encoded vocabulary for host-local A2A admission
/// records. Anything else is not a transparency record.
fn classify_a2a_gate(gate: &str) -> Option<(TransparencyKind, String)> {
    if let Some(task) = gate.strip_prefix("a2a-inbound-approval:") {
        return Some((TransparencyKind::AwaitingApproval, task.to_owned()));
    }
    if let Some(task) = gate.strip_prefix("a2a-status-query:") {
        return Some((TransparencyKind::StatusQueried, task.to_owned()));
    }
    None
}

/// Recover a plain legacy A2A task suffix from a node id.
///
/// Production node ids encode remote identifiers as `p-<base64url>/t-<base64url>`.
/// That representation is internal routing metadata, not an original remote
/// task id, so it is never disclosed as one. New events carry their original
/// task id explicitly; only pre-18.2 plain suffixes are recovered here.
fn legacy_task_id_from_node(node: &str) -> Option<String> {
    let rest = node
        .strip_prefix("a2a-in/")
        .or_else(|| node.strip_prefix("a2a/"))?;
    let (peer, task) = rest.split_once('/')?;
    if peer.starts_with("p-") && task.starts_with("t-") {
        return None;
    }
    (!task.is_empty() && !task.contains('/')).then(|| task.to_owned())
}

/// Fixed, host-authored summaries. `Policy { detail }` is the one arm carrying
/// free text; every caller of this function sanitizes the result.
fn reject_reason_summary(reason: &RejectReason) -> String {
    match reason {
        RejectReason::Policy { detail } => detail.clone(),
        RejectReason::InvalidSignature => "refused: invalid signature".to_owned(),
        RejectReason::Expired => "refused: envelope expired".to_owned(),
        RejectReason::Replay => "refused: replayed envelope".to_owned(),
        RejectReason::UnknownRecipient => "refused: unknown recipient".to_owned(),
        RejectReason::Malformed => "refused: malformed envelope".to_owned(),
    }
}

/// `YYYY-MM-DD HH:MM:SS` in UTC from unix milliseconds, without pulling a date
/// crate into `domain/` (which may depend only on serde/thiserror/async-trait/
/// tracing).
#[must_use]
pub fn format_unix_millis(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}Z",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// Howard Hinnant's `civil_from_days` — days since 1970-01-01 to (y, m, d).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{AgentId, ContentHash, PeerId};

    fn peer() -> PeerId {
        PeerId::from_public_key(&[7u8; 32]).unwrap()
    }

    fn room_entry(seq: u64, recorded_at_ms: i64, event: RoomEvent) -> JournalEntry {
        JournalEntry::new(seq, JournalRecord::Room(event), recorded_at_ms)
    }

    #[test]
    fn strips_c0_and_c1_but_keeps_legitimate_unicode() {
        let hostile = "task\x1b[2K\rid\u{9b}31m";
        assert_eq!(sanitize_disclosable(hostile, 64), "task[2Kid31m");

        // Positive control: RTL marks, combining marks, CJK and emoji all
        // survive byte-for-byte. A strip that mangles legitimate text is a
        // different bug wearing the same fix.
        let legitimate = "مرحبا e\u{301}\u{200f} 日本語 🚀";
        assert_eq!(sanitize_disclosable(legitimate, 256), legitimate);
    }

    #[test]
    fn truncation_is_visible_never_silent() {
        let long = "a".repeat(600);
        let out = sanitize_disclosable(&long, 16);
        assert_eq!(out, format!("{}{TRUNCATION_MARKER}", "a".repeat(16)));
        assert!(out.ends_with(TRUNCATION_MARKER));
    }

    #[test]
    fn truncation_never_splits_a_codepoint() {
        // 3-byte chars against a 5-byte budget: one fits whole, the second
        // must be dropped rather than halved (a split codepoint would make the
        // export invalid UTF-8).
        assert_eq!(
            sanitize_disclosable("日本語", 5),
            format!("日{TRUNCATION_MARKER}")
        );
        assert_eq!(
            sanitize_disclosable("日本語", 6),
            format!("日本{TRUNCATION_MARKER}")
        );
        assert_eq!(sanitize_disclosable("日本語", 9), "日本語");
    }

    #[test]
    fn pre_18_2_entry_renders_unknown_time_never_epoch_zero() {
        let entry = room_entry(
            1,
            0,
            RoomEvent::RemoteEnvelopeRejected {
                peer: peer(),
                reason: RejectReason::Policy {
                    detail: "nope".to_owned(),
                },
                direction: Direction::Unknown,
                task: None,
            },
        );
        let row = transparency_row(&entry).expect("rejection projects");
        assert_eq!(row.recorded_at_ms, None);
        assert_eq!(row.timestamp_label(), "—");
        assert_eq!(row.direction, Direction::Unknown);
        assert_eq!(row.direction.label(), "unknown");
    }

    #[test]
    fn unrecognised_variant_renders_a_row_never_a_drop() {
        let entry = room_entry(1, 5, RoomEvent::Unrecognized);
        let row = transparency_row(&entry).expect("unknown records still render");
        assert_eq!(row.kind, TransparencyKind::Unknown);
        assert!(row.summary.contains("unrecognised"));
    }

    #[test]
    fn duplicate_retractions_mark_disclosure_once_and_add_no_rows() {
        let entries = [
            room_entry(
                1,
                1_700_000_000_000,
                RoomEvent::PeerDisclosure {
                    peer: Some(peer()),
                    node: AgentId::from_validated("peer-response-node"),
                    task: Some("remote-task".to_owned()),
                    disclosed_bytes: 42,
                },
            ),
            room_entry(
                2,
                1_700_000_060_000,
                RoomEvent::AutoResponseRetracted {
                    target_seq: 1,
                    retracted_at_ms: 1_700_000_060_000,
                },
            ),
            room_entry(
                3,
                1_700_000_120_000,
                RoomEvent::AutoResponseRetracted {
                    target_seq: 1,
                    retracted_at_ms: 1_700_000_060_000,
                },
            ),
        ];

        let rows = fold_transparency(&entries);
        assert_eq!(
            rows.len(),
            1,
            "retraction appends must not duplicate or delete"
        );
        let row = &rows[0];
        assert_eq!(row.seq, 1);
        assert_eq!(row.retracted_at_ms, Some(1_700_000_060_000));
        assert_eq!(row.task.as_deref(), Some("remote-task"));
        assert!(row.summary.contains("42 bytes"));
        assert!(row.one_line().contains("[retracted 2023-11-14 22:14:20Z]"));
    }

    #[test]
    fn retract_fold_ignores_non_disclosure_target() {
        let entries = [
            room_entry(
                1,
                1_700_000_000_000,
                RoomEvent::RemoteEnvelopeAccepted {
                    peer: peer(),
                    node: AgentId::from_validated("a2a-in/peer/task"),
                    content_hash: ContentHash::from_bytes([0u8; 32]),
                    direction: Direction::Inbound,
                    task: Some("remote-task".to_owned()),
                },
            ),
            room_entry(
                2,
                1_700_000_060_000,
                RoomEvent::AutoResponseRetracted {
                    target_seq: 1,
                    retracted_at_ms: 1_700_000_060_000,
                },
            ),
        ];

        let rows = fold_transparency(&entries);
        assert_eq!(rows.len(), 1, "retraction entries never render rows");
        assert_eq!(rows[0].kind, TransparencyKind::Accepted);
        assert_eq!(rows[0].retracted_at_ms, None);
    }

    #[test]
    fn transparency_prefers_persisted_task_and_only_recovers_plain_legacy_suffixes() {
        for (node, expected) in [
            ("a2a-in/submitter/task-7", Some("task-7")),
            ("a2a/peer-x/task-9", Some("task-9")),
            // Production node ids encode both segments; their opaque task
            // segment is routing metadata, not an original remote task id.
            ("a2a-in/p-c3VibWl0dGVy/t-cmVtb3RlLXRhc2s", None),
        ] {
            let entry = room_entry(
                1,
                5,
                RoomEvent::RemoteEnvelopeAccepted {
                    peer: peer(),
                    node: AgentId::from_validated(node.to_owned()),
                    content_hash: ContentHash::from_bytes([0u8; 32]),
                    direction: Direction::Inbound,
                    task: None,
                },
            );
            let row = transparency_row(&entry).expect("accept projects");
            assert_eq!(row.task.as_deref(), expected);
        }

        let entry = room_entry(
            1,
            5,
            RoomEvent::RemoteEnvelopeAccepted {
                peer: peer(),
                node: AgentId::from_validated("a2a-in/p-c3VibWl0dGVy/t-cmVtb3RlLXRhc2s".to_owned()),
                content_hash: ContentHash::from_bytes([0u8; 32]),
                direction: Direction::Inbound,
                task: Some("original-remote-task".to_owned()),
            },
        );
        assert_eq!(
            transparency_row(&entry)
                .expect("accept projects")
                .task
                .as_deref(),
            Some("original-remote-task")
        );
    }

    #[test]
    fn only_a2a_admission_gates_project() {
        let deferred = |gate: &str| {
            room_entry(
                1,
                5,
                RoomEvent::AdmissionDeferred {
                    coordinator: AgentId::root(),
                    spoke: "peer".to_owned(),
                    gate: gate.to_owned(),
                },
            )
        };
        assert_eq!(
            transparency_row(&deferred("a2a-inbound-approval:t1")).map(|r| r.kind),
            Some(TransparencyKind::AwaitingApproval)
        );
        assert_eq!(
            transparency_row(&deferred("a2a-status-query:t1")).map(|r| r.kind),
            Some(TransparencyKind::StatusQueried)
        );
        // A supervisor flood-control defer is not an A2A disclosure.
        assert!(transparency_row(&deferred("fork-join-rate")).is_none());
    }

    #[test]
    fn peer_disclosure_projects_as_an_outbound_distinct_row() {
        let entry = room_entry(
            7,
            5,
            RoomEvent::PeerDisclosure {
                peer: Some(peer()),
                node: AgentId::from_validated("a2a-in/p-peer/t-task".to_owned()),
                task: Some("remote-task".to_owned()),
                disclosed_bytes: 42,
            },
        );
        let row = transparency_row(&entry).expect("disclosure projects");
        assert_eq!(row.direction, Direction::Outbound);
        assert_eq!(row.kind, TransparencyKind::Disclosed);
        assert_eq!(row.task.as_deref(), Some("remote-task"));
        assert_eq!(row.peer, peer().as_str());
        assert_eq!(row.summary, "disclosed result to peer (42 bytes)");
        assert_eq!(row.kind.glyph(), "⇢");
        assert_eq!(row.kind.label(), "disclosed");
        assert!(
            TransparencyFilter::parse("kind=disclosed")
                .expect("disclosed filter parses")
                .matches(&row)
        );
    }

    /// Regression: `flatten_batches` gives every record on an atomic
    /// `JournalRecord::Batch` line the SAME `seq`. A naive contiguity check
    /// would mark every parked-node journal structurally divergent.
    #[test]
    fn a_flattened_atomic_batch_is_not_a_sequence_gap() {
        let entries = vec![
            room_entry(1, 10, RoomEvent::Unrecognized),
            // One batch line, three flattened records, one shared seq.
            room_entry(2, 11, RoomEvent::Unrecognized),
            room_entry(2, 11, RoomEvent::Unrecognized),
            room_entry(2, 11, RoomEvent::Unrecognized),
            room_entry(3, 12, RoomEvent::Unrecognized),
        ];
        assert!(
            validate_replay_structure(&entries).is_empty(),
            "a shared batch seq is legal: {:?}",
            validate_replay_structure(&entries)
        );

        // …but a genuine gap after a batch is still caught.
        let mut torn = entries.clone();
        torn.pop();
        torn.push(room_entry(5, 13, RoomEvent::Unrecognized));
        assert!(matches!(
            validate_replay_structure(&torn).as_slice(),
            [StructuralDivergence::SequenceGap {
                expected: 3,
                found: 5
            }]
        ));
    }

    #[test]
    fn validate_replay_structure_detects_structural_divergence() {
        let clean = vec![
            room_entry(1, 10, RoomEvent::Unrecognized),
            room_entry(2, 11, RoomEvent::Unrecognized),
            room_entry(3, 12, RoomEvent::Unrecognized),
        ];
        assert!(validate_replay_structure(&clean).is_empty());

        // A line removed from the middle: seq is no longer contiguous.
        let truncated = vec![clean[0].clone(), clean[2].clone()];
        assert!(matches!(
            validate_replay_structure(&truncated).as_slice(),
            [StructuralDivergence::SequenceGap {
                expected: 2,
                found: 3
            }]
        ));

        // A timestamp edited backwards while seq advances — structurally
        // impossible for correct code, which is exactly why it is evidence.
        let mut tampered = clean.clone();
        tampered[2].recorded_at_ms = 1;
        assert!(matches!(
            validate_replay_structure(&tampered).as_slice(),
            [StructuralDivergence::TimestampDescent { seq: 3, .. }]
        ));
    }

    #[test]
    fn equal_millis_are_legal() {
        let entries = vec![
            room_entry(1, 10, RoomEvent::Unrecognized),
            room_entry(2, 10, RoomEvent::Unrecognized),
        ];
        assert!(validate_replay_structure(&entries).is_empty());
    }

    #[test]
    fn legacy_zero_is_legal_but_nonlegacy_timestamp_descents_are_not() {
        let legacy_then_earlier_wall_clock = vec![
            room_entry(1, 0, RoomEvent::Unrecognized),
            room_entry(2, -1, RoomEvent::Unrecognized),
        ];
        assert!(
            validate_replay_structure(&legacy_then_earlier_wall_clock).is_empty(),
            "zero is a legacy marker, not a persisted timestamp"
        );

        let descending_nonlegacy = vec![
            room_entry(1, 10, RoomEvent::Unrecognized),
            room_entry(2, 0, RoomEvent::Unrecognized),
            room_entry(3, 9, RoomEvent::Unrecognized),
        ];
        assert!(matches!(
            validate_replay_structure(&descending_nonlegacy).as_slice(),
            [StructuralDivergence::TimestampDescent { seq: 3, .. }]
        ));
    }

    #[test]
    fn filters_reject_invalid_single_values_and_and_remaining_terms() {
        let row = TransparencyRow {
            seq: 1,
            recorded_at_ms: Some(1),
            retracted_at_ms: None,
            direction: Direction::Inbound,
            kind: TransparencyKind::Accepted,
            peer: "peer-a".to_owned(),
            task: Some("task-a".to_owned()),
            summary: "accepted".to_owned(),
        };
        for spec in ["", " \t", "peer=", "direction=", "kind="] {
            assert!(
                TransparencyFilter::parse(spec).is_err(),
                "{spec:?} must fail"
            );
        }
        for spec in [
            "direction=inbound direction=inbound",
            "direction=inbound direction=outbound",
            "kind=accepted kind=accepted",
            "kind=accepted kind=refused",
        ] {
            assert!(
                TransparencyFilter::parse(spec).is_err(),
                "{spec:?} must reject a duplicate single-valued filter"
            );
        }
        assert!(
            TransparencyFilter::parse("peer-a accepted")
                .expect("multiple bare terms parse")
                .matches(&row)
        );
        assert!(
            !TransparencyFilter::parse("missing accepted")
                .expect("multiple bare terms parse")
                .matches(&row),
            "multiple bare terms are ANDed rather than overwritten"
        );
        assert!(
            !TransparencyFilter::parse("peer=peer peer=other")
                .expect("repeated peer terms parse")
                .matches(&row),
            "repeated peer terms are ANDed rather than overwritten"
        );
    }

    #[test]
    fn export_is_one_line_per_row_and_regenerates_identically() {
        let rows = fold_transparency(&[
            room_entry(
                1,
                1_700_000_000_000,
                RoomEvent::RemoteEnvelopeAccepted {
                    peer: peer(),
                    node: AgentId::from_validated("a2a-in/s/t-1".to_owned()),
                    content_hash: ContentHash::from_bytes([0u8; 32]),
                    direction: Direction::Inbound,
                    task: Some("inbound-task".to_owned()),
                },
            ),
            room_entry(
                2,
                1_700_000_000_001,
                RoomEvent::RemoteEnvelopeRejected {
                    peer: peer(),
                    reason: RejectReason::Policy {
                        detail: "policy".to_owned(),
                    },
                    direction: Direction::Outbound,
                    task: Some("outbound-task".to_owned()),
                },
            ),
        ]);
        let first = render_export(&rows);
        assert_eq!(first.lines().count(), 2);
        assert_eq!(first, render_export(&rows));
        assert!(first.contains("\"direction\":\"inbound\""));
        assert!(first.contains("\"direction\":\"outbound\""));
        assert!(first.contains("\"structural_replay\""));
    }

    #[test]
    fn format_unix_millis_is_utc_civil() {
        assert_eq!(format_unix_millis(0), "1970-01-01 00:00:00Z");
        assert_eq!(
            format_unix_millis(1_700_000_000_000),
            "2023-11-14 22:13:20Z"
        );
    }
}
