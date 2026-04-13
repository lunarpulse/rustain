//! System clipboard adapters.
//!
//! `ArboardClipboard` reads images and text from the OS clipboard via the
//! `arboard` crate (pure-Rust X11/Wayland/macOS/Win32 bindings — no xclip or
//! wl-paste binaries invoked at runtime). Only compiled when `feature = "clipboard"`.
//!
//! `NoOpClipboard` always returns `Ok(None)` and is used in headless / test builds.

use async_trait::async_trait;

use crate::domain::errors::ClipboardError;
use crate::domain::ports::ClipboardPort;

// ── ArboardClipboard ────────────────────────────────────────────────────────

/// Clipboard adapter backed by the `arboard` crate.
///
/// Each `read_*` call constructs a fresh `arboard::Clipboard` inside
/// `spawn_blocking` — the `Clipboard` type is not `Send` on X11 (it holds an
/// `Arc<Context>` tied to a background thread), so we never hold one across
/// an `.await` point.
///
/// A 500 ms timeout guards against a compositor that never responds (e.g.
/// screen-locked GNOME Wayland).
#[cfg(feature = "clipboard")]
#[derive(Default)]
pub struct ArboardClipboard;

#[cfg(feature = "clipboard")]
impl ArboardClipboard {
    pub fn new() -> Self {
        Self
    }
}

#[cfg(feature = "clipboard")]
#[async_trait]
impl ClipboardPort for ArboardClipboard {
    async fn read_image_png(&self) -> Result<Option<Vec<u8>>, ClipboardError> {
        use std::time::Duration;

        let result = tokio::time::timeout(
            Duration::from_millis(500),
            tokio::task::spawn_blocking(|| -> Result<Option<Vec<u8>>, ClipboardError> {
                let mut cb = arboard::Clipboard::new()
                    .map_err(|e| ClipboardError::Backend(e.to_string()))?;
                match cb.get_image() {
                    Ok(img) => {
                        let width = img.width;
                        let height = img.height;
                        let bytes: Vec<u8> = img.bytes.into_owned();
                        // Validate RGBA: must be exactly width * height * 4 bytes
                        let expected = width.saturating_mul(height).saturating_mul(4);
                        if bytes.len() != expected {
                            return Err(ClipboardError::Backend(format!(
                                "unexpected image data length {} (expected {}x{}x4={})",
                                bytes.len(),
                                width,
                                height,
                                expected
                            )));
                        }
                        encode_rgba_to_png(width as u32, height as u32, &bytes).map(Some)
                    }
                    Err(arboard::Error::ContentNotAvailable) => Ok(None),
                    // ContentNotAvailable is the canonical "no image" signal.
                    // Any other error is a genuine backend failure.
                    Err(e) => Err(ClipboardError::Backend(e.to_string())),
                }
            }),
        )
        .await;

        match result {
            Err(_elapsed) => Err(ClipboardError::Timeout),
            Ok(Err(join_err)) => Err(ClipboardError::Backend(format!(
                "task panicked: {join_err}"
            ))),
            Ok(Ok(inner)) => inner,
        }
    }

    async fn read_text(&self) -> Result<Option<String>, ClipboardError> {
        use std::time::Duration;

        let result = tokio::time::timeout(
            Duration::from_millis(500),
            tokio::task::spawn_blocking(|| -> Result<Option<String>, ClipboardError> {
                let mut cb = arboard::Clipboard::new()
                    .map_err(|e| ClipboardError::Backend(e.to_string()))?;
                match cb.get_text() {
                    Ok(text) => Ok(Some(text)),
                    Err(arboard::Error::ContentNotAvailable) => Ok(None),
                    Err(e) => Err(ClipboardError::Backend(e.to_string())),
                }
            }),
        )
        .await;

        match result {
            Err(_elapsed) => Err(ClipboardError::Timeout),
            Ok(Err(join_err)) => Err(ClipboardError::Backend(format!(
                "task panicked: {join_err}"
            ))),
            Ok(Ok(inner)) => inner,
        }
    }
}

/// Encode raw RGBA pixels to in-memory PNG bytes using the `png` crate.
#[cfg(feature = "clipboard")]
fn encode_rgba_to_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, ClipboardError> {
    let mut buf: Vec<u8> = Vec::with_capacity(rgba.len() / 4 + 256);
    let mut encoder = png::Encoder::new(&mut buf, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|e| ClipboardError::Backend(e.to_string()))?;
    writer
        .write_image_data(rgba)
        .map_err(|e| ClipboardError::Backend(e.to_string()))?;
    drop(writer); // flush PNG footer before returning buf
    Ok(buf)
}

// ── NoOpClipboard ────────────────────────────────────────────────────────────

/// Clipboard stub that always returns `Ok(None)`.
/// Used in tests and headless CI builds where no display server is available.
#[derive(Debug, Default)]
pub struct NoOpClipboard;

#[allow(dead_code)]
impl NoOpClipboard {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ClipboardPort for NoOpClipboard {
    async fn read_image_png(&self) -> Result<Option<Vec<u8>>, ClipboardError> {
        Ok(None)
    }

    async fn read_text(&self) -> Result<Option<String>, ClipboardError> {
        Ok(None)
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn noop_image_returns_none() {
        let cb = NoOpClipboard::new();
        assert!(cb.read_image_png().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn noop_text_returns_none() {
        let cb = NoOpClipboard::new();
        assert!(cb.read_text().await.unwrap().is_none());
    }

    /// Round-trip: 2×2 RGBA → PNG → detect_image_format returns "image/png".
    #[cfg(feature = "clipboard")]
    #[test]
    fn encode_rgba_to_png_roundtrip() {
        // 2×2 RGBA pixels (8 bytes × 4 = 32 bytes)
        let rgba: Vec<u8> = vec![
            255, 0, 0, 255, // red
            0, 255, 0, 255, // green
            0, 0, 255, 255, // blue
            255, 255, 0, 255, // yellow
        ];
        let png_bytes = encode_rgba_to_png(2, 2, &rgba).expect("encode should succeed");
        // PNG magic: \x89PNG\r\n\x1a\n
        assert_eq!(&png_bytes[..8], b"\x89PNG\r\n\x1a\n");
        // Should be recognized as image/png by the existing format detector
        let detected = crate::adapters::tui::image::detect_image_format(&png_bytes);
        assert_eq!(detected, Ok("image/png"));
    }

    /// arboard adapter is only tested interactively (requires a display server).
    /// Run with: cargo test -- --ignored clipboard_arboard
    #[cfg(feature = "clipboard")]
    #[tokio::test]
    #[ignore = "requires display server — run manually"]
    async fn clipboard_arboard_smoke() {
        let cb = ArboardClipboard::new();
        // We can't assert on the result without controlling the clipboard,
        // but this confirms the adapter doesn't panic on construction.
        let _ = cb.read_image_png().await;
        let _ = cb.read_text().await;
    }
}
