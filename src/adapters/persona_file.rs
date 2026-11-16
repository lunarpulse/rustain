//! Story C-2 (AD-3) — bounded, contained loader for `[persona] config = { file }`.
//!
//! The file is resolved relative to `<config_dir>/personas/`. On unix the path
//! is walked with `openat(2)` one segment at a time using `O_NOFOLLOW`, so a
//! symlink anywhere below `personas_dir` is refused (the `personas_dir` itself
//! may be a symlink; it is the trust root). The final descriptor is inspected
//! with `fstat` (no check/use race), must be a regular file, is size-bounded
//! and must be strict UTF-8. Error text never carries file content.
//!
//! Symlink rule (documented and tested): `ELOOP` on any segment is
//! `SymlinkRefused`. `ENOTDIR` on a directory segment is `SymlinkRefused` when
//! the entry is a symlink (Linux reports `ENOTDIR` for `O_DIRECTORY|O_NOFOLLOW`
//! on a symlink) and `NotRegularFile` when it is any other non-directory.

use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::domain::models::persona_config::PersonaFileConfig;

/// Largest accepted persona file, in bytes.
pub const PERSONA_FILE_MAX_BYTES: u64 = 65_536;

/// Validated persona prompt text. `Debug` never prints the body.
#[derive(Clone, PartialEq)]
pub struct PersonaText(String);

impl PersonaText {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Debug for PersonaText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PersonaText({} bytes)", self.0.len())
    }
}

/// `path` is always the joined `personas_dir/<file>` as configured, never a
/// symlink target.
#[derive(Debug, Error)]
pub enum PersonaFileError {
    #[error("persona file {} not found; create it or correct [persona] config.file", .path.display())]
    NotFound { path: PathBuf },
    #[error("persona file {} is not a regular file; point [persona] config.file at a regular file", .path.display())]
    NotRegularFile { path: PathBuf },
    #[error("persona file {} is or traverses a symlink, which is refused; use a regular file inside the personas directory", .path.display())]
    SymlinkRefused { path: PathBuf },
    #[error("persona file {} is too large ({size} bytes, max {max})", .path.display())]
    TooLarge { path: PathBuf, size: u64, max: u64 },
    #[error("persona file {} is not valid UTF-8", .path.display())]
    NotUtf8 { path: PathBuf },
    #[error("persona file {} is empty after trimming whitespace", .path.display())]
    Empty { path: PathBuf },
    #[error("persona file {} could not be read: {source}", .path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// Load the persona file named by `cfg` from `personas_dir`.
pub fn load_persona_file(
    cfg: &PersonaFileConfig,
    personas_dir: &Path,
) -> Result<PersonaText, PersonaFileError> {
    let joined = personas_dir.join(&cfg.file);
    let file = open_contained(cfg, personas_dir, &joined)?;
    finish(file, joined)
}

#[cfg(unix)]
fn open_contained(
    cfg: &PersonaFileConfig,
    personas_dir: &Path,
    joined: &Path,
) -> Result<std::fs::File, PersonaFileError> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::os::unix::ffi::OsStrExt;

    let io_err = |errno: i32| PersonaFileError::Io {
        path: joined.to_path_buf(),
        source: std::io::Error::from_raw_os_error(errno),
    };
    let last_errno = || {
        std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO)
    };

    // The trust root: it may itself be a symlink, so no O_NOFOLLOW here.
    let root_name = CString::new(personas_dir.as_os_str().as_bytes()).map_err(|_| {
        PersonaFileError::NotFound {
            path: joined.to_path_buf(),
        }
    })?;
    // SAFETY: `root_name` is a valid NUL-terminated C string that outlives the call.
    let raw = unsafe {
        libc::open(
            root_name.as_ptr(),
            libc::O_DIRECTORY | libc::O_RDONLY | libc::O_CLOEXEC,
        )
    };
    if raw < 0 {
        return Err(match last_errno() {
            libc::ENOENT | libc::ENOTDIR => PersonaFileError::NotFound {
                path: joined.to_path_buf(),
            },
            errno => io_err(errno),
        });
    }
    // SAFETY: `raw` is a freshly opened descriptor that nothing else owns.
    let mut dir = unsafe { OwnedFd::from_raw_fd(raw) };

    let openat = |dirfd: RawFd, name: &str, flags: libc::c_int| -> Result<OwnedFd, i32> {
        let c_name = CString::new(name).map_err(|_| libc::ENOENT)?;
        // SAFETY: `dirfd` is a live descriptor and `c_name` a valid C string
        // that outlives the call.
        let fd = unsafe { libc::openat(dirfd, c_name.as_ptr(), flags) };
        if fd < 0 {
            return Err(last_errno());
        }
        // SAFETY: `fd` is a freshly opened descriptor that nothing else owns.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    };
    let is_symlink = |dirfd: RawFd, name: &str| -> bool {
        let Ok(c_name) = CString::new(name) else {
            return false;
        };
        // SAFETY: an all-zero `stat` is a valid plain-data value.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: `dirfd` is live, `c_name` is a valid C string, `st` is a
        // valid writable `stat`.
        let rc =
            unsafe { libc::fstatat(dirfd, c_name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) };
        rc == 0 && (st.st_mode & libc::S_IFMT) == libc::S_IFLNK
    };

    let mut segments = cfg.segments().peekable();
    while let Some(segment) = segments.next() {
        let last = segments.peek().is_none();
        let flags = if last {
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC
        } else {
            libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_RDONLY | libc::O_CLOEXEC
        };
        let fd = openat(dir.as_raw_fd(), segment, flags).map_err(|errno| match errno {
            libc::ENOENT => PersonaFileError::NotFound {
                path: joined.to_path_buf(),
            },
            libc::ELOOP => PersonaFileError::SymlinkRefused {
                path: joined.to_path_buf(),
            },
            libc::ENOTDIR if is_symlink(dir.as_raw_fd(), segment) => {
                PersonaFileError::SymlinkRefused {
                    path: joined.to_path_buf(),
                }
            }
            libc::ENOTDIR => PersonaFileError::NotRegularFile {
                path: joined.to_path_buf(),
            },
            other => io_err(other),
        })?;
        if last {
            return Ok(std::fs::File::from(fd));
        }
        dir = fd;
    }
    // `split('/')` always yields at least one segment, so the loop returns.
    Err(PersonaFileError::NotFound {
        path: joined.to_path_buf(),
    })
}

/// Fallback without the symlink/FIFO guarantees of the unix `openat` walk.
#[cfg(not(unix))]
fn open_contained(
    _cfg: &PersonaFileConfig,
    _personas_dir: &Path,
    joined: &Path,
) -> Result<std::fs::File, PersonaFileError> {
    std::fs::File::open(joined).map_err(|source| match source.kind() {
        std::io::ErrorKind::NotFound => PersonaFileError::NotFound {
            path: joined.to_path_buf(),
        },
        _ => PersonaFileError::Io {
            path: joined.to_path_buf(),
            source,
        },
    })
}

/// Shared steps: fstat checks, bounded read, strict UTF-8, BOM, trim.
fn finish(file: std::fs::File, joined: PathBuf) -> Result<PersonaText, PersonaFileError> {
    let meta = file.metadata().map_err(|source| PersonaFileError::Io {
        path: joined.clone(),
        source,
    })?;
    if !meta.is_file() {
        return Err(PersonaFileError::NotRegularFile { path: joined });
    }
    if meta.len() > PERSONA_FILE_MAX_BYTES {
        return Err(PersonaFileError::TooLarge {
            path: joined,
            size: meta.len(),
            max: PERSONA_FILE_MAX_BYTES,
        });
    }
    let mut buf = Vec::new();
    file.take(PERSONA_FILE_MAX_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|source| PersonaFileError::Io {
            path: joined.clone(),
            source,
        })?;
    if buf.len() as u64 > PERSONA_FILE_MAX_BYTES {
        return Err(PersonaFileError::TooLarge {
            path: joined,
            size: buf.len() as u64,
            max: PERSONA_FILE_MAX_BYTES,
        });
    }
    let text = String::from_utf8(buf).map_err(|_| PersonaFileError::NotUtf8 {
        path: joined.clone(),
    })?;
    let body = text.strip_prefix('\u{feff}').unwrap_or(&text).trim_end();
    if body.is_empty() {
        return Err(PersonaFileError::Empty { path: joined });
    }
    let body = body.to_owned();
    tracing::info!(path = %joined.display(), bytes = body.len(), "persona file loaded");
    Ok(PersonaText(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CANARY: &str = "C2-CANARY-BODY";

    fn cfg(file: &str) -> PersonaFileConfig {
        PersonaFileConfig {
            file: file.to_string(),
        }
    }

    /// A personas dir `<tmp>/personas` inside a fresh tempdir.
    fn personas() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("personas");
        std::fs::create_dir(&dir).expect("mkdir personas");
        (tmp, dir)
    }

    fn assert_no_body(err: &PersonaFileError) {
        assert!(!err.to_string().contains(CANARY), "Display leaked: {err}");
        assert!(
            !format!("{err:?}").contains(CANARY),
            "Debug leaked: {err:?}"
        );
    }

    #[test]
    fn missing_file_is_not_found_with_joined_path() {
        let (_t, dir) = personas();
        let err = load_persona_file(&cfg("x.md"), &dir).unwrap_err();
        let expected = dir.join("x.md");
        match &err {
            PersonaFileError::NotFound { path } => assert_eq!(path, &expected),
            other => panic!("expected NotFound, got {other:?}"),
        }
        assert!(err.to_string().contains(&expected.display().to_string()));
    }

    #[test]
    fn missing_root_is_not_found() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("no-such-personas");
        let err = load_persona_file(&cfg("x.md"), &dir).unwrap_err();
        match err {
            PersonaFileError::NotFound { path } => assert_eq!(path, dir.join("x.md")),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn directory_named_like_the_file_is_not_regular() {
        let (_t, dir) = personas();
        std::fs::create_dir(dir.join("x.md")).expect("mkdir");
        let err = load_persona_file(&cfg("x.md"), &dir).unwrap_err();
        assert!(
            matches!(err, PersonaFileError::NotRegularFile { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn exactly_max_bytes_loads() {
        let (_t, dir) = personas();
        let body = "a".repeat(65_536);
        std::fs::write(dir.join("x.md"), &body).expect("write");
        let text = load_persona_file(&cfg("x.md"), &dir).expect("load");
        assert_eq!(text.as_str().len(), 65_536);
        assert_eq!(text.into_string(), body);
    }

    #[test]
    fn one_byte_over_max_is_too_large() {
        let (_t, dir) = personas();
        let mut body = format!("{CANARY}-1").into_bytes();
        body.resize(65_537, b'a');
        std::fs::write(dir.join("x.md"), &body).expect("write");
        let err = load_persona_file(&cfg("x.md"), &dir).unwrap_err();
        match &err {
            PersonaFileError::TooLarge { size, max, .. } => {
                assert_eq!(*size, 65_537);
                assert_eq!(*max, 65_536);
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
        assert_no_body(&err);
    }

    #[test]
    fn invalid_utf8_is_not_utf8() {
        let (_t, dir) = personas();
        let mut body = format!("{CANARY}-2 ").into_bytes();
        body.extend_from_slice(&[0xff, 0xfe, 0xfd]);
        std::fs::write(dir.join("x.md"), &body).expect("write");
        let err = load_persona_file(&cfg("x.md"), &dir).unwrap_err();
        assert!(matches!(err, PersonaFileError::NotUtf8 { .. }), "{err:?}");
        assert_no_body(&err);
    }

    #[test]
    fn whitespace_only_is_empty() {
        let (_t, dir) = personas();
        std::fs::write(dir.join("x.md"), " \r\n\t\n  \n").expect("write");
        let err = load_persona_file(&cfg("x.md"), &dir).unwrap_err();
        assert!(matches!(err, PersonaFileError::Empty { .. }), "{err:?}");
    }

    #[test]
    fn bom_only_is_empty() {
        let (_t, dir) = personas();
        std::fs::write(dir.join("x.md"), "\u{feff}\n").expect("write");
        let err = load_persona_file(&cfg("x.md"), &dir).unwrap_err();
        assert!(matches!(err, PersonaFileError::Empty { .. }), "{err:?}");
    }

    #[test]
    fn bom_is_stripped_once() {
        let (_t, dir) = personas();
        std::fs::write(dir.join("x.md"), "\u{feff}\u{feff}You are ops.").expect("write");
        let text = load_persona_file(&cfg("x.md"), &dir).expect("load");
        assert_eq!(text.as_str(), "\u{feff}You are ops.");
    }

    #[test]
    fn trailing_whitespace_trimmed_and_inner_crlf_kept() {
        let (_t, dir) = personas();
        std::fs::write(dir.join("x.md"), "line one\r\nline two\r\n\n\n  \n").expect("write");
        let text = load_persona_file(&cfg("x.md"), &dir).expect("load");
        assert_eq!(text.as_str(), "line one\r\nline two");
    }

    #[test]
    fn persona_text_debug_hides_body() {
        let text = PersonaText(format!("{CANARY}-3"));
        let dbg = format!("{text:?}");
        assert!(!dbg.contains(CANARY), "{dbg}");
        assert_eq!(dbg, "PersonaText(16 bytes)");
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::os::unix::fs::symlink;
        use std::sync::mpsc;
        use std::time::Duration;

        #[test]
        fn fifo_is_not_regular_and_returns_promptly() {
            use std::os::unix::ffi::OsStrExt;
            let (_t, dir) = personas();
            let fifo = dir.join("x.md");
            let c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).expect("cstring");
            // SAFETY: `c` is a valid NUL-terminated path that outlives the call.
            let rc = unsafe { libc::mkfifo(c.as_ptr(), 0o600) };
            assert_eq!(rc, 0, "mkfifo failed");
            let (tx, rx) = mpsc::channel();
            let dir2 = dir.clone();
            std::thread::spawn(move || {
                let _ = tx.send(load_persona_file(&cfg("x.md"), &dir2));
            });
            let result = rx
                .recv_timeout(Duration::from_secs(2))
                .expect("FIFO load must return within 2s");
            assert!(
                matches!(result, Err(PersonaFileError::NotRegularFile { .. })),
                "{result:?}"
            );
        }

        #[test]
        fn symlink_to_outside_file_is_refused_without_leaks() {
            let (_t, dir) = personas();
            let outside = tempfile::tempdir().expect("outside");
            let secret = outside.path().join("secret.md");
            std::fs::write(&secret, format!("{CANARY}-4")).expect("write");
            symlink(&secret, dir.join("x.md")).expect("symlink");
            let err = load_persona_file(&cfg("x.md"), &dir).unwrap_err();
            assert!(
                matches!(err, PersonaFileError::SymlinkRefused { .. }),
                "{err:?}"
            );
            assert_no_body(&err);
            let outside_str = outside.path().display().to_string();
            assert!(!err.to_string().contains(&outside_str));
            assert!(!format!("{err:?}").contains(&outside_str));
            match err {
                PersonaFileError::SymlinkRefused { path } => assert_eq!(path, dir.join("x.md")),
                other => panic!("unexpected {other:?}"),
            }
        }

        #[test]
        fn symlink_to_inside_file_is_still_refused() {
            let (_t, dir) = personas();
            std::fs::create_dir(dir.join("team")).expect("mkdir");
            std::fs::write(dir.join("team/x.md"), format!("{CANARY}-5")).expect("write");
            symlink(dir.join("team/x.md"), dir.join("x.md")).expect("symlink");
            let err = load_persona_file(&cfg("x.md"), &dir).unwrap_err();
            assert!(
                matches!(err, PersonaFileError::SymlinkRefused { .. }),
                "{err:?}"
            );
            assert_no_body(&err);
        }

        #[test]
        fn symlinked_subdirectory_is_refused() {
            let (_t, dir) = personas();
            let outside = tempfile::tempdir().expect("outside");
            std::fs::write(outside.path().join("x.md"), format!("{CANARY}-6")).expect("write");
            symlink(outside.path(), dir.join("team")).expect("symlink");
            let err = load_persona_file(&cfg("team/x.md"), &dir).unwrap_err();
            assert!(
                matches!(err, PersonaFileError::SymlinkRefused { .. }),
                "{err:?}"
            );
            assert_no_body(&err);
            assert!(
                !err.to_string()
                    .contains(&outside.path().display().to_string())
            );
        }

        #[test]
        fn regular_file_used_as_directory_segment_is_not_regular() {
            let (_t, dir) = personas();
            std::fs::write(dir.join("team"), format!("{CANARY}-7")).expect("write");
            let err = load_persona_file(&cfg("team/x.md"), &dir).unwrap_err();
            assert!(
                matches!(err, PersonaFileError::NotRegularFile { .. }),
                "{err:?}"
            );
            assert_no_body(&err);
        }

        #[test]
        fn personas_dir_itself_may_be_a_symlink() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let real = tmp.path().join("real");
            std::fs::create_dir(&real).expect("mkdir");
            std::fs::write(real.join("x.md"), "You are ops.\n").expect("write");
            let link = tmp.path().join("personas");
            symlink(&real, &link).expect("symlink");
            let text = load_persona_file(&cfg("x.md"), &link).expect("load");
            assert_eq!(text.as_str(), "You are ops.");
        }

        #[test]
        fn real_subdirectory_loads() {
            let (_t, dir) = personas();
            std::fs::create_dir(dir.join("team")).expect("mkdir");
            std::fs::write(dir.join("team/x.md"), "Team persona\n").expect("write");
            let text = load_persona_file(&cfg("team/x.md"), &dir).expect("load");
            assert_eq!(text.as_str(), "Team persona");
        }
    }
}
