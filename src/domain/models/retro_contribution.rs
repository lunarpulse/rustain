//! Received-side retrospective facts and aggregate-only reports (Story 19.25).

use super::{
    ItemAddress, JournalEntry, JournalRecord, RoomEvent, semantic_message_type_metadata,
    semantic_message_type_tokens,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write;

pub const RETRO_REPLY_TEXT: &str = "Retrospective reply.";
pub const NOT_SHARED_RETRO: &str =
    "Not shared: message content, senders, your sends, conversations, plans, files, activity";
pub const RETRO_HONESTY_LINE: &str = "This report contains only what was consented. It does not report who contributed, or how many.";
pub const RETRO_WINDOW_MS: i64 = 14 * 24 * 60 * 60 * 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum RetroField {
    #[serde(rename = "received")]
    Received,
    #[serde(rename = "replyTimes")]
    ReplyTimes,
    #[serde(rename = "ackTimes")]
    AckTimes,
}

pub type RetroFieldSet = BTreeSet<RetroField>;

impl RetroField {
    pub const ALL: [Self; 3] = [Self::Received, Self::ReplyTimes, Self::AckTimes];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Received => "received",
            Self::ReplyTimes => "replyTimes",
            Self::AckTimes => "ackTimes",
        }
    }

    pub fn from_token(token: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|field| field.as_str() == token)
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Received => "received items",
            Self::ReplyTimes => "reply times",
            Self::AckTimes => "acknowledgement times",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetroContribution {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub received: BTreeMap<String, u32>,
    #[serde(default, rename = "replyTimes", skip_serializing_if = "Vec::is_empty")]
    pub reply_minutes: Vec<u32>,
    #[serde(default, rename = "ackTimes", skip_serializing_if = "Vec::is_empty")]
    pub ack_minutes: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetroPreview {
    pub contribution: RetroContribution,
    pub nothing_in_window: bool,
}

impl RetroContribution {
    pub fn nothing() -> Self {
        Self::default()
    }

    pub fn is_nothing(&self) -> bool {
        self.received.is_empty() && self.reply_minutes.is_empty() && self.ack_minutes.is_empty()
    }

    pub fn without(&self, fields: &RetroFieldSet) -> Self {
        Self {
            received: if fields.contains(&RetroField::Received) {
                BTreeMap::new()
            } else {
                self.received.clone()
            },
            reply_minutes: if fields.contains(&RetroField::ReplyTimes) {
                Vec::new()
            } else {
                self.reply_minutes.clone()
            },
            ack_minutes: if fields.contains(&RetroField::AckTimes) {
                Vec::new()
            } else {
                self.ack_minutes.clone()
            },
        }
    }

    pub fn to_wire(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("retrospective fields are JSON values")
    }

    /// One validator for served threads and journal folding.
    pub fn from_wire(value: &serde_json::Value) -> Option<Self> {
        let object = value.as_object()?;
        if object
            .keys()
            .any(|key| RetroField::from_token(key).is_none())
        {
            return None;
        }
        let mut result = Self::nothing();
        if let Some(received) = object.get("received") {
            let counts = received.as_object()?;
            if counts.is_empty() {
                return None;
            }
            for (token, count) in counts {
                if token != "other" && !semantic_message_type_tokens().any(|known| token == known) {
                    return None;
                }
                let count = count.as_u64()?;
                if !(1..=1_000_000).contains(&count) {
                    return None;
                }
                result.received.insert(token.clone(), count as u32);
            }
        }
        for (token, target) in [
            ("replyTimes", &mut result.reply_minutes),
            ("ackTimes", &mut result.ack_minutes),
        ] {
            if let Some(raw) = object.get(token) {
                let values = raw.as_array()?;
                if values.is_empty() || values.len() > 512 {
                    return None;
                }
                target.reserve(values.len());
                for value in values {
                    let value = value.as_u64()?;
                    if value > 20_160 || target.last().is_some_and(|last| u64::from(*last) > value)
                    {
                        return None;
                    }
                    target.push(value as u32);
                }
            }
        }
        Some(result)
    }

    /// Fold only received-side events fixed before this request's arrival.
    pub fn compose(entries: &[JournalEntry], request: &ItemAddress) -> RetroPreview {
        let arrival = entries.iter().find_map(|entry| match &entry.record {
            JournalRecord::Room(RoomEvent::RecipientItemReceived { address, .. })
                if address == request && entry.recorded_at_ms != 0 =>
            {
                Some(entry.recorded_at_ms)
            }
            _ => None,
        });
        let mut contribution = Self::nothing();
        let Some(arrival) = arrival else {
            return RetroPreview {
                contribution,
                nothing_in_window: true,
            };
        };
        let lower = arrival.saturating_sub(RETRO_WINDOW_MS);
        let mut items = HashMap::new();
        for entry in entries {
            if entry.recorded_at_ms == 0
                || entry.recorded_at_ms < lower
                || entry.recorded_at_ms >= arrival
            {
                continue;
            }
            if let JournalRecord::Room(RoomEvent::RecipientItemReceived {
                address,
                message_type,
                ..
            }) = &entry.record
            {
                items
                    .entry(address)
                    .or_insert((entry.recorded_at_ms, entry.seq, *message_type));
            }
        }
        let by_reply = items
            .keys()
            .map(|address| ((address.principal(), address.item()), *address))
            .collect::<HashMap<_, _>>();
        let mut replies = HashMap::new();
        let mut acknowledgements = HashMap::new();
        for entry in entries {
            let at = entry.recorded_at_ms;
            if at == 0 || at >= arrival {
                continue;
            }
            match &entry.record {
                JournalRecord::Room(RoomEvent::RecipientItemReplied {
                    address,
                    answers,
                    kind,
                    ..
                }) if kind.wire_name() != "retro" => {
                    // Principal plus item, never a bare id from another sender.
                    if let Some(key) = by_reply.get(&(address.principal(), answers)) {
                        if at >= items[key].0 {
                            replies.entry(*key).or_insert(at);
                        }
                    }
                }
                JournalRecord::Room(RoomEvent::RecipientItemAcknowledged { address, .. })
                    if items.get(address).is_some_and(|item| at >= item.0) =>
                {
                    acknowledgements.entry(address).or_insert(at);
                }
                _ => {}
            }
        }
        let mut reply_samples = Vec::new();
        let mut ack_samples = Vec::new();
        for (address, (at, seq, kind)) in &items {
            let token = if kind.is_unknown() {
                "other"
            } else {
                semantic_message_type_metadata(*kind).token
            };
            *contribution.received.entry(token.to_owned()).or_default() += 1;
            if let Some(reply) = replies.get(address) {
                reply_samples.push((*at, *seq, ((reply - at) / 60_000) as u32));
            }
            if let Some(ack) = acknowledgements.get(address) {
                ack_samples.push((*at, *seq, ((ack - at) / 60_000) as u32));
            }
        }
        fn newest(mut samples: Vec<(i64, u64, u32)>) -> Vec<u32> {
            samples.sort_unstable_by_key(|sample| std::cmp::Reverse((sample.0, sample.1)));
            samples.truncate(512);
            let mut values = samples
                .into_iter()
                .map(|sample| sample.2)
                .collect::<Vec<_>>();
            values.sort_unstable();
            values
        }
        contribution.reply_minutes = newest(reply_samples);
        contribution.ack_minutes = newest(ack_samples);
        RetroPreview {
            contribution,
            nothing_in_window: items.is_empty(),
        }
    }

    pub fn text_lines(&self) -> Vec<String> {
        let received = ordered_counts(&self.received);
        vec![
            format!(
                "[1] received items: {}",
                if received.is_empty() {
                    "none"
                } else {
                    &received
                }
            ),
            sample_line("[2] reply times", &self.reply_minutes, "reply", "replies"),
            sample_line(
                "[3] acknowledgement times",
                &self.ack_minutes,
                "acknowledgement",
                "acknowledgements",
            ),
        ]
    }
}

fn ordered_counts<T: std::fmt::Display>(counts: &BTreeMap<String, T>) -> String {
    semantic_message_type_tokens()
        .chain(std::iter::once("other"))
        .filter_map(|token| counts.get(token).map(|count| format!("{token} {count}")))
        .collect::<Vec<_>>()
        .join(" · ")
}

fn sample_line(label: &str, values: &[u32], singular: &str, plural: &str) -> String {
    match median(values) {
        Some(value) => format!(
            "{label}: {} {}, median {}",
            values.len(),
            if values.len() == 1 { singular } else { plural },
            retro_duration(value)
        ),
        None => format!("{label}: none"),
    }
}

fn median(sorted: &[u32]) -> Option<u32> {
    sorted.get(sorted.len().saturating_sub(1) / 2).copied()
}

pub fn retro_duration(minutes: u32) -> String {
    match minutes {
        0 => "<1m".to_owned(),
        1..60 => format!("{minutes}m"),
        60..1440 => format!("{}h {}m", minutes / 60, minutes % 60),
        _ => format!("{}d {}h", minutes / 1440, (minutes % 1440) / 60),
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RetroReport {
    pub received: BTreeMap<String, u64>,
    pub reply_minutes: Vec<u32>,
    pub ack_minutes: Vec<u32>,
    pub read_failed: bool,
    pub omitted_turns: bool,
    pub unreadable: bool,
    pub unknown_outcomes: bool,
}

impl RetroReport {
    pub fn from_contributions<'a>(
        contributions: impl IntoIterator<Item = &'a RetroContribution>,
    ) -> Self {
        let mut report = Self::default();
        for contribution in contributions {
            for (token, count) in &contribution.received {
                *report.received.entry(token.clone()).or_default() += u64::from(*count);
            }
            report
                .reply_minutes
                .extend_from_slice(&contribution.reply_minutes);
            report
                .ack_minutes
                .extend_from_slice(&contribution.ack_minutes);
        }
        report.reply_minutes.sort_unstable();
        report.ack_minutes.sort_unstable();
        report
    }

    pub fn render(&self, subject: Option<&str>, attempted: &str) -> String {
        let mut out = "Retrospective report · retro_request\n".to_owned();
        if let Some(subject) = subject {
            let _ = writeln!(out, "{subject}");
        }
        let _ = writeln!(out, "attempted {attempted}");
        if !self.received.is_empty() {
            let _ = writeln!(
                out,
                "  Received items (14 days): {}",
                ordered_counts(&self.received)
            );
        }
        for (label, values, noun) in [
            ("Time to first reply", &self.reply_minutes, "replies"),
            ("Time to acknowledge", &self.ack_minutes, "acknowledgements"),
        ] {
            if let Some(value) = median(values) {
                let _ = writeln!(
                    out,
                    "  {label}: median {} · {} {noun}",
                    retro_duration(value),
                    values.len()
                );
            }
        }
        if self.received.is_empty() && self.reply_minutes.is_empty() && self.ack_minutes.is_empty()
        {
            out.push_str("Nothing has been shared so far.\n");
        }
        for (present, note) in [
            (
                self.read_failed,
                "Some replies could not be read now; read the report again later.",
            ),
            (
                self.omitted_turns,
                "Some replies may be in thread turns a peer did not serve.",
            ),
            (
                self.unreadable,
                "Some replies are not readable by this build.",
            ),
            (
                self.unknown_outcomes,
                "Some send outcomes are not recognised by this build.",
            ),
        ] {
            if present {
                let _ = writeln!(out, "{note}");
            }
        }
        let _ = writeln!(out, "{RETRO_HONESTY_LINE}");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(seq: u64, at: i64, value: serde_json::Value) -> JournalEntry {
        JournalEntry::new(
            seq,
            JournalRecord::Room(serde_json::from_value(value).unwrap()),
            at,
        )
    }

    fn received(seq: u64, at: i64, id: &str, token: &str) -> JournalEntry {
        entry(
            seq,
            at,
            json!({"event":"recipient_item_received","address":{"principal":{"kind":"unknown"},"item":id},"task":id,"message_type":token,"content":"private body"}),
        )
    }

    fn reply(seq: u64, at: i64, item: &str, kind: &str) -> JournalEntry {
        entry(
            seq,
            at,
            json!({"event":"recipient_item_replied","address":{"principal":{"kind":"unknown"},"turn":format!("rr-{seq}")},"answers":item,"content":"private answer","authorship":"human-written","kind":{"type":kind}}),
        )
    }

    fn ack(seq: u64, at: i64, item: &str) -> JournalEntry {
        entry(
            seq,
            at,
            json!({"event":"recipient_item_acknowledged","address":{"principal":{"kind":"unknown"},"item":item}}),
        )
    }

    fn request_address(entry: &JournalEntry) -> &ItemAddress {
        match &entry.record {
            JournalRecord::Room(RoomEvent::RecipientItemReceived { address, .. }) => address,
            _ => panic!("received fixture"),
        }
    }

    #[test]
    fn compose_window_edges_timestamps_first_reply_and_snapshot() {
        let now = 2 * RETRO_WINDOW_MS;
        let entries = vec![
            received(1, now - RETRO_WINDOW_MS - 1, "old", "consultation"),
            received(2, now - RETRO_WINDOW_MS, "edge", "consultation"),
            received(3, 0, "legacy", "consultation"),
            received(4, now - 600_000, "known", "consultation"),
            reply(5, now - 540_001, "known", "answer"),
            reply(6, now - 120_000, "known", "status"),
            ack(7, now - 60_000, "known"),
            received(8, now - 60_000, "future", "new-kind"),
            reply(9, now, "future", "answer"),
            ack(10, now + 1, "future"),
            received(11, now, "request", "retro_request"),
            received(12, now, "same-time", "consultation"),
            received(13, now + 1, "after", "consultation"),
        ];
        let preview = RetroContribution::compose(&entries, request_address(&entries[10]));
        assert!(!preview.nothing_in_window);
        assert_eq!(
            preview.contribution.to_wire(),
            json!({"received":{"consultation":2,"other":1},"replyTimes":[0],"ackTimes":[9]})
        );
        let empty = vec![received(1, now, "request", "retro_request")];
        let preview = RetroContribution::compose(&empty, request_address(&empty[0]));
        assert!(preview.nothing_in_window);
        assert_eq!(preview.contribution, RetroContribution::nothing());
        let unstamped = vec![received(1, 0, "request", "retro_request")];
        assert!(
            RetroContribution::compose(&unstamped, request_address(&unstamped[0]))
                .nothing_in_window
        );
    }

    #[test]
    fn compose_keeps_the_512_newest_samples_then_sorts_values() {
        let now = 2 * RETRO_WINDOW_MS;
        let mut entries = Vec::new();
        for index in 0..513 {
            let id = format!("ri-{index}");
            let at = now - RETRO_WINDOW_MS + (index as i64) * 60_000;
            entries.push(received(index * 3 + 1, at, &id, "consultation"));
            entries.push(reply(
                index * 3 + 2,
                at + ((513 - index) as i64) * 60_000,
                &id,
                "answer",
            ));
            entries.push(ack(
                index * 3 + 3,
                at + ((513 - index) as i64) * 60_000,
                &id,
            ));
        }
        entries.push(received(2000, now, "request", "retro_request"));
        let preview =
            RetroContribution::compose(&entries, request_address(entries.last().unwrap()));
        assert_eq!(preview.contribution.received["consultation"], 513);
        assert_eq!(
            preview.contribution.reply_minutes,
            (1..=512).collect::<Vec<_>>()
        );
        assert_eq!(
            preview.contribution.ack_minutes,
            preview.contribution.reply_minutes
        );
    }

    #[test]
    fn compose_first_non_retro_reply_ignores_earlier_retrospective_turn() {
        let now = 2 * RETRO_WINDOW_MS;
        let entries = vec![
            received(1, now - 600_000, "item", "consultation"),
            reply(2, now - 599_000, "item", "retro"),
            reply(3, now - 480_000, "item", "answer"),
            reply(4, now - 240_000, "item", "answer"),
            received(5, now, "request", "retro_request"),
        ];
        assert_eq!(
            RetroContribution::compose(&entries, request_address(&entries[4]))
                .contribution
                .reply_minutes,
            vec![2]
        );
    }

    #[test]
    fn canonical_wire_redactions_and_closed_fields() {
        let full = RetroContribution::from_wire(&json!({"received":{"consultation":3,"other":1},"replyTimes":[0,25,25,90],"ackTimes":[60]})).unwrap();
        assert_eq!(
            RetroContribution::from_wire(&full.to_wire()),
            Some(full.clone())
        );
        assert_eq!(RetroContribution::nothing().to_wire(), json!({}));
        assert!(RetroContribution::nothing().is_nothing());
        let all: RetroFieldSet = [
            RetroField::Received,
            RetroField::ReplyTimes,
            RetroField::AckTimes,
        ]
        .into_iter()
        .collect();
        assert_eq!(full.without(&all), RetroContribution::nothing());
        let left = full.without(&[RetroField::ReplyTimes].into_iter().collect());
        assert!(left.to_wire().get("replyTimes").is_none());
        assert_eq!(left.received, full.received);
        for (token, field) in [
            ("received", RetroField::Received),
            ("replyTimes", RetroField::ReplyTimes),
            ("ackTimes", RetroField::AckTimes),
        ] {
            assert_eq!(field.as_str(), token);
            assert_eq!(RetroField::from_token(token), Some(field));
        }
        assert_eq!(RetroField::from_token("content"), None);
        for bad in [
            json!(null),
            json!([]),
            json!({"content":"private"}),
            json!({"received":{}}),
            json!({"replyTimes":[]}),
            json!({"ackTimes":[]}),
            json!({"received":{"unknown":1}}),
            json!({"received":{"consultation":0}}),
            json!({"received":{"consultation":1000001}}),
            json!({"received":{"consultation":-1}}),
            json!({"received":{"consultation":1.5}}),
            json!({"received":[]}),
            json!({"replyTimes":[20161]}),
            json!({"ackTimes":[-1]}),
            json!({"replyTimes":[1.5]}),
            json!({"replyTimes":[2,1]}),
            json!({"ackTimes":[3,2]}),
            json!({"replyTimes":vec![0;513]}),
            json!({"ackTimes":{}}),
        ] {
            assert_eq!(RetroContribution::from_wire(&bad), None, "{bad}");
        }
        assert!(
            RetroContribution::from_wire(
                &json!({"received":{"consultation":1000000},"ackTimes":[0,20160]})
            )
            .is_some()
        );
    }

    #[test]
    fn aggregate_identity_lower_medians_notes_and_durations() {
        let a=RetroContribution::from_wire(&json!({"received":{"story_assignment":2,"consultation":3,"other":1},"replyTimes":[4,25,90],"ackTimes":[60]})).unwrap();
        let b = RetroContribution::from_wire(
            &json!({"received":{"consultation":4},"replyTimes":[10],"ackTimes":[80]}),
        )
        .unwrap();
        let baseline = RetroReport::from_contributions([&a, &b]);
        assert_eq!(
            baseline,
            RetroReport::from_contributions([&b, &RetroContribution::nothing(), &a])
        );
        let text = baseline.render(Some("Sprint 7"), "2m ago");
        assert_eq!(
            text,
            format!(
                "Retrospective report · retro_request\nSprint 7\nattempted 2m ago\n  Received items (14 days): consultation 7 · story_assignment 2 · other 1\n  Time to first reply: median 10m · 4 replies\n  Time to acknowledge: median 1h 0m · 2 acknowledgements\n{RETRO_HONESTY_LINE}\n"
            )
        );
        assert!(
            RetroReport::from_contributions([&a])
                .render(None, "now")
                .contains("median 25m")
        );
        let mut empty = RetroReport::default();
        assert!(
            empty
                .render(None, "now")
                .contains("Nothing has been shared so far.")
        );
        empty.read_failed = true;
        empty.omitted_turns = true;
        empty.unreadable = true;
        empty.unknown_outcomes = true;
        let text = empty.render(None, "now");
        for note in [
            "Some replies could not be read now; read the report again later.",
            "Some replies may be in thread turns a peer did not serve.",
            "Some replies are not readable by this build.",
            "Some send outcomes are not recognised by this build.",
        ] {
            assert_eq!(text.matches(note).count(), 1);
        }
        for (minutes, text) in [
            (0, "<1m"),
            (1, "1m"),
            (59, "59m"),
            (60, "1h 0m"),
            (70, "1h 10m"),
            (1439, "23h 59m"),
            (1440, "1d 0h"),
            (20160, "14d 0h"),
        ] {
            assert_eq!(retro_duration(minutes), text);
        }
        assert_eq!(a.text_lines()[1], "[2] reply times: 3 replies, median 25m");
    }
}
