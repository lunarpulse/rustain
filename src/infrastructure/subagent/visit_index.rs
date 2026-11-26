//! Rebuildable, process-local projection of an exact canonical journal revision.
//! Nothing here authorizes a visit or makes a secondary durable record.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read as _, Write as _};
use std::path::Path;

use crate::domain::models::artifact::EvidenceArtifact;
use crate::domain::models::orchestration_room::MainVisitKey;
use crate::domain::models::{AgentId, JournalEntry, JournalRecord, RoomEvent};

use super::{JournalError, JournalFileStamp, MAX_VISIT_JOURNAL_BYTES, parse_entry};

#[derive(Clone)]
pub(in crate::infrastructure::subagent) struct LinkRow {
    pub key: MainVisitKey,
    pub attempt: AgentId,
    pub artifact: EvidenceArtifact,
    pub stamp_ms: i64,
}

#[derive(Clone)]
pub(in crate::infrastructure::subagent) struct TurnRow {
    pub root: String,
    pub ordinal: u32,
    pub artifact: EvidenceArtifact,
    pub partial: bool,
}

pub(in crate::infrastructure::subagent) struct VisitRows {
    pub links: Vec<(LinkRow, bool)>,
    pub turns: Vec<TurnRow>,
}

#[derive(Default)]
pub(super) struct Index {
    links: HashMap<String, Vec<LinkRow>>,
    // Keep only whether the attempt is unique (u8 saturates at two).
    link_counts: HashMap<AgentId, u8>,
    turns: HashMap<AgentId, Vec<TurnRow>>,
    head: u64,
    last_stamp_ms: i64,
}

pub(super) struct IndexedRevision {
    stamp: JournalFileStamp,
    index: Index,
}

impl IndexedRevision {
    pub(super) fn rebuild_if_changed<'a>(
        cached: &'a mut Option<Self>,
        file: &File,
        path: &Path,
    ) -> Result<&'a mut Self, JournalError> {
        let stamp = verified_stamp(file, path)?;
        if cached.as_ref().is_none_or(|old| old.stamp != stamp) {
            *cached = None;
            if stamp.len > MAX_VISIT_JOURNAL_BYTES as u64 {
                return Err(io::Error::other("Visit journal exceeds its scan budget").into());
            }
            let mut index = Index::default();
            let mut reader = BufReader::with_capacity(64 * 1024, file.take(stamp.len));
            let mut line = Vec::new();
            let mut read = 0;
            while read < stamp.len {
                line.clear();
                let count = reader.read_until(b'\n', &mut line)?;
                read += count as u64;
                if !line.ends_with(b"\n") {
                    return Err(io::Error::other("incomplete Visit journal revision").into());
                }
                let entry = parse_entry(&line, index.head + 1, false)?
                    .expect("a non-tail record is either parsed or rejected");
                index.push(entry);
            }
            if verified_stamp(file, path)? != stamp {
                return Err(io::Error::other("Visit journal changed during rebuild").into());
            }
            *cached = Some(Self { stamp, index });
        }
        Ok(cached.as_mut().expect("verified or rebuilt index"))
    }

    pub(super) fn index(&self) -> &Index {
        &self.index
    }

    pub(super) fn append(
        &mut self,
        mut file: &File,
        path: &Path,
        event: RoomEvent,
        max_bytes: usize,
        now_ms: i64,
    ) -> Result<JournalEntry, JournalError> {
        if self.index.conflicts(&event) {
            return Err(io::Error::other("private Visit record already exists").into());
        }
        let seq = self
            .index
            .head
            .checked_add(1)
            .ok_or(JournalError::SequenceExhausted)?;
        if seq == u64::MAX {
            return Err(JournalError::SequenceExhausted);
        }
        let stamp = if self.index.last_stamp_ms == 0 {
            now_ms
        } else {
            now_ms.max(self.index.last_stamp_ms)
        };
        let entry = JournalEntry::new(seq, JournalRecord::Room(event), stamp);
        let mut bytes = serde_json::to_vec(&entry)?;
        bytes.push(b'\n');
        let len = self.stamp.len + bytes.len() as u64;
        if len > max_bytes.min(MAX_VISIT_JOURNAL_BYTES) as u64 {
            return Err(io::Error::other("private journal append exceeds its byte budget").into());
        }
        if verified_stamp(file, path)? != self.stamp {
            return Err(io::Error::other("Visit journal changed before append").into());
        }
        file.write_all(&bytes)?;
        file.sync_all()?;
        let after = verified_stamp(file, path)?;
        if after.len != len {
            return Err(io::Error::other("Visit journal changed during append").into());
        }
        self.index.head = entry.seq;
        if entry.has_timestamp() {
            self.index.last_stamp_ms = stamp;
        }
        self.index.push_record(entry.record.clone(), stamp);
        self.stamp = after;
        Ok(entry)
    }
}

fn verified_stamp(file: &File, path: &Path) -> Result<JournalFileStamp, JournalError> {
    let descriptor = file.metadata()?;
    let named = std::fs::symlink_metadata(path)?;
    let stamp = JournalFileStamp::of(&descriptor);
    if !descriptor.is_file() || !named.is_file() || stamp != JournalFileStamp::of(&named) {
        return Err(io::Error::other("Visit journal is not the opened canonical file").into());
    }
    Ok(stamp)
}

impl Index {
    fn push(&mut self, entry: JournalEntry) {
        self.head = entry.seq;
        if entry.has_timestamp() {
            self.last_stamp_ms = entry.recorded_at_ms;
        }
        self.push_record(entry.record, entry.recorded_at_ms);
    }

    fn push_record(&mut self, record: JournalRecord, stamp_ms: i64) {
        match record {
            JournalRecord::Batch(records) => {
                for record in records {
                    self.push_record(record, stamp_ms);
                }
            }
            JournalRecord::Room(RoomEvent::VisitLinked {
                key,
                attempt,
                artifact,
            }) => {
                let count = self.link_counts.entry(attempt.clone()).or_default();
                *count = (*count + 1).min(2);
                self.links
                    .entry(key.conversation_id.clone())
                    .or_default()
                    .push(LinkRow {
                        key,
                        attempt,
                        artifact,
                        stamp_ms,
                    });
            }
            JournalRecord::Room(RoomEvent::VisitTurnRecorded {
                root,
                attempt,
                ordinal,
                artifact,
                partial,
            }) => {
                self.turns
                    .entry(attempt)
                    .or_insert_with(|| Vec::with_capacity(1))
                    .push(TurnRow {
                        root,
                        ordinal,
                        artifact,
                        partial,
                    });
            }
            _ => {}
        }
    }

    fn conflicts(&self, event: &RoomEvent) -> bool {
        match event {
            RoomEvent::VisitLinked { key, attempt, .. } => {
                self.link_counts.contains_key(attempt)
                    || self
                        .links
                        .get(&key.conversation_id)
                        .is_some_and(|links| links.iter().any(|row| row.key == *key))
            }
            RoomEvent::VisitTurnRecorded {
                root,
                attempt,
                ordinal,
                ..
            } => self.turns.get(attempt).is_some_and(|rows| {
                rows.iter()
                    .any(|row| row.ordinal == *ordinal && row.root == *root)
            }),
            _ => true,
        }
    }

    pub(super) fn links(&self, root: &str) -> Vec<(LinkRow, bool)> {
        self.links
            .get(root)
            .into_iter()
            .flatten()
            .map(|row| (row.clone(), self.link_counts.get(&row.attempt) == Some(&1)))
            .collect()
    }

    pub(super) fn select(&self, key: &MainVisitKey, turn_cap: usize) -> Option<VisitRows> {
        let links = self.links(&key.conversation_id);
        let selected = links.iter().find(|(row, _)| &row.key == key);
        let turns = match selected.and_then(|(row, _)| self.turns.get(&row.attempt)) {
            Some(rows) if rows.len() > turn_cap => return None,
            Some(rows) => rows.clone(),
            None => Vec::new(),
        };
        Some(VisitRows { links, turns })
    }
}
