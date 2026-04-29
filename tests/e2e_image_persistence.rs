//! E2E tests for Story 4-3a.1: Image Reference Persistence (DF-067).
//!
//! AC[1]: ImageReference Domain Type
//! AC[2]: Session Directory Layout Migration
//! AC[3]: Image Persistence on Message Submit
//! AC[4]: Image Load on Session Restore
//! AC[5]: Image Copy During Fork
//! AC[6]: Image Extension Case Insensitivity
//!
//! Follows the TestHarness pattern from `tests/e2e_fork.rs`.

use tempfile::TempDir;

use rustain::adapters::filesystem::{FileSystemStorage, content_hash, normalize_extension};
use rustain::domain::models::MessageRole;
use rustain::domain::models::checkpoint::CheckpointId;
use rustain::domain::models::conversation::{
    ChatMessage, Conversation, ImageReference, generate_conversation_id,
};
use rustain::domain::ports::StoragePort;

// ── Helpers ──────────────────────────────────────────────────────────────────

fn make_message_with_images(
    role: MessageRole,
    content: &str,
    images: Vec<ImageReference>,
) -> ChatMessage {
    ChatMessage {
        synthetic: false,
        id: generate_conversation_id(),
        role,
        content: content.to_string(),
        content_blocks: vec![],
        tool_calls: vec![],
        created_at: 1_700_000_000,
        token_count: None,
        stop_reason: None,
        images,
    }
}

fn make_conversation(id: &str, messages: Vec<ChatMessage>) -> Conversation {
    Conversation {
        id: id.to_string(),
        title: "Image Test".to_string(),
        messages,
        created_at: 1_700_000_000,
        updated_at: 1_700_000_001,
        last_response_at: None,
        session_id: None,
        usage: None,
        plans: std::collections::HashMap::new(),
        fork_source: None,
    }
}

fn sample_png_bytes(marker: u8) -> Vec<u8> {
    // Short, deterministic payload per test so hashes are stable.
    let mut v = vec![0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
    v.extend_from_slice(&[marker; 32]);
    v
}

fn image_ref_for(bytes: &[u8], media_type: &str) -> ImageReference {
    let ext = normalize_extension(media_type);
    ImageReference {
        file_name: format!("{}.{}", content_hash(bytes), ext),
        media_type: media_type.to_string(),
        original_size: bytes.len(),
    }
}

// ── AC[1]: ImageReference Domain Type ────────────────────────────────────────

#[test]
fn test_e2e_image_reference_serialization() {
    // Given: a ChatMessage with two images
    let img1 = ImageReference {
        file_name: "aaaa1111bbbb2222.png".to_string(),
        media_type: "image/png".to_string(),
        original_size: 1024,
    };
    let img2 = ImageReference {
        file_name: "ccc33333ddd44444.jpg".to_string(),
        media_type: "image/jpeg".to_string(),
        original_size: 2048,
    };
    let msg = make_message_with_images(
        MessageRole::User,
        "look at these",
        vec![img1.clone(), img2.clone()],
    );

    // When: round-tripped through serde
    let json = serde_json::to_string(&msg).expect("serialize");
    let decoded: ChatMessage = serde_json::from_str(&json).expect("deserialize");

    // Then: images survive the round-trip with camelCase field names on disk
    assert_eq!(decoded.images.len(), 2);
    assert_eq!(decoded.images[0].file_name, img1.file_name);
    assert_eq!(decoded.images[0].media_type, "image/png");
    assert_eq!(decoded.images[0].original_size, 1024);
    assert_eq!(decoded.images[1].file_name, img2.file_name);
    assert!(
        json.contains("\"fileName\""),
        "ImageReference must serialize with camelCase field names: {}",
        json
    );
}

#[test]
fn test_e2e_image_reference_backward_compat() {
    // Given: a legacy pre-4-3a.1 ChatMessage JSON with NO images field
    let legacy = r#"{
        "id": "legacy-1",
        "role": "user",
        "content": "hello world",
        "contentBlocks": [],
        "toolCalls": [],
        "createdAt": 1700000000,
        "tokenCount": null
    }"#;

    // When: deserialized with the new schema
    let msg: ChatMessage =
        serde_json::from_str(legacy).expect("legacy JSON must still deserialize");

    // Then: images defaults to empty Vec, no panic
    assert_eq!(msg.id, "legacy-1");
    assert!(
        msg.images.is_empty(),
        "images must default to empty for legacy messages"
    );
}

#[test]
fn test_e2e_empty_images_vec_omitted() {
    // Given: a ChatMessage with an empty images vec
    let msg = make_message_with_images(MessageRole::User, "text only", vec![]);

    // When: serialized
    let json = serde_json::to_string(&msg).expect("serialize");

    // Then: "images" field is entirely absent from the JSON (skip_serializing_if)
    assert!(
        !json.contains("\"images\""),
        "empty images vec must be omitted from JSON for size/back-compat: {}",
        json
    );
}

// ── AC[2]: Session Directory Layout Migration ────────────────────────────────

#[tokio::test]
async fn test_e2e_session_directory_layout_with_images() {
    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join("sessions");
    let storage = FileSystemStorage::new(sessions.clone());

    // Given: raw image bytes and a conversation that references them
    let raw = sample_png_bytes(0x11);
    let img_ref = image_ref_for(&raw, "image/png");
    let msg = make_message_with_images(MessageRole::User, "picture", vec![img_ref.clone()]);
    let conv = make_conversation("conv-dir-layout", vec![msg]);

    // When: save_image then save_conversation
    storage.save_image(&conv.id, &img_ref, &raw).await.unwrap();
    storage.save_conversation(&conv).await.unwrap();

    // Then: directory layout artifacts exist
    let dir = sessions.join("conv-dir-layout");
    assert!(
        dir.join("conversation.json").is_file(),
        "conversation.json must exist"
    );
    assert!(
        dir.join("meta.json").is_file(),
        "meta.json sidecar must exist"
    );
    assert!(
        dir.join("images").join(&img_ref.file_name).is_file(),
        "image file must exist"
    );

    // And: reload round-trips the image reference
    let loaded = storage
        .load_conversation(&conv.id)
        .await
        .unwrap()
        .expect("should load");
    assert_eq!(loaded.messages[0].images.len(), 1);
    assert_eq!(loaded.messages[0].images[0].file_name, img_ref.file_name);
}

#[tokio::test]
async fn test_e2e_old_flat_format_still_loads() {
    // Given: a manually-seeded legacy flat session (pre-4-3a.1, no images)
    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let main = r#"{
        "id": "legacy-flat",
        "title": "Legacy Session",
        "messages": [
            {
                "id": "m0",
                "role": "user",
                "content": "pre-4-3a.1 message",
                "contentBlocks": [],
                "toolCalls": [],
                "createdAt": 1700000000,
                "tokenCount": null
            }
        ],
        "createdAt": 1700000000,
        "cleanExit": true
    }"#;
    std::fs::write(sessions.join("legacy-flat.meta.json"), main).unwrap();

    // When: loaded by the current adapter
    let storage = FileSystemStorage::new(sessions);
    let loaded = storage
        .load_conversation("legacy-flat")
        .await
        .unwrap()
        .expect("legacy flat session must load");

    // Then: conversation deserializes and images field defaults to empty
    assert_eq!(loaded.id, "legacy-flat");
    assert_eq!(loaded.messages.len(), 1);
    assert!(loaded.messages[0].images.is_empty());
}

#[tokio::test]
async fn test_e2e_atomic_migration_on_first_image() {
    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join("sessions");
    let storage = FileSystemStorage::new(sessions.clone());

    // Given: a previously saved flat (image-free) session
    let mut conv = make_conversation(
        "conv-migrate",
        vec![make_message_with_images(
            MessageRole::User,
            "text only",
            vec![],
        )],
    );
    storage.save_conversation(&conv).await.unwrap();
    assert!(sessions.join("conv-migrate.meta.json").is_file());
    assert!(sessions.join("conv-migrate.session.json").is_file());

    // When: the user attaches an image and re-saves
    let raw = sample_png_bytes(0x22);
    let img_ref = image_ref_for(&raw, "image/png");
    storage.save_image(&conv.id, &img_ref, &raw).await.unwrap();
    conv.messages[0].images = vec![img_ref.clone()];
    storage.save_conversation(&conv).await.unwrap();

    // Then: new directory layout exists
    let dir = sessions.join("conv-migrate");
    assert!(dir.join("conversation.json").is_file());
    assert!(dir.join("meta.json").is_file());
    assert!(dir.join("images").join(&img_ref.file_name).is_file());

    // And: the legacy flat files are gone (atomic swap)
    assert!(!sessions.join("conv-migrate.meta.json").exists());
    assert!(!sessions.join("conv-migrate.session.json").exists());
}

#[tokio::test]
async fn test_e2e_migration_rollback_on_failure() {
    // Inject failure by pre-populating the target `conversation.json` path as a
    // *file* instead of a directory. `save_directory_layout` calls
    // `create_dir_all` which will fail because the path already exists as a
    // regular file. We verify the legacy flat files are untouched when the
    // migration write path errors out.
    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join("sessions");
    let storage = FileSystemStorage::new(sessions.clone());

    // Given: a flat session already on disk
    let mut conv = make_conversation(
        "conv-rollback",
        vec![make_message_with_images(MessageRole::User, "text", vec![])],
    );
    storage.save_conversation(&conv).await.unwrap();
    std::fs::create_dir_all(sessions.join("conv-rollback").parent().unwrap()).ok();

    // Block the target directory path by creating a plain file where the
    // session directory should go.
    std::fs::write(sessions.join("conv-rollback"), b"blocker").unwrap();

    // When: we try to save with an image attached (would migrate to dir layout)
    let raw = sample_png_bytes(0x33);
    let img_ref = ImageReference {
        file_name: format!("{}.png", content_hash(&raw)),
        media_type: "image/png".to_string(),
        original_size: raw.len(),
    };
    conv.messages[0].images = vec![img_ref];
    let result = storage.save_conversation(&conv).await;

    // Then: the save errors out and the legacy flat file is left untouched.
    assert!(
        result.is_err(),
        "save should fail when target path is blocked"
    );
    assert!(
        sessions.join("conv-rollback.meta.json").is_file(),
        "legacy flat main file must survive a failed migration"
    );
}

#[tokio::test]
async fn test_e2e_list_conversations_finds_both_formats() {
    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join("sessions");
    let storage = FileSystemStorage::new(sessions.clone());

    // Given: one flat session (no images) and one directory session (with image)
    let flat = make_conversation(
        "conv-flat",
        vec![make_message_with_images(
            MessageRole::User,
            "flat only",
            vec![],
        )],
    );
    storage.save_conversation(&flat).await.unwrap();

    let raw = sample_png_bytes(0x44);
    let img_ref = image_ref_for(&raw, "image/png");
    let dir = make_conversation(
        "conv-dir",
        vec![make_message_with_images(
            MessageRole::User,
            "with image",
            vec![img_ref.clone()],
        )],
    );
    storage.save_image(&dir.id, &img_ref, &raw).await.unwrap();
    storage.save_conversation(&dir).await.unwrap();

    // When: list_conversations runs
    let summaries = storage.list_conversations().await.unwrap();

    // Then: both are found, deduplicated, in updated_at desc order
    assert_eq!(summaries.len(), 2);
    let ids: Vec<_> = summaries.iter().map(|s| s.id.clone()).collect();
    assert!(ids.contains(&"conv-flat".to_string()));
    assert!(ids.contains(&"conv-dir".to_string()));
}

// ── AC[3]: Image Persistence on Message Submit ───────────────────────────────

#[tokio::test]
async fn test_e2e_image_saved_on_submit_main_path() {
    // Simulates what event_loop.rs does at the main submit drain site:
    // compute hash, save bytes, attach ImageReference to a ChatMessage,
    // then save_conversation. This is the direct E2E cover for AC3.
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));

    let raw = sample_png_bytes(0x55);
    let img_ref = image_ref_for(&raw, "image/png");
    storage
        .save_image("conv-submit", &img_ref, &raw)
        .await
        .unwrap();

    let msg = make_message_with_images(MessageRole::User, "look", vec![img_ref.clone()]);
    let conv = make_conversation("conv-submit", vec![msg]);
    storage.save_conversation(&conv).await.unwrap();

    // Reload and check the image reference round-tripped
    let loaded = storage
        .load_conversation("conv-submit")
        .await
        .unwrap()
        .expect("load");
    assert_eq!(loaded.messages[0].images.len(), 1);
    assert_eq!(loaded.messages[0].images[0].file_name, img_ref.file_name);
    // And the raw file is still readable
    let raw_loaded = storage.load_image("conv-submit", &img_ref).await.unwrap();
    assert_eq!(raw_loaded, raw);
}

#[tokio::test]
async fn test_e2e_image_saved_on_turn_restart() {
    // Turn restart drain site (event_loop.rs ~:605) shares the same
    // `persist_image_attachments` helper as the main path. This test mimics
    // the restart: an existing conversation gains a new message with an
    // image, and we verify the layout transitions from flat → directory.
    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join("sessions");
    let storage = FileSystemStorage::new(sessions.clone());

    let mut conv = make_conversation(
        "conv-restart",
        vec![make_message_with_images(
            MessageRole::User,
            "turn 1",
            vec![],
        )],
    );
    storage.save_conversation(&conv).await.unwrap();

    let raw = sample_png_bytes(0x66);
    let img_ref = image_ref_for(&raw, "image/png");
    storage.save_image(&conv.id, &img_ref, &raw).await.unwrap();
    conv.messages.push(make_message_with_images(
        MessageRole::User,
        "turn 2 with image",
        vec![img_ref.clone()],
    ));
    storage.save_conversation(&conv).await.unwrap();

    let loaded = storage
        .load_conversation(&conv.id)
        .await
        .unwrap()
        .expect("load");
    assert_eq!(loaded.messages.len(), 2);
    assert_eq!(loaded.messages[1].images.len(), 1);
    assert_eq!(loaded.messages[1].images[0].file_name, img_ref.file_name);
}

#[test]
fn test_e2e_image_hash_filename_stable_across_saves() {
    // AC3 sub-clause: the content-addressed filename must be stable so two
    // identical attachments dedupe on disk. This guards against accidental
    // reintroduction of a non-deterministic hasher (e.g. DefaultHasher).
    let a = sample_png_bytes(0x77);
    let b = sample_png_bytes(0x77);
    assert_eq!(content_hash(&a), content_hash(&b));
    // Different payloads produce different hashes
    let c = sample_png_bytes(0x88);
    assert_ne!(content_hash(&a), content_hash(&c));
}

// ── AC[4]: Image Load on Session Restore ─────────────────────────────────────

#[tokio::test]
async fn test_e2e_image_load_on_restore() {
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));

    let raw = sample_png_bytes(0x99);
    let img_ref = image_ref_for(&raw, "image/png");
    storage
        .save_image("conv-restore", &img_ref, &raw)
        .await
        .unwrap();
    let msg = make_message_with_images(MessageRole::User, "saved", vec![img_ref.clone()]);
    let conv = make_conversation("conv-restore", vec![msg]);
    storage.save_conversation(&conv).await.unwrap();

    // Drop & recreate storage to simulate process restart
    let storage2 = FileSystemStorage::new(tmp.path().join("sessions"));
    let loaded = storage2
        .load_conversation("conv-restore")
        .await
        .unwrap()
        .expect("load");
    assert_eq!(loaded.messages[0].images.len(), 1);
    let raw_loaded = storage2.load_image("conv-restore", &img_ref).await.unwrap();
    assert_eq!(raw_loaded, raw);
}

#[tokio::test]
async fn test_e2e_missing_image_graceful_degradation() {
    // Given: a directory-layout session whose meta references an image file
    // that has been deleted from disk (simulating accidental user cleanup).
    let tmp = TempDir::new().unwrap();
    let storage = FileSystemStorage::new(tmp.path().join("sessions"));

    let raw = sample_png_bytes(0xAA);
    let img_ref = image_ref_for(&raw, "image/png");
    storage
        .save_image("conv-missing", &img_ref, &raw)
        .await
        .unwrap();
    let msg = make_message_with_images(MessageRole::User, "photo", vec![img_ref.clone()]);
    let conv = make_conversation("conv-missing", vec![msg]);
    storage.save_conversation(&conv).await.unwrap();

    // Delete the raw file behind the storage's back
    let img_path = tmp
        .path()
        .join("sessions/conv-missing/images")
        .join(&img_ref.file_name);
    std::fs::remove_file(&img_path).unwrap();

    // When: the conversation is loaded
    let loaded = storage
        .load_conversation("conv-missing")
        .await
        .unwrap()
        .expect("load must succeed even with a dangling image ref");

    // Then: the ImageReference is preserved (graceful degradation, warn only)
    assert_eq!(loaded.messages[0].images.len(), 1);
    assert_eq!(loaded.messages[0].images[0].file_name, img_ref.file_name);
}

// ── AC[5]: Image Copy During Fork ────────────────────────────────────────────

#[tokio::test]
async fn test_e2e_fork_copies_image_files() {
    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join("sessions");
    let storage = FileSystemStorage::new(sessions.clone());

    // Source session: two messages, each with one image
    let raw1 = sample_png_bytes(0xB1);
    let raw2 = sample_png_bytes(0xB2);
    let ref1 = image_ref_for(&raw1, "image/png");
    let ref2 = image_ref_for(&raw2, "image/png");

    storage
        .save_image("src-fork-img", &ref1, &raw1)
        .await
        .unwrap();
    storage
        .save_image("src-fork-img", &ref2, &raw2)
        .await
        .unwrap();

    let conv = make_conversation(
        "src-fork-img",
        vec![
            make_message_with_images(MessageRole::User, "first", vec![ref1.clone()]),
            make_message_with_images(MessageRole::Assistant, "second", vec![ref2.clone()]),
        ],
    );
    storage.save_conversation(&conv).await.unwrap();

    // Fork at checkpoint 1 (keeps messages 0 and 1)
    let new_id = storage
        .fork_at_checkpoint("src-fork-img", CheckpointId(1))
        .await
        .unwrap();

    // Assert forked images directory contains both files
    let forked_images = sessions.join(&new_id).join("images");
    assert!(
        forked_images.join(&ref1.file_name).is_file(),
        "forked session must carry over image 1"
    );
    assert!(
        forked_images.join(&ref2.file_name).is_file(),
        "forked session must carry over image 2"
    );
}

#[tokio::test]
async fn test_e2e_fork_no_images_noop() {
    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join("sessions");
    let storage = FileSystemStorage::new(sessions.clone());

    // Conversation with no images at all
    let conv = make_conversation(
        "src-no-img",
        vec![make_message_with_images(
            MessageRole::User,
            "text only",
            vec![],
        )],
    );
    storage.save_conversation(&conv).await.unwrap();

    let new_id = storage
        .fork_at_checkpoint("src-no-img", CheckpointId(0))
        .await
        .unwrap();

    // The forked session should exist but have no images/ dir
    let forked_images = sessions.join(&new_id).join("images");
    assert!(
        !forked_images.exists(),
        "forked session without images must not create an images/ dir"
    );
}

#[tokio::test]
async fn test_e2e_fork_tolerates_missing_source_image() {
    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join("sessions");
    let storage = FileSystemStorage::new(sessions.clone());

    // Source has two images but we delete one from disk before fork
    let raw1 = sample_png_bytes(0xC1);
    let raw2 = sample_png_bytes(0xC2);
    let ref1 = image_ref_for(&raw1, "image/png");
    let ref2 = image_ref_for(&raw2, "image/png");
    storage
        .save_image("src-partial", &ref1, &raw1)
        .await
        .unwrap();
    storage
        .save_image("src-partial", &ref2, &raw2)
        .await
        .unwrap();

    let conv = make_conversation(
        "src-partial",
        vec![
            make_message_with_images(MessageRole::User, "one", vec![ref1.clone()]),
            make_message_with_images(MessageRole::User, "two", vec![ref2.clone()]),
        ],
    );
    storage.save_conversation(&conv).await.unwrap();

    // Delete ref1's file from source behind the adapter's back
    std::fs::remove_file(sessions.join("src-partial/images").join(&ref1.file_name)).unwrap();

    // Fork should still succeed; copy is best-effort
    let new_id = storage
        .fork_at_checkpoint("src-partial", CheckpointId(1))
        .await
        .unwrap();

    let forked_images = sessions.join(&new_id).join("images");
    assert!(
        forked_images.join(&ref2.file_name).is_file(),
        "surviving image must be copied"
    );
    assert!(
        !forked_images.join(&ref1.file_name).exists(),
        "deleted source image must remain absent in the fork"
    );
}

// ── AC[6]: Image Extension Case Insensitivity ────────────────────────────────

#[test]
fn test_e2e_image_extension_case_insensitive() {
    // Same ext regardless of media_type casing (AC6 — resolves 4-1 P2 finding)
    assert_eq!(normalize_extension("image/png"), "png");
    assert_eq!(normalize_extension("IMAGE/PNG"), "png");
    assert_eq!(normalize_extension("Image/Png"), "png");

    assert_eq!(normalize_extension("image/jpeg"), "jpg");
    assert_eq!(normalize_extension("IMAGE/JPEG"), "jpg");
    assert_eq!(normalize_extension("Image/Jpg"), "jpg");

    assert_eq!(normalize_extension("image/gif"), "gif");
    assert_eq!(normalize_extension("IMAGE/GIF"), "gif");

    assert_eq!(normalize_extension("image/webp"), "webp");
    assert_eq!(normalize_extension("IMAGE/WEBP"), "webp");

    // Unknown media types fall through to "bin" so we never write a
    // mis-extension file to disk.
    assert_eq!(normalize_extension("application/octet-stream"), "bin");
}

// ── AC[9] (Addendum 2, 2026-04-12): Multi-turn image rehydration ─────────────
//
// The runtime half of DF-067: Story 4-3a.1 originally covered persisting
// images to disk so they survive reload, but not re-attaching them to the
// outbound API request on subsequent turns within the same session. This
// manifested as "assistant forgets the image on the next round" in a
// multi-turn vision conversation. Addendum 2 closes that gap by adding
// `rehydrate_historical_images` on the send path.
//
// Inline unit tests in `event_loop.rs::tests` cover the happy path, the
// skip-already-populated guard, the missing-file degradation, and the
// index-parity case with tool-call results. The integration test below
// exercises the **reload boundary** — the specific path that matters when
// a user restarts the app and then sends another turn on an existing
// conversation.

#[tokio::test(flavor = "multi_thread")]
async fn test_e2e_reload_then_next_turn_rehydrates() {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use rustain::domain::services::message_builder::build_api_messages;
    use rustain::infrastructure::runtime::event_loop::rehydrate_historical_images;

    let tmp = TempDir::new().unwrap();
    let sessions = tmp.path().join("sessions");
    let storage = FileSystemStorage::new(sessions.clone());
    let conv_id = "reload-boundary";

    // 1) Persist an image on turn 1 (simulates `persist_image_attachments`).
    let bytes = sample_png_bytes(0xAB);
    let image_ref = image_ref_for(&bytes, "image/png");
    storage
        .save_image(conv_id, &image_ref, &bytes)
        .await
        .unwrap();

    // 2) Build a conversation with the persisted ref on the user message
    //    and save it via the full conversation-save path (switches to the
    //    directory layout because images are present).
    let conv_saved = make_conversation(
        conv_id,
        vec![
            make_message_with_images(MessageRole::User, "what is this?", vec![image_ref.clone()]),
            make_message_with_images(MessageRole::Assistant, "a cat", vec![]),
        ],
    );
    storage.save_conversation(&conv_saved).await.unwrap();

    // 3) Reload from disk — this is the boundary the test exists to guard.
    //    `StoragePort::load_conversation` returns `Option<Conversation>`;
    //    unwrap twice: once for the storage Result, once for the Option.
    let mut conv_reloaded = storage
        .load_conversation(conv_id)
        .await
        .unwrap()
        .expect("reloaded conversation must exist");
    assert_eq!(
        conv_reloaded.messages[0].images.len(),
        1,
        "reload must preserve the ImageReference (AC4)"
    );

    // 4) Append a fresh turn-2 user message with no images (the user just
    //    types a text follow-up on a freshly reloaded session).
    conv_reloaded.messages.push(make_message_with_images(
        MessageRole::User,
        "colour?",
        vec![],
    ));

    // 5) Build the API request the way `start_turn` does on turn 2, then
    //    rehydrate. This is the code path that was broken before Addendum 2:
    //    `build_api_messages` emitted `images: vec![]` for the historical
    //    message, and nobody re-read the bytes from disk.
    let mut messages = build_api_messages(&conv_reloaded);
    rehydrate_historical_images(&conv_reloaded, &mut messages, &storage);

    // 6) Assert: the historical user message now carries the bytes in its
    //    API-level `images` vec, so the provider sees the image context on
    //    turn 2 even after a full app restart.
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0].role, MessageRole::User);
    assert_eq!(
        messages[0].images.len(),
        1,
        "reloaded historical image must be rehydrated for turn 2"
    );
    assert_eq!(messages[0].images[0].media_type, "image/png");
    assert_eq!(
        messages[0].images[0].data,
        STANDARD.encode(&bytes),
        "rehydrated base64 must match the bytes persisted on turn 1"
    );

    assert_eq!(messages[2].role, MessageRole::User);
    assert!(
        messages[2].images.is_empty(),
        "fresh turn-2 user message has no images"
    );
}
