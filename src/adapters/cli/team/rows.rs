//! Shared row construction for `rustain team log` (Story 18.2, AC6).
//!
//! Mirrors `cli/session/rows.rs`: a thin, pure layer over the domain core so
//! the CLI and the TUI cannot drift. There is deliberately **no** second fold
//! here — `build_transparency_rows` filters the output of
//! [`crate::domain::services::transparency::fold_transparency`] using the same
//! [`TransparencyFilter`] the slash command parses.

use crate::domain::models::JournalEntry;
use crate::domain::services::transparency::{
    TransparencyFilter, TransparencyRow, fold_transparency,
};

/// Version of the `--json` envelope. Bump only on a breaking shape change.
pub const TRANSPARENCY_LOG_SCHEMA_VERSION: u32 = 1;

/// Fold entries into rows, then apply the optional filter.
///
/// # Errors
///
/// Returns the operator-facing message when the filter grammar is invalid. A
/// bad filter must refuse loudly: silently returning every row would tell the
/// operator their filter matched everything.
pub fn build_transparency_rows(
    entries: &[JournalEntry],
    filter: Option<&str>,
) -> Result<Vec<TransparencyRow>, String> {
    let rows = fold_transparency(entries);
    let Some(spec) = filter else {
        return Ok(rows);
    };
    let filter = TransparencyFilter::parse(spec)?;
    Ok(rows.into_iter().filter(|row| filter.matches(row)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{Direction, JournalRecord, PeerId, RejectReason, RoomEvent};

    fn entry(seq: u64, direction: Direction) -> JournalEntry {
        JournalEntry::new(
            seq,
            JournalRecord::Room(RoomEvent::RemoteEnvelopeRejected {
                peer: PeerId::from_public_key(&[seq as u8; 32]).unwrap(),
                reason: RejectReason::Policy {
                    detail: "policy".to_owned(),
                },
                direction,
                task: None,
            }),
            1_700_000_000_000 + seq as i64,
        )
    }

    #[test]
    fn the_cli_rows_are_the_domain_fold_verbatim() {
        let entries = [entry(1, Direction::Inbound), entry(2, Direction::Outbound)];
        assert_eq!(
            build_transparency_rows(&entries, None).unwrap(),
            fold_transparency(&entries),
            "the CLI must not have a second fold"
        );
    }

    #[test]
    fn a_filter_narrows_and_a_bad_filter_refuses() {
        let entries = [entry(1, Direction::Inbound), entry(2, Direction::Outbound)];
        let rows = build_transparency_rows(&entries, Some("direction=outbound")).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].direction, Direction::Outbound);
        assert!(build_transparency_rows(&entries, Some("direction=up")).is_err());
    }
}
