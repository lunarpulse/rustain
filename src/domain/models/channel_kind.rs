//! `ChannelKind` — the origin abstraction for a conversation message (Story 12.2b
//! AC5).
//!
//! Epic 12 turns rustain into an always-on daemon that several *channels* feed:
//! the interactive terminal (Story 12.2c attach), Telegram (Story 12.3), and cron
//! (Story 12.4). A message's **origin channel** is intrinsic provenance — the
//! unified multi-channel scrollback (the core Epic-12 promise) shows a
//! `[telegram]`/`[cron]` prefix on every message regardless of which client is
//! attached.
//!
//! ## Why this is persisted, not render-time (party-mode 2026-06-08, unanimous)
//!
//! Origin is carried on `ChatMessage.origin` (`#[serde(default)]`) and written to
//! the session file. A re-attached or crash-recovery-replayed historical message
//! MUST still show its origin prefix — render-time reconstruction cannot recover
//! which channel a message came from after a daemon restart. `#[serde(default)]`
//! keeps pre-12.2b session files parseable (a missing `origin` field deserialises
//! to [`ChannelKind::Terminal`], the legacy single-channel reality — no migration).

use serde::{Deserialize, Serialize};

/// Which channel a conversation message originated from.
///
/// `Default` is [`ChannelKind::Terminal`] so legacy session files (no `origin`
/// field) and every pre-existing `ChatMessage` construction site read as the
/// terminal channel — the only origin that existed before Epic 12's daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ChannelKind {
    /// The interactive terminal — the local TUI or an attached client (Story
    /// 12.2c). The historical default for every message rustain has ever stored.
    #[default]
    Terminal,
    /// The Telegram channel adapter (Story 12.3).
    Telegram,
    /// A scheduled (cron) job context (Story 12.4).
    Cron,
}

impl ChannelKind {
    /// The dimmed scrollback prefix the multi-channel TUI renders ahead of a
    /// message from this channel (Story 12.2c consumes it). Lowercase + bracketed
    /// to read as metadata, not content.
    pub fn as_prefix(&self) -> &'static str {
        match self {
            ChannelKind::Terminal => "[terminal]",
            ChannelKind::Telegram => "[telegram]",
            ChannelKind::Cron => "[cron]",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_terminal() {
        assert_eq!(ChannelKind::default(), ChannelKind::Terminal);
    }

    #[test]
    fn prefixes_are_lowercase_bracketed() {
        assert_eq!(ChannelKind::Terminal.as_prefix(), "[terminal]");
        assert_eq!(ChannelKind::Telegram.as_prefix(), "[telegram]");
        assert_eq!(ChannelKind::Cron.as_prefix(), "[cron]");
    }

    #[test]
    fn serde_round_trips_each_variant() {
        for kind in [
            ChannelKind::Terminal,
            ChannelKind::Telegram,
            ChannelKind::Cron,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            let back: ChannelKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, back);
        }
    }

    #[test]
    fn legacy_missing_origin_defaults_to_terminal() {
        // A struct with `#[serde(default)] origin` must read a pre-12.2b record
        // (no `origin` key) as Terminal.
        #[derive(serde::Deserialize)]
        struct Holder {
            #[serde(default)]
            origin: ChannelKind,
        }
        let h: Holder = serde_json::from_str("{}").unwrap();
        assert_eq!(h.origin, ChannelKind::Terminal);
    }

    #[test]
    fn terminal_serializes_camel_case() {
        assert_eq!(
            serde_json::to_string(&ChannelKind::Terminal).unwrap(),
            "\"terminal\""
        );
    }
}
