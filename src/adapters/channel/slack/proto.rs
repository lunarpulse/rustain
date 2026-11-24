//! Slack Socket Mode wire handling, pure and network-free (Story C-12 Task 0/2).
//!
//! Documented contract this module pins (api.slack.com, Socket Mode and the
//! Events API `message.im` event):
//!
//! * the server speaks first with `{"type":"hello",...}`;
//! * every deliverable arrives as an *envelope* `{"envelope_id":"...","type":
//!   "events_api"|"interactive"|"slash_commands","payload":{...}}` that must be
//!   acknowledged with `{"envelope_id":"..."}` within three seconds, or Slack
//!   redelivers it; an acknowledgement says only "received", never "allowed";
//! * `{"type":"disconnect","reason":"..."}` asks the client to reconnect;
//! * a direct message to the app is an `events_api` envelope whose
//!   `payload.event` is `{"type":"message","channel_type":"im","channel":"D…",
//!   "user":"U…","text":"…","ts":"…"}` with no `subtype` and no `bot_id`.
//!
//! Admission policy (fail closed, in this order): envelope and payload shape,
//! event type, DM channel type and `D…` channel, no subtype, no bot marker, a
//! sender user ID, the exact-match allowlist, not our own bot user, a message
//! identity not seen before, and non-empty sanitized text. Everything that is
//! not admitted is acknowledged (when it carries an `envelope_id`) and dropped:
//! no dispatch, no reply, no context row. A denied DM's text is never read.

use std::collections::{HashSet, VecDeque};

use serde_json::{Value, json};

use crate::adapters::channel::sanitize::sanitize_inbound_text;

/// Largest frame this module will parse. Slack events are a few KiB; the
/// transport allows twice this so an oversize frame is dropped, not fatal.
pub const MAX_FRAME_BYTES: usize = 256 * 1024;
/// Longest inbound DM text handed to the agent (characters), as for Telegram.
pub const MAX_SLACK_INPUT_LEN: usize = 8192;
/// Recently admitted message identities remembered to drop redeliveries.
pub const DEDUPE_CAPACITY: usize = 256;
/// Longest single `chat.postMessage` text this adapter sends (characters).
pub const SLACK_POST_CHUNK_CHARS: usize = 3000;
/// Longest whole reply (characters); the remainder is cut with a marker.
pub const SLACK_MAX_REPLY_CHARS: usize = 30_000;
const TRUNCATION_MARKER: &str = "\n[reply truncated]";

/// Why a frame produced no dispatch. Never carries user text, IDs or tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// Larger than [`MAX_FRAME_BYTES`].
    TooLarge,
    /// Not JSON, or not a JSON object.
    Malformed,
    /// An envelope type this adapter does not serve (`interactive`, commands).
    UnsupportedEnvelope,
    /// An `events_api` payload that is not an `event_callback`.
    NotEventCallback,
    /// An event that is not a `message`.
    NotMessage,
    /// A message outside an app DM (public, private, group, MPIM).
    NotDirectMessage,
    /// A message with a `subtype` (edits, deletions, bot posts, file shares).
    Subtype,
    /// A message carrying a bot marker.
    Bot,
    /// A message authored by this app's own bot user.
    SelfEcho,
    /// A required field is missing or has the wrong type or shape.
    MalformedEvent,
    /// A sender who is not on the allowlist.
    UnknownSender,
    /// A message identity already admitted (a redelivery).
    Duplicate,
    /// Nothing left after sanitization.
    EmptyText,
}

/// One admitted DM: the sanitized text and the route its reply must take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundMessage {
    /// The admitted DM conversation (`D…`): the only place the reply goes.
    pub channel: String,
    pub text: String,
}

/// What the connection loop must do with one received frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// The server's greeting: the connection is usable.
    Hello,
    /// The server asks for a reconnect.
    Disconnect,
    /// An admitted DM for the turn queue.
    Dispatch(InboundMessage),
    /// Nothing to dispatch.
    Drop(RejectReason),
    /// A frame type that needs no action (for example `slack_connections` info).
    Ignore,
}

/// The result of handling one frame: an optional acknowledgement to send first
/// (it is sent before any dispatch and never depends on admission), and the action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handled {
    pub ack: Option<String>,
    pub action: Action,
}

/// Bounded memory of recently admitted message identities.
#[derive(Debug)]
pub struct DedupeWindow {
    order: VecDeque<String>,
    seen: HashSet<String>,
    capacity: usize,
}

impl DedupeWindow {
    pub fn new(capacity: usize) -> Self {
        Self {
            order: VecDeque::with_capacity(capacity),
            seen: HashSet::with_capacity(capacity),
            capacity: capacity.max(1),
        }
    }

    /// `true` when `key` was not remembered (and is now); the oldest key is
    /// evicted beyond the capacity, so the memory is bounded.
    pub fn insert(&mut self, key: String) -> bool {
        if self.seen.contains(&key) {
            return false;
        }
        if self.order.len() >= self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.seen.remove(&oldest);
            }
        }
        self.seen.insert(key.clone());
        self.order.push_back(key);
        true
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }
}

/// Handle one received text frame against the sender allowlist.
pub fn handle_frame(raw: &str, allowed: &[String], seen: &mut DedupeWindow) -> Handled {
    if raw.len() > MAX_FRAME_BYTES {
        return dropped(None, RejectReason::TooLarge);
    }
    let Ok(Value::Object(frame)) = serde_json::from_str::<Value>(raw) else {
        return dropped(None, RejectReason::Malformed);
    };
    let kind = frame.get("type").and_then(Value::as_str).unwrap_or("");
    let envelope_id = frame
        .get("envelope_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty() && id.len() <= 256);
    let ack = envelope_id.map(ack_frame);

    match (kind, envelope_id) {
        ("hello", _) => Handled {
            ack: None,
            action: Action::Hello,
        },
        ("disconnect", _) => Handled {
            ack: None,
            action: Action::Disconnect,
        },
        ("events_api", Some(_)) => {
            let action = match admit(frame.get("payload"), allowed, seen) {
                Ok(message) => Action::Dispatch(message),
                Err(reason) => Action::Drop(reason),
            };
            Handled { ack, action }
        }
        ("interactive" | "slash_commands", Some(_)) => {
            dropped(ack, RejectReason::UnsupportedEnvelope)
        }
        (_, Some(_)) => dropped(ack, RejectReason::UnsupportedEnvelope),
        ("events_api", None) => dropped(None, RejectReason::Malformed),
        _ => Handled {
            ack: None,
            action: Action::Ignore,
        },
    }
}

fn dropped(ack: Option<String>, reason: RejectReason) -> Handled {
    Handled {
        ack,
        action: Action::Drop(reason),
    }
}

/// The acknowledgement for one envelope.
pub fn ack_frame(envelope_id: &str) -> String {
    json!({ "envelope_id": envelope_id }).to_string()
}

fn admit(
    payload: Option<&Value>,
    allowed: &[String],
    seen: &mut DedupeWindow,
) -> Result<InboundMessage, RejectReason> {
    let payload = payload
        .and_then(Value::as_object)
        .ok_or(RejectReason::Malformed)?;
    if payload.get("type").and_then(Value::as_str) != Some("event_callback") {
        return Err(RejectReason::NotEventCallback);
    }
    let event = payload
        .get("event")
        .and_then(Value::as_object)
        .ok_or(RejectReason::Malformed)?;
    if event.get("type").and_then(Value::as_str) != Some("message") {
        return Err(RejectReason::NotMessage);
    }
    if event.get("channel_type").and_then(Value::as_str) != Some("im") {
        return Err(RejectReason::NotDirectMessage);
    }
    if event.get("subtype").is_some_and(|v| !v.is_null()) {
        return Err(RejectReason::Subtype);
    }
    if event.get("bot_id").is_some_and(|v| !v.is_null()) {
        return Err(RejectReason::Bot);
    }
    let channel = event
        .get("channel")
        .and_then(Value::as_str)
        .filter(|c| is_dm_channel_id(c))
        .ok_or(RejectReason::NotDirectMessage)?;
    let user = event
        .get("user")
        .and_then(Value::as_str)
        .ok_or(RejectReason::MalformedEvent)?;
    if !allowed.iter().any(|id| id == user) {
        return Err(RejectReason::UnknownSender);
    }
    let own_bot = payload
        .get("authorizations")
        .and_then(Value::as_array)
        .is_some_and(|auths| {
            auths.iter().any(|auth| {
                auth.get("user_id").and_then(Value::as_str) == Some(user)
                    && auth.get("is_bot").and_then(Value::as_bool) != Some(false)
            })
        });
    if own_bot {
        return Err(RejectReason::SelfEcho);
    }
    let ts = event
        .get("ts")
        .and_then(Value::as_str)
        .filter(|ts| !ts.is_empty() && ts.len() <= 64)
        .ok_or(RejectReason::MalformedEvent)?;
    let text = event
        .get("text")
        .and_then(Value::as_str)
        .ok_or(RejectReason::MalformedEvent)?;
    if !seen.insert(format!("{channel}:{ts}")) {
        return Err(RejectReason::Duplicate);
    }
    let text = sanitize_inbound_text(&unescape_slack_text(text), MAX_SLACK_INPUT_LEN, "slack");
    if text.is_empty() {
        return Err(RejectReason::EmptyText);
    }
    Ok(InboundMessage {
        channel: channel.to_string(),
        text,
    })
}

/// A Slack DM conversation ID: `D` and uppercase ASCII letters or digits.
pub fn is_dm_channel_id(channel: &str) -> bool {
    channel.len() >= 2
        && channel.len() <= 32
        && channel.starts_with('D')
        && channel
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// Slack escapes `&`, `<` and `>` in message text; undo exactly those three.
fn unescape_slack_text(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// Split one logical reply into ordered `chat.postMessage` texts of at most
/// [`SLACK_POST_CHUNK_CHARS`] characters, preferring line then word boundaries.
/// The whole reply is capped at [`SLACK_MAX_REPLY_CHARS`] characters. Nothing
/// is returned for a blank reply.
pub fn split_reply(text: &str) -> Vec<String> {
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }
    let mut owned;
    let mut rest = if text.chars().count() > SLACK_MAX_REPLY_CHARS {
        let cut = text
            .char_indices()
            .nth(SLACK_MAX_REPLY_CHARS)
            .map_or(text.len(), |(index, _)| index);
        owned = text[..cut].trim_end().to_string();
        owned.push_str(TRUNCATION_MARKER);
        owned.as_str()
    } else {
        text
    };
    let mut chunks = Vec::new();
    while !rest.is_empty() {
        if rest.chars().count() <= SLACK_POST_CHUNK_CHARS {
            chunks.push(rest.to_string());
            break;
        }
        let window_end = rest
            .char_indices()
            .nth(SLACK_POST_CHUNK_CHARS)
            .map_or(rest.len(), |(index, _)| index);
        let window = &rest[..window_end];
        let half = window_end / 2;
        let cut = window
            .rfind('\n')
            .filter(|&at| at >= half)
            .or_else(|| window.rfind(char::is_whitespace).filter(|&at| at >= half))
            .unwrap_or(window_end);
        let (head, tail) = rest.split_at(cut);
        let head = head.trim_end();
        if !head.is_empty() {
            chunks.push(head.to_string());
        }
        rest = tail.trim_start();
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed() -> Vec<String> {
        vec!["U111AAAA".to_string(), "U222BBBB".to_string()]
    }

    fn window() -> DedupeWindow {
        DedupeWindow::new(DEDUPE_CAPACITY)
    }

    fn envelope(event: Value) -> String {
        json!({
            "envelope_id": "env-1",
            "type": "events_api",
            "accepts_response_payload": false,
            "payload": {
                "type": "event_callback",
                "event_id": "Ev1",
                "authorizations": [{"user_id": "UBOT0001", "is_bot": true}],
                "event": event,
            },
        })
        .to_string()
    }

    fn dm(user: &str, text: &str, ts: &str) -> Value {
        json!({"type": "message", "channel_type": "im", "channel": "D0AAAA111",
               "user": user, "text": text, "ts": ts})
    }

    fn acked(handled: &Handled) -> bool {
        handled.ack.as_deref() == Some(r#"{"envelope_id":"env-1"}"#)
    }

    #[test]
    fn hello_and_disconnect_need_no_ack() {
        let mut seen = window();
        let hello = handle_frame(
            r#"{"type":"hello","num_connections":1}"#,
            &allowed(),
            &mut seen,
        );
        assert_eq!(
            hello,
            Handled {
                ack: None,
                action: Action::Hello
            }
        );
        let bye = handle_frame(
            r#"{"type":"disconnect","reason":"refresh_requested"}"#,
            &allowed(),
            &mut seen,
        );
        assert_eq!(
            bye,
            Handled {
                ack: None,
                action: Action::Disconnect
            }
        );
    }

    #[test]
    fn allowlisted_dm_is_acked_and_dispatched_with_its_own_dm_route() {
        let handled = handle_frame(
            &envelope(dm("U111AAAA", "hello there", "1.0001")),
            &allowed(),
            &mut window(),
        );
        assert!(acked(&handled));
        assert_eq!(
            handled.action,
            Action::Dispatch(InboundMessage {
                channel: "D0AAAA111".into(),
                text: "hello there".into(),
            })
        );
    }

    #[test]
    fn every_non_admitted_envelope_is_acked_and_never_dispatched() {
        let mut cases: Vec<(&str, Value, RejectReason)> = vec![
            (
                "unknown sender",
                dm("U999ZZZZ", "hi", "2.1"),
                RejectReason::UnknownSender,
            ),
            (
                "lowercase twin",
                dm("u111aaaa", "hi", "2.2"),
                RejectReason::UnknownSender,
            ),
            (
                "public channel",
                json!({"type":"message","channel_type":"channel","channel":"C0AAAA111",
                       "user":"U111AAAA","text":"hi","ts":"2.3"}),
                RejectReason::NotDirectMessage,
            ),
            (
                "group dm",
                json!({"type":"message","channel_type":"mpim","channel":"G0AAAA111",
                       "user":"U111AAAA","text":"hi","ts":"2.4"}),
                RejectReason::NotDirectMessage,
            ),
            (
                "im type but conversation id is not a DM",
                json!({"type":"message","channel_type":"im","channel":"C0AAAA111",
                       "user":"U111AAAA","text":"hi","ts":"2.5"}),
                RejectReason::NotDirectMessage,
            ),
            (
                "bot id",
                json!({"type":"message","channel_type":"im","channel":"D0AAAA111",
                       "user":"U111AAAA","bot_id":"B1","text":"hi","ts":"2.6"}),
                RejectReason::Bot,
            ),
            (
                "subtype",
                json!({"type":"message","subtype":"message_changed","channel_type":"im",
                       "channel":"D0AAAA111","user":"U111AAAA","text":"hi","ts":"2.7"}),
                RejectReason::Subtype,
            ),
            (
                "not a message",
                json!({"type":"reaction_added","user":"U111AAAA"}),
                RejectReason::NotMessage,
            ),
            (
                "numeric user",
                json!({"type":"message","channel_type":"im","channel":"D0AAAA111",
                       "user":42,"text":"hi","ts":"2.8"}),
                RejectReason::MalformedEvent,
            ),
            (
                "missing ts",
                json!({"type":"message","channel_type":"im","channel":"D0AAAA111",
                       "user":"U111AAAA","text":"hi"}),
                RejectReason::MalformedEvent,
            ),
            (
                "text is not a string",
                json!({"type":"message","channel_type":"im","channel":"D0AAAA111",
                       "user":"U111AAAA","text":["hi"],"ts":"2.9"}),
                RejectReason::MalformedEvent,
            ),
            (
                "empty after sanitization",
                dm("U111AAAA", " \u{7}\t ", "3.0"),
                RejectReason::EmptyText,
            ),
        ];
        for (name, event, reason) in cases.drain(..) {
            let handled = handle_frame(&envelope(event), &allowed(), &mut window());
            assert!(acked(&handled), "{name}: ack is sent for every envelope");
            assert_eq!(handled.action, Action::Drop(reason), "{name}");
        }
    }

    #[test]
    fn own_bot_echo_is_rejected_even_if_listed() {
        let mut list = allowed();
        list.push("UBOT0001".to_string());
        let handled = handle_frame(
            &envelope(dm("UBOT0001", "echo", "4.0")),
            &list,
            &mut window(),
        );
        assert!(acked(&handled));
        assert_eq!(handled.action, Action::Drop(RejectReason::SelfEcho));
    }

    #[test]
    fn denied_dm_text_is_never_admitted_or_remembered() {
        let mut seen = window();
        let handled = handle_frame(
            &envelope(dm("U999ZZZZ", "secret ask", "5.0")),
            &allowed(),
            &mut seen,
        );
        assert_eq!(handled.action, Action::Drop(RejectReason::UnknownSender));
        assert!(
            seen.is_empty(),
            "a denied sender cannot poison the dedupe window"
        );
    }

    #[test]
    fn replayed_identity_is_acked_but_dispatched_once() {
        let mut seen = window();
        let frame = envelope(dm("U111AAAA", "once", "6.0"));
        let first = handle_frame(&frame, &allowed(), &mut seen);
        let second = handle_frame(&frame, &allowed(), &mut seen);
        assert!(matches!(first.action, Action::Dispatch(_)));
        assert!(acked(&second));
        assert_eq!(second.action, Action::Drop(RejectReason::Duplicate));
    }

    #[test]
    fn dedupe_window_is_bounded_and_evicts_oldest_first() {
        let mut seen = DedupeWindow::new(3);
        for key in ["a", "b", "c", "d"] {
            assert!(seen.insert(key.to_string()));
        }
        assert_eq!(seen.len(), 3);
        assert!(seen.insert("a".to_string()), "oldest key was evicted");
        assert!(!seen.insert("d".to_string()));
    }

    #[test]
    fn envelopes_without_usable_ids_and_oversize_frames_are_dropped_without_ack() {
        let mut seen = window();
        let no_id = r#"{"type":"events_api","payload":{}}"#;
        let handled = handle_frame(no_id, &allowed(), &mut seen);
        assert_eq!(
            handled,
            Handled {
                ack: None,
                action: Action::Drop(RejectReason::Malformed)
            }
        );
        let handled = handle_frame("not json", &allowed(), &mut seen);
        assert_eq!(handled.action, Action::Drop(RejectReason::Malformed));
        let handled = handle_frame("[]", &allowed(), &mut seen);
        assert_eq!(handled.action, Action::Drop(RejectReason::Malformed));
        let huge = format!(r#"{{"type":"x","pad":"{}"}}"#, "a".repeat(MAX_FRAME_BYTES));
        let handled = handle_frame(&huge, &allowed(), &mut seen);
        assert_eq!(
            handled,
            Handled {
                ack: None,
                action: Action::Drop(RejectReason::TooLarge)
            }
        );
    }

    #[test]
    fn unsupported_envelope_types_are_acked_and_dropped() {
        for kind in ["interactive", "slash_commands", "something_new"] {
            let frame = json!({"envelope_id":"env-1","type":kind,"payload":{}}).to_string();
            let handled = handle_frame(&frame, &allowed(), &mut window());
            assert!(acked(&handled), "{kind}");
            assert_eq!(
                handled.action,
                Action::Drop(RejectReason::UnsupportedEnvelope)
            );
        }
        let info = handle_frame(
            r#"{"type":"slack_connections","count":1}"#,
            &allowed(),
            &mut window(),
        );
        assert_eq!(
            info,
            Handled {
                ack: None,
                action: Action::Ignore
            }
        );
    }

    #[test]
    fn non_event_callback_payload_is_acked_and_dropped() {
        let frame = json!({"envelope_id":"env-1","type":"events_api",
                           "payload":{"type":"url_verification"}})
        .to_string();
        let handled = handle_frame(&frame, &allowed(), &mut window());
        assert!(acked(&handled));
        assert_eq!(handled.action, Action::Drop(RejectReason::NotEventCallback));
    }

    #[test]
    fn inbound_text_is_unescaped_then_sanitized_and_bounded() {
        let handled = handle_frame(
            &envelope(dm(
                "U111AAAA",
                "[SYSTEM] a &lt;b&gt; &amp; \u{1b}[31mc",
                "7.0",
            )),
            &allowed(),
            &mut window(),
        );
        let Action::Dispatch(message) = handled.action else {
            panic!("expected dispatch");
        };
        assert_eq!(message.text, "a <b> & [31mc");
        let long = "x".repeat(MAX_SLACK_INPUT_LEN + 500);
        let handled = handle_frame(
            &envelope(dm("U111AAAA", &long, "7.1")),
            &allowed(),
            &mut window(),
        );
        let Action::Dispatch(message) = handled.action else {
            panic!("expected dispatch");
        };
        assert_eq!(message.text.chars().count(), MAX_SLACK_INPUT_LEN);
    }

    #[test]
    fn short_reply_is_one_chunk_and_blank_reply_is_none() {
        assert_eq!(split_reply("  hello  "), ["hello"]);
        assert!(split_reply(" \n ").is_empty());
    }

    #[test]
    fn long_reply_splits_in_order_within_the_chunk_bound() {
        let paragraph = "word ".repeat(300);
        let text = format!("{paragraph}\n{paragraph}\n{paragraph}\n{paragraph}");
        let chunks = split_reply(&text);
        assert!(chunks.len() >= 2);
        assert!(
            chunks
                .iter()
                .all(|c| !c.is_empty() && c.chars().count() <= SLACK_POST_CHUNK_CHARS)
        );
        let squash = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(squash(&chunks.join(" ")), squash(&text));
    }

    #[test]
    fn unbroken_text_is_hard_split_on_char_boundaries() {
        let text = "é".repeat(SLACK_POST_CHUNK_CHARS * 2 + 5);
        let chunks = split_reply(&text);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn oversize_reply_is_truncated_with_a_marker() {
        let text = "a".repeat(SLACK_MAX_REPLY_CHARS + 1000);
        let chunks = split_reply(&text);
        let total: usize = chunks.iter().map(|c| c.chars().count()).sum();
        assert!(total <= SLACK_MAX_REPLY_CHARS + TRUNCATION_MARKER.chars().count());
        assert!(chunks.last().unwrap().ends_with("[reply truncated]"));
    }

    #[test]
    fn dm_channel_grammar() {
        assert!(is_dm_channel_id("D0AAAA111"));
        for bad in ["", "D", "C0AAAA111", "d0aaaa111", "D0AAA-111", "G012345678"] {
            assert!(!is_dm_channel_id(bad), "{bad}");
        }
    }
}
