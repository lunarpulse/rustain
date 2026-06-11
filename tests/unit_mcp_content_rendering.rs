//! Story 9.2 — non-text MCP content blocks render as bracketed placeholders.
//!
//! docs/mcp.md guarantees:
//!   * `[image: <mime>]`     for image content
//!   * `[resource: <uri>]`   for embedded resources (text + blob) and resource links
//!   * `[audio]`             for audio content
//!
//! The existing inline tests in `src/adapters/mcp/tool_projection.rs` cover
//! text and image. This file extends coverage to resource (text + blob),
//! resource_link, audio, and mixed multi-block results.
//!
//! Risk closed:
//!   * R10 — non-text content blocks render as placeholders (not panic, not raw base64)

#![cfg(feature = "mcp")]

use rmcp::model::{
    CallToolResult, Content, RawAudioContent, RawContent, RawResource, ResourceContents,
};

use rustain::adapters::mcp::tool_projection::project_rmcp_result;

fn raw_resource(uri: &str, name: &str) -> RawResource {
    RawResource {
        uri: uri.into(),
        name: name.into(),
        title: None,
        description: None,
        mime_type: None,
        size: None,
        icons: None,
        meta: None,
    }
}

#[test]
fn r10_text_resource_renders_as_bracketed_uri() {
    let result = CallToolResult::success(vec![Content::embedded_text(
        "file:///tmp/notes.md",
        "# Notes\nhello",
    )]);
    let projected = project_rmcp_result(result, "id-text-resource".into());
    assert_eq!(projected.content, "[resource: file:///tmp/notes.md]");
    assert!(!projected.is_error);
}

#[test]
fn r10_blob_resource_renders_as_bracketed_uri() {
    let blob_resource = ResourceContents::BlobResourceContents {
        uri: "data:image/png;base64,zzzz".into(),
        mime_type: Some("image/png".into()),
        blob: "zzzz".into(),
        meta: None,
    };
    let result = CallToolResult::success(vec![Content::resource(blob_resource)]);
    let projected = project_rmcp_result(result, "id-blob".into());
    assert_eq!(projected.content, "[resource: data:image/png;base64,zzzz]");
    assert!(!projected.is_error);
}

#[test]
fn r10_resource_link_renders_as_bracketed_uri() {
    let link = raw_resource("https://example.com/handbook", "Handbook");
    let result = CallToolResult::success(vec![Content::resource_link(link)]);
    let projected = project_rmcp_result(result, "id-link".into());
    assert_eq!(
        projected.content,
        "[resource: https://example.com/handbook]"
    );
}

#[test]
fn r10_audio_renders_as_bracketed_audio_marker() {
    // No `Content::audio` constructor — build the variant directly.
    let raw = RawContent::Audio(RawAudioContent {
        data: "base64-bytes".into(),
        mime_type: "audio/mpeg".into(),
    });
    let content = Content {
        raw,
        annotations: None,
    };
    let result = CallToolResult::success(vec![content]);
    let projected = project_rmcp_result(result, "id-audio".into());
    assert_eq!(projected.content, "[audio]");
}

#[test]
fn r10_mixed_blocks_concatenate_with_newlines_in_order() {
    let link = raw_resource("res://a/b", "AB");
    let result = CallToolResult::success(vec![
        Content::text("opening line"),
        Content::image("zzz", "image/png"),
        Content::embedded_text("file:///x.md", "ignored"),
        Content::resource_link(link),
        Content::text("closing line"),
    ]);
    let projected = project_rmcp_result(result, "id-mixed".into());
    // Order preserved, text inline, non-text as placeholders, joined with \n.
    assert_eq!(
        projected.content,
        "opening line\n[image: image/png]\n[resource: file:///x.md]\n[resource: res://a/b]\nclosing line"
    );
}

#[test]
fn r10_error_with_non_text_still_renders_placeholders() {
    let result = CallToolResult::error(vec![
        Content::text("upload rejected:"),
        Content::image("zzz", "image/jpeg"),
    ]);
    let projected = project_rmcp_result(result, "id-err".into());
    assert!(projected.is_error, "is_error flag must propagate");
    assert_eq!(
        projected.content, "upload rejected:\n[image: image/jpeg]",
        "error path must still produce placeholder output, not panic on non-text"
    );
}
