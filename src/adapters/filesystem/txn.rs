//! Session transactions of [`FileSystemStorage`] under per-session authority (D-24).
//!
//! Every transcript, layout, sidecar, checkpoint-log and journal writer runs here as one
//! owned operation: acquire the session authority, recover any prepared predecessor, read the
//! dependent source *inside* the hold, admit the capture, prepare the successor record, publish
//! the first authoritative artifact, run the remaining effects and settle. Nothing in this module
//! holds a registry guard or a lock across provider/network or unrelated-session I/O.

use super::session_authority::{
    self as authority, Admit, Effect, Held, Intent, Opened, Prepared, Record, Witness,
};
use super::*;
use crate::domain::models::checkpoint::{CheckpointId, CheckpointMeta};
use crate::domain::models::persist_fence::{Base, PersistTag, Stamp};
use crate::domain::models::transaction::{RewindBinding, RewindTxn, RewindTxnPhase};
use crate::domain::ports::{RewindStep, Rewound};

/// A transcript read from disk together with its layout.
pub(super) struct Loaded {
    pub conv: Conversation,
    pub clean_exit: bool,
    pub layout: SessionLayout,
}

/// What a finished commit settled on.
pub(super) struct Committed {
    pub record: Record,
    /// Set when new authoritative state is visible but a later required effect did not
    /// finish; the effects stay recorded for recovery.
    pub partial: Option<StorageError>,
}

/// One operation ready to commit.
pub(super) struct Plan {
    pub op: &'static str,
    /// Root-relative path of the first authoritative artifact.
    pub publish: String,
    pub bytes: Vec<u8>,
    pub effects: Vec<Effect>,
    pub stamp: Option<Stamp>,
    pub admit: Admit,
    /// A failing effect is logged, not reported (the flat sidecar: a stale count only
    /// degrades listing, as before).
    pub best_effort_effects: bool,
}

fn flat_main(sid: &str) -> String {
    format!("{sid}.meta.json")
}
fn flat_meta(sid: &str) -> String {
    format!("{sid}.session.json")
}
fn dir_conversation(sid: &str) -> String {
    format!("{sid}/conversation.json")
}
fn dir_meta(sid: &str) -> String {
    format!("{sid}/meta.json")
}
fn log_path(sid: &str) -> String {
    format!("{sid}/checkpoints.json")
}

fn json_text<T: serde::Serialize>(value: &T) -> Result<String, StorageError> {
    serde_json::to_string_pretty(value).map_err(|e| StorageError::SerializationError(e.to_string()))
}

/// The sidecar after a transcript write: existing sidecar-only fields survive (DF-088).
fn sidecar_for(existing: Option<SessionMeta>, fresh: &SessionMeta, count: usize) -> SessionMeta {
    let mut meta = existing.unwrap_or_else(|| fresh.clone());
    meta.title = fresh.title.clone();
    meta.created_at = fresh.created_at;
    meta.updated_at = fresh.updated_at;
    meta.message_count = count;
    meta.fork_source = fresh.fork_source.clone();
    meta
}

/// Publication path and sidecar effects of one transcript write in either layout.
fn layout_effects(
    sid: &str,
    directory: bool,
    meta: &SessionMeta,
    extra: Vec<Effect>,
) -> Result<(String, Vec<Effect>), StorageError> {
    let meta_json = json_text(meta)?;
    let mut effects = Vec::new();
    let publish = if directory {
        effects.push(Effect::Write {
            path: dir_meta(sid),
            content: meta_json,
        });
        effects.push(Effect::Remove {
            path: flat_main(sid),
        });
        effects.push(Effect::Remove {
            path: flat_meta(sid),
        });
        dir_conversation(sid)
    } else {
        effects.push(Effect::Write {
            path: flat_meta(sid),
            content: meta_json,
        });
        flat_main(sid)
    };
    effects.extend(extra);
    Ok((publish, effects))
}

impl FileSystemStorage {
    /// Acquire the session authority and run `op` as an owned task that finishes even when
    /// the awaiting caller is dropped.
    pub(super) async fn transact<T, F, Fut>(&self, id: &str, op: F) -> Result<T, StorageError>
    where
        T: Send + 'static,
        F: FnOnce(FileSystemStorage, Held) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, StorageError>> + Send + 'static,
    {
        let held = authority::acquire(&self.sessions_dir, id).await?;
        let this = self.clone();
        authority::owned(held, move |held| op(this, held)).await
    }

    /// Finish a predecessor's recorded work before anything new is admitted.
    pub(super) async fn recover(&self, held: &mut Held) -> Result<(), StorageError> {
        let Some(record) = held.record().cloned() else {
            return Ok(());
        };
        if let Some(prepared) = record.prepared.clone() {
            if held.witness_published(&prepared.witness).await? {
                let mut published = (*prepared.next).clone();
                published.pending = prepared.effects.clone();
                held.write_record(published.clone()).await?;
                return self.finish_pending(held, published).await;
            }
            let mut cleared = record;
            cleared.prepared = None;
            return held.write_record(cleared).await;
        }
        if record.pending.is_empty() {
            return Ok(());
        }
        self.finish_pending(held, record).await
    }

    async fn finish_pending(
        &self,
        held: &mut Held,
        mut record: Record,
    ) -> Result<(), StorageError> {
        let effects = std::mem::take(&mut record.pending);
        held.run_effects(&effects).await.map_err(|e| {
            StorageError::Indeterminate(format!(
                "recorded effects of an accepted version did not finish: {e}"
            ))
        })?;
        held.write_record(record).await
    }

    /// Publish one prepared operation and settle it (see the module notes).
    pub(super) async fn commit(
        &self,
        held: &mut Held,
        plan: Plan,
    ) -> Result<Committed, StorageError> {
        let seq = match plan.stamp.as_ref().and_then(|s| s.seq) {
            Some(seq) => seq,
            None => held.issue_seq()?,
        };
        let next = authority::next_record(held.live(), &plan.admit, plan.stamp.as_ref(), seq);
        let before = held.live().cloned();
        let mut marker = before.clone().unwrap_or_default();
        marker.prepared = Some(Box::new(Prepared {
            op: plan.op.to_string(),
            witness: Witness::File {
                path: plan.publish.clone(),
                digest: authority::digest(&plan.bytes),
            },
            next: Box::new(next.clone()),
            effects: plan.effects.clone(),
        }));
        held.write_record(marker).await?;
        #[cfg(test)]
        if super::session_authority::fault::hit(held.root(), "prepared").await {
            return Err(StorageError::Indeterminate(
                "injected stop after prepare".into(),
            ));
        }
        let target = held.resolve(&plan.publish)?;
        let published = async {
            if let Some(parent) = target.parent() {
                tokio::fs::create_dir_all(parent).await.map_err(|e| {
                    StorageError::IoError(format!("Failed to create session dir: {e}"))
                })?;
            }
            Self::atomic_write(&self.sessions_dir, target, plan.bytes)
                .await
                .map_err(|(stage, e)| match stage {
                    WriteStage::Write => {
                        StorageError::IoError(format!("Failed to write {}: {e}", plan.publish))
                    }
                    WriteStage::Rename => {
                        StorageError::IoError(format!("Failed to rename {}: {e}", plan.publish))
                    }
                })
        }
        .await;
        if let Err(error) = published {
            // Nothing changed: the temporary file is gone and the target was not replaced.
            held.write_record(before.unwrap_or_default()).await?;
            return Err(error);
        }
        #[cfg(test)]
        if super::session_authority::fault::hit(held.root(), "published").await {
            return Err(StorageError::Indeterminate(
                "injected stop after publication".into(),
            ));
        }
        let mut visible = next.clone();
        visible.pending = plan.effects.clone();
        if let Err(error) = held.write_record(visible).await {
            return Ok(Committed {
                record: next,
                partial: Some(StorageError::PartialCommit(format!(
                    "the new version is published but its record is not settled: {error}"
                ))),
            });
        }
        #[cfg(test)]
        if super::session_authority::fault::hit(held.root(), "visible").await {
            return Err(StorageError::Indeterminate(
                "injected stop before effects".into(),
            ));
        }
        let mut partial = None;
        if let Err(error) = held.run_effects(&plan.effects).await {
            if plan.best_effort_effects {
                tracing::warn!("session effect failed after publication: {error}");
            } else {
                partial = Some(StorageError::PartialCommit(format!(
                    "the new version is published but a required effect failed: {error}"
                )));
            }
        }
        if partial.is_none()
            && let Err(error) = held.write_record(next.clone()).await
        {
            partial = Some(StorageError::PartialCommit(format!(
                "the new version is published but its settlement failed: {error}"
            )));
        }
        Ok(Committed {
            record: next,
            partial,
        })
    }

    /// Record the accepted result on the capture's lineage, whatever the caller does next.
    fn settle(held: &mut Held, stamp: Option<&Stamp>, committed: &Committed) {
        if let Some(stamp) = stamp {
            stamp.settle(committed.record.base(), held.issuer());
        }
    }

    /// A conversation installed from a store-returned state: fresh lineage at the new base.
    fn installed(mut conv: Conversation, held: &mut Held, committed: &Committed) -> Conversation {
        conv.persist = PersistTag::loaded_armed(committed.record.base(), held.issuer());
        conv
    }

    // ── reads ────────────────────────────────────────────────────────────────

    /// Read the transcript (either layout) with no authority; callers hold it or accept a
    /// lock-free read.
    pub(super) async fn read_inner(&self, id: &str) -> Result<Option<Loaded>, StorageError> {
        let layout = self.detect_layout(id).await;
        let path = match layout {
            SessionLayout::Directory => self.conversation_file(id),
            SessionLayout::Flat => self.session_path(id),
            SessionLayout::Missing => return Ok(None),
        };
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(StorageError::IoError(format!(
                    "Failed to read session file: {}",
                    e
                )));
            }
        };
        let persisted: PersistedConversation = serde_json::from_str(&content)
            .map_err(|e| StorageError::SerializationError(e.to_string()))?;
        let clean_exit = persisted.clean_exit;
        let conv = persisted.to_conversation();
        if layout == SessionLayout::Directory {
            // Story 4-3a.1 AC4: missing image files degrade gracefully (DF-098: one read_dir).
            let mut present = std::collections::HashSet::new();
            if let Ok(mut rd) = tokio::fs::read_dir(self.images_dir(&conv.id)).await {
                while let Ok(Some(e)) = rd.next_entry().await {
                    if e.file_type().await.is_ok_and(|ft| ft.is_file())
                        && let Some(name) = e.file_name().to_str()
                    {
                        present.insert(name.to_string());
                    }
                }
            }
            for msg in &conv.messages {
                for img in msg
                    .images
                    .iter()
                    .filter(|i| !present.contains(&i.file_name))
                {
                    tracing::warn!(
                        conversation_id = %conv.id,
                        message_id = %msg.id,
                        file_name = %img.file_name,
                        "Image file missing on load (graceful degradation)"
                    );
                }
            }
        }
        Ok(Some(Loaded {
            conv,
            clean_exit,
            layout,
        }))
    }

    pub(super) async fn read_meta_inner(
        &self,
        id: &str,
    ) -> Result<Option<SessionMeta>, StorageError> {
        let layout = self.detect_layout(id).await;
        let path = match layout {
            SessionLayout::Directory => self.meta_file_in_dir(id),
            SessionLayout::Flat | SessionLayout::Missing => self.session_meta_path(id),
        };
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(StorageError::IoError(format!(
                    "Failed to read session meta file: {}",
                    e
                )));
            }
        };
        serde_json::from_str(&content)
            .map(Some)
            .map_err(|e| StorageError::SerializationError(e.to_string()))
    }

    /// Load a conversation with the base it was accepted at (consistent with the transcript).
    pub(super) async fn load_with_base(
        &self,
        id: &str,
    ) -> Result<Option<(Conversation, bool)>, StorageError> {
        let mut held = match authority::acquire_existing(&self.sessions_dir, id).await {
            Ok(held) => held,
            Err(StorageError::InvalidId(_)) => return Ok(None),
            Err(e) => return Err(e),
        };
        if let Some(held) = held.as_mut() {
            // A read still serves the published transcript when recovery cannot finish; the
            // next writer is refused until it does.
            if let Err(error) = self.recover(held).await {
                tracing::warn!("session recovery unfinished while loading {id}: {error}");
            }
            if held.live().is_some_and(|r| r.tombstone) {
                return Ok(None);
            }
        }
        let Some(Loaded {
            mut conv,
            clean_exit,
            ..
        }) = self.read_inner(id).await?
        else {
            return Ok(None);
        };
        conv.persist = match held.as_mut() {
            Some(held) => PersistTag::loaded_armed(
                held.live().map_or(Base::Legacy, Record::base),
                held.issuer(),
            ),
            None => PersistTag::loaded(Base::Legacy),
        };
        Ok(Some((conv, clean_exit)))
    }

    // ── save ─────────────────────────────────────────────────────────────────

    /// Layout-aware save of the captured conversation (admitted against its stamp).
    pub(super) async fn save_captured(
        &self,
        conv: &Conversation,
        clean_exit: bool,
        intent: Intent,
    ) -> Result<(), StorageError> {
        let stamp = conv.persist.capture();
        let persisted = PersistedConversation::from_conversation_with_exit(conv, clean_exit);
        let has_images = persisted.messages.iter().any(|m| !m.images.is_empty());
        let count = persisted.messages.len();
        let json = json_text(&persisted)?;
        let fresh = SessionMeta::from_conversation(conv);
        let id = conv.id.clone();
        let task_stamp = stamp.clone();
        let result = self
            .transact(&id, move |this, mut held| async move {
                let id = held_id(&held);
                this.recover(&mut held).await?;
                let layout = this.detect_layout(&id).await;
                let admit = authority::admit(
                    held.live(),
                    layout != SessionLayout::Missing,
                    Some(&task_stamp),
                    intent,
                )?;
                let existing = match this.read_meta_inner(&id).await {
                    Ok(meta) => meta,
                    Err(error) => {
                        tracing::warn!("Failed to load session meta sidecar for {}: {}", id, error);
                        None
                    }
                };
                let directory = has_images || layout == SessionLayout::Directory;
                let meta = sidecar_for(existing, &fresh, count);
                let (publish, effects) = layout_effects(&id, directory, &meta, Vec::new())?;
                let committed = this
                    .commit(
                        &mut held,
                        Plan {
                            op: "save",
                            publish,
                            bytes: json.into_bytes(),
                            effects,
                            stamp: Some(task_stamp.clone()),
                            admit,
                            best_effort_effects: !directory,
                        },
                    )
                    .await?;
                Self::settle(&mut held, Some(&task_stamp), &committed);
                this.maybe_note_workspace().await;
                committed.partial.map_or(Ok(()), Err)
            })
            .await;
        if matches!(result, Ok(()) | Err(StorageError::PartialCommit(_))) {
            conv.persist.advance(&stamp);
        }
        result
    }

    // ── delete and sidecars ──────────────────────────────────────────────────

    pub(super) async fn delete_session(&self, id: &str) -> Result<(), StorageError> {
        let layout = self.detect_layout(id).await;
        let existing = authority::acquire_existing(&self.sessions_dir, id).await?;
        if existing.is_none()
            && layout == SessionLayout::Missing
            && !tokio::fs::try_exists(self.session_meta_path(id))
                .await
                .unwrap_or(false)
        {
            return Ok(());
        }
        drop(existing);
        let id = id.to_string();
        self.transact(&id.clone(), move |this, mut held| async move {
            this.recover(&mut held).await?;
            let sid = &id;
            let effects = match this.detect_layout(sid).await {
                SessionLayout::Directory => vec![Effect::RemoveDir { path: sid.clone() }],
                SessionLayout::Flat | SessionLayout::Missing => {
                    vec![
                        Effect::Remove {
                            path: flat_main(sid),
                        },
                        Effect::Remove {
                            path: flat_meta(sid),
                        },
                    ]
                }
            };
            let mut tombstone = authority::tombstone_record(held.live());
            tombstone.pending = effects.clone();
            held.write_record(tombstone.clone()).await?;
            if let Err(error) = held.run_effects(&effects).await {
                return Err(StorageError::PartialCommit(format!(
                    "the session is deleted but its files were not all removed: {error}"
                )));
            }
            tombstone.pending.clear();
            held.write_record(tombstone).await?;
            tracing::debug!("Deleted conversation {}", id);
            Ok(())
        })
        .await
    }

    /// Whole-sidecar write under authority (refused for a deleted session).
    pub(super) async fn write_meta(
        &self,
        id: &str,
        meta: &SessionMeta,
    ) -> Result<(), StorageError> {
        let json = json_text(meta)?;
        self.ensure_dir().await?;
        let mut held = authority::acquire(&self.sessions_dir, id).await?;
        self.recover(&mut held).await?;
        if held.live().is_some_and(|r| r.tombstone) {
            return Err(StorageError::Stale("the session was deleted".into()));
        }
        self.write_meta_unlocked(id, json).await
    }

    async fn write_meta_unlocked(&self, id: &str, json: String) -> Result<(), StorageError> {
        let target = match self.detect_layout(id).await {
            SessionLayout::Directory => {
                tokio::fs::create_dir_all(self.session_dir(id))
                    .await
                    .map_err(|e| {
                        StorageError::IoError(format!("Failed to create session dir: {}", e))
                    })?;
                self.meta_file_in_dir(id)
            }
            SessionLayout::Flat | SessionLayout::Missing => self.session_meta_path(id),
        };
        Self::atomic_write(&self.sessions_dir, target, json.into_bytes())
            .await
            .map_err(|(stage, e)| match stage {
                WriteStage::Write => {
                    StorageError::IoError(format!("Failed to write session meta file: {}", e))
                }
                WriteStage::Rename => {
                    StorageError::IoError(format!("Failed to rename session meta file: {}", e))
                }
            })
    }

    /// Read-modify-write of the sidecar inside one authority hold.
    pub(super) async fn update_meta(
        &self,
        id: &str,
        update: &(dyn Fn(SessionMeta) -> SessionMeta + Send + Sync),
    ) -> Result<(), StorageError> {
        let mut held = authority::acquire(&self.sessions_dir, id).await?;
        self.recover(&mut held).await?;
        if held.live().is_some_and(|r| r.tombstone) {
            return Err(StorageError::Stale("the session was deleted".into()));
        }
        let meta = self
            .read_meta_inner(id)
            .await?
            .ok_or_else(|| StorageError::NotFound(format!("session metadata: {id}")))?;
        self.write_meta_unlocked(id, json_text(&update(meta))?)
            .await
    }

    // ── checkpoints, revert, truncate ────────────────────────────────────────

    pub(super) async fn checkpoint_for_stamp(
        &self,
        id: &str,
        stamp: Option<Stamp>,
    ) -> Result<CheckpointId, StorageError> {
        let retention = self.snapshot_retention_count;
        self.transact(id, move |this, mut held| async move {
            let id = held_id(&held);
            this.recover(&mut held).await?;
            let Some(Loaded { conv, layout, .. }) = this.read_inner(&id).await? else {
                return Err(StorageError::NotFound(format!(
                    "conversation not found: {id}"
                )));
            };
            let intent = if stamp.is_some() {
                Intent::Dependent
            } else {
                Intent::Derived
            };
            let mut admit = authority::admit(held.live(), true, stamp.as_ref(), intent)?;
            admit.keep_writer = stamp.is_none();
            let mut log = this.load_checkpoint_log(&id).await?;
            let next_id = log.entries.iter().map(|e| e.id.0).max().unwrap_or(0) + 1;
            let meta = CheckpointMeta {
                id: CheckpointId(next_id),
                message_index: conv.messages.len().saturating_sub(1),
                created_at: crate::domain::models::session_meta::now_unix(),
            };
            log.entries.push(meta.clone());
            let log_json = json_text(&log)?;
            let (publish, bytes, effects) = if layout == SessionLayout::Directory {
                (log_path(&id), log_json.into_bytes(), Vec::new())
            } else {
                // First creation migrates the flat session into the directory layout.
                let persisted = PersistedConversation::from_conversation_with_exit(&conv, false);
                let existing = this.read_meta_inner(&id).await.ok().flatten();
                let sidecar = existing.unwrap_or_else(|| SessionMeta::from_conversation(&conv));
                let (publish, effects) = layout_effects(
                    &id,
                    true,
                    &sidecar,
                    vec![Effect::Write {
                        path: log_path(&id),
                        content: log_json,
                    }],
                )?;
                (publish, json_text(&persisted)?.into_bytes(), effects)
            };
            let committed = this
                .commit(
                    &mut held,
                    Plan {
                        op: "checkpoint",
                        publish,
                        bytes,
                        effects,
                        stamp: stamp.clone(),
                        admit,
                        best_effort_effects: false,
                    },
                )
                .await?;
            Self::settle(&mut held, stamp.as_ref(), &committed);
            if let Some(error) = committed.partial {
                return Err(error);
            }
            // DF-106: opportunistic retention, best effort, still inside the hold.
            if let Some(retention) = retention
                && log.entries.len() > retention
            {
                let _ = this.prune_old_snapshots(&id, &log, retention).await;
            }
            Ok(meta.id)
        })
        .await
    }

    /// Truncate or revert under the hold: same discipline, different prune rule.
    pub(super) async fn rewrite_locked(
        &self,
        held: &mut Held,
        id: &str,
        cut: Cut,
        stamp: Option<Stamp>,
    ) -> Result<(Conversation, Committed), StorageError> {
        let Some(Loaded {
            mut conv, layout, ..
        }) = self.read_inner(id).await?
        else {
            return Err(StorageError::NotFound(format!(
                "conversation not found: {id}"
            )));
        };
        let intent = if stamp.is_some() {
            Intent::Dependent
        } else {
            Intent::Derived
        };
        let admit = authority::admit(held.live(), true, stamp.as_ref(), intent)?;
        let mut checkpoint_log = None;
        let keep = match cut {
            Cut::Checkpoint(checkpoint) => {
                let found = self.load_checkpoint_log(id).await?;
                let meta = found
                    .entries
                    .iter()
                    .find(|e| e.id == checkpoint)
                    .ok_or_else(|| {
                        StorageError::NotFound(format!(
                            "checkpoint {} not found for conversation {}",
                            checkpoint.0, id
                        ))
                    })?
                    .clone();
                checkpoint_log = Some(found);
                meta.message_index + 1
            }
            Cut::Message(index) => index + 1,
        };
        conv.messages.truncate(keep.min(conv.messages.len()));
        conv.updated_at = crate::domain::models::session_meta::now_unix();
        if matches!(cut, Cut::Message(_)) {
            let surviving: std::collections::HashSet<String> =
                conv.messages.iter().map(|m| m.id.clone()).collect();
            conv.turns.retain(|turn| surviving.contains(&turn.id.0));
            conv.plans.retain(|_, plan| {
                plan.host_message_id
                    .as_ref()
                    .is_none_or(|host| surviving.contains(host))
            });
        }
        let mut extra = Vec::new();
        match (cut, checkpoint_log) {
            (Cut::Checkpoint(checkpoint), Some(found)) => {
                let mut kept = CheckpointLog {
                    entries: found
                        .entries
                        .into_iter()
                        .filter(|e| e.id <= checkpoint)
                        .collect(),
                };
                kept.entries.sort_by_key(|e| e.id);
                extra.push(Effect::Write {
                    path: log_path(id),
                    content: json_text(&kept)?,
                });
            }
            (Cut::Message(index), _) => {
                // Text-only sessions never created a log: nothing to prune, nothing created.
                if tokio::fs::try_exists(self.checkpoints_file(id))
                    .await
                    .unwrap_or(false)
                {
                    let found = self.load_checkpoint_log(id).await?;
                    let mut kept = CheckpointLog {
                        entries: found
                            .entries
                            .into_iter()
                            .filter(|e| e.message_index <= index)
                            .collect(),
                    };
                    kept.entries.sort_by_key(|e| e.id);
                    extra.push(Effect::Write {
                        path: log_path(id),
                        content: json_text(&kept)?,
                    });
                }
            }
            (Cut::Checkpoint(_), None) => {}
        }
        let persisted = PersistedConversation::from_conversation_with_exit(&conv, false);
        let directory = persisted.messages.iter().any(|m| !m.images.is_empty())
            || layout == SessionLayout::Directory;
        let existing = self.read_meta_inner(id).await.ok().flatten();
        let meta = sidecar_for(
            existing,
            &SessionMeta::from_conversation(&conv),
            persisted.messages.len(),
        );
        let (publish, effects) = layout_effects(id, directory, &meta, extra)?;
        let committed = self
            .commit(
                held,
                Plan {
                    op: "rewrite",
                    publish,
                    bytes: json_text(&persisted)?.into_bytes(),
                    effects,
                    stamp: stamp.clone(),
                    admit,
                    best_effort_effects: false,
                },
            )
            .await?;
        Self::settle(held, stamp.as_ref(), &committed);
        let installed = Self::installed(conv, held, &committed);
        Ok((installed, committed))
    }

    pub(super) async fn rewrite(
        &self,
        id: &str,
        cut: Cut,
        stamp: Option<Stamp>,
    ) -> Result<Conversation, StorageError> {
        self.transact(id, move |this, mut held| async move {
            let id = held_id(&held);
            this.recover(&mut held).await?;
            let (conv, committed) = this.rewrite_locked(&mut held, &id, cut, stamp).await?;
            committed.partial.map_or(Ok(conv), Err)
        })
        .await
    }

    // ── fork ─────────────────────────────────────────────────────────────────

    /// Fork under the source's authority (accepted source version) into a new incarnation.
    pub(super) async fn fork_from(
        &self,
        source_id: &str,
        message_index: usize,
        stamp: Option<Stamp>,
    ) -> Result<String, StorageError> {
        use crate::domain::models::conversation::{ForkSource, generate_conversation_id};
        self.transact(source_id, move |this, mut held| async move {
            let source_id = held_id(&held);
            this.recover(&mut held).await?;
            let Some(Loaded { conv: source, .. }) = this.read_inner(&source_id).await? else {
                return Err(StorageError::NotFound(format!("conversation not found: {source_id}")));
            };
            let intent = if stamp.is_some() { Intent::Dependent } else { Intent::Derived };
            authority::admit(held.live(), true, stamp.as_ref(), intent)?;
            if source.messages.is_empty() {
                return Err(StorageError::Other("empty conversation".to_string()));
            }
            if message_index >= source.messages.len() {
                return Err(StorageError::Other(format!(
                    "message index {} out of bounds (conversation has {} messages)",
                    message_index,
                    source.messages.len()
                )));
            }
            let new_id = generate_conversation_id();
            let truncated: Vec<_> = source.messages[..=message_index].to_vec();
            let now = crate::domain::models::session_meta::now_unix();
            let mut forked_plans = std::collections::HashMap::new();
            for (plan_id, plan) in &source.plans {
                // A standalone plan's display host is deliberately absent after save; legacy
                // durable hosts still prune.
                if plan.host_message_id.as_ref().is_none_or(|host| truncated.iter().any(|m| &m.id == host)) {
                    forked_plans.insert(plan_id.clone(), plan.clone());
                }
            }
            let forked_title = if source.title.trim().is_empty() {
                "Fork of (Untitled)".to_string()
            } else {
                format!("Fork of {}", source.title)
            };
            let forked = Conversation {
                id: new_id.clone(),
                title: forked_title,
                messages: truncated,
                created_at: now,
                updated_at: now,
                session_id: Some(generate_conversation_id()),
                plans: forked_plans,
                fork_source: Some(ForkSource {
                    conversation_id: source.id.clone(),
                    message_index,
                    checkpoint_id: CheckpointId(message_index as u64),
                }),
                ..Default::default()
            };
            let mut unique_refs: Vec<ImageReference> = Vec::new();
            let mut seen = std::collections::HashSet::<String>::new();
            for img in forked.messages.iter().flat_map(|m| &m.images) {
                if seen.insert(img.file_name.clone()) {
                    unique_refs.push(img.clone());
                }
            }
            // Amendment 2: the checkpoint log (and snapshot files) up to the fork point.
            let source_log = this.load_checkpoint_log(&source_id).await?;
            let filtered: Vec<_> = source_log.entries.into_iter().filter(|e| e.message_index <= message_index).collect();
            let eligible: std::collections::HashSet<u64> = filtered.iter().map(|e| e.id.0).collect();
            let persisted = PersistedConversation::from_conversation_with_exit(&forked, false);
            let directory = !unique_refs.is_empty() || !filtered.is_empty();
            let sidecar = sidecar_for(None, &SessionMeta::from_conversation(&forked), persisted.messages.len());
            let mut extra = Vec::new();
            if !filtered.is_empty() {
                extra.push(Effect::Write {
                    path: log_path(&new_id),
                    content: json_text(&CheckpointLog { entries: filtered })?,
                });
            }
            let (publish, effects) = layout_effects(&new_id, directory, &sidecar, extra)?;
            // The new id is unshared: its own create authority, always taken after the source's.
            let mut target = authority::acquire(&this.sessions_dir, &new_id).await?;
            let target_stamp = forked.persist.capture();
            let admit = authority::admit(target.live(), false, Some(&target_stamp), Intent::Create)?;
            let committed = this
                .commit(
                    &mut target,
                    Plan {
                        op: "fork",
                        publish,
                        bytes: json_text(&persisted)?.into_bytes(),
                        effects,
                        stamp: Some(target_stamp),
                        admit,
                        best_effort_effects: false,
                    },
                )
                .await?;
            if let Some(error) = committed.partial {
                return Err(StorageError::PartialCommit(format!("fork {new_id} was created but not completed: {error}")));
            }
            // Best-effort copies, with the source version still held.
            if !unique_refs.is_empty()
                && let Err(e) = this.copy_images(&source_id, &new_id, &unique_refs).await
            {
                tracing::warn!(source = source_id, target = %new_id, "Failed to copy images during fork: {}", e);
            }
            if !eligible.is_empty() {
                this.copy_snapshots(&source_id, &new_id, &eligible).await;
            }
            tracing::debug!("Forked conversation {} -> {} at message {}", source_id, new_id, message_index);
            Ok(new_id)
        })
        .await
    }

    /// Copy snapshot files whose checkpoint id survives the fork (best effort).
    async fn copy_snapshots(
        &self,
        source: &str,
        target: &str,
        eligible: &std::collections::HashSet<u64>,
    ) {
        let (src_dir, dst_dir) = (self.snapshots_dir(source), self.snapshots_dir(target));
        let Ok(mut entries) = tokio::fs::read_dir(&src_dir).await else {
            return;
        };
        let _ = tokio::fs::create_dir_all(&dst_dir).await;
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".tmp") {
                continue;
            }
            let Some(cp_id) = name.split('_').next().and_then(|s| s.parse::<u64>().ok()) else {
                continue;
            };
            if !eligible.contains(&cp_id) {
                continue;
            }
            if let Err(e) = tokio::fs::copy(entry.path(), dst_dir.join(&name)).await {
                tracing::warn!(source, target, snapshot = %name, "Failed to copy snapshot file during fork: {}", e);
            }
        }
    }

    // ── rewind ───────────────────────────────────────────────────────────────

    async fn write_journal(&self, id: &str, txn: &RewindTxn) -> Result<(), StorageError> {
        tokio::fs::create_dir_all(self.session_dir(id))
            .await
            .map_err(|e| StorageError::IoError(format!("Failed to create session dir: {}", e)))?;
        let content = serde_json::to_vec_pretty(txn)
            .map_err(|e| StorageError::SerializationError(e.to_string()))?;
        Self::atomic_write(&self.sessions_dir, self.rewind_txn_path(id), content)
            .await
            .map_err(|(_, e)| StorageError::IoError(format!("rewind_txn write: {}", e)))
    }

    /// Journal, file reversion, inclusive truncation and journal removal as one operation
    /// that owns the session (and is bound to the accepted history `stamp` names).
    pub(super) async fn rewind_bound(
        &self,
        conv: &Conversation,
        target: usize,
    ) -> Result<Rewound, RewindStep> {
        let stamp = conv.persist.capture();
        let id = conv.id.clone();
        let task_stamp = stamp.clone();
        let applied = self
            .transact(&id, move |this, mut held| async move {
                Ok(this.rewind_locked(&mut held, task_stamp, target).await)
            })
            .await
            .map_err(RewindStep::Begin)??;
        conv.persist.advance(&stamp);
        Ok(applied)
    }

    async fn rewind_locked(
        &self,
        held: &mut Held,
        stamp: Stamp,
        target: usize,
    ) -> Result<Rewound, RewindStep> {
        let id = held_id(held);
        self.recover(held).await.map_err(RewindStep::Begin)?;
        // Validate the accepted version and the durable target before any effect.
        let Some(Loaded { conv: disk, .. }) =
            self.read_inner(&id).await.map_err(RewindStep::Begin)?
        else {
            return Err(RewindStep::Begin(StorageError::NotFound(format!(
                "conversation not found: {id}"
            ))));
        };
        authority::admit(held.live(), true, Some(&stamp), Intent::Dependent)
            .map_err(RewindStep::Begin)?;
        if target >= disk.messages.len() {
            return Err(RewindStep::Begin(StorageError::Stale(
                "the rewind target is not part of the accepted history".into(),
            )));
        }
        let bound = held.live().map(|r| RewindBinding {
            epoch: r.epoch.clone(),
            version: r.version,
        });
        let mut txn = RewindTxn {
            conversation_id: id.clone(),
            target_message_index: target,
            phase: RewindTxnPhase::Pending,
            created_at: crate::domain::models::session_meta::now_unix(),
            bound,
            files_begun: false,
        };
        self.write_journal(&id, &txn)
            .await
            .map_err(RewindStep::Begin)?;
        #[cfg(test)]
        if super::session_authority::fault::hit(held.root(), "rewind_begun").await {
            return Err(RewindStep::Begin(StorageError::Indeterminate(
                "injected stop after the journal began".into(),
            )));
        }
        // From here a Pending journal no longer proves that nothing started.
        txn.files_begun = true;
        self.write_journal(&id, &txn)
            .await
            .map_err(RewindStep::Begin)?;
        #[cfg(test)]
        if super::session_authority::fault::hit(held.root(), "rewind_files_begun").await {
            return Err(RewindStep::Begin(StorageError::Indeterminate(
                "injected stop after files began".into(),
            )));
        }
        let reverted = self
            .revert_file_snapshots(&id, target)
            .await
            .unwrap_or_default();
        txn.phase = RewindTxnPhase::FilesReverted;
        self.write_journal(&id, &txn)
            .await
            .map_err(RewindStep::Truncate)?;
        #[cfg(test)]
        if super::session_authority::fault::hit(held.root(), "rewind_files_reverted").await {
            return Err(RewindStep::Truncate(StorageError::Indeterminate(
                "injected stop before truncation".into(),
            )));
        }
        let (conversation, committed) = self
            .rewrite_locked(held, &id, Cut::Message(target), Some(stamp))
            .await
            .map_err(RewindStep::Truncate)?;
        if let Some(error) = committed.partial {
            return Err(RewindStep::Truncate(error));
        }
        if let Err(error) = tokio::fs::remove_file(self.rewind_txn_path(&id)).await
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!("commit_rewind_txn failed (non-fatal): {}", error);
        }
        Ok(Rewound {
            conversation,
            reverted,
        })
    }

    /// Unbound journal calls: serialized and refused for a deleted session.
    pub(super) async fn journal_locked<T, F, Fut>(&self, id: &str, op: F) -> Result<T, StorageError>
    where
        T: Send + 'static,
        F: FnOnce(FileSystemStorage) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, StorageError>> + Send + 'static,
    {
        self.transact(id, move |this, mut held| async move {
            this.recover(&mut held).await?;
            if held.live().is_some_and(|r| r.tombstone) {
                return Err(StorageError::Stale("the session was deleted".into()));
            }
            op(this).await
        })
        .await
    }

    pub(super) async fn begin_journal(&self, id: &str, target: usize) -> Result<(), StorageError> {
        let txn = RewindTxn {
            conversation_id: id.to_string(),
            target_message_index: target,
            phase: RewindTxnPhase::Pending,
            created_at: crate::domain::models::session_meta::now_unix(),
            bound: None,
            files_begun: false,
        };
        let id = id.to_string();
        self.journal_locked(&id.clone(), move |this| async move {
            this.write_journal(&id, &txn).await
        })
        .await
    }

    pub(super) async fn advance_journal(
        &self,
        id: &str,
        phase: RewindTxnPhase,
    ) -> Result<(), StorageError> {
        let id = id.to_string();
        self.journal_locked(&id.clone(), move |this| async move {
            let mut txn = this.load_rewind_txn_inner(&id).await?.ok_or_else(|| {
                StorageError::IoError(format!(
                    "write_rewind_phase: no active transaction for {}",
                    id
                ))
            })?;
            txn.phase = phase;
            this.write_journal(&id, &txn).await
        })
        .await
    }

    pub(super) async fn remove_journal(&self, id: &str) -> Result<(), StorageError> {
        let id = id.to_string();
        self.journal_locked(&id.clone(), move |this| async move {
            match tokio::fs::remove_file(this.rewind_txn_path(&id)).await {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(StorageError::IoError(format!(
                    "commit_rewind_txn: failed to remove journal: {}",
                    e
                ))),
            }
        })
        .await
    }

    /// Startup recovery of journals. Live and in-use sessions are skipped; a bound journal
    /// acts only when the record proves the intended state.
    pub(super) async fn reconcile_journals(&self) -> Result<(), StorageError> {
        let mut entries = match tokio::fs::read_dir(&self.sessions_dir).await {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                return Err(StorageError::IoError(format!(
                    "reconcile_pending_txns: read_dir failed: {}",
                    e
                )));
            }
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            if !entry.metadata().await.is_ok_and(|m| m.is_dir()) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.')
                || !tokio::fs::try_exists(entry.path().join("rewind_txn.json"))
                    .await
                    .unwrap_or(false)
            {
                continue;
            }
            let txn = match self.load_rewind_txn_inner(&name).await {
                Ok(Some(t)) => t,
                Ok(None) => continue,
                Err(e) => {
                    tracing::warn!(conversation_id = %name, "reconcile_pending_txns: failed to load journal: {}", e);
                    continue;
                }
            };
            tracing::info!(
                conversation_id = %name,
                phase = ?txn.phase,
                target_message_index = txn.target_message_index,
                "reconcile_pending_txns: found incomplete rewind transaction"
            );
            match authority::try_acquire_existing(&self.sessions_dir, &name).await {
                Ok(Opened::Busy) => {
                    tracing::info!(conversation_id = %name, "reconcile_pending_txns: session in use; journal left to its owner");
                }
                Ok(Opened::Held(mut held)) => match txn.bound.clone() {
                    Some(bound) => self.reconcile_bound(&mut held, &name, &txn, &bound).await,
                    None => {
                        drop(held);
                        self.reconcile_unbound(&name, &txn).await;
                    }
                },
                Ok(Opened::Absent) => self.reconcile_unbound(&name, &txn).await,
                Err(e) => {
                    tracing::warn!(conversation_id = %name, "reconcile_pending_txns: authority unavailable: {}", e);
                }
            }
        }
        Ok(())
    }

    /// A journal bound to an accepted version: act only on proven, abandoned state.
    async fn reconcile_bound(
        &self,
        held: &mut Held,
        id: &str,
        txn: &RewindTxn,
        bound: &RewindBinding,
    ) {
        if let Err(e) = self.recover(held).await {
            tracing::warn!(conversation_id = %id, "reconcile_pending_txns: recovery failed, journal kept: {}", e);
            return;
        }
        let Some(record) = held.live().cloned() else {
            tracing::warn!(conversation_id = %id, "reconcile_pending_txns: bound journal has no accepted record; kept");
            return;
        };
        let remove = |why: &str| {
            tracing::info!(conversation_id = %id, "reconcile_pending_txns: {why}");
            std::fs::remove_file(self.rewind_txn_path(id))
        };
        if record.epoch != bound.epoch || record.version < bound.version {
            tracing::warn!(conversation_id = %id, "reconcile_pending_txns: journal belongs to another incarnation; kept");
            return;
        }
        match txn.phase {
            RewindTxnPhase::Committed => {
                let _ = remove("removed a committed journal");
            }
            RewindTxnPhase::Pending if !txn.files_begun && record.version == bound.version => {
                let _ = remove("nothing had started; journal removed");
            }
            RewindTxnPhase::FilesReverted if record.version == bound.version => {
                match self
                    .rewrite_locked(held, id, Cut::Message(txn.target_message_index), None)
                    .await
                {
                    Ok((_, committed)) if committed.partial.is_none() => {
                        let _ = remove("completed the missing truncation");
                    }
                    other => {
                        tracing::warn!(conversation_id = %id, "reconcile_pending_txns: truncation not completed, journal kept: {:?}", other.err())
                    }
                }
            }
            RewindTxnPhase::FilesReverted if record.version == bound.version + 1 => {
                let _ = remove("the truncation had already published; journal removed");
            }
            _ => tracing::warn!(
                conversation_id = %id,
                phase = ?txn.phase,
                "reconcile_pending_txns: state not provably abandoned; journal kept intact"
            ),
        }
    }

    /// A journal written through the unbound id-only API: the DF-109 recovery, unchanged.
    async fn reconcile_unbound(&self, id: &str, txn: &RewindTxn) {
        let path = self.rewind_txn_path(id);
        let completed = match txn.phase {
            RewindTxnPhase::Pending | RewindTxnPhase::Committed => true,
            RewindTxnPhase::MessagesTruncated => {
                match self
                    .revert_file_snapshots(id, txn.target_message_index)
                    .await
                {
                    Ok(_) => true,
                    Err(e) => {
                        tracing::warn!(conversation_id = %id, "reconcile_pending_txns: file revert completion failed: {e}; preserving journal at {}", path.display());
                        false
                    }
                }
            }
            RewindTxnPhase::FilesReverted => {
                match self
                    .truncate_conversation(id, txn.target_message_index)
                    .await
                {
                    Ok(_) => true,
                    Err(e) => {
                        tracing::warn!(conversation_id = %id, "reconcile_pending_txns: truncation completion failed: {e}; preserving journal at {}", path.display());
                        false
                    }
                }
            }
        };
        if completed && let Err(e) = tokio::fs::remove_file(&path).await {
            tracing::warn!(conversation_id = %id, "reconcile_pending_txns: failed to remove journal: {}", e);
        }
    }
}

/// How a rewrite cuts the transcript.
#[derive(Clone, Copy)]
pub(super) enum Cut {
    /// Keep rows through the checkpoint's message index; prune the log by checkpoint id.
    Checkpoint(CheckpointId),
    /// Keep rows `0..=index`; prune the log by message index.
    Message(usize),
}

/// The validated id the hold was acquired for.
fn held_id(held: &Held) -> String {
    held.id().to_string()
}
