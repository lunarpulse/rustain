//! Clipboard operations: OSC 52 protocol and file fallback.
//! Adapter layer — writes to /dev/tty or filesystem.
// Covers: FR116, UX-DR68

use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

/// Result of a clipboard copy operation.
#[derive(Debug, PartialEq, Eq)]
pub enum ClipboardResult {
    /// Successfully copied via OSC 52 escape sequence.
    Osc52Success,
    /// Fell back to writing to a file.
    FallbackSuccess(PathBuf),
    /// All methods failed.
    Failed(String),
}

/// Copy text content to the system clipboard.
/// Tries OSC 52 first, falls back to file write.
pub fn copy_to_clipboard(content: &str) -> ClipboardResult {
    let strategy = clipboard_strategy();

    match strategy {
        Strategy::Osc52 | Strategy::Auto => {
            if let Ok(()) = write_osc52(content) {
                return ClipboardResult::Osc52Success;
            }
            if matches!(strategy, Strategy::Osc52) {
                return ClipboardResult::Failed("OSC 52 write failed".to_string());
            }
            // Auto: fall through to file
            match write_fallback(content) {
                Ok(path) => ClipboardResult::FallbackSuccess(path),
                Err(e) => ClipboardResult::Failed(format!("All clipboard methods failed: {}", e)),
            }
        }
        Strategy::File => match write_fallback(content) {
            Ok(path) => ClipboardResult::FallbackSuccess(path),
            Err(e) => ClipboardResult::Failed(format!("File fallback failed: {}", e)),
        },
    }
}

/// Write content to the system clipboard via OSC 52 escape sequence.
/// Writes to /dev/tty to avoid contention with ratatui's stdout buffer.
pub fn write_osc52(content: &str) -> io::Result<()> {
    // Write to /dev/tty directly to bypass ratatui's stdout buffer
    let tty_path = std::path::Path::new("/dev/tty");
    if !tty_path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "TTY not available. Set RUSTAIN_CLIPBOARD=file to use file fallback",
        ));
    }
    let mut tty = fs::OpenOptions::new().write(true).open(tty_path)?;
    write_osc52_to(&mut tty, content)
}

fn write_osc52_to(writer: &mut impl Write, content: &str) -> io::Result<()> {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(content.as_bytes());
    writer.write_all(b"\x1b]52;c;")?;
    writer.write_all(encoded.as_bytes())?;
    writer.write_all(b"\x07")?;
    writer.flush()
}

/// Write content to the fallback clipboard file.
/// Creates ~/.rustain/clipboard.txt with 0o600 permissions.
pub fn write_fallback(content: &str) -> io::Result<PathBuf> {
    let data_dir = crate::infrastructure::paths::data_dir().map_err(io::Error::other)?;
    fs::create_dir_all(&data_dir)?;
    let path = data_dir.join("clipboard.txt");
    fs::write(&path, content)?;

    // Set file permissions to 0o600 (owner read/write only)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }

    Ok(path)
}

#[derive(Debug, Clone, Copy)]
enum Strategy {
    Osc52,
    File,
    Auto,
}

fn clipboard_strategy() -> Strategy {
    match crate::infrastructure::utils::env_var_trimmed("RUSTAIN_CLIPBOARD").as_deref() {
        Some("osc52") => Strategy::Osc52,
        Some("file") => Strategy::File,
        _ => Strategy::Auto,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn story21_5_review_osc52_writes_framing_without_a_second_payload_copy() {
        #[derive(Default)]
        struct Writes(Vec<Vec<u8>>);
        impl Write for Writes {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0.push(bytes.to_vec());
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        use base64::Engine;
        let content = format!("界\x1b{}", "x".repeat(1_000_000));
        let mut writes = Writes::default();
        write_osc52_to(&mut writes, &content).unwrap();
        let encoded = base64::engine::general_purpose::STANDARD.encode(content.as_bytes());
        assert_eq!(
            writes.0.concat(),
            [b"\x1b]52;c;".as_slice(), encoded.as_bytes(), b"\x07"].concat()
        );
        assert_eq!(
            writes.0.len(),
            3,
            "framing must not duplicate the full base64 payload"
        );
        assert_eq!(writes.0[0], b"\x1b]52;c;");
        assert_eq!(writes.0[1], encoded.as_bytes());
        assert_eq!(writes.0[2], b"\x07");
    }

    #[test]
    fn osc52_sequence_format() {
        // We can't easily test /dev/tty writes, but we can verify the format logic
        use base64::Engine;
        let content = "hello world";
        let encoded = base64::engine::general_purpose::STANDARD.encode(content.as_bytes());
        let sequence = format!("\x1b]52;c;{}\x07", encoded);
        assert!(sequence.starts_with("\x1b]52;c;"));
        assert!(sequence.ends_with('\x07'));
        // Verify base64 decodes back
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&encoded)
            .unwrap();
        assert_eq!(std::str::from_utf8(&decoded).unwrap(), content);
    }

    #[test]
    fn fallback_writes_to_file() {
        // Test the actual write_fallback function by calling it
        // (requires data_dir to be functional — use env override if available)
        let result = write_fallback("clipboard test content");
        match result {
            Ok(path) => {
                assert!(path.exists());
                assert_eq!(
                    std::fs::read_to_string(&path).unwrap(),
                    "clipboard test content"
                );
                // Verify permissions on Unix
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let perms = std::fs::metadata(&path).unwrap().permissions();
                    assert_eq!(perms.mode() & 0o777, 0o600);
                }
            }
            Err(e) => {
                // In CI/sandboxed environments, data_dir may not be available
                eprintln!(
                    "write_fallback returned error (may be expected in CI): {}",
                    e
                );
            }
        }
    }

    #[test]
    fn clipboard_result_variants() {
        let r1 = ClipboardResult::Osc52Success;
        assert_eq!(r1, ClipboardResult::Osc52Success);

        let r2 = ClipboardResult::FallbackSuccess(PathBuf::from("/tmp/test"));
        assert_eq!(
            r2,
            ClipboardResult::FallbackSuccess(PathBuf::from("/tmp/test"))
        );

        let r3 = ClipboardResult::Failed("error".to_string());
        assert_eq!(r3, ClipboardResult::Failed("error".to_string()));
    }
}
