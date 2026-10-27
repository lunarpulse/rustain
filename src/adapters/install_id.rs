//! Shared installation identity, adapted from oh-my-pi.
//!
//! MIT License
//!
//! Copyright (c) 2025 Mario Zechner
//! Copyright (c) 2025-2026 Can Bölük
//! Copyright (c) 2026 Stencil Labs, Inc.
//!
//! Permission is hereby granted, free of charge, to any person obtaining a copy
//! of this software and associated documentation files (the "Software"), to deal
//! in the Software without restriction, including without limitation the rights
//! to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
//! copies of the Software, and to permit persons to whom the Software is
//! furnished to do so, subject to the following conditions:
//!
//! The above copyright notice and this permission notice shall be included in all
//! copies or substantial portions of the Software.
//!
//! THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
//! IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
//! FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
//! AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
//! LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
//! OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
//! SOFTWARE.

use crate::domain::errors::ProviderError;
use rand_core::{OsRng, RngCore};
use std::io::{Read, Seek, Write};

pub fn uuid() -> String {
    let mut bytes = [0; 16];
    OsRng.fill_bytes(&mut bytes);
    bytes[6] = (bytes[6] & 15) | 0x40;
    bytes[8] = (bytes[8] & 63) | 0x80;
    let h = hex::encode(bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

pub fn install_id() -> Result<String, ProviderError> {
    let path = crate::infrastructure::paths::data_dir()
        .map_err(|_| identity_error())?
        .join("install-id");
    let mut file = std::fs::OpenOptions::new();
    file.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        file.mode(0o600);
    }
    let mut file = file.open(path).map_err(|_| identity_error())?;
    // One cross-process lock fences creation; no cached path survives test isolation.
    file.lock().map_err(|_| identity_error())?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(|_| identity_error())?;
    if let Some(id) = std::str::from_utf8(&bytes)
        .ok()
        .map(str::trim)
        .filter(|id| is_uuid(id))
    {
        return Ok(id.to_owned());
    }
    // An empty, partial or foreign file is replaced rather than failing every request.
    let id = uuid();
    file.set_len(0).map_err(|_| identity_error())?;
    file.rewind().map_err(|_| identity_error())?;
    file.write_all(id.as_bytes())
        .map_err(|_| identity_error())?;
    file.sync_all().map_err(|_| identity_error())?;
    Ok(id)
}

fn is_uuid(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_digit() || (b'a'..=b'f').contains(&b),
        })
}

fn identity_error() -> ProviderError {
    // Keep the existing Claude failure contract when sharing its storage implementation.
    ProviderError::Other("Could not initialize Claude Code identity".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_uuid_is_random_version_four() {
        let first = uuid();
        let second = uuid();
        assert!(is_uuid(&first));
        assert!(is_uuid(&second));
        assert_eq!(&first[14..15], "4");
        assert!(matches!(&first[19..20], "8" | "9" | "a" | "b"));
        assert_ne!(first, second);
    }
}
