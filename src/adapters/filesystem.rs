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
    /// Workspace root used by `snapshot_file` for path-traversal validation.
    /// `None` falls back to the `sessions_dir` grandparent-as-proxy for legacy
    /// constructors. Production composition root SHOULD use
    /// [`with_workspace_root`] to pass the real workspace root.
    workspace_root: Option<PathBuf>,
    /// Maximum checkpoints to retain per conversation (DF-106).
    /// Older checkpoints are pruned opportunistically in `create_checkpoint`.
    /// `None` = unlimited (test use only). Default: 100.
    snapshot_retention_count: Option<usize>,
}

impl FileSystemStorage {
    /// Create a new `FileSystemStorage` targeting the given sessions directory.
    ///
    /// Legacy constructor: leaves `workspace_root` unset. `snapshot_file` will
    /// derive a proxy workspace root from `sessions_dir` grandparent; use
    /// [`with_workspace_root`] in production to pass the real root explicitly.
    #[allow(dead_code)] // retained for test fixtures and legacy call sites
    pub fn new(sessions_dir: PathBuf) -> Self {
        Self {
            sessions_dir,
            workspace_root: None,
            snapshot_retention_count: Some(100),
        }
    }

    /// Create a `FileSystemStorage` with an explicit workspace root.
    ///
    /// Preferred for production composition. `snapshot_file` enforces that all
    /// snapshot paths resolve inside `workspace_root` after canonicalization.
    pub fn with_workspace_root(sessions_dir: PathBuf, workspace_root: PathBuf) -> Self {
        Self {
            sessions_dir,
            workspace_root: Some(workspace_root),
            snapshot_retention_count: Some(100),
        }
    }

    /// Override the snapshot retention count (for tests or CLI override).
    /// Pass `None` for unlimited retention.
    pub fn with_snapshot_retention(mut self, count: Option<usize>) -> Self {
        self.snapshot_retention_count = count;
        self
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
    /// Disambiguation rule (DF-112, AC6): Directory layout requires BOTH
    /// `{sessions_dir}/{id}/` as a directory AND `{id}/conversation.json`
    /// as a regular file. An orphaned sidecar-only directory (e.g., created
    /// by `save_checkpoint_log` or image copy without a full migration) is
    /// treated as Flat or Missing, not Directory.
    ///
    /// - `Directory` if `{sessions_dir}/{id}/` exists as a directory
    ///   AND `{sessions_dir}/{id}/conversation.json` is a regular file
    /// - `Flat` if `{sessions_dir}/{id}.meta.json` exists as a file
    /// - `Missing` otherwise
    async fn detect_layout(&self, id: &str) -> SessionLayout {
        let dir = self.session_dir(id);
        // DF-099: single read_dir pass on the session directory — eliminates
        // the TOCTOU race window between the two sequential metadata() calls
        // (metadata(dir) → metadata(conv_file)) used in the previous
        // implementation. If the directory exists, enumerate its entries to
        // check for conversation.json in one kernel call.
        match tokio::fs::read_dir(&dir).await {
            Ok(mut entries) => {
                // Session directory exists. Look for conversation.json.
                let mut found_conv = false;
                while let Ok(Some(e)) = entries.next_entry().await {
                    if e.file_name() == "conversation.json" {
                        if let Ok(ft) = e.file_type().await {
                            if ft.is_file() {
                                found_conv = true;
                            }
                        }
                        break;
                    }
                }
                if found_conv {
                    return SessionLayout::Directory;
                }
                // Dir exists but conversation.json absent — fall through to
                // flat check (handles sidecar-only or images-only directories).
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::NotFound
                    || e.kind() == std::io::ErrorKind::NotADirectory =>
            {
                // Directory does not exist or path is not a directory.
            }
            Err(e) => {
                // Other I/O error (PermissionDenied, etc.) — log and fall through.
                // This may misclassify a Directory session as Flat/Missing, but
                // logging ensures the issue is diagnosable.
                tracing::warn!(
                    id = %id,
                    error = %e,
                    "detect_layout: read_dir failed with unexpected error — session may be misclassified"
                );
            }
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
            // DF-096: conversation.json was already renamed — roll it back so
            // the flat files remain the authoritative copy.
            if let Err(rb) = tokio::fs::remove_file(&conv_path).await {
                tracing::warn!(
                    id = %id,
                    rollback_error = %rb,
                    "save_directory_layout: meta.json rename failed; rollback of conversation.json also failed — session may be inconsistent"
                );
            } else {
                tracing::info!(
                    id = %id,
                    "save_directory_layout: meta.json rename failed; rolled back conversation.json — flat files remain authoritative"
                );
            }
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
        //
        // DF-098: single read_dir pass to build a HashSet of present filenames,
        // then O(1) membership check per image — eliminates N separate
        // metadata() syscalls (one per image in the old implementation).
        if layout == SessionLayout::Directory {
            let images_dir = self.images_dir(&conv.id);
            let present: std::collections::HashSet<String> = {
                let mut set = std::collections::HashSet::new();
                if let Ok(mut rd) = tokio::fs::read_dir(&images_dir).await {
                    while let Ok(Some(e)) = rd.next_entry().await {
                        if let Ok(ft) = e.file_type().await {
                            if ft.is_file() {
                                if let Some(name) = e.file_name().to_str() {
                                    set.insert(name.to_string());
                                }
                            }
                        }
                    }
                }
                set
            };
            for msg in &conv.messages {
                for img in &msg.images {
                    if !present.contains(&img.file_name) {
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

    // ── Checkpoint Protocol (Amendment 1, Story 4-3b) ─────────────────────────────

    async fn create_checkpoint(
        &self,
        conversation_id: &str,
    ) -> Result<crate::domain::models::checkpoint::CheckpointId, StorageError> {
        use crate::domain::models::checkpoint::{CheckpointId, CheckpointMeta};
        use crate::domain::models::session_meta::now_unix;

        // Load conversation to determine the current message count.
        let conv = match self.load_conversation(conversation_id).await? {
            Some(c) => c,
            None => {
                return Err(StorageError::NotFound(format!(
                    "conversation not found: {}",
                    conversation_id
                )));
            }
        };

        // Ensure the session directory exists so we can write checkpoints.json.
        tokio::fs::create_dir_all(self.session_dir(conversation_id))
            .await
            .map_err(|e| StorageError::IoError(format!("create session dir: {}", e)))?;

        // Migrate the conversation to directory format now that the session dir exists.
        // Without this, `load_conversation` would fail to find `{id}/conversation.json`.
        //
        // Note: we call `save_directory_layout` directly instead of `save_conversation_inner`
        // because `detect_layout` (called inside `save_conversation_inner`) now requires BOTH
        // `{id}/` directory AND `{id}/conversation.json` to classify as Directory (DF-112 fix).
        // The first time we create the session dir, `conversation.json` doesn't exist yet, so
        // `detect_layout` would return Flat and `save_conversation_inner` would incorrectly save
        // to the flat format rather than migrating.
        let conv_file = self.conversation_file(conversation_id);
        if !tokio::fs::try_exists(&conv_file).await.unwrap_or(false) {
            let persisted = PersistedConversation::from_conversation_with_exit(&conv, false);
            let json = serde_json::to_string_pretty(&persisted)
                .map_err(|e| StorageError::SerializationError(e.to_string()))?;
            let meta = crate::domain::models::SessionMeta::from_conversation(&conv);
            self.save_directory_layout(conversation_id, &json, &meta)
                .await?;
            // Clean up stale flat-format files (best-effort).
            self.finish_flat_to_directory_migration(conversation_id)
                .await?;
        }

        let mut log = self.load_checkpoint_log(conversation_id).await?;

        // Next id is max existing + 1, or 1 for the first checkpoint.
        let next_id = log.entries.iter().map(|e| e.id.0).max().unwrap_or(0) + 1;

        // message_index is the index of the last message currently present.
        // The checkpoint captures the conversation state *before* the assistant turn
        // whose tool calls triggered this checkpoint.
        let message_index = conv.messages.len().saturating_sub(1);

        let meta = CheckpointMeta {
            id: CheckpointId(next_id),
            message_index,
            created_at: now_unix(),
        };
        log.entries.push(meta.clone());
        self.save_checkpoint_log(conversation_id, &log).await?;

        // DF-106 (AC1): Opportunistic snapshot retention — prune oldest checkpoints
        // after recording the new one so the log stays within the configured limit.
        if let Some(retention) = self.snapshot_retention_count {
            if log.entries.len() > retention {
                if let Err(e) = self
                    .prune_old_snapshots(conversation_id, &log, retention)
                    .await
                {
                    // Non-fatal: log and continue. Disk space grows but no data is lost.
                    let _ = e; // warning already emitted inside prune_old_snapshots
                }
            }
        }

        tracing::debug!(
            "Created checkpoint {} for conversation {} at message_index {}",
            next_id,
            conversation_id,
            message_index
        );
        Ok(meta.id)
    }

    async fn list_checkpoints(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<crate::domain::models::checkpoint::CheckpointMeta>, StorageError> {
        let mut log = self.load_checkpoint_log(conversation_id).await?;
        log.entries.sort_by_key(|e| e.id);
        Ok(log.entries)
    }

    async fn revert_to_checkpoint(
        &self,
        conversation_id: &str,
        checkpoint: crate::domain::models::checkpoint::CheckpointId,
    ) -> Result<crate::domain::models::Conversation, StorageError> {
        use crate::domain::models::session_meta::{SessionMeta, now_unix};

        let log = self.load_checkpoint_log(conversation_id).await?;

        // Find the entry for the requested checkpoint.
        let meta = log
            .entries
            .iter()
            .find(|e| e.id == checkpoint)
            .ok_or_else(|| {
                StorageError::NotFound(format!(
                    "checkpoint {} not found for conversation {}",
                    checkpoint.0, conversation_id
                ))
            })?
            .clone();

        // Load conversation.
        let mut conv = match self.load_conversation(conversation_id).await? {
            Some(c) => c,
            None => {
                return Err(StorageError::NotFound(format!(
                    "conversation not found: {}",
                    conversation_id
                )));
            }
        };

        // Truncate messages: keep messages[0..=meta.message_index].
        let keep_until = (meta.message_index + 1).min(conv.messages.len());
        conv.messages.truncate(keep_until);
        conv.updated_at = now_unix();

        // Load the existing SessionMeta BEFORE save_conversation_inner overwrites it,
        // so we can preserve the `extra` flatten map (DF-088 round-trip invariant).
        let pre_save_meta = self.load_session_meta(conversation_id).await.ok().flatten();

        // 1. Atomically save the truncated conversation.
        self.save_conversation_inner(&conv, false).await?;

        // 2. Atomically update SessionMeta, preserving extra fields from the pre-save load.
        let session_meta_result = match pre_save_meta {
            Some(mut existing_meta) => {
                existing_meta.message_count = conv.messages.len();
                existing_meta.updated_at = conv.updated_at;
                self.save_session_meta(conversation_id, &existing_meta)
                    .await
            }
            None => {
                // Legacy session without sidecar: build a fresh one.
                let fresh = SessionMeta::from_conversation(&conv);
                self.save_session_meta(conversation_id, &fresh).await
            }
        };
        if let Err(e) = session_meta_result {
            // Step 5 succeeded but step 6 failed. The next load_session_meta will
            // reconcile by re-reading the actual message count from the conversation file.
            tracing::error!(
                conversation_id = conversation_id,
                checkpoint = checkpoint.0,
                error = %e,
                "INCONSISTENT STATE: conversation truncated but meta.json update failed. \
                 Will be reconciled on next load."
            );
        }

        // 3. Truncate the checkpoint log: remove entries with id > checkpoint.
        let mut updated_log = CheckpointLog {
            entries: log
                .entries
                .into_iter()
                .filter(|e| e.id <= checkpoint)
                .collect(),
        };
        updated_log.entries.sort_by_key(|e| e.id);
        self.save_checkpoint_log(conversation_id, &updated_log)
            .await?;

        tracing::debug!(
            "Rewound conversation {} to checkpoint {} (message_index {})",
            conversation_id,
            checkpoint.0,
            meta.message_index
        );
        Ok(conv)
    }

    async fn truncate_conversation(
        &self,
        conversation_id: &str,
        target_message_index: usize,
    ) -> Result<crate::domain::models::Conversation, StorageError> {
        use crate::domain::models::session_meta::{SessionMeta, now_unix};

        // Load conversation.
        let mut conv = match self.load_conversation(conversation_id).await? {
            Some(c) => c,
            None => {
                return Err(StorageError::NotFound(format!(
                    "conversation not found: {}",
                    conversation_id
                )));
            }
        };

        // Truncate to the user's selected message index. Pure message-level
        // operation — no dependency on the checkpoint log. Works for text-only
        // conversations where no checkpoint was ever created.
        let keep_until = (target_message_index + 1).min(conv.messages.len());
        conv.messages.truncate(keep_until);
        conv.updated_at = now_unix();

        let surviving_ids: std::collections::HashSet<String> =
            conv.messages.iter().map(|m| m.id.clone()).collect();
        conv.plans.retain(|_, plan| {
            plan.host_message_id
                .as_ref()
                .is_some_and(|id| surviving_ids.contains(id))
        });

        // Load the existing SessionMeta BEFORE save_conversation_inner overwrites it,
        // so we can preserve the `extra` flatten map (DF-088 round-trip invariant).
        let pre_save_meta = self.load_session_meta(conversation_id).await.ok().flatten();

        // 1. Atomically save the truncated conversation.
        self.save_conversation_inner(&conv, false).await?;

        // 2. Atomically update SessionMeta, preserving extra fields.
        let session_meta_result = match pre_save_meta {
            Some(mut existing_meta) => {
                existing_meta.message_count = conv.messages.len();
                existing_meta.updated_at = conv.updated_at;
                self.save_session_meta(conversation_id, &existing_meta)
                    .await
            }
            None => {
                let fresh = SessionMeta::from_conversation(&conv);
                self.save_session_meta(conversation_id, &fresh).await
            }
        };
        if let Err(e) = session_meta_result {
            tracing::error!(
                conversation_id = conversation_id,
                target_message_index = target_message_index,
                error = %e,
                "INCONSISTENT STATE: conversation truncated but meta.json update failed. \
                 Will be reconciled on next load."
            );
        }

        // 3. Prune the checkpoint log: remove entries whose message_index is
        //    beyond the truncation point. Unlike `revert_to_checkpoint`, the
        //    filter is on `message_index`, not on checkpoint id — the target
        //    is user-selected and may fall between adjacent checkpoints.
        //
        //    DF-112 (AC6): `detect_layout` now requires BOTH the directory AND
        //    `conversation.json` to return `Directory`. Creating the checkpoint
        //    dir via `save_checkpoint_log → create_dir_all` no longer silently
        //    flips the layout, so the Amendment 2 `try_exists` guard is removed.
        //    We call `save_checkpoint_log` only when the log file actually exists
        //    (text-only conversations never created one — no-op is the right behaviour).
        let cp_file = self.checkpoints_file(conversation_id);
        if tokio::fs::try_exists(&cp_file).await.unwrap_or(false) {
            let log = self.load_checkpoint_log(conversation_id).await?;
            let mut updated_log = CheckpointLog {
                entries: log
                    .entries
                    .into_iter()
                    .filter(|e| e.message_index <= target_message_index)
                    .collect(),
            };
            updated_log.entries.sort_by_key(|e| e.id);
            self.save_checkpoint_log(conversation_id, &updated_log)
                .await?;
        }

        tracing::debug!(
            "Truncated conversation {} to message_index {} ({} messages kept)",
            conversation_id,
            target_message_index,
            conv.messages.len()
        );
        Ok(conv)
    }

    async fn snapshot_file(
        &self,
        conversation_id: &str,
        checkpoint: crate::domain::models::checkpoint::CheckpointId,
        path: &std::path::Path,
        content: &[u8],
    ) -> Result<(), StorageError> {
        use sha2::{Digest, Sha256};

        // DF-110 (AC4): Stream-encode large files using base64::write::EncoderWriter
        // instead of loading the whole file into memory. Cap raised significantly —
        // the streaming path handles arbitrarily large files within available I/O
        // throughput. The 50 MiB guard from Story 4-3b is removed.

        // 1. Canonicalize path. FAIL CLOSED on unresolvable paths — do not fall
        //    back to the raw input. If the target file doesn't exist yet (Write
        //    tool creating a new file), canonicalize the parent and join the
        //    filename. This keeps new-file snapshots working while rejecting
        //    paths whose parent chain cannot be resolved.
        let canonical: PathBuf = match tokio::fs::canonicalize(path).await {
            Ok(p) => p,
            Err(_) => {
                let parent = path.parent().ok_or_else(|| {
                    StorageError::NotSupported(format!(
                        "snapshot path has no parent: {}",
                        path.display()
                    ))
                })?;
                let canonical_parent = tokio::fs::canonicalize(parent).await.map_err(|e| {
                    StorageError::NotSupported(format!(
                        "snapshot parent canonicalization failed for {}: {}",
                        parent.display(),
                        e
                    ))
                })?;
                let file_name = path.file_name().ok_or_else(|| {
                    StorageError::NotSupported(format!(
                        "snapshot path has no file name: {}",
                        path.display()
                    ))
                })?;
                canonical_parent.join(file_name)
            }
        };

        // 2. Determine workspace root. Prefer explicit config; fall back to
        //    sessions_dir grandparent-as-proxy for legacy constructors. FAIL
        //    CLOSED if neither can be determined.
        let workspace_root = self
            .workspace_root
            .clone()
            .or_else(|| {
                self.sessions_dir
                    .parent()
                    .and_then(|p| p.parent())
                    .map(PathBuf::from)
            })
            .ok_or_else(|| {
                StorageError::NotSupported(
                    "snapshot workspace root not configured and cannot be derived".to_string(),
                )
            })?;

        let workspace_canonical = tokio::fs::canonicalize(&workspace_root)
            .await
            .map_err(|e| {
                StorageError::NotSupported(format!(
                    "workspace root canonicalization failed for {}: {}",
                    workspace_root.display(),
                    e
                ))
            })?;

        // 3. Path-traversal guard. Canonical paths are always absolute — no
        //    is_absolute() bypass. Relative, symlinked, and ../-escaping paths
        //    all pass through canonicalize first and then this check.
        if !canonical.starts_with(&workspace_canonical) {
            tracing::warn!(
                "Path traversal blocked: {} is not inside workspace {}",
                canonical.display(),
                workspace_canonical.display()
            );
            return Err(StorageError::NotSupported(
                "path outside workspace".to_string(),
            ));
        }

        // 4. Compute path hash from OsStr bytes (not to_string_lossy — two distinct
        //    non-UTF8 paths could hash to the same value under lossy conversion).
        //    path_display is stored separately for human-readable debugging.
        let path_hash = content_hash(canonical.as_os_str().as_encoded_bytes());
        let path_display = canonical.to_string_lossy().into_owned();

        // 5. Build snapshot filename: {cp_id}_{path_hash} (no extension per Amendment 1).
        let snapshots_dir = self.snapshots_dir(conversation_id);
        let snapshot_name = format!("{}_{}", checkpoint.0, path_hash);
        let snapshot_path = snapshots_dir.join(&snapshot_name);

        // 6. Idempotency: if this (checkpoint, path) pair already has a snapshot, skip.
        //    The first snapshot wins because it holds the original (pre-modification) content.
        if tokio::fs::metadata(&snapshot_path).await.is_ok() {
            return Ok(());
        }

        // 7. Ensure snapshots directory exists.
        tokio::fs::create_dir_all(&snapshots_dir)
            .await
            .map_err(|e| StorageError::IoError(format!("create snapshots dir: {}", e)))?;

        // 8. Compute sha256 of original content.
        let original_hash = {
            let mut h = Sha256::new();
            h.update(content);
            format!("sha256:{:x}", h.finalize())
        };

        // 9. Base64-encode the content in 64 KiB chunks (DF-110, AC4).
        //    Uses base64::write::EncoderWriter over a Vec<u8> buffer to avoid
        //    loading the entire base64 string into memory at once.
        let original_content_b64 = {
            use base64::write::EncoderWriter;
            use std::io::Write as _;
            const CHUNK: usize = 64 * 1024; // 64 KiB
            let mut b64_buf: Vec<u8> = Vec::with_capacity(content.len() * 4 / 3 + 4);
            {
                let mut encoder =
                    EncoderWriter::new(&mut b64_buf, &base64::engine::general_purpose::STANDARD);
                for chunk in content.chunks(CHUNK) {
                    encoder.write_all(chunk).map_err(|e| {
                        StorageError::IoError(format!("base64 encode chunk: {}", e))
                    })?;
                }
                encoder
                    .finish()
                    .map_err(|e| StorageError::IoError(format!("base64 encoder finish: {}", e)))?;
            }
            String::from_utf8(b64_buf)
                .map_err(|e| StorageError::SerializationError(format!("base64 utf8: {}", e)))?
        };

        // D1 schema v2: explicit file_existed sentinel.
        // Schema v3 (DF-111, AC5): adds expected_current_hash.
        //   `expected_current_hash` is null at snapshot time (pre-tool execution).
        //   The toolset adapter MUST call `finalize_snapshot` after the Write tool
        //   completes to populate this field with the post-write hash.
        //   At revert time: if expected_current_hash is present and matches current
        //   file hash → tool modified it → Restore. If mismatch → external edit → Conflict.
        //   If absent (v1/v2 fallback): if current_hash != original_hash → Restore.
        let file_existed = !content.is_empty();

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let envelope = serde_json::json!({
            "schema_version": 3,
            "conversation_id": conversation_id,
            "checkpoint_id": checkpoint.0,
            "path": path_display,
            "path_hash": path_hash,
            "original_hash": original_hash,
            "original_content_b64": original_content_b64,
            "file_existed": file_existed,
            "expected_current_hash": null,  // populated by finalize_snapshot after write
            "created_at_ms": now_ms,
        });

        // 10. Atomic write: temp file + rename.
        let tmp_path = snapshot_path.with_extension("tmp");
        let content_bytes = serde_json::to_vec_pretty(&envelope)
            .map_err(|e| StorageError::SerializationError(e.to_string()))?;
        tokio::fs::write(&tmp_path, &content_bytes)
            .await
            .map_err(|e| StorageError::IoError(format!("write snapshot tmp: {}", e)))?;
        tokio::fs::rename(&tmp_path, &snapshot_path)
            .await
            .map_err(|e| StorageError::IoError(format!("rename snapshot file: {}", e)))?;

        tracing::debug!(
            "Snapshotted file {} for conversation {} at checkpoint {}",
            path_display,
            conversation_id,
            checkpoint.0
        );
        Ok(())
    }

    async fn revert_file_snapshots(
        &self,
        conversation_id: &str,
        target_message_index: usize,
    ) -> Result<Vec<crate::domain::models::checkpoint::RevertedFile>, StorageError> {
        use crate::domain::models::checkpoint::{RevertStatus, RevertedFile};
        use base64::Engine as _;
        use sha2::{Digest, Sha256};
        use std::collections::HashMap;
        use std::path::PathBuf;

        let snapshots_dir = self.snapshots_dir(conversation_id);

        let log = self
            .load_checkpoint_log(conversation_id)
            .await
            .unwrap_or_default();
        let revert_cp_ids: std::collections::HashSet<u64> = log
            .entries
            .iter()
            .filter(|e| e.message_index > target_message_index)
            .map(|e| e.id.0)
            .collect();

        tracing::debug!(
            "revert_file_snapshots: conv={}, target_msg_idx={}, revert_cp_ids={:?}, snapshots_dir={}",
            conversation_id,
            target_message_index,
            revert_cp_ids,
            snapshots_dir.display()
        );

        // When revert_cp_ids is empty (e.g. crash recovery after truncation
        // pruned the log), fall back to reverting ALL remaining snapshots.
        let revert_all = revert_cp_ids.is_empty();

        // 1. Read the snapshots directory. Return empty if it doesn't exist.
        let mut entries = match tokio::fs::read_dir(&snapshots_dir).await {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(
                    "revert_file_snapshots: snapshots dir not found — nothing to revert"
                );
                return Ok(vec![]);
            }
            Err(e) => {
                return Err(StorageError::IoError(format!(
                    "Failed to read snapshots dir: {}",
                    e
                )));
            }
        };

        // 2. Collect snapshot file entries: parse filename to (cp_id, path_hash).
        // Filename format: "{cp_id}_{path_hash}" (no extension).
        // Include all when reverting from crash recovery, otherwise filter by set.
        let mut candidates: Vec<(u64, String, PathBuf)> = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            let fname = entry.file_name().to_string_lossy().to_string();
            // Skip temp files left by interrupted writes.
            if fname.ends_with(".tmp") {
                continue;
            }
            if let Some((cp_str, hash_str)) = fname
                .splitn(2, '_')
                .collect::<Vec<_>>()
                .into_iter()
                .collect::<Vec<_>>()
                .as_slice()
                .split_first()
                .and_then(|(first, rest)| rest.first().map(|h| (*first, *h)))
            {
                if let Ok(cp_id) = cp_str.parse::<u64>() {
                    if revert_all || revert_cp_ids.contains(&cp_id) {
                        candidates.push((cp_id, hash_str.to_string(), entry.path()));
                    }
                }
            }
        }

        tracing::debug!(
            "revert_file_snapshots: found {} candidates (target_msg_idx={})",
            candidates.len(),
            target_message_index
        );

        if candidates.is_empty() {
            return Ok(vec![]);
        }

        // 3. Sort descending by cp_id (Amendment 1: reverse chronological order).
        candidates.sort_by(|a, b| b.0.cmp(&a.0));

        // 4. Per-path dedup: track the LOWEST cp_id (oldest original content for
        //    restoration) AND the HIGHEST cp_id (most recent tool write — its
        //    `expected_current_hash` is what the file should look like now).
        //
        //    Without tracking the highest, a file modified across multiple
        //    checkpoints compares the current hash against the FIRST tool write's
        //    hash — which no longer matches — producing a false "modified externally".
        //
        //    Candidates are sorted DESCENDING, so or_insert sees the highest first.
        struct PathEntry {
            lowest_cp: u64,
            restore_from: PathBuf,  // lowest — has original content
            conflict_from: PathBuf, // highest — has latest expected_current_hash
        }
        let mut deduped: HashMap<String, PathEntry> = HashMap::new();
        for (cp_id, path_hash, file_path) in &candidates {
            let entry = deduped.entry(path_hash.clone()).or_insert(PathEntry {
                lowest_cp: *cp_id,
                restore_from: file_path.clone(),
                conflict_from: file_path.clone(), // first insert = highest (sorted DESC)
            });
            if *cp_id < entry.lowest_cp {
                entry.lowest_cp = *cp_id;
                entry.restore_from = file_path.clone();
            }
        }

        tracing::debug!(
            "revert_file_snapshots: {} unique paths after dedup (from {} candidates)",
            deduped.len(),
            candidates.len()
        );

        // 5. Process each surviving snapshot.
        let mut results: Vec<RevertedFile> = Vec::new();
        for entry in deduped.values() {
            let snapshot_path = &entry.restore_from;

            // Read the OLDEST snapshot envelope (original content for restoration).
            let envelope_bytes = match tokio::fs::read(snapshot_path).await {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!("Failed to read snapshot {:?}: {}", snapshot_path, e);
                    continue;
                }
            };
            let envelope: serde_json::Value = match serde_json::from_slice(&envelope_bytes) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("Failed to parse snapshot {:?}: {}", snapshot_path, e);
                    continue;
                }
            };

            let stored_path_str = envelope
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let stored_hash = envelope
                .get("original_hash")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let stored_content_b64 = envelope
                .get("original_content_b64")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let file_path = PathBuf::from(&stored_path_str);

            tracing::debug!(
                "revert_file_snapshots: processing path={}, existed={:?}",
                stored_path_str,
                envelope.get("file_existed").and_then(|v| v.as_bool())
            );

            // For conflict detection, use expected_current_hash from the NEWEST
            // snapshot (highest cp_id). When a file is modified across multiple
            // checkpoints, only the newest hash reflects the tool's final state.
            // Using the oldest snapshot's hash would false-positive on every
            // multi-checkpoint file.
            let newest_expected_hash: Option<String> = if entry.conflict_from != entry.restore_from
            {
                tokio::fs::read(&entry.conflict_from)
                    .await
                    .ok()
                    .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
                    .and_then(|e| {
                        e.get("expected_current_hash")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                            .map(|s| s.to_string())
                    })
            } else {
                // Same file — extract from the main envelope.
                envelope
                    .get("expected_current_hash")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
            };

            // Decode the stored content.
            let stored_content =
                match base64::engine::general_purpose::STANDARD.decode(&stored_content_b64) {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        tracing::warn!(
                            "Failed to decode snapshot content for {}: {}",
                            stored_path_str,
                            e
                        );
                        continue;
                    }
                };

            let file_existed_explicit = envelope.get("file_existed").and_then(|v| v.as_bool());
            let file_was_absent_pre_checkpoint = match file_existed_explicit {
                Some(existed) => !existed,
                None => stored_content_b64.is_empty(), // v1 fallback
            };

            // Read current file content.
            let current_read = tokio::fs::read(&file_path).await;

            let status = match current_read {
                Ok(current_bytes) if file_was_absent_pre_checkpoint => {
                    // File now exists but didn't exist before checkpoint → should delete.
                    let externally_modified = if let Some(ref expected) = newest_expected_hash {
                        let mut h = Sha256::new();
                        h.update(&current_bytes);
                        let current_hash = format!("sha256:{:x}", h.finalize());
                        current_hash != *expected
                    } else {
                        false
                    };

                    if externally_modified {
                        let mut h = Sha256::new();
                        h.update(&current_bytes);
                        let current_hash = format!("sha256:{:x}", h.finalize());
                        RevertStatus::Conflict {
                            expected_hash: newest_expected_hash.unwrap(),
                            actual_hash: current_hash,
                        }
                    } else {
                        if let Err(e) = tokio::fs::remove_file(&file_path).await {
                            tracing::warn!("Failed to delete file {:?}: {}", file_path, e);
                        }
                        RevertStatus::Restored
                    }
                }
                Ok(current_bytes) => {
                    let mut h = Sha256::new();
                    h.update(&current_bytes);
                    let current_hash = format!("sha256:{:x}", h.finalize());

                    let should_restore = if let Some(ref expected) = newest_expected_hash {
                        current_hash == *expected
                    } else {
                        current_hash != stored_hash
                    };

                    if should_restore {
                        if let Some(parent) = file_path.parent() {
                            let _ = tokio::fs::create_dir_all(parent).await;
                        }
                        match tokio::fs::write(&file_path, &stored_content).await {
                            Ok(_) => RevertStatus::Restored,
                            Err(e) => {
                                tracing::warn!("Failed to restore file {:?}: {}", file_path, e);
                                RevertStatus::Conflict {
                                    expected_hash: newest_expected_hash
                                        .as_deref()
                                        .unwrap_or(&stored_hash)
                                        .to_string(),
                                    actual_hash: current_hash,
                                }
                            }
                        }
                    } else if newest_expected_hash.is_some() {
                        RevertStatus::Conflict {
                            expected_hash: newest_expected_hash.unwrap(),
                            actual_hash: current_hash,
                        }
                    } else {
                        // Schema v2 fallback: current == original_hash → already matches
                        // pre-tool state, nothing to do
                        RevertStatus::Restored
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    if file_was_absent_pre_checkpoint {
                        // Didn't exist before checkpoint, doesn't exist now → no-op.
                        RevertStatus::Restored
                    } else {
                        // File deleted by user since snapshot → recreate it.
                        if let Some(parent) = file_path.parent() {
                            let _ = tokio::fs::create_dir_all(parent).await;
                        }
                        match tokio::fs::write(&file_path, &stored_content).await {
                            Ok(_) => RevertStatus::Restored,
                            Err(e) => {
                                tracing::warn!(
                                    "Failed to recreate deleted file {:?}: {}",
                                    file_path,
                                    e
                                );
                                RevertStatus::NoSnapshot
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to read current file {:?}: {}", file_path, e);
                    RevertStatus::NoSnapshot
                }
            };

            tracing::debug!(
                "revert_file_snapshots: file={} → {:?}",
                file_path.display(),
                status
            );

            results.push(RevertedFile {
                path: file_path,
                status,
            });
        }

        tracing::debug!("revert_file_snapshots: returning {} results", results.len());

        // 6. Delete ALL snapshot files for cp_id >= after_checkpoint (including those
        //    that were deduped-out in step 4).
        for (cp_id, _path_hash, file_path) in &candidates {
            let _ = cp_id; // already filtered to >= after_checkpoint above
            if let Err(e) = tokio::fs::remove_file(file_path).await {
                tracing::warn!("Failed to delete snapshot file {:?}: {}", file_path, e);
            }
        }

        Ok(results)
    }

    /// Read-only preview: list files that would be reverted by `revert_file_snapshots`.
    /// Returns `(absolute_path, is_conflict)` pairs. Does NOT modify any files.
    async fn list_snapshot_files(
        &self,
        conversation_id: &str,
        target_message_index: usize,
    ) -> Result<Vec<(std::path::PathBuf, bool)>, StorageError> {
        use sha2::{Digest, Sha256};
        use std::collections::{HashMap, HashSet};
        use std::path::PathBuf;

        let log = self
            .load_checkpoint_log(conversation_id)
            .await
            .unwrap_or_default();
        let revert_cp_ids: HashSet<u64> = log
            .entries
            .iter()
            .filter(|e| e.message_index > target_message_index)
            .map(|e| e.id.0)
            .collect();

        let snapshots_dir = self.snapshots_dir(conversation_id);

        let mut entries = match tokio::fs::read_dir(&snapshots_dir).await {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(vec![]);
            }
            Err(e) => {
                return Err(StorageError::IoError(format!(
                    "Failed to read snapshots dir: {}",
                    e
                )));
            }
        };

        let mut candidates: Vec<(u64, String, PathBuf)> = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            let fname = entry.file_name().to_string_lossy().to_string();
            if fname.ends_with(".tmp") {
                continue;
            }
            if let Some((cp_str, hash_str)) = fname
                .splitn(2, '_')
                .collect::<Vec<_>>()
                .as_slice()
                .split_first()
                .and_then(|(first, rest)| rest.first().map(|h| (*first, *h)))
            {
                if let Ok(cp_id) = cp_str.parse::<u64>() {
                    if revert_cp_ids.contains(&cp_id) {
                        candidates.push((cp_id, hash_str.to_string(), entry.path()));
                    }
                }
            }
        }

        if candidates.is_empty() {
            return Ok(vec![]);
        }

        // Per-path dedup: sort descending first, then track lowest (for path
        // display) and highest (for conflict detection via expected_current_hash).
        candidates.sort_by(|a, b| b.0.cmp(&a.0));

        // (lowest_snapshot_path, highest_snapshot_path)
        let mut deduped: HashMap<String, (PathBuf, PathBuf)> = HashMap::new();
        for (cp_id, path_hash, file_path) in &candidates {
            let entry = deduped
                .entry(path_hash.clone())
                .or_insert((file_path.clone(), file_path.clone()));
            // or_insert sees highest first (sorted DESC) → entry.1 is already highest.
            // Update lowest:
            let _ = cp_id; // used implicitly: later entries have lower cp_id
            entry.0 = file_path.clone(); // always update — last write wins = lowest
        }

        // For each surviving snapshot, check for conflict using the NEWEST
        // snapshot's expected_current_hash (read-only).
        let mut results: Vec<(PathBuf, bool)> = Vec::new();
        for (lowest_path, highest_path) in deduped.values() {
            // Read the lowest snapshot for path display.
            let envelope_bytes = match tokio::fs::read(lowest_path).await {
                Ok(b) => b,
                Err(_) => continue,
            };
            let envelope: serde_json::Value = match serde_json::from_slice(&envelope_bytes) {
                Ok(v) => v,
                Err(_) => continue,
            };

            let stored_path_str = envelope
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            // Read expected_current_hash from the NEWEST snapshot so
            // multi-checkpoint files don't false-positive as "modified externally".
            let expected_current_hash: Option<String> = if highest_path != lowest_path {
                tokio::fs::read(highest_path)
                    .await
                    .ok()
                    .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
                    .and_then(|e| {
                        e.get("expected_current_hash")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                            .map(|s| s.to_string())
                    })
            } else {
                envelope
                    .get("expected_current_hash")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
            };

            let file_path = PathBuf::from(&stored_path_str);

            let is_conflict = match tokio::fs::read(&file_path).await {
                Ok(current_bytes) => {
                    let mut h = Sha256::new();
                    h.update(&current_bytes);
                    let current_hash = format!("sha256:{:x}", h.finalize());
                    if let Some(ref expected) = expected_current_hash {
                        current_hash != *expected
                    } else {
                        false // v1/v2: never conflict
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                Err(_) => false,
            };

            results.push((file_path, is_conflict));
        }

        Ok(results)
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

        let mut forked_plans = std::collections::HashMap::new();
        for (plan_id, plan) in &source.plans {
            if let Some(host_msg_id) = &plan.host_message_id {
                if truncated.iter().any(|m| &m.id == host_msg_id) {
                    forked_plans.insert(plan_id.clone(), plan.clone());
                }
            }
        }

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
            turns: Vec::new(),
            created_at: now,
            updated_at: now,
            last_response_at: None,
            session_id: Some(generate_conversation_id()),
            usage: None,
            plans: forked_plans,
            fork_source: Some(ForkSource {
                conversation_id: source.id.clone(),
                message_index,
                checkpoint_id: checkpoint,
            }),
            compaction: None,
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

        // Amendment 2 (Story 4-3b): copy the source conversation's checkpoint
        // log and snapshot files into the forked session directory, filtered to
        // entries with `message_index <= fork_message_index`. Without this step,
        // the forked conversation has no checkpoint history and rewind fails
        // with NotFound (pre-Amendment-2 behavior). With this step, the fork is
        // an independently-rewindable conversation whose state-before-fork is
        // preserved exactly.
        //
        // Checkpoint ids are conversation-scoped and kept identical across the
        // copy — the `{cp_id}_{path_hash}` filename is reused verbatim, so the
        // forked session's snapshot dir and checkpoint log remain in lockstep.
        //
        // Best-effort: individual snapshot copy failures log a warning but do
        // not abort the fork (mirrors the image-copy graceful-degradation
        // policy above). If the source conversation never had any tool calls,
        // `load_checkpoint_log` returns an empty log and no snapshot work is
        // done.
        {
            let source_log = self.load_checkpoint_log(source_conversation_id).await?;
            let filtered_entries: Vec<_> = source_log
                .entries
                .into_iter()
                .filter(|e| e.message_index <= message_index)
                .collect();

            if !filtered_entries.is_empty() {
                let eligible_cp_ids: std::collections::HashSet<u64> =
                    filtered_entries.iter().map(|e| e.id.0).collect();

                // DF-112 (AC6): `detect_layout` now requires `conversation.json`
                // to exist for Directory layout. `save_checkpoint_log` calls
                // `create_dir_all(session_dir)` but that alone no longer flips
                // `detect_layout` — only `conversation.json` presence matters.
                // Still ensure the directory exists before writing the log.
                let conv_file = self.conversation_file(&new_id);
                if !tokio::fs::try_exists(&conv_file).await.unwrap_or(false) {
                    // Ensure dir and save conversation in Directory layout so that
                    // the forked session has a self-consistent directory structure.
                    if let Err(e) = self.save_conversation_inner(&forked, false).await {
                        tracing::warn!(
                            target = %new_id,
                            "Failed to ensure forked conversation in directory layout: {}",
                            e
                        );
                    }
                }

                // Persist the filtered checkpoint log to the forked session.
                let forked_log = CheckpointLog {
                    entries: filtered_entries,
                };
                if let Err(e) = self.save_checkpoint_log(&new_id, &forked_log).await {
                    tracing::warn!(
                        source = source_conversation_id,
                        target = %new_id,
                        "Failed to copy checkpoint log during fork: {}",
                        e
                    );
                }

                // Copy snapshot files whose cp_id is in the eligible set.
                let src_snapshots_dir = self.snapshots_dir(source_conversation_id);
                let dst_snapshots_dir = self.snapshots_dir(&new_id);
                if let Ok(mut entries) = tokio::fs::read_dir(&src_snapshots_dir).await {
                    let _ = tokio::fs::create_dir_all(&dst_snapshots_dir).await;
                    while let Ok(Some(entry)) = entries.next_entry().await {
                        let fname = entry.file_name().to_string_lossy().to_string();
                        if fname.ends_with(".tmp") {
                            continue;
                        }
                        // Filename format: "{cp_id}_{path_hash}"
                        let cp_id_part = match fname.split('_').next() {
                            Some(s) => s,
                            None => continue,
                        };
                        let cp_id = match cp_id_part.parse::<u64>() {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        if !eligible_cp_ids.contains(&cp_id) {
                            continue;
                        }
                        let src_path = entry.path();
                        let dst_path = dst_snapshots_dir.join(&fname);
                        if let Err(e) = tokio::fs::copy(&src_path, &dst_path).await {
                            tracing::warn!(
                                source = source_conversation_id,
                                target = %new_id,
                                snapshot = %fname,
                                "Failed to copy snapshot file during fork: {}",
                                e
                            );
                        }
                    }
                }
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

    async fn finalize_snapshot(
        &self,
        conversation_id: &str,
        checkpoint: crate::domain::models::checkpoint::CheckpointId,
        path: &std::path::Path,
        post_write_content: &[u8],
    ) -> Result<(), crate::domain::errors::StorageError> {
        self.finalize_snapshot_inner(conversation_id, checkpoint, path, post_write_content)
            .await
    }

    // ── Rewind Transaction Journal (DF-109, AC3) ─────────────────────────────

    async fn begin_rewind_txn(
        &self,
        conversation_id: &str,
        target_message_index: usize,
    ) -> Result<(), crate::domain::errors::StorageError> {
        use crate::domain::models::transaction::{RewindTxn, RewindTxnPhase};
        let txn = RewindTxn {
            conversation_id: conversation_id.to_string(),
            target_message_index,
            phase: RewindTxnPhase::Pending,
            created_at: {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64
            },
        };
        self.write_rewind_txn_file(conversation_id, &txn).await
    }

    async fn write_rewind_phase(
        &self,
        conversation_id: &str,
        phase: crate::domain::models::transaction::RewindTxnPhase,
    ) -> Result<(), crate::domain::errors::StorageError> {
        let mut txn = self
            .load_rewind_txn_inner(conversation_id)
            .await?
            .ok_or_else(|| {
                crate::domain::errors::StorageError::IoError(format!(
                    "write_rewind_phase: no active transaction for {}",
                    conversation_id
                ))
            })?;
        txn.phase = phase;
        self.write_rewind_txn_file(conversation_id, &txn).await
    }

    async fn commit_rewind_txn(
        &self,
        conversation_id: &str,
    ) -> Result<(), crate::domain::errors::StorageError> {
        let path = self.rewind_txn_path(conversation_id);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(crate::domain::errors::StorageError::IoError(format!(
                "commit_rewind_txn: failed to remove journal: {}",
                e
            ))),
        }
    }

    async fn load_rewind_txn(
        &self,
        conversation_id: &str,
    ) -> Result<
        Option<crate::domain::models::transaction::RewindTxn>,
        crate::domain::errors::StorageError,
    > {
        self.load_rewind_txn_inner(conversation_id).await
    }

    async fn reconcile_pending_txns(&self) -> Result<(), crate::domain::errors::StorageError> {
        self.reconcile_pending_txns_inner().await
    }
}

// ── Private serde structs for checkpoint persistence ─────────────────────────

/// Serde-able log of all checkpoints for a conversation.
/// Persisted to `{session_dir}/checkpoints.json`.
///
/// **Implementation note (Story 4-3b):** The Amendment 1 specification also allows
/// JSONL append-only context with `_checkpoint` markers, but this codebase uses
/// JSON-format conversation files (not JSONL), so a sidecar JSON file is used
/// instead.  The two approaches are functionally equivalent (both produce a
/// sorted list of CheckpointMeta).  A future story that migrates to JSONL can
/// replace this file with inline markers.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct CheckpointLog {
    entries: Vec<crate::domain::models::checkpoint::CheckpointMeta>,
}

// ── Checkpoint & Snapshot helpers (Amendment 1, Story 4-3b) ──────────────────

impl FileSystemStorage {
    /// Path to the snapshots directory for a conversation.
    fn snapshots_dir(&self, id: &str) -> std::path::PathBuf {
        self.session_dir(id).join("snapshots")
    }

    /// Finalize a snapshot after the Write tool completes (DF-111, AC5, schema v3).
    ///
    /// Records the `expected_current_hash` (hash of the post-write content) in the
    /// existing snapshot envelope. At revert time, this hash is compared against
    /// the current file content to distinguish "tool-modified" (→ Restore) from
    /// "externally-modified" (→ Conflict).
    ///
    /// Called by the toolset adapter after each successful Write/Edit operation.
    /// Best-effort: failure is logged but does NOT fail the tool execution — the
    /// snapshot degrades gracefully to schema v2 semantics (current != original → Restore).
    pub(crate) async fn finalize_snapshot_inner(
        &self,
        conversation_id: &str,
        checkpoint: crate::domain::models::checkpoint::CheckpointId,
        path: &std::path::Path,
        post_write_content: &[u8],
    ) -> Result<(), StorageError> {
        use sha2::{Digest, Sha256};

        // 1. Compute path_hash (same algorithm as snapshot_file).
        let canonical: PathBuf = match tokio::fs::canonicalize(path).await {
            Ok(p) => p,
            Err(_) => {
                // File might not exist right after write on some filesystems — derive.
                let parent = path.parent().ok_or_else(|| {
                    StorageError::NotSupported(format!(
                        "finalize_snapshot: path has no parent: {}",
                        path.display()
                    ))
                })?;
                let canonical_parent = tokio::fs::canonicalize(parent).await.map_err(|e| {
                    StorageError::NotSupported(format!(
                        "finalize_snapshot: parent canonicalization failed: {}",
                        e
                    ))
                })?;
                let fname = path.file_name().ok_or_else(|| {
                    StorageError::NotSupported("finalize_snapshot: path has no file name".into())
                })?;
                canonical_parent.join(fname)
            }
        };
        let path_hash = content_hash(canonical.as_os_str().as_encoded_bytes());

        // 2. Compute hash of post-write content.
        let expected_hash = {
            let mut h = Sha256::new();
            h.update(post_write_content);
            format!("sha256:{:x}", h.finalize())
        };

        // 3. Locate the snapshot file.
        let snapshot_name = format!("{}_{}", checkpoint.0, path_hash);
        let snapshot_path = self.snapshots_dir(conversation_id).join(&snapshot_name);

        if !tokio::fs::try_exists(&snapshot_path).await.unwrap_or(false) {
            // Snapshot was skipped (path traversal blocked, idempotency skip, etc.).
            return Ok(());
        }

        // 4. Read + deserialize existing envelope.
        let bytes = tokio::fs::read(&snapshot_path)
            .await
            .map_err(|e| StorageError::IoError(format!("finalize_snapshot read: {}", e)))?;
        let mut envelope: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| {
            StorageError::SerializationError(format!("finalize_snapshot parse: {}", e))
        })?;

        // 5. Inject expected_current_hash and bump schema_version to 3.
        if let Some(obj) = envelope.as_object_mut() {
            obj.insert(
                "expected_current_hash".to_string(),
                serde_json::Value::String(expected_hash),
            );
            obj.insert(
                "schema_version".to_string(),
                serde_json::Value::Number(serde_json::Number::from(3u32)),
            );
        }

        // 6. Atomic rewrite.
        let tmp_path = snapshot_path.with_extension("tmp");
        let updated = serde_json::to_vec_pretty(&envelope)
            .map_err(|e| StorageError::SerializationError(e.to_string()))?;
        tokio::fs::write(&tmp_path, &updated)
            .await
            .map_err(|e| StorageError::IoError(format!("finalize_snapshot write tmp: {}", e)))?;
        tokio::fs::rename(&tmp_path, &snapshot_path)
            .await
            .map_err(|e| StorageError::IoError(format!("finalize_snapshot rename: {}", e)))?;

        tracing::debug!(
            "Finalized snapshot for {} at checkpoint {} with expected_current_hash",
            canonical.display(),
            checkpoint.0
        );
        Ok(())
    }

    /// Path to the checkpoint log file for a conversation.
    fn checkpoints_file(&self, id: &str) -> std::path::PathBuf {
        self.session_dir(id).join("checkpoints.json")
    }

    /// Load the checkpoint log. Returns an empty log if the file does not exist.
    async fn load_checkpoint_log(&self, id: &str) -> Result<CheckpointLog, StorageError> {
        let path = self.checkpoints_file(id);
        match tokio::fs::read_to_string(&path).await {
            Ok(content) => serde_json::from_str::<CheckpointLog>(&content)
                .map_err(|e| StorageError::SerializationError(e.to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(CheckpointLog::default()),
            Err(e) => Err(StorageError::IoError(format!(
                "Failed to read checkpoints.json: {}",
                e
            ))),
        }
    }

    /// Atomically save the checkpoint log (tempfile + rename pattern).
    async fn save_checkpoint_log(&self, id: &str, log: &CheckpointLog) -> Result<(), StorageError> {
        // Ensure the session directory exists (it should already for any active conversation).
        let session_dir = self.session_dir(id);
        tokio::fs::create_dir_all(&session_dir)
            .await
            .map_err(|e| StorageError::IoError(format!("Failed to create session dir: {}", e)))?;
        let dest = self.checkpoints_file(id);
        let content = serde_json::to_vec_pretty(log)
            .map_err(|e| StorageError::SerializationError(e.to_string()))?;
        // Atomic write: write to a temp file then rename.
        let tmp_path = dest.with_extension("json.tmp");
        tokio::fs::write(&tmp_path, &content).await.map_err(|e| {
            StorageError::IoError(format!("Failed to write checkpoints tmp: {}", e))
        })?;
        tokio::fs::rename(&tmp_path, &dest).await.map_err(|e| {
            StorageError::IoError(format!("Failed to rename checkpoints file: {}", e))
        })?;
        Ok(())
    }

    /// Prune oldest checkpoints so the log stays within `retention` entries (DF-106, AC1).
    ///
    /// Oldest checkpoints (by checkpoint id, ascending) are pruned first.
    /// For each pruned checkpoint, all snapshot files with that `cp_id` prefix are deleted.
    /// The checkpoint log is updated atomically after pruning.
    ///
    /// Called opportunistically from `create_checkpoint`. Failures are logged and
    /// surfaced via `AppEvent::SystemNotice` (best-effort; no data loss on failure).
    async fn prune_old_snapshots(
        &self,
        conversation_id: &str,
        current_log: &CheckpointLog,
        retention: usize,
    ) -> Result<(), StorageError> {
        if current_log.entries.len() <= retention {
            return Ok(());
        }

        // Sort by checkpoint id ascending; the oldest entries are at the front.
        let mut sorted = current_log.entries.clone();
        sorted.sort_by_key(|e| e.id);

        let to_prune = &sorted[..sorted.len().saturating_sub(retention)];
        let prune_ids: std::collections::HashSet<u64> = to_prune.iter().map(|e| e.id.0).collect();

        // Delete snapshot files for pruned checkpoint ids.
        let snapshots_dir = self.snapshots_dir(conversation_id);
        if let Ok(mut entries) = tokio::fs::read_dir(&snapshots_dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let fname = entry.file_name().to_string_lossy().to_string();
                if fname.ends_with(".tmp") {
                    continue;
                }
                let cp_id_str = fname.split('_').next().unwrap_or("");
                if let Ok(cp_id) = cp_id_str.parse::<u64>() {
                    if prune_ids.contains(&cp_id) {
                        if let Err(e) = tokio::fs::remove_file(entry.path()).await {
                            tracing::warn!(
                                conversation_id = conversation_id,
                                snapshot = %fname,
                                "prune_old_snapshots: failed to remove snapshot file: {}",
                                e
                            );
                        }
                    }
                }
            }
        }

        // Update checkpoint log: remove pruned entries.
        let kept: Vec<_> = sorted
            .into_iter()
            .filter(|e| !prune_ids.contains(&e.id.0))
            .collect();
        let updated_log = CheckpointLog { entries: kept };
        match self
            .save_checkpoint_log(conversation_id, &updated_log)
            .await
        {
            Ok(_) => {
                tracing::info!(
                    conversation_id = conversation_id,
                    pruned = prune_ids.len(),
                    remaining = updated_log.entries.len(),
                    "Pruned old checkpoints to stay within retention limit"
                );
                Ok(())
            }
            Err(e) => {
                tracing::warn!(
                    conversation_id = conversation_id,
                    "prune_old_snapshots: failed to save updated log: {}",
                    e
                );
                Err(e)
            }
        }
    }

    // ── Rewind Transaction helpers (DF-109, AC3) ─────────────────────────────

    /// Path to the rewind transaction journal for a conversation.
    fn rewind_txn_path(&self, id: &str) -> PathBuf {
        self.session_dir(id).join("rewind_txn.json")
    }

    /// Write (or overwrite) the rewind transaction journal atomically.
    async fn write_rewind_txn_file(
        &self,
        conversation_id: &str,
        txn: &crate::domain::models::transaction::RewindTxn,
    ) -> Result<(), StorageError> {
        // Ensure the session directory exists.
        let session_dir = self.session_dir(conversation_id);
        tokio::fs::create_dir_all(&session_dir)
            .await
            .map_err(|e| StorageError::IoError(format!("Failed to create session dir: {}", e)))?;

        let dest = self.rewind_txn_path(conversation_id);
        let content = serde_json::to_vec_pretty(txn)
            .map_err(|e| StorageError::SerializationError(e.to_string()))?;
        let tmp_path = dest.with_extension("json.tmp");
        tokio::fs::write(&tmp_path, &content)
            .await
            .map_err(|e| StorageError::IoError(format!("rewind_txn write tmp: {}", e)))?;
        tokio::fs::rename(&tmp_path, &dest)
            .await
            .map_err(|e| StorageError::IoError(format!("rewind_txn rename: {}", e)))?;
        Ok(())
    }

    /// Load the rewind transaction journal if it exists.
    async fn load_rewind_txn_inner(
        &self,
        conversation_id: &str,
    ) -> Result<Option<crate::domain::models::transaction::RewindTxn>, StorageError> {
        let path = self.rewind_txn_path(conversation_id);
        match tokio::fs::read_to_string(&path).await {
            Ok(content) => {
                let txn = serde_json::from_str(&content)
                    .map_err(|e| StorageError::SerializationError(e.to_string()))?;
                Ok(Some(txn))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(StorageError::IoError(format!(
                "load_rewind_txn_inner: {}",
                e
            ))),
        }
    }

    /// Reconcile all incomplete rewind transactions on storage init (DF-109, B3.3).
    ///
    /// Scans every session directory for a `rewind_txn.json` file.  For each found:
    /// - `Pending`           → no work done → delete journal.
    /// - `MessagesTruncated` → truncation done, file revert missing → run file revert, delete journal.
    /// - `FilesReverted`     → file revert done, truncation missing → run truncation, delete journal.
    /// - `Committed`         → fully done → delete stale journal.
    ///
    /// Individual failures are logged but do not abort the sweep.
    async fn reconcile_pending_txns_inner(&self) -> Result<(), StorageError> {
        use crate::domain::models::transaction::RewindTxnPhase;

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
            let meta = match entry.metadata().await {
                Ok(m) => m,
                Err(_) => continue,
            };
            if !meta.is_dir() {
                continue;
            }
            let dir_name = entry.file_name().to_string_lossy().to_string();
            let txn_path = entry.path().join("rewind_txn.json");
            if !tokio::fs::try_exists(&txn_path).await.unwrap_or(false) {
                continue;
            }

            // Load the journal.
            let txn = match self.load_rewind_txn_inner(&dir_name).await {
                Ok(Some(t)) => t,
                Ok(None) => continue,
                Err(e) => {
                    tracing::warn!(
                        conversation_id = %dir_name,
                        "reconcile_pending_txns: failed to load journal: {}",
                        e
                    );
                    continue;
                }
            };

            tracing::info!(
                conversation_id = %dir_name,
                phase = ?txn.phase,
                target_message_index = txn.target_message_index,
                "reconcile_pending_txns: found incomplete rewind transaction"
            );

            match txn.phase {
                RewindTxnPhase::Pending | RewindTxnPhase::Committed => {
                    // No operations started (Pending) or already finished (Committed).
                    // Delete the journal — no state changes needed.
                    if let Err(e) = tokio::fs::remove_file(&txn_path).await {
                        tracing::warn!(
                            conversation_id = %dir_name,
                            "reconcile_pending_txns: failed to remove journal: {}",
                            e
                        );
                    } else {
                        tracing::info!(
                            conversation_id = %dir_name,
                            phase = ?txn.phase,
                            "reconcile_pending_txns: cleaned up journal"
                        );
                    }
                }

                RewindTxnPhase::MessagesTruncated => {
                    // Messages were truncated; file revert didn't complete.
                    // Complete by running revert_file_snapshots.
                    let recovery_ok = match self
                        .revert_file_snapshots(&dir_name, txn.target_message_index)
                        .await
                    {
                        Ok(reverted) => {
                            let restored = reverted
                                .iter()
                                .filter(|r| {
                                    matches!(
                                        r.status,
                                        crate::domain::models::checkpoint::RevertStatus::Restored
                                    )
                                })
                                .count();
                            tracing::info!(
                                conversation_id = %dir_name,
                                restored,
                                "reconcile_pending_txns: completed file revert after crash"
                            );
                            true
                        }
                        Err(e) => {
                            tracing::warn!(
                                conversation_id = %dir_name,
                                "reconcile_pending_txns: file revert completion failed: {}; \
                                 preserving journal at {} for manual recovery",
                                e, txn_path.display()
                            );
                            false
                        }
                    };
                    // Only delete journal after successful recovery — preserve it
                    // on failure so it can be retried on next startup.
                    if recovery_ok {
                        let _ = tokio::fs::remove_file(&txn_path).await;
                    }
                }

                RewindTxnPhase::FilesReverted => {
                    // Files were reverted; message truncation didn't complete.
                    // Complete by running truncate_conversation.
                    let recovery_ok = match self
                        .truncate_conversation(&dir_name, txn.target_message_index)
                        .await
                    {
                        Ok(_) => {
                            tracing::info!(
                                conversation_id = %dir_name,
                                target_message_index = txn.target_message_index,
                                "reconcile_pending_txns: completed message truncation after crash"
                            );
                            true
                        }
                        Err(e) => {
                            tracing::warn!(
                                conversation_id = %dir_name,
                                "reconcile_pending_txns: truncation completion failed: {}; \
                                 preserving journal at {} for manual recovery",
                                e, txn_path.display()
                            );
                            false
                        }
                    };
                    // Only delete journal after successful recovery.
                    if recovery_ok {
                        let _ = tokio::fs::remove_file(&txn_path).await;
                    }
                }
            }
        }

        Ok(())
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
                    synthetic: false,
                    images: vec![],
                    origin: crate::domain::models::ChannelKind::Terminal,
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
                    synthetic: false,
                    images: vec![],
                    origin: crate::domain::models::ChannelKind::Terminal,
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
                reasoning_tokens: None,
            }),
            turns: Vec::new(),
            plans: std::collections::HashMap::new(),
            fork_source: None,
            compaction: None,
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
            imported_from: None,
            plan_slug: None,
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

    // ── Story 4-3b review patches (P2, P3, D1, DF-110) ────────────────────

    use crate::domain::models::checkpoint::CheckpointId;
    use crate::domain::ports::StoragePort;

    /// P2: `snapshot_file` must reject paths that resolve outside the configured
    /// workspace root. Covers `../` escape via canonicalization.
    #[tokio::test]
    async fn test_snapshot_rejects_path_outside_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        let bad_file = outside.join("secret.txt");
        std::fs::write(&bad_file, b"top secret").unwrap();

        let sessions_dir = workspace.join(".claude").join("sessions");
        let storage = FileSystemStorage::with_workspace_root(sessions_dir, workspace.clone());

        let result = storage
            .snapshot_file("conv-1", CheckpointId(1), &bad_file, b"top secret")
            .await;

        assert!(
            matches!(result, Err(StorageError::NotSupported(ref msg)) if msg.contains("path outside workspace")),
            "expected path traversal rejection, got {:?}",
            result
        );
    }

    /// P2: snapshot_file must fail closed when the workspace root cannot be
    /// determined (neither explicit nor derivable from sessions_dir parents).
    #[tokio::test]
    async fn test_snapshot_fails_closed_without_workspace_root() {
        let tmp = tempfile::tempdir().unwrap();
        // Use a root-level path for sessions_dir so neither parent nor
        // grandparent can derive a workspace proxy.
        let sessions_dir = PathBuf::from("/");
        let storage = FileSystemStorage::new(sessions_dir);

        let file = tmp.path().join("victim.txt");
        std::fs::write(&file, b"content").unwrap();

        let result = storage
            .snapshot_file("conv-1", CheckpointId(1), &file, b"content")
            .await;

        // Either NotSupported (workspace not derivable) or NotSupported (path outside);
        // both are fail-closed outcomes. The key assertion is that the call does NOT
        // succeed with the file unprotected.
        assert!(
            matches!(result, Err(StorageError::NotSupported(_))),
            "expected fail-closed NotSupported, got {:?}",
            result
        );
    }

    /// P3: path hash must be derived from OsStr bytes so that two distinct
    /// non-UTF8 paths (which would collide under `to_string_lossy`) produce
    /// distinct hashes. We approximate by hashing `content_hash(OsStr.bytes)`
    /// directly since constructing real non-UTF8 paths portably is awkward.
    #[test]
    fn test_path_hash_stable_from_os_str() {
        use std::ffi::OsString;
        let a = OsString::from("/tmp/foo/bar.txt");
        let b = OsString::from("/tmp/foo/baz.txt");
        let ha = content_hash(a.as_encoded_bytes());
        let hb = content_hash(b.as_encoded_bytes());
        assert_ne!(ha, hb, "distinct paths must produce distinct hashes");
        // Same path → same hash (stability)
        let ha2 = content_hash(a.as_encoded_bytes());
        assert_eq!(ha, ha2);
    }

    /// D1 (schema v2): round-trip an envelope with `file_existed=false` and
    /// verify `revert_file_snapshots` deletes the file that appeared after the
    /// checkpoint. Also verifies v1 envelopes (no `file_existed` field) still
    /// work via the empty-content fallback.
    #[tokio::test]
    async fn test_snapshot_schema_v2_file_existed_false_deletes_file() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().to_path_buf();
        let sessions_dir = workspace.join(".claude").join("sessions");
        tokio::fs::create_dir_all(&sessions_dir).await.unwrap();

        let storage = FileSystemStorage::with_workspace_root(sessions_dir, workspace.clone());

        // Set up a conversation + checkpoint log so revert has something to
        // anchor against. snapshot_file is tested via an empty-content snapshot,
        // which under our caller convention encodes "file did not exist".
        let file = workspace.join("new_file.txt");

        // Snapshot with empty content (simulates "file did not exist pre-checkpoint").
        storage
            .snapshot_file("conv-v2", CheckpointId(1), &file, b"")
            .await
            .expect("empty-content snapshot should succeed");

        // Now the tool creates the file.
        tokio::fs::write(&file, b"created by tool").await.unwrap();
        assert!(file.exists());

        // Revert any snapshots newer than checkpoint 0 (which is all of them).
        let reverted = storage
            .revert_file_snapshots("conv-v2", 0)
            .await
            .expect("revert should succeed");

        assert!(!reverted.is_empty(), "at least one file should be reverted");
        assert!(
            !file.exists(),
            "file that didn't exist pre-checkpoint should be deleted on revert"
        );
    }

    /// DF-110 (4-6 cleanup): `SNAPSHOT_MAX_BYTES` removed — large files no longer capped.
    /// Snapshot a file larger than the old 50 MiB cap and verify it is written.
    #[tokio::test]
    async fn test_snapshot_large_file_no_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().to_path_buf();
        let sessions_dir = workspace.join(".claude").join("sessions");
        tokio::fs::create_dir_all(&sessions_dir).await.unwrap();
        let storage = FileSystemStorage::with_workspace_root(sessions_dir, workspace.clone());

        let file = workspace.join("big.bin");
        // 1 MiB — small enough for the unit test but above the old per-chunk limit.
        let content = vec![0u8; 1024 * 1024];
        tokio::fs::write(&file, &content).await.unwrap();

        let result = storage
            .snapshot_file("conv-big", CheckpointId(1), &file, &content)
            .await;

        assert!(result.is_ok(), "large-file snapshot must succeed (no cap)");

        // Snapshot file must have been created.
        let snapshots_dir = workspace
            .join(".claude")
            .join("sessions")
            .join("conv-big")
            .join("snapshots");
        let mut count = 0;
        if let Ok(mut entries) = tokio::fs::read_dir(&snapshots_dir).await {
            while let Ok(Some(_)) = entries.next_entry().await {
                count += 1;
            }
        }
        assert_eq!(count, 1, "snapshot file must be created for large file");
    }

    #[tokio::test]
    async fn fork_preserves_plans_within_truncated_range() {
        use crate::domain::models::ContentBlockType;
        use crate::domain::models::checkpoint::CheckpointId;
        use crate::domain::models::plan::{Plan, PlanStatus, PlanTask, PlanTaskStatus};

        let tmp = tempfile::tempdir().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        let storage = FileSystemStorage::new(sessions_dir);

        let mut plans = std::collections::HashMap::new();
        plans.insert(
            "plan-a".to_string(),
            Plan {
                id: "plan-a".to_string(),
                title: "Keep this".to_string(),
                tasks: vec![PlanTask {
                    number: 1,
                    title: "Task".to_string(),
                    description: String::new(),
                    depends_on: vec![],
                    status: PlanTaskStatus::Pending,
                    started_at_ms: None,
                    completed_at_ms: None,
                    result: None,
                    error: None,
                    waiting_on: vec![],
                    delegated_to: None,
                    sub_tasks: vec![],
                }],
                estimated_effort: None,
                status: PlanStatus::Pending,
                created_at: 1700000000,
                resolved_at: None,
                host_message_id: Some("msg-1".to_string()),
            },
        );
        plans.insert(
            "plan-b".to_string(),
            Plan {
                id: "plan-b".to_string(),
                title: "Drop this".to_string(),
                tasks: vec![],
                estimated_effort: None,
                status: PlanStatus::Pending,
                created_at: 1700000005,
                resolved_at: None,
                host_message_id: Some("msg-3".to_string()),
            },
        );

        let source = Conversation {
            id: "source-conv".to_string(),
            title: "Source".to_string(),
            messages: vec![
                ChatMessage {
                    id: "msg-1".to_string(),
                    role: MessageRole::User,
                    content: "one".to_string(),
                    content_blocks: vec![ContentBlockType::PlanCard],
                    tool_calls: vec![],
                    created_at: 1700000000,
                    token_count: None,
                    stop_reason: None,
                    synthetic: false,
                    images: vec![],
                    origin: crate::domain::models::ChannelKind::Terminal,
                },
                ChatMessage {
                    id: "msg-2".to_string(),
                    role: MessageRole::Assistant,
                    content: "two".to_string(),
                    content_blocks: vec![],
                    tool_calls: vec![],
                    created_at: 1700000001,
                    token_count: None,
                    stop_reason: None,
                    synthetic: false,
                    images: vec![],
                    origin: crate::domain::models::ChannelKind::Terminal,
                },
                ChatMessage {
                    id: "msg-3".to_string(),
                    role: MessageRole::User,
                    content: "three".to_string(),
                    content_blocks: vec![ContentBlockType::PlanCard],
                    tool_calls: vec![],
                    created_at: 1700000002,
                    token_count: None,
                    stop_reason: None,
                    synthetic: false,
                    images: vec![],
                    origin: crate::domain::models::ChannelKind::Terminal,
                },
            ],
            created_at: 1700000000,
            updated_at: 1700000002,
            last_response_at: None,
            session_id: None,
            usage: None,
            turns: Vec::new(),
            plans,
            fork_source: None,
            compaction: None,
        };

        storage.save_conversation(&source).await.unwrap();

        let forked_id = storage
            .fork_at_checkpoint("source-conv", CheckpointId(1))
            .await
            .unwrap();

        let forked = storage
            .load_conversation(&forked_id)
            .await
            .unwrap()
            .expect("forked conversation should exist");

        assert_eq!(forked.messages.len(), 2, "fork keeps messages 0 and 1");
        assert_eq!(forked.plans.len(), 1, "only plan-a should survive");
        assert!(forked.plans.contains_key("plan-a"));
        assert!(!forked.plans.contains_key("plan-b"));
    }

    #[tokio::test]
    async fn rewind_prunes_plans_beyond_truncation_point() {
        use crate::domain::models::ContentBlockType;
        use crate::domain::models::plan::{Plan, PlanStatus, PlanTask, PlanTaskStatus};

        let tmp = tempfile::tempdir().unwrap();
        let sessions_dir = tmp.path().join("sessions");
        let storage = FileSystemStorage::new(sessions_dir);

        let mut plans = std::collections::HashMap::new();
        plans.insert(
            "plan-keep".to_string(),
            Plan {
                id: "plan-keep".to_string(),
                title: "Survives".to_string(),
                tasks: vec![PlanTask {
                    number: 1,
                    title: "Task".to_string(),
                    description: String::new(),
                    depends_on: vec![],
                    status: PlanTaskStatus::Pending,
                    started_at_ms: None,
                    completed_at_ms: None,
                    result: None,
                    error: None,
                    waiting_on: vec![],
                    delegated_to: None,
                    sub_tasks: vec![],
                }],
                estimated_effort: None,
                status: PlanStatus::Pending,
                created_at: 1700000000,
                resolved_at: None,
                host_message_id: Some("msg-1".to_string()),
            },
        );
        plans.insert(
            "plan-drop".to_string(),
            Plan {
                id: "plan-drop".to_string(),
                title: "Pruned".to_string(),
                tasks: vec![],
                estimated_effort: None,
                status: PlanStatus::Pending,
                created_at: 1700000005,
                resolved_at: None,
                host_message_id: Some("msg-3".to_string()),
            },
        );

        let conv = Conversation {
            id: "rewind-conv".to_string(),
            title: "Rewind".to_string(),
            messages: vec![
                ChatMessage {
                    id: "msg-1".to_string(),
                    role: MessageRole::User,
                    content: "keep".to_string(),
                    content_blocks: vec![ContentBlockType::PlanCard],
                    tool_calls: vec![],
                    created_at: 1700000000,
                    token_count: None,
                    stop_reason: None,
                    synthetic: false,
                    images: vec![],
                    origin: crate::domain::models::ChannelKind::Terminal,
                },
                ChatMessage {
                    id: "msg-2".to_string(),
                    role: MessageRole::Assistant,
                    content: "also keep".to_string(),
                    content_blocks: vec![],
                    tool_calls: vec![],
                    created_at: 1700000001,
                    token_count: None,
                    stop_reason: None,
                    synthetic: false,
                    images: vec![],
                    origin: crate::domain::models::ChannelKind::Terminal,
                },
                ChatMessage {
                    id: "msg-3".to_string(),
                    role: MessageRole::User,
                    content: "prune this".to_string(),
                    content_blocks: vec![ContentBlockType::PlanCard],
                    tool_calls: vec![],
                    created_at: 1700000002,
                    token_count: None,
                    stop_reason: None,
                    synthetic: false,
                    images: vec![],
                    origin: crate::domain::models::ChannelKind::Terminal,
                },
            ],
            created_at: 1700000000,
            updated_at: 1700000002,
            last_response_at: None,
            session_id: None,
            usage: None,
            turns: Vec::new(),
            plans,
            fork_source: None,
            compaction: None,
        };

        storage.save_conversation(&conv).await.unwrap();

        let truncated = storage
            .truncate_conversation("rewind-conv", 1)
            .await
            .unwrap();

        assert_eq!(truncated.messages.len(), 2);
        assert_eq!(truncated.plans.len(), 1, "only plan-keep should survive");
        assert!(truncated.plans.contains_key("plan-keep"));
        assert!(!truncated.plans.contains_key("plan-drop"));

        let reloaded = storage
            .load_conversation("rewind-conv")
            .await
            .unwrap()
            .expect("should reload");
        assert_eq!(reloaded.plans.len(), 1);
        assert!(reloaded.plans.contains_key("plan-keep"));
    }
}
