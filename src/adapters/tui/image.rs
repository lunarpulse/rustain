//! Image format detection, validation, and indicator formatting.
//! Adapter layer — uses std::fs for file reads, base64 for encoding.
// Covers: FR112 (image attachment)

/// Supported image media types.
#[allow(dead_code)] // used for validation in future story; defined here for discoverability
pub const SUPPORTED_MEDIA_TYPES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];

/// Size threshold for large image warning (5MB in base64-encoded bytes).
const LARGE_IMAGE_THRESHOLD: usize = 5 * 1024 * 1024;

/// Detect image format from raw bytes by checking magic bytes.
/// Returns the media type string on success, or an error message for unsupported formats.
pub fn detect_image_format(data: &[u8]) -> Result<&'static str, String> {
    if data.len() < 4 {
        return Err("Data too short to identify image format".to_string());
    }

    // PNG: \x89PNG
    if data.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        return Ok("image/png");
    }

    // JPEG: \xFF\xD8\xFF
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Ok("image/jpeg");
    }

    // GIF: GIF8 (GIF87a or GIF89a)
    if data.starts_with(b"GIF8") {
        return Ok("image/gif");
    }

    // WebP: RIFF....WEBP (bytes 0-3 = "RIFF", bytes 8-11 = "WEBP")
    if data.len() >= 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP" {
        return Ok("image/webp");
    }

    Err("Unsupported image format. Supported: PNG, JPEG, GIF, WebP".to_string())
}

/// Check if a file extension corresponds to a supported image format.
pub fn is_image_extension(ext: &str) -> bool {
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp"
    )
}

/// Validate base64-encoded image size. Returns a warning message if over threshold.
pub fn validate_image_size(base64_len: usize) -> Option<String> {
    if base64_len > LARGE_IMAGE_THRESHOLD {
        let size_mb = base64_len as f64 / (1024.0 * 1024.0);
        Some(format!(
            "Large image ({:.1}MB). This will consume significant context. Attach anyway? [y/n]",
            size_mb
        ))
    } else {
        None
    }
}

/// Format the image indicator text for the input box.
pub fn format_image_indicator(count: usize, total_kb: usize) -> String {
    if count == 1 {
        format!("[image attached: {}KB]", total_kb)
    } else {
        format!("[{} images attached: {}KB total]", count, total_kb)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- detect_image_format tests ---

    #[test]
    fn detect_png_magic_bytes() {
        let data = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        assert_eq!(detect_image_format(&data), Ok("image/png"));
    }

    #[test]
    fn detect_jpeg_magic_bytes() {
        let data = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        assert_eq!(detect_image_format(&data), Ok("image/jpeg"));
    }

    #[test]
    fn detect_gif_magic_bytes() {
        let data = b"GIF89a\x01\x00\x01\x00";
        assert_eq!(detect_image_format(data), Ok("image/gif"));
    }

    #[test]
    fn detect_webp_magic_bytes() {
        let mut data = Vec::new();
        data.extend_from_slice(b"RIFF");
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // file size placeholder
        data.extend_from_slice(b"WEBP");
        assert_eq!(detect_image_format(&data), Ok("image/webp"));
    }

    #[test]
    fn reject_unsupported_format() {
        // BMP magic bytes
        let data = [0x42, 0x4D, 0x00, 0x00, 0x00, 0x00];
        assert!(detect_image_format(&data).is_err());
        assert!(
            detect_image_format(&data)
                .unwrap_err()
                .contains("Unsupported image format")
        );
    }

    #[test]
    fn reject_data_too_short() {
        let data = [0x89, 0x50];
        assert!(detect_image_format(&data).is_err());
    }

    // --- validate_image_size tests ---

    #[test]
    fn size_under_threshold_returns_none() {
        assert!(validate_image_size(1024 * 1024).is_none()); // 1MB
    }

    #[test]
    fn size_at_threshold_returns_none() {
        assert!(validate_image_size(5 * 1024 * 1024).is_none()); // exactly 5MB
    }

    #[test]
    fn size_over_threshold_returns_warning() {
        let warning = validate_image_size(8 * 1024 * 1024 + 200 * 1024);
        assert!(warning.is_some());
        assert!(warning.unwrap().contains("Large image"));
    }

    // --- format_image_indicator tests ---

    #[test]
    fn single_image_indicator() {
        assert_eq!(format_image_indicator(1, 245), "[image attached: 245KB]");
    }

    #[test]
    fn multiple_images_indicator() {
        assert_eq!(
            format_image_indicator(3, 789),
            "[3 images attached: 789KB total]"
        );
    }

    // --- is_image_extension tests ---

    #[test]
    fn image_extensions_recognized() {
        assert!(is_image_extension("png"));
        assert!(is_image_extension("PNG"));
        assert!(is_image_extension("jpg"));
        assert!(is_image_extension("jpeg"));
        assert!(is_image_extension("gif"));
        assert!(is_image_extension("webp"));
    }

    #[test]
    fn non_image_extensions_rejected() {
        assert!(!is_image_extension("svg"));
        assert!(!is_image_extension("bmp"));
        assert!(!is_image_extension("tiff"));
        assert!(!is_image_extension("txt"));
        assert!(!is_image_extension("rs"));
    }
}
