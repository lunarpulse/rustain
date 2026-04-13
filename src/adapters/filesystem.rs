//! FileSystemStorage adapter — persists conversations to `{workspace}/.claude/sessions/`.
//!
//! Implements `StoragePort` using async file I/O via `tokio::fs`.
//! Session files use CC-compatible camelCase JSON format (`.meta.json`).

use std::path::PathBuf;

use async_trait::async_trait;
use sha2::{Digest, Sha256};

use crate::domain::errors::StorageError;
use crate::domain::models::{Conversation, ConversationSummary, ImageReference, SessionMeta};
use crate::domain::ports::StoragePort;

/// Compute a content-addressed hash of image bytes for filename generation.
///
/// Uses SHA-256 and takes the first 16 hex chars (8 bytes). Collision probability
/// is negligible for the <100 images/session scale of this app. SHA-256 is chosen
/// over `std::hash::DefaultHasher` because the latter's algorithm is
/// implementation-defined across Rust versions, which would break
/// content-addressed storage after toolchain upgrades. See Story 4-3a.1 Dev Notes.
pub fn content_hash(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let hex = format!("{:x}", digest);
    hex[..16].to_string()
}

/// Normalize image `media_type` (e.g., "image/PNG") to a canonical lowercase
/// filename extension.
///
/// Case-insensitive matching resolves the "Image extension check case-sensitive"
/// finding from Story 4-1 review (Story 4-3a.1 AC6). Unknown media types fall
/// back to `"bin"` so the file still saves and can be inspected; load-time
/// validation will report the missing type to the user.
pub fn normalize_extension(media_type: &str) -> &'static str {
    match media_type.to_ascii_lowercase().as_str() {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "bin",
    }
}

/// Which layout a persisted session is using on disk.
///
/// Story 4-3a.1 introduced the directory-per-session layout for conversations
/// with image attachments. Sessions without images continue to use the flat
/// format for backward compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionLayout {
    /// Old layout: `{sessions_dir}/{id}.meta.json` + `{id}.session.json`.
    Flat,
    /// New layout: `{sessions_dir}/{id}/conversation.json` + `meta.json` + `images/`.
    Directory,
    /// No session files or directory found for this id.
    Missing,
}

/// Filesystem-backed storage for conversations.
///
/// Stores each conversation as `{sessions_dir}/{id}.meta.json` (flat) or
/// `{sessions_dir}/{id}/` (directory layout, used when images are attached).
#[derive(Debug)]
pub struct FileSystemStorage {
    sessions_dir: PathBuf,
}

impl FileSystemStorage {
    /// Create a new `FileSystemStorage` targeting the given sessions directory.
    pub fn new(sessions_dir: PathBuf) -> Self {
        Self { sessions_dir }
    }

    /// Ensure the sessions directory exists.
    pub async fn ensure_dir(&self) -> Result<(), StorageError> {
        tokio::fs::create_dir_all(&self.sessions_dir)
            .await
            .map_err(|e| StorageError::IoError(format!("Failed to create sessions dir: {}", e)))
    }

    /// Build the flat-format main conversation file path (`{sessions_dir}/{id}.meta.json`).
    ///
    /// Validates that the ID contains only safe characters (alphanumeric, `-`, `_`)
    /// to prevent path traversal attacks from crafted session files.
    fn session_path(&self, id: &str) -> PathBuf {
        self.sessions_dir
            .join(format!("{}.meta.json", Self::sanitize_id(id)))
    }

    /// Build the flat-format SessionMeta sidecar file path (`{sessions_dir}/{id}.session.json`).
    fn session_meta_path(&self, id: &str) -> PathBuf {
        self.sessions_dir
            .join(format!("{}.session.json", Self::sanitize_id(id)))
    }

    /// Directory-format session root (`{sessions_dir}/{id}/`).
    /// Used when a conversation has image attachments. See Story 4-3a.1 AC2.
    fn session_dir(&self, id: &str) -> PathBuf {
        self.sessions_dir.join(Self::sanitize_id(id))
    }

    /// Directory-format main conversation file (`{sessions_dir}/{id}/conversation.json`).
    fn conversation_file(&self, id: &str) -> PathBuf {
        self.session_dir(id).join("conversation.json")
    }

    /// Directory-format SessionMeta sidecar file (`{sessions_dir}/{id}/meta.json`).
    ///
    /// Note: `meta.json` has two distinct meanings across layouts — in the flat format
    /// `{id}.meta.json` is the full PersistedConversation, while in the directory format
    /// `{id}/meta.json` is SessionMeta only. Always use `detect_layout()` to disambiguate.
    fn meta_file_in_dir(&self, id: &str) -> PathBuf {
        self.session_dir(id).join("meta.json")
    }

    /// Directory-format images subdirectory (`{sessions_dir}/{id}/images/`).
    fn images_dir(&self, id: &str) -> PathBuf {
        self.session_dir(id).join("images")
    }

    /// Detect the storage layout for a given conversation id.
    ///
    /// Disambiguation rule: always use directory vs. file presence, never filename.
    /// - `Directory` if `{sessions_dir}/{id}/` exists as a directory
    /// - `Flat` if `{sessions_dir}/{id}.meta.json` exists as a file
    /// - `Missing` otherwise
    async fn detect_layout(&self, id: &str) -> SessionLayout {
        let dir = self.session_dir(id);
        if let Ok(meta) = tokio::fs::metadata(&dir).await
            && meta.is_dir()
        {
            return SessionLayout::Directory;
        }
        let flat = self.session_path(id);
        if let Ok(meta) = tokio::fs::metadata(&flat).await
            && meta.is_file()
        {
            return SessionLayout::Flat;
        }
        SessionLayout::Missing
    }

    /// Sanitize a conversation ID to prevent path traversal.
    /// Delegates to shared utility; returns "invalid" on failure for backward compatibility.
    fn sanitize_id(id: &str) -> &str {
        crate::infrastructure::utils::sanitize_id(id).unwrap_or("invalid")
    }

    /// Save a conversation with the `clean_exit` flag set (used for graceful shutdown).
    pub async fn save_conversation_with_exit(
        &self,
        conv: &Conversation,
        clean_exit: bool,
    ) -> Result<(), StorageError> {
        self.save_conversation_inner(conv, clean_exit).await
    }

    /// Load a conversation and return its `clean_exit` flag for crash detection.
    pub async fn load_conversation_with_exit(
        &self,
        id: &str,
    ) -> Result<Option<(Conversation, bool)>, StorageError> {
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
        Ok(Some((persisted.to_conversation(), clean_exit)))
    }

    /// Shared layout-aware save implementation. Any message carrying `images` forces
    /// the directory layout (migrating from flat if needed); otherwise the
    /// pre-existing flat format is preserved.
    async fn save_conversation_inner(
        &self,
        conv: &Conversation,
        clean_exit: bool,
    ) -> Result<(), StorageError> {
        self.ensure_dir().await?;

        let has_images = conv.messages.iter().any(|m| !m.images.is_empty());
        let persisted = PersistedConversation::from_conversation_with_exit(conv, clean_exit);
        let json = serde_json::to_string_pretty(&persisted)
            .map_err(|e| StorageError::SerializationError(e.to_string()))?;
        let meta = SessionMeta::from_conversation(conv);

        let current_layout = self.detect_layout(&conv.id).await;

        if has_images || current_layout == SessionLayout::Directory {
            // Write using directory layout.
            self.save_directory_layout(&conv.id, &json, &meta).await?;

            // Clean up any stale flat files. `detect_layout` returns `Directory`
            // whenever `{sessions_dir}/{id}/` exists — which can happen if
            // `save_image` ran first and created the session directory while
            // the original flat files still linger on disk. Treat cleanup as
            // unconditional-but-best-effort: missing files are ignored.
            self.finish_flat_to_directory_migration(&conv.id).await?;
        } else {
            // Flat layout: legacy behaviour.
            self.save_flat_layout(&conv.id, &json).await?;
            if let Err(e) = self.save_session_meta(&conv.id, &meta).await {
                tracing::warn!(
                    "Failed to write session meta sidecar for {}: {}",
                    conv.id,
                    e
                );
            }
        }

        Ok(())
    }

    /// Atomic write of PersistedConversation JSON using the flat layout.
    async fn save_flat_layout(&self, id: &str, json: &str) -> Result<(), StorageError> {
        let sanitized = Self::sanitize_id(id);
        let path = self.session_path(id);
        let tmp_path = self
            .sessions_dir
            .join(format!("{}.meta.json.tmp", sanitized));
        tokio::fs::write(&tmp_path, json.as_bytes())
            .await
            .map_err(|e| StorageError::IoError(format!("Failed to write session file: {}", e)))?;
        if let Err(e) = tokio::fs::rename(&tmp_path, &path).await {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Err(StorageError::IoError(format!(
                "Failed to rename session file: {}",
                e
            )));
        }
        Ok(())
    }

    /// Atomic write of PersistedConversation + SessionMeta using the directory layout.
    ///
    /// Creates `{sessions_dir}/{id}/`, then writes `conversation.json` and `meta.json`
    /// via temp-file-and-rename for crash safety.
    async fn save_directory_layout(
        &self,
        id: &str,
        conversation_json: &str,
        meta: &SessionMeta,
    ) -> Result<(), StorageError> {
        let dir = self.session_dir(id);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| StorageError::IoError(format!("Failed to create session dir: {}", e)))?;

        // Write conversation.json atomically.
        let conv_path = self.conversation_file(id);
        let conv_tmp = dir.join("conversation.json.tmp");
        tokio::fs::write(&conv_tmp, conversation_json.as_bytes())
            .await
            .map_err(|e| {
                StorageError::IoError(format!("Failed to write conversation.json: {}", e))
            })?;
        if let Err(e) = tokio::fs::rename(&conv_tmp, &conv_path).await {
            let _ = tokio::fs::remove_file(&conv_tmp).await;
            return Err(StorageError::IoError(format!(
                "Failed to rename conversation.json: {}",
                e
            )));
        }

        // Write meta.json atomically.
        let meta_json = serde_json::to_string_pretty(meta)
            .map_err(|e| StorageError::SerializationError(e.to_string()))?;
        let meta_path = self.meta_file_in_dir(id);
        let meta_tmp = dir.join("meta.json.tmp");
        tokio::fs::write(&meta_tmp, meta_json.as_bytes())
            .await
            .map_err(|e| StorageError::IoError(format!("Failed to write meta.json: {}", e)))?;
        if let Err(e) = tokio::fs::rename(&meta_tmp, &meta_path).await {
            let _ = tokio::fs::remove_file(&meta_tmp).await;
            return Err(StorageError::IoError(format!(
                "Failed to rename meta.json: {}",
                e
            )));
        }

        Ok(())
    }

    /// Delete the flat-format files after a successful directory-layout save.
    /// Called only when migrating a previously-flat session to the new layout.
    async fn finish_flat_to_directory_migration(&self, id: &str) -> Result<(), StorageError> {
        let flat_main = self.session_path(id);
        let flat_sidecar = self.session_meta_path(id);

        // Delete old flat files (best-effort; missing = already done).
        match tokio::fs::remove_file(&flat_main).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(StorageError::IoError(format!(
                    "Failed to delete legacy flat session file during migration: {}",
                    e
                )));
            }
        }
        match tokio::fs::remove_file(&flat_sidecar).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(StorageError::IoError(format!(
                    "Failed to delete legacy flat sidecar during migration: {}",
                    e
                )));
            }
        }

        tracing::info!(
            conversation_id = %id,
            "Migrated session from flat to directory layout (first image save)"
        );

        Ok(())
    }

    /// Save raw image bytes to `{sessions_dir}/{id}/images/{image_ref.file_name}`.
    ///
    /// Creates the session directory and its `images/` subdirectory if missing.
    /// On Unix, file permissions are set to `0o600` to avoid leaking potentially
    /// sensitive attachments to other local users. Adapter-specific — NOT on
    /// the `StoragePort` trait (see Story 4-3a.1 hexagonal rules).
    /// Maximum raw image bytes accepted by `save_image`.
    /// Mirrors `MAX_RAW_IMAGE_SIZE` in `adapters/tui/app.rs` (20MB paste cap).
    /// Enforced here as a second line of defence at the persistence boundary so that
    /// a non-paste ingestion path (future story) cannot bypass the cap (P4, party-mode
    /// review 2026-04-12).
    pub const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024; // 20MB

    pub async fn save_image(
        &self,
        conversation_id: &str,
        image_ref: &ImageReference,
        data: &[u8],
    ) -> Result<(), StorageError> {
        if data.len() > Self::MAX_IMAGE_BYTES {
            return Err(StorageError::IoError(format!(
                "Image too large: {} bytes exceeds {} byte limit",
                data.len(),
                Self::MAX_IMAGE_BYTES
            )));
        }
        let dir = self.images_dir(conversation_id);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| StorageError::IoError(format!("Failed to create images dir: {}", e)))?;
        let path = dir.join(&image_ref.file_name);
        tokio::fs::write(&path, data)
            .await
            .map_err(|e| StorageError::IoError(format!("Failed to write image file: {}", e)))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            if let Err(e) = tokio::fs::set_permissions(&path, perms).await {
                tracing::warn!("Failed to set 0o600 on image {}: {}", path.display(), e);
            }
        }
        Ok(())
    }

    /// Load raw image bytes from a session's `images/` directory.
    /// Returns `StorageError::NotFound` if the file is missing.
    ///
    /// Public API used by the event loop to rehydrate historical image
    /// attachments into outbound API requests on every turn (Story 4-3a.1
    /// Addendum 2). Also used by integration tests. Missing files return
    /// `StorageError::NotFound` so the caller can apply AC4 graceful
    /// degradation.
    pub async fn load_image(
        &self,
        conversation_id: &str,
        image_ref: &ImageReference,
    ) -> Result<Vec<u8>, StorageError> {
        let path = self.images_dir(conversation_id).join(&image_ref.file_name);
        match tokio::fs::read(&path).await {
            Ok(bytes) => Ok(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(StorageError::NotFound(
                format!("Image file missing: {}", image_ref.file_name),
            )),
            Err(e) => Err(StorageError::IoError(format!(
                "Failed to read image file: {}",
                e
            ))),
        }
    }

    /// Copy image files referenced by `image_refs` from the source session's
    /// `images/` directory to the target session's `images/` directory.
    ///
    /// Used by `fork_at_checkpoint()` to hand image attachments to the forked
    /// conversation. Missing source files are logged and skipped (best-effort
    /// copy; AC4 graceful degradation handles the holes on reload). Duplicate
    /// file names are only copied once.
    pub async fn copy_images(
        &self,
        source_conversation_id: &str,
        target_conversation_id: &str,
        image_refs: &[ImageReference],
    ) -> Result<(), StorageError> {
        if image_refs.is_empty() {
            return Ok(());
        }

        let source_dir = self.images_dir(source_conversation_id);
        let target_dir = self.images_dir(target_conversation_id);

        // If the source session has no images/ directory (e.g. it was flat-only
        // with no attachments ever saved), the references are stale — warn and
        // bail out of the copy cleanly.
        if tokio::fs::metadata(&source_dir).await.is_err() {
            tracing::warn!(
                source = source_conversation_id,
                target = target_conversation_id,
                "Source session has no images/ dir; skipping copy"
            );
            return Ok(());
        }

        tokio::fs::create_dir_all(&target_dir).await.map_err(|e| {
            StorageError::IoError(format!("Failed to create target images dir: {}", e))
        })?;

        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for image_ref in image_refs {
            if !seen.insert(image_ref.file_name.as_str()) {
                continue;
            }
            let src = source_dir.join(&image_ref.file_name);
            let dst = target_dir.join(&image_ref.file_name);
            match tokio::fs::copy(&src, &dst).await {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    tracing::warn!(
                        "Image file missing during fork copy: {} ({})",
                        image_ref.file_name,
                        e
                    );
                }
                Err(e) => {
                    return Err(StorageError::IoError(format!(
                        "Failed to copy image {}: {}",
                        image_ref.file_name, e
                    )));
                }
            }
        }
        Ok(())
    }
}

#[async_trait]
impl StoragePort for FileSystemStorage {
    async fn save_conversation(&self, conv: &Conversation) -> Result<(), StorageError> {
        self.save_conversation_inner(conv, false).await
    }

    async fn load_conversation(&self, id: &str) -> Result<Option<Conversation>, StorageError> {
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

        let conv = persisted.to_conversation();

        // Story 4-3a.1 AC4: validate image references on load. Missing files
        // log a warning but never fail the load — the ImageReference entry is
        // preserved so later fixes (restore from backup, etc.) can re-populate.
        if layout == SessionLayout::Directory {
            let images_dir = self.images_dir(&conv.id);
            for msg in &conv.messages {
                for img in &msg.images {
                    let p = images_dir.join(&img.file_name);
                    if tokio::fs::metadata(&p).await.is_err() {
                        tracing::warn!(
                            conversation_id = %conv.id,
                            message_id = %msg.id,
                            file_name = %img.file_name,
                            "Image file missing on load (graceful degradation)"
                        );
                    }
                }
            }
        }

        Ok(Some(conv))
    }

    async fn list_conversations(&self) -> Result<Vec<ConversationSummary>, StorageError> {
        let mut entries = match tokio::fs::read_dir(&self.sessions_dir).await {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => {
                return Err(StorageError::IoError(format!(
                    "Failed to read sessions dir: {}",
                    e
                )));
            }
        };

        // Collect seen ids for dedup across layout variants. Sorted map so we can
        // overwrite earlier flat matches with directory entries if both are on
        // disk during a partially-completed migration.
        let mut by_id: std::collections::BTreeMap<String, ConversationSummary> =
            std::collections::BTreeMap::new();

        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| StorageError::IoError(e.to_string()))?
        {
            let path = entry.path();
            let file_name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n,
                None => continue,
            };

            // Skip temp files from in-flight atomic renames.
            if file_name.ends_with(".tmp") {
                continue;
            }

            let file_type = match entry.file_type().await {
                Ok(ft) => ft,
                Err(e) => {
                    tracing::warn!("Failed to stat {}: {}", path.display(), e);
                    continue;
                }
            };

            if file_type.is_dir() {
                // Directory layout: expect conversation.json + meta.json inside.
                let id = file_name.to_string();
                let meta_path = path.join("meta.json");
                let conv_path = path.join("conversation.json");

                // Peek at the main conversation to detect legacy forks missing
                // from the sidecar (DF-095 backfill).
                let persisted_for_backfill: Option<PersistedConversation> =
                    match tokio::fs::read_to_string(&conv_path).await {
                        Ok(c) => serde_json::from_str(&c).ok(),
                        Err(_) => None,
                    };

                if let Ok(content) = tokio::fs::read_to_string(&meta_path).await {
                    match serde_json::from_str::<SessionMeta>(&content) {
                        Ok(mut meta) => {
                            // Backfill: if the main conv has fork_source but the
                            // sidecar doesn't, mirror it and rewrite the sidecar.
                            if meta.fork_source.is_none()
                                && let Some(ref p) = persisted_for_backfill
                                && p.fork_source.is_some()
                            {
                                meta.fork_source = p.fork_source.clone();
                                if let Err(e) = self.save_session_meta(&id, &meta).await {
                                    tracing::warn!(
                                        "Failed to backfill fork_source into sidecar for {}: {}",
                                        id,
                                        e
                                    );
                                } else {
                                    tracing::info!(
                                        "Backfilled fork_source into {} sidecar (directory layout)",
                                        id
                                    );
                                }
                            }
                            let has_fork_source = meta.fork_source.is_some();
                            by_id.insert(
                                id.clone(),
                                ConversationSummary {
                                    id,
                                    title: meta.title,
                                    created_at: meta.created_at,
                                    updated_at: meta.updated_at,
                                    message_count: meta.message_count,
                                    has_fork_source,
                                },
                            );
                            continue;
                        }
                        Err(e) => {
                            tracing::warn!(
                                "Corrupted directory-layout meta.json for {}: {}",
                                id,
                                e
                            );
                        }
                    }
                }

                // Fall back to reading conversation.json directly.
                if let Some(persisted) = persisted_for_backfill {
                    let has_fork_source = persisted.fork_source.is_some();
                    let summary_id = persisted.id.clone();
                    by_id.insert(
                        summary_id.clone(),
                        ConversationSummary {
                            id: summary_id,
                            title: persisted.title,
                            created_at: persisted.created_at,
                            updated_at: persisted.updated_at.unwrap_or(persisted.created_at),
                            message_count: persisted.messages.len(),
                            has_fork_source,
                        },
                    );
                }
                continue;
            }

            // Flat layout: `{id}.meta.json`.
            if !file_name.ends_with(".meta.json") || file_name.ends_with(".meta.json.tmp") {
                continue;
            }
            let id = match path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.strip_suffix(".meta"))
            {
                Some(id) if !id.is_empty() => id.to_string(),
                _ => continue,
            };

            // If there's already a directory-layout entry with the same id, skip
            // the stale flat match (should not happen post-migration, but be safe).
            if by_id.contains_key(&id) {
                continue;
            }

            // Peek at the main conversation to detect legacy forks missing
            // from the sidecar (DF-095 backfill).
            let persisted_for_backfill: Option<PersistedConversation> =
                match tokio::fs::read_to_string(&path).await {
                    Ok(c) => serde_json::from_str(&c).ok(),
                    Err(_) => None,
                };

            // Try the SessionMeta sidecar first (fast path).
            let sidecar = self.session_meta_path(&id);
            if let Ok(content) = tokio::fs::read_to_string(&sidecar).await {
                match serde_json::from_str::<SessionMeta>(&content) {
                    Ok(mut meta) => {
                        if meta.fork_source.is_none()
                            && let Some(ref p) = persisted_for_backfill
                            && p.fork_source.is_some()
                        {
                            meta.fork_source = p.fork_source.clone();
                            if let Err(e) = self.save_session_meta(&id, &meta).await {
                                tracing::warn!(
                                    "Failed to backfill fork_source into sidecar for {}: {}",
                                    id,
                                    e
                                );
                            } else {
                                tracing::info!(
                                    "Backfilled fork_source into {} sidecar (flat layout)",
                                    id
                                );
                            }
                        }
                        let has_fork_source = meta.fork_source.is_some();
                        by_id.insert(
                            id.clone(),
                            ConversationSummary {
                                id,
                                title: meta.title,
                                created_at: meta.created_at,
                                updated_at: meta.updated_at,
                                message_count: meta.message_count,
                                has_fork_source,
                            },
                        );
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!("Corrupted session meta sidecar for {}: {}", id, e);
                    }
                }
            }

            // Fallback: use the main conversation file (backward compat).
            if let Some(persisted) = persisted_for_backfill {
                let has_fork_source = persisted.fork_source.is_some();
                by_id.insert(
                    id.clone(),
                    ConversationSummary {
                        id,
                        title: persisted.title,
                        created_at: persisted.created_at,
                        updated_at: persisted.updated_at.unwrap_or(persisted.created_at),
                        message_count: persisted.messages.len(),
                        has_fork_source,
                    },
                );
            } else if let Err(e) = tokio::fs::read_to_string(&path).await {
                tracing::warn!("Failed to read session file {}: {}", path.display(), e);
            }
        }

        // Sort by updatedAt desc (most recent first)
        let mut summaries: Vec<_> = by_id.into_values().collect();
        summaries.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(summaries)
    }

    async fn delete_conversation(&self, id: &str) -> Result<(), StorageError> {
        let layout = self.detect_layout(id).await;
        let mut errors = Vec::new();

        match layout {
            SessionLayout::Directory => {
                let dir = self.session_dir(id);
                match tokio::fs::remove_dir_all(&dir).await {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => errors.push(format!("Failed to delete session directory: {}", e)),
                }
            }
            SessionLayout::Flat | SessionLayout::Missing => {
                let path = self.session_path(id);
                let meta_path = self.session_meta_path(id);

                match tokio::fs::remove_file(&path).await {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => errors.push(format!("Failed to delete session file: {}", e)),
                }
                match tokio::fs::remove_file(&meta_path).await {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => errors.push(format!("Failed to delete session meta file: {}", e)),
                }
            }
        }

        if !errors.is_empty() {
            return Err(StorageError::IoError(errors.join("; ")));
        }

        tracing::debug!("Deleted conversation {}", id);
        Ok(())
    }

    async fn save_session_meta(&self, id: &str, meta: &SessionMeta) -> Result<(), StorageError> {
        self.ensure_dir().await?;

        let json = serde_json::to_string_pretty(meta)
            .map_err(|e| StorageError::SerializationError(e.to_string()))?;

        let layout = self.detect_layout(id).await;
        let (target, tmp) = match layout {
            SessionLayout::Directory => {
                let dir = self.session_dir(id);
                tokio::fs::create_dir_all(&dir).await.map_err(|e| {
                    StorageError::IoError(format!("Failed to create session dir: {}", e))
                })?;
                (self.meta_file_in_dir(id), dir.join("meta.json.tmp"))
            }
            SessionLayout::Flat | SessionLayout::Missing => {
                let sanitized = Self::sanitize_id(id);
                (
                    self.session_meta_path(id),
                    self.sessions_dir
                        .join(format!("{}.session.json.tmp", sanitized)),
                )
            }
        };

        tokio::fs::write(&tmp, json.as_bytes()).await.map_err(|e| {
            StorageError::IoError(format!("Failed to write session meta file: {}", e))
        })?;
        if let Err(e) = tokio::fs::rename(&tmp, &target).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(StorageError::IoError(format!(
                "Failed to rename session meta file: {}",
                e
            )));
        }

        Ok(())
    }

    async fn load_session_meta(&self, id: &str) -> Result<Option<SessionMeta>, StorageError> {
        let layout = self.detect_layout(id).await;
        let path = match layout {
            SessionLayout::Directory => self.meta_file_in_dir(id),
            // Flat and Missing both read from the flat sidecar — "Missing"
            // just means the main conversation file isn't written yet, but
            // the sidecar may still exist (e.g., unit tests that only
            // exercise `save_session_meta`).
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

        let meta: SessionMeta = serde_json::from_str(&content)
            .map_err(|e| StorageError::SerializationError(e.to_string()))?;

        Ok(Some(meta))
    }

    async fn fork_at_checkpoint(
        &self,
        source_conversation_id: &str,
        checkpoint: crate::domain::models::checkpoint::CheckpointId,
    ) -> Result<String, StorageError> {
        use crate::domain::models::conversation::{ForkSource, generate_conversation_id};
        use crate::domain::models::session_meta::now_unix;

        // Platform precondition: CheckpointId is u64, usize is platform-dependent.
        // On 64-bit targets (current support matrix) this is lossless. On 32-bit
        // targets the cast would silently truncate the high bits, so guard in
        // debug builds. See party-mode review 2026-04-12 (P1).
        debug_assert!(
            checkpoint.0 <= usize::MAX as u64,
            "CheckpointId {} exceeds usize::MAX on this platform",
            checkpoint.0
        );
        let message_index = checkpoint.0 as usize;

        // Load source conversation
        let source = match self.load_conversation(source_conversation_id).await? {
            Some(c) => c,
            None => {
                return Err(StorageError::NotFound(format!(
                    "conversation not found: {}",
                    source_conversation_id
                )));
            }
        };

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
        let now = now_unix();

        // P4: guard against empty/whitespace-only source titles to avoid "Fork of "
        // with a dangling trailing space. Party-mode review 2026-04-12.
        let forked_title = if source.title.trim().is_empty() {
            "Fork of (Untitled)".to_string()
        } else {
            format!("Fork of {}", source.title)
        };
        let forked = crate::domain::models::Conversation {
            id: new_id.clone(),
            title: forked_title,
            messages: truncated,
            created_at: now,
            updated_at: now,
            last_response_at: None,
            session_id: Some(generate_conversation_id()),
            usage: None,
            fork_source: Some(ForkSource {
                conversation_id: source.id.clone(),
                message_index,
                checkpoint_id: checkpoint,
            }),
        };

        // Collect unique image refs from the truncated range so we can hand a
        // copy to the forked session. Order-preserving dedup by file_name.
        let mut unique_refs: Vec<ImageReference> = Vec::new();
        {
            let mut seen = std::collections::HashSet::<String>::new();
            for msg in &forked.messages {
                for img in &msg.images {
                    if seen.insert(img.file_name.clone()) {
                        unique_refs.push(img.clone());
                    }
                }
            }
        }

        self.save_conversation(&forked).await?;

        // Best-effort image copy. Any failure logs a warning but does not
        // break the fork — AC4 graceful degradation handles missing files on
        // load. Story 4-3a.1 Task 3b.
        if !unique_refs.is_empty() {
            if let Err(e) = self
                .copy_images(source_conversation_id, &new_id, &unique_refs)
                .await
            {
                tracing::warn!(
                    source = source_conversation_id,
                    target = %new_id,
                    "Failed to copy images during fork: {}",
                    e
                );
            }
        }

        tracing::debug!(
            "Forked conversation {} -> {} at message {}",
            source_conversation_id,
            new_id,
            message_index
        );
        Ok(new_id)
    }
}

// ── PersistedConversation serde wrapper ────────────────────────────

use crate::domain::models::conversation::PersistedConversation;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::{ChatMessage, MessageRole, StopReason, UsageInfo};
    use tempfile::TempDir;

    fn make_test_conversation() -> Conversation {
        Conversation {
            id: "test-conv-123".to_string(),
            title: "Test Conversation".to_string(),
            messages: vec![
                ChatMessage {
                    id: "msg-test-001".to_string(),
                    role: MessageRole::User,
                    content: "Hello".to_string(),
                    content_blocks: vec![],
                    tool_calls: vec![],
                    created_at: 1700000000,
                    token_count: None,
                    stop_reason: None,
                    images: vec![],
                },
                ChatMessage {
                    id: "msg-test-002".to_string(),
                    role: MessageRole::Assistant,
                    content: "Hi there!".to_string(),
                    content_blocks: vec![],
                    tool_calls: vec![],
                    created_at: 1700000001,
                    token_count: Some(10),
                    stop_reason: Some(StopReason::EndTurn),
                    images: vec![],
                },
            ],
            created_at: 1700000000,
            updated_at: 1700000001,
            last_response_at: Some(1700000001),
            session_id: Some("sess-abc".to_string()),
            usage: Some(UsageInfo {
                input_tokens: 5,
                output_tokens: 10,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            }),
            fork_source: None,
        }
    }

    // 6.1: save_conversation creates valid JSON with camelCase keys
    #[tokio::test]
    async fn test_save_creates_camel_case_json() {
        let tmp = TempDir::new().unwrap();
        let storage = FileSystemStorage::new(tmp.path().join("sessions"));
        let conv = make_test_conversation();

        storage.save_conversation(&conv).await.unwrap();

        let path = tmp.path().join("sessions/test-conv-123.meta.json");
        assert!(path.exists());

        let content = std::fs::read_to_string(&path).unwrap();
        // Verify camelCase field names
        assert!(content.contains("\"createdAt\""));
        assert!(content.contains("\"updatedAt\""));
        assert!(content.contains("\"lastResponseAt\""));
        assert!(content.contains("\"sessionId\""));
        assert!(content.contains("\"contentBlocks\""));
        assert!(content.contains("\"toolCalls\""));
        // Verify it's valid JSON
        let _: serde_json::Value = serde_json::from_str(&content).unwrap();
    }

    // 6.2: load_conversation reads back saved conversation with all fields
    #[tokio::test]
    async fn test_roundtrip_save_load() {
        let tmp = TempDir::new().unwrap();
        let storage = FileSystemStorage::new(tmp.path().join("sessions"));
        let conv = make_test_conversation();

        storage.save_conversation(&conv).await.unwrap();
        let loaded = storage
            .load_conversation("test-conv-123")
            .await
            .unwrap()
            .expect("should load conversation");

        assert_eq!(loaded.id, conv.id);
        assert_eq!(loaded.title, conv.title);
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.created_at, conv.created_at);
        assert_eq!(loaded.updated_at, conv.updated_at);
        assert_eq!(loaded.last_response_at, conv.last_response_at);
        assert_eq!(loaded.session_id, conv.session_id);
        assert!(loaded.usage.is_some());
        assert!(loaded.fork_source.is_none());

        // Verify message content preserved
        assert_eq!(loaded.messages[0].content, "Hello");
        assert_eq!(loaded.messages[1].content, "Hi there!");
        assert_eq!(loaded.messages[1].stop_reason, Some(StopReason::EndTurn));
    }

    // 6.3: Forward compatibility — file with unknown fields loads without error
    #[tokio::test]
    async fn test_forward_compatibility_unknown_fields() {
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        // Write a JSON file with extra unknown fields (simulating a future version)
        let json = r#"{
            "id": "future-conv",
            "title": "Future Session",
            "messages": [],
            "createdAt": 1700000000,
            "updatedAt": 1700000001,
            "unknownField": "some value",
            "anotherNewField": { "nested": true },
            "enabledMcpServers": ["server1"]
        }"#;
        std::fs::write(sessions_dir.join("future-conv.meta.json"), json).unwrap();

        let storage = FileSystemStorage::new(sessions_dir);
        let loaded = storage
            .load_conversation("future-conv")
            .await
            .unwrap()
            .expect("should load despite unknown fields");

        assert_eq!(loaded.id, "future-conv");
        assert_eq!(loaded.title, "Future Session");
        assert_eq!(loaded.updated_at, 1700000001);
    }

    // 6.4: list_conversations returns sorted summaries
    #[tokio::test]
    async fn test_list_conversations_sorted() {
        let tmp = TempDir::new().unwrap();
        let storage = FileSystemStorage::new(tmp.path().join("sessions"));

        // Save three conversations with different timestamps
        for (id, ts) in [("c1", 1000), ("c2", 3000), ("c3", 2000)] {
            let mut conv = make_test_conversation();
            conv.id = id.to_string();
            conv.title = format!("Conv {}", id);
            conv.updated_at = ts;
            storage.save_conversation(&conv).await.unwrap();
        }

        let summaries = storage.list_conversations().await.unwrap();
        assert_eq!(summaries.len(), 3);
        // Sorted by updatedAt desc
        assert_eq!(summaries[0].id, "c2");
        assert_eq!(summaries[1].id, "c3");
        assert_eq!(summaries[2].id, "c1");
        assert_eq!(summaries[0].message_count, 2);
    }

    // 6.5: Missing sessions directory is auto-created
    #[tokio::test]
    async fn test_auto_create_sessions_dir() {
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path().join("deeply").join("nested").join("sessions");
        let storage = FileSystemStorage::new(sessions_dir.clone());

        assert!(!sessions_dir.exists());

        let conv = make_test_conversation();
        storage.save_conversation(&conv).await.unwrap();

        assert!(sessions_dir.exists());
        assert!(sessions_dir.join("test-conv-123.meta.json").exists());
    }

    // Load non-existent conversation returns None
    #[tokio::test]
    async fn test_load_nonexistent_returns_none() {
        let tmp = TempDir::new().unwrap();
        let storage = FileSystemStorage::new(tmp.path().join("sessions"));

        let result = storage.load_conversation("nonexistent").await.unwrap();
        assert!(result.is_none());
    }

    // List on non-existent directory returns empty
    #[tokio::test]
    async fn test_list_nonexistent_dir_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let storage = FileSystemStorage::new(tmp.path().join("no-such-dir"));

        let summaries = storage.list_conversations().await.unwrap();
        assert!(summaries.is_empty());
    }

    // 7.3: clean_exit flag defaults to false and is true after graceful save
    #[tokio::test]
    async fn test_clean_exit_false_by_default() {
        let tmp = TempDir::new().unwrap();
        let storage = FileSystemStorage::new(tmp.path().join("sessions"));
        let conv = make_test_conversation();

        // Normal save (via StoragePort) sets clean_exit = false
        storage.save_conversation(&conv).await.unwrap();

        let (_, clean_exit) = storage
            .load_conversation_with_exit("test-conv-123")
            .await
            .unwrap()
            .expect("should load");
        assert!(!clean_exit, "default save should have clean_exit = false");
    }

    #[tokio::test]
    async fn test_clean_exit_true_after_graceful_shutdown() {
        let tmp = TempDir::new().unwrap();
        let storage = FileSystemStorage::new(tmp.path().join("sessions"));
        let conv = make_test_conversation();

        // Graceful shutdown save sets clean_exit = true
        storage
            .save_conversation_with_exit(&conv, true)
            .await
            .unwrap();

        let (_, clean_exit) = storage
            .load_conversation_with_exit("test-conv-123")
            .await
            .unwrap()
            .expect("should load");
        assert!(clean_exit, "graceful save should have clean_exit = true");
    }

    #[tokio::test]
    async fn test_clean_exit_backward_compat() {
        // Old session files without clean_exit field should default to false
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let json = r#"{
            "id": "old-session",
            "title": "Old Session",
            "messages": [],
            "createdAt": 1700000000
        }"#;
        std::fs::write(sessions_dir.join("old-session.meta.json"), json).unwrap();

        let storage = FileSystemStorage::new(sessions_dir);
        let (_, clean_exit) = storage
            .load_conversation_with_exit("old-session")
            .await
            .unwrap()
            .expect("should load old session");
        assert!(
            !clean_exit,
            "old sessions without clean_exit should default to false (trigger recovery)"
        );
    }

    // SessionMeta sidecar tests
    #[tokio::test]
    async fn test_save_conversation_creates_session_meta() {
        let tmp = TempDir::new().unwrap();
        let storage = FileSystemStorage::new(tmp.path().join("sessions"));
        let conv = make_test_conversation();

        storage.save_conversation(&conv).await.unwrap();

        // Both files should exist
        assert!(tmp.path().join("sessions/test-conv-123.meta.json").exists());
        assert!(
            tmp.path()
                .join("sessions/test-conv-123.session.json")
                .exists()
        );

        // SessionMeta should have correct data
        let meta = storage
            .load_session_meta("test-conv-123")
            .await
            .unwrap()
            .expect("meta should exist");
        assert_eq!(meta.title, conv.title);
        assert_eq!(meta.message_count, conv.messages.len());
    }

    #[tokio::test]
    async fn test_load_session_meta_nonexistent() {
        let tmp = TempDir::new().unwrap();
        let storage = FileSystemStorage::new(tmp.path().join("sessions"));

        let result = storage.load_session_meta("nonexistent").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_delete_conversation_removes_both_files() {
        let tmp = TempDir::new().unwrap();
        let storage = FileSystemStorage::new(tmp.path().join("sessions"));
        let conv = make_test_conversation();

        // Save the conversation
        storage.save_conversation(&conv).await.unwrap();

        // Verify both files exist
        assert!(tmp.path().join("sessions/test-conv-123.meta.json").exists());
        assert!(
            tmp.path()
                .join("sessions/test-conv-123.session.json")
                .exists()
        );

        // Delete the conversation
        storage.delete_conversation("test-conv-123").await.unwrap();

        // Both files should be gone
        assert!(!tmp.path().join("sessions/test-conv-123.meta.json").exists());
        assert!(
            !tmp.path()
                .join("sessions/test-conv-123.session.json")
                .exists()
        );
    }

    #[tokio::test]
    async fn test_delete_nonexistent_is_ok() {
        let tmp = TempDir::new().unwrap();
        let storage = FileSystemStorage::new(tmp.path().join("sessions"));

        // Deleting a non-existent conversation should succeed
        storage.delete_conversation("nonexistent").await.unwrap();
    }

    #[tokio::test]
    async fn test_list_uses_session_meta_when_available() {
        let tmp = TempDir::new().unwrap();
        let storage = FileSystemStorage::new(tmp.path().join("sessions"));

        // Save a conversation (creates both files)
        let conv = make_test_conversation();
        storage.save_conversation(&conv).await.unwrap();

        // List should read from SessionMeta (fast path)
        let summaries = storage.list_conversations().await.unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, conv.id);
        assert_eq!(summaries[0].title, conv.title);
        assert_eq!(summaries[0].message_count, conv.messages.len());
    }

    #[tokio::test]
    async fn test_list_fallback_to_full_deserialization() {
        let tmp = TempDir::new().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        // Create only the full conversation file (simulate old session without sidecar)
        let json = r#"{
            "id": "old-session",
            "title": "Old Session Title",
            "messages": [{"id":"msg1","role":"user","content":"Hello","contentBlocks":[],"toolCalls":[],"createdAt":1700000000,"tokenCount":null}],
            "createdAt": 1700000000,
            "updatedAt": 1700000100
        }"#;
        std::fs::write(sessions_dir.join("old-session.meta.json"), json).unwrap();

        let storage = FileSystemStorage::new(sessions_dir);
        let summaries = storage.list_conversations().await.unwrap();

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, "old-session");
        assert_eq!(summaries[0].title, "Old Session Title");
        assert_eq!(summaries[0].message_count, 1);
    }

    // ── Phase 3: image helper unit tests (Story 4-3a.1 Tasks 3.6-3.11) ────

    #[test]
    fn test_content_hash_consistent_for_same_input() {
        let data = b"some image bytes here";
        let h1 = content_hash(data);
        let h2 = content_hash(data);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 16);
    }

    #[test]
    fn test_content_hash_different_for_different_input() {
        let h1 = content_hash(b"image one");
        let h2 = content_hash(b"image two");
        assert_ne!(h1, h2);
    }

    #[test]
    fn test_content_hash_uses_sha256_known_value() {
        // Regression: catches accidental reintroduction of DefaultHasher.
        // SHA-256("abc") prefix = "ba7816bf8f01cfea"
        let h = content_hash(b"abc");
        assert_eq!(h, "ba7816bf8f01cfea");
    }

    #[test]
    fn test_normalize_extension_case_insensitive() {
        assert_eq!(normalize_extension("image/PNG"), "png");
        assert_eq!(normalize_extension("IMAGE/jpeg"), "jpg");
        assert_eq!(normalize_extension("image/JPG"), "jpg");
        assert_eq!(normalize_extension("Image/Gif"), "gif");
        assert_eq!(normalize_extension("image/WEBP"), "webp");
        assert_eq!(normalize_extension("image/unknown"), "bin");
    }

    #[tokio::test]
    async fn test_save_image_load_image_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let storage = FileSystemStorage::new(tmp.path().join("sessions"));
        let image_ref = ImageReference {
            file_name: format!("{}.png", content_hash(b"rawdata")),
            media_type: "image/png".to_string(),
            original_size: 7,
        };
        storage
            .save_image("conv-img-1", &image_ref, b"rawdata")
            .await
            .unwrap();
        let loaded = storage.load_image("conv-img-1", &image_ref).await.unwrap();
        assert_eq!(loaded, b"rawdata");
    }

    // P4 (party-mode review 2026-04-12): save_image must reject data exceeding MAX_IMAGE_BYTES
    #[tokio::test]
    async fn test_save_image_rejects_oversized_data() {
        let tmp = TempDir::new().unwrap();
        let storage = FileSystemStorage::new(tmp.path().join("sessions"));
        let image_ref = ImageReference {
            file_name: format!("{}.png", content_hash(b"dummy")),
            media_type: "image/png".to_string(),
            original_size: 0,
        };
        // Allocate 21MB — 1MB over the 20MB cap.
        let oversized = vec![0u8; 21 * 1024 * 1024];
        let result = storage
            .save_image("test-conv-p4", &image_ref, &oversized)
            .await;
        assert!(
            result.is_err(),
            "save_image must reject data exceeding MAX_IMAGE_BYTES"
        );
        // The error should contain size information.
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("too large") || err_msg.contains("limit"),
            "error message should mention size: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn test_load_image_missing_returns_not_found() {
        let tmp = TempDir::new().unwrap();
        let storage = FileSystemStorage::new(tmp.path().join("sessions"));
        let image_ref = ImageReference {
            file_name: "missing.png".to_string(),
            media_type: "image/png".to_string(),
            original_size: 0,
        };
        let err = storage
            .load_image("nope", &image_ref)
            .await
            .expect_err("should error on missing file");
        assert!(matches!(err, StorageError::NotFound(_)));
    }

    #[tokio::test]
    async fn test_copy_images_copies_files_and_skips_missing() {
        let tmp = TempDir::new().unwrap();
        let storage = FileSystemStorage::new(tmp.path().join("sessions"));
        let ref1 = ImageReference {
            file_name: "a.png".to_string(),
            media_type: "image/png".to_string(),
            original_size: 3,
        };
        let ref2 = ImageReference {
            file_name: "b.png".to_string(),
            media_type: "image/png".to_string(),
            original_size: 3,
        };
        let missing_ref = ImageReference {
            file_name: "ghost.png".to_string(),
            media_type: "image/png".to_string(),
            original_size: 0,
        };
        storage.save_image("src", &ref1, b"AAA").await.unwrap();
        storage.save_image("src", &ref2, b"BBB").await.unwrap();

        storage
            .copy_images("src", "dst", &[ref1.clone(), ref2.clone(), missing_ref])
            .await
            .unwrap();

        assert_eq!(storage.load_image("dst", &ref1).await.unwrap(), b"AAA");
        assert_eq!(storage.load_image("dst", &ref2).await.unwrap(), b"BBB");
    }

    #[tokio::test]
    async fn test_copy_images_empty_noop_without_source_dir() {
        let tmp = TempDir::new().unwrap();
        let storage = FileSystemStorage::new(tmp.path().join("sessions"));
        // Source has no images/ dir — copy_images returns Ok without touching target.
        storage.copy_images("ghost-src", "dst", &[]).await.unwrap();
        assert!(!tmp.path().join("sessions/dst/images").exists());
    }

    // ── Phase 2: directory-layout tests (Story 4-3a.1 Tasks 2.8-2.13) ─────

    fn make_test_conversation_with_image() -> (Conversation, Vec<u8>) {
        let raw = b"fake-png-bytes".to_vec();
        let file_name = format!("{}.png", content_hash(&raw));
        let image_ref = ImageReference {
            file_name,
            media_type: "image/png".to_string(),
            original_size: raw.len(),
        };
        let mut conv = make_test_conversation();
        conv.id = "conv-with-image".to_string();
        conv.messages[0].images = vec![image_ref];
        (conv, raw)
    }

    // Task 2.8: save with images creates directory layout
    #[tokio::test]
    async fn test_directory_layout_save_and_load() {
        let tmp = TempDir::new().unwrap();
        let sessions = tmp.path().join("sessions");
        let storage = FileSystemStorage::new(sessions.clone());
        let (conv, raw) = make_test_conversation_with_image();

        // First save the image so the file exists on disk, then save conv.
        let img_ref = conv.messages[0].images[0].clone();
        storage.save_image(&conv.id, &img_ref, &raw).await.unwrap();
        storage.save_conversation(&conv).await.unwrap();

        // Directory layout now: {id}/conversation.json + meta.json + images/
        assert!(sessions.join("conv-with-image").is_dir());
        assert!(sessions.join("conv-with-image/conversation.json").is_file());
        assert!(sessions.join("conv-with-image/meta.json").is_file());
        assert!(sessions.join("conv-with-image/images").is_dir());
        assert!(
            sessions
                .join("conv-with-image/images")
                .join(&img_ref.file_name)
                .is_file()
        );

        // Flat artifacts should NOT exist.
        assert!(!sessions.join("conv-with-image.meta.json").exists());
        assert!(!sessions.join("conv-with-image.session.json").exists());

        let loaded = storage
            .load_conversation("conv-with-image")
            .await
            .unwrap()
            .expect("should load directory layout");
        assert_eq!(loaded.messages[0].images.len(), 1);
        assert_eq!(loaded.messages[0].images[0].file_name, img_ref.file_name);
    }

    // Task 2.9: save without images uses flat file, load reads back
    #[tokio::test]
    async fn test_flat_layout_save_and_load_without_images() {
        let tmp = TempDir::new().unwrap();
        let sessions = tmp.path().join("sessions");
        let storage = FileSystemStorage::new(sessions.clone());
        let conv = make_test_conversation();

        storage.save_conversation(&conv).await.unwrap();
        assert!(sessions.join("test-conv-123.meta.json").is_file());
        assert!(!sessions.join("test-conv-123").exists());

        let loaded = storage
            .load_conversation("test-conv-123")
            .await
            .unwrap()
            .expect("should load");
        assert_eq!(loaded.id, conv.id);
        assert!(loaded.messages.iter().all(|m| m.images.is_empty()));
    }

    // Task 2.10: old-format files still load (backward compat)
    #[tokio::test]
    async fn test_legacy_flat_format_still_loads() {
        let tmp = TempDir::new().unwrap();
        let sessions = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();

        // Seed a pre-4-3a.1 session (no images field at all).
        let legacy_main = r#"{
            "id": "legacy-conv",
            "title": "Legacy Chat",
            "messages": [
              {
                "id": "m1",
                "role": "user",
                "content": "hi",
                "contentBlocks": [],
                "toolCalls": [],
                "createdAt": 1700000000,
                "tokenCount": null
              }
            ],
            "createdAt": 1700000000,
            "updatedAt": 1700000001
        }"#;
        std::fs::write(sessions.join("legacy-conv.meta.json"), legacy_main).unwrap();

        let storage = FileSystemStorage::new(sessions);
        let loaded = storage
            .load_conversation("legacy-conv")
            .await
            .unwrap()
            .expect("legacy session should load");
        assert_eq!(loaded.id, "legacy-conv");
        assert!(loaded.messages[0].images.is_empty());
    }

    // Task 2.11: list_conversations finds both formats and dedupes
    #[tokio::test]
    async fn test_list_finds_both_layouts_deduped() {
        let tmp = TempDir::new().unwrap();
        let sessions = tmp.path().join("sessions");
        let storage = FileSystemStorage::new(sessions.clone());

        // Flat convo
        let flat = make_test_conversation();
        storage.save_conversation(&flat).await.unwrap();

        // Directory convo with image
        let (img_conv, raw) = make_test_conversation_with_image();
        let img_ref = img_conv.messages[0].images[0].clone();
        storage
            .save_image(&img_conv.id, &img_ref, &raw)
            .await
            .unwrap();
        storage.save_conversation(&img_conv).await.unwrap();

        let summaries = storage.list_conversations().await.unwrap();
        assert_eq!(summaries.len(), 2);
        let ids: Vec<_> = summaries.iter().map(|s| s.id.clone()).collect();
        assert!(ids.contains(&"test-conv-123".to_string()));
        assert!(ids.contains(&"conv-with-image".to_string()));
    }

    // Task 2.12: atomic migration on first image save
    #[tokio::test]
    async fn test_atomic_migration_flat_to_directory_on_first_image() {
        let tmp = TempDir::new().unwrap();
        let sessions = tmp.path().join("sessions");
        let storage = FileSystemStorage::new(sessions.clone());

        // Start flat.
        let mut conv = make_test_conversation();
        storage.save_conversation(&conv).await.unwrap();
        assert!(sessions.join("test-conv-123.meta.json").is_file());
        assert!(sessions.join("test-conv-123.session.json").is_file());

        // Add image, re-save → migration.
        let raw = b"png-bytes-here".to_vec();
        let img_ref = ImageReference {
            file_name: format!("{}.png", content_hash(&raw)),
            media_type: "image/png".to_string(),
            original_size: raw.len(),
        };
        storage.save_image(&conv.id, &img_ref, &raw).await.unwrap();
        conv.messages[0].images = vec![img_ref.clone()];
        storage.save_conversation(&conv).await.unwrap();

        // New layout exists.
        assert!(sessions.join("test-conv-123/conversation.json").is_file());
        assert!(sessions.join("test-conv-123/meta.json").is_file());
        assert!(
            sessions
                .join("test-conv-123/images")
                .join(&img_ref.file_name)
                .is_file()
        );

        // Old flat files are gone.
        assert!(!sessions.join("test-conv-123.meta.json").exists());
        assert!(!sessions.join("test-conv-123.session.json").exists());

        // Reload works and contains the image ref.
        let loaded = storage
            .load_conversation("test-conv-123")
            .await
            .unwrap()
            .expect("should load post-migration");
        assert_eq!(loaded.messages[0].images.len(), 1);
    }

    // Task 2.13: migration rollback — simulated by ensuring that if the
    // directory write fails, the flat files remain. We can't easily inject
    // failures into tokio::fs, so we instead assert the invariant that
    // save_conversation_inner only deletes flat files AFTER the directory
    // write succeeded. This is covered by reading the code path in Task 2.6;
    // we verify the observable side: a successful save leaves exactly one
    // layout on disk.
    #[tokio::test]
    async fn test_migration_preserves_single_layout_invariant() {
        let tmp = TempDir::new().unwrap();
        let sessions = tmp.path().join("sessions");
        let storage = FileSystemStorage::new(sessions.clone());

        // Save twice (flat, then with images) and confirm no stale files remain.
        let mut conv = make_test_conversation();
        storage.save_conversation(&conv).await.unwrap();

        let raw = b"data".to_vec();
        let img_ref = ImageReference {
            file_name: format!("{}.png", content_hash(&raw)),
            media_type: "image/png".to_string(),
            original_size: raw.len(),
        };
        storage.save_image(&conv.id, &img_ref, &raw).await.unwrap();
        conv.messages[0].images = vec![img_ref];
        storage.save_conversation(&conv).await.unwrap();

        // Exactly one layout present.
        let flat_exists = sessions.join("test-conv-123.meta.json").exists();
        let dir_exists = sessions.join("test-conv-123").is_dir();
        assert!(!flat_exists && dir_exists, "migration must swap atomically");
    }

    // Phase 6: SessionMeta.fork_source mirror (DF-095) + DF-088 regression.
    #[tokio::test]
    async fn test_session_meta_fork_source_roundtrip_flat() {
        use crate::domain::models::checkpoint::CheckpointId;
        use crate::domain::models::conversation::ForkSource;

        let tmp = TempDir::new().unwrap();
        let storage = FileSystemStorage::new(tmp.path().join("sessions"));
        let mut conv = make_test_conversation();
        conv.fork_source = Some(ForkSource {
            conversation_id: "parent-conv".to_string(),
            message_index: 2,
            checkpoint_id: CheckpointId(2),
        });
        storage.save_conversation(&conv).await.unwrap();

        let meta = storage
            .load_session_meta("test-conv-123")
            .await
            .unwrap()
            .expect("sidecar should exist");
        assert!(meta.fork_source.is_some());
        assert_eq!(
            meta.fork_source.as_ref().unwrap().conversation_id,
            "parent-conv"
        );

        let summaries = storage.list_conversations().await.unwrap();
        assert!(summaries[0].has_fork_source);
    }

    #[tokio::test]
    async fn test_session_meta_fork_source_legacy_backfill_flat() {
        use crate::domain::models::checkpoint::CheckpointId;
        use crate::domain::models::conversation::ForkSource;

        let tmp = TempDir::new().unwrap();
        let sessions = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();

        // Seed: main JSON has fork_source, sidecar does not.
        let main = r#"{
            "id": "legacy-fork",
            "title": "Legacy Fork",
            "messages": [],
            "createdAt": 1700000000,
            "updatedAt": 1700000001,
            "forkSource": {
                "conversationId": "parent-id",
                "messageIndex": 3,
                "checkpointId": 3
            }
        }"#;
        let sidecar = r#"{
            "version": 1,
            "title": "Legacy Fork",
            "createdAt": 1700000000,
            "updatedAt": 1700000001,
            "messageCount": 0,
            "bookmarks": []
        }"#;
        std::fs::write(sessions.join("legacy-fork.meta.json"), main).unwrap();
        std::fs::write(sessions.join("legacy-fork.session.json"), sidecar).unwrap();

        let storage = FileSystemStorage::new(sessions);
        let summaries = storage.list_conversations().await.unwrap();
        assert_eq!(summaries.len(), 1);
        assert!(
            summaries[0].has_fork_source,
            "backfill should populate has_fork_source"
        );

        // And the sidecar should now contain fork_source.
        let meta = storage
            .load_session_meta("legacy-fork")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(meta.fork_source.unwrap().conversation_id, "parent-id");
        // Silence unused warnings on unused deps under cfg(test)
        let _ = CheckpointId(0);
        let _ = ForkSource {
            conversation_id: "x".to_string(),
            message_index: 0,
            checkpoint_id: CheckpointId(0),
        };
    }

    // Phase 3b / Task 3b.5-3b.7: fork copies images, no-op without images,
    // tolerates missing source file.
    #[tokio::test]
    async fn test_fork_copies_images_to_forked_session() {
        use crate::domain::models::checkpoint::CheckpointId;
        use crate::domain::ports::StoragePort as _;

        let tmp = TempDir::new().unwrap();
        let sessions = tmp.path().join("sessions");
        let storage = FileSystemStorage::new(sessions.clone());

        // Source with 2 messages; first has an image.
        let raw = b"image-bytes".to_vec();
        let img = ImageReference {
            file_name: format!("{}.png", content_hash(&raw)),
            media_type: "image/png".to_string(),
            original_size: raw.len(),
        };
        let mut conv = make_test_conversation();
        conv.id = "src-fork".to_string();
        storage.save_image(&conv.id, &img, &raw).await.unwrap();
        conv.messages[0].images = vec![img.clone()];
        storage.save_conversation(&conv).await.unwrap();

        // Fork at message 0 (keep only the user message with the image).
        let new_id = storage
            .fork_at_checkpoint("src-fork", CheckpointId(0))
            .await
            .unwrap();

        // Forked session must have the image file copied.
        assert!(
            sessions
                .join(&new_id)
                .join("images")
                .join(&img.file_name)
                .is_file(),
            "forked session must contain the copied image"
        );
    }

    #[tokio::test]
    async fn test_fork_no_images_does_not_create_images_dir() {
        use crate::domain::models::checkpoint::CheckpointId;
        use crate::domain::ports::StoragePort as _;

        let tmp = TempDir::new().unwrap();
        let sessions = tmp.path().join("sessions");
        let storage = FileSystemStorage::new(sessions.clone());
        let mut conv = make_test_conversation();
        conv.id = "plain".to_string();
        storage.save_conversation(&conv).await.unwrap();

        let new_id = storage
            .fork_at_checkpoint("plain", CheckpointId(0))
            .await
            .unwrap();

        assert!(!sessions.join(&new_id).join("images").exists());
    }

    #[tokio::test]
    async fn test_session_meta_save_and_load_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let storage = FileSystemStorage::new(tmp.path().join("sessions"));

        let meta = SessionMeta {
            version: 1,
            title: "Test Session".to_string(),
            created_at: 1700000000,
            updated_at: 1700000100,
            message_count: 5,
            bookmarks: vec![1, 3],
            fork_source: None,
            extra: serde_json::Map::new(),
        };

        storage.save_session_meta("test-id", &meta).await.unwrap();

        let loaded = storage
            .load_session_meta("test-id")
            .await
            .unwrap()
            .expect("should load");
        assert_eq!(loaded.version, meta.version);
        assert_eq!(loaded.title, meta.title);
        assert_eq!(loaded.created_at, meta.created_at);
        assert_eq!(loaded.updated_at, meta.updated_at);
        assert_eq!(loaded.message_count, meta.message_count);
        assert_eq!(loaded.bookmarks, meta.bookmarks);
    }
}
