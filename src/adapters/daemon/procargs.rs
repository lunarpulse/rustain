//! Pure PROCARGS2 parser (Story 12-1d AC-12-1d-3, Task 3a).
//!
//! Parses the macOS `KERN_PROCARGS2` sysctl buffer into its components. The module
//! is **cfg-free** — compiled and tested on all targets so the byte-built fixture
//! corpus runs on Linux CI every PR. The unsafe sysctl wrapper lives in `pidfile.rs`
//! (`cfg(target_os = "macos")` only); this module is pure parsing, no unsafe.
//!
//! Format (reference: Apple `adv_cmds` `ps` source):
//!   `argc: i32` (LE) | exec_path (NUL-terminated) | variable NUL padding |
//!   argv[0..argc] (NUL-separated) | env key=val pairs (NUL-separated to buffer end)

/// Parsed result from a PROCARGS2 buffer.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcArgs {
    pub exec_path: String,
    pub argv: Vec<String>,
    pub env: Vec<String>,
}

/// Parse a raw `KERN_PROCARGS2` buffer. Returns `None` on any malformation.
pub fn parse_procargs2(buf: &[u8]) -> Option<ProcArgs> {
    if buf.len() < 4 {
        return None;
    }

    let argc = i32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if argc < 0 {
        return None;
    }
    let argc = argc as usize;

    let rest = &buf[4..];

    // exec_path: NUL-terminated string starting at offset 4.
    let exec_end = rest.iter().position(|&b| b == 0)?;
    let exec_path = String::from_utf8_lossy(&rest[..exec_end]).into_owned();

    // Skip past exec_path's NUL terminator and any additional NUL padding.
    let mut pos = exec_end;
    while pos < rest.len() && rest[pos] == 0 {
        pos += 1;
    }

    // Parse argc argv entries (NUL-separated).
    let mut argv = Vec::with_capacity(argc);
    for _ in 0..argc {
        if pos >= rest.len() {
            return None;
        }
        let start = pos;
        while pos < rest.len() && rest[pos] != 0 {
            pos += 1;
        }
        argv.push(String::from_utf8_lossy(&rest[start..pos]).into_owned());
        if pos < rest.len() {
            pos += 1; // skip NUL terminator
        }
    }

    // Remaining NUL-separated entries are environment variables.
    let mut env = Vec::new();
    while pos < rest.len() {
        let start = pos;
        while pos < rest.len() && rest[pos] != 0 {
            pos += 1;
        }
        if start < pos {
            env.push(String::from_utf8_lossy(&rest[start..pos]).into_owned());
        }
        if pos < rest.len() {
            pos += 1;
        }
    }

    Some(ProcArgs {
        exec_path,
        argv,
        env,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_procargs2(
        argc: i32,
        exec: &[u8],
        padding: usize,
        argv: &[&[u8]],
        env: &[&[u8]],
    ) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&argc.to_le_bytes());
        buf.extend_from_slice(exec);
        buf.push(0); // NUL-terminate exec_path
        buf.extend(std::iter::repeat(0).take(padding));
        for _arg in argv.iter() {
            buf.extend_from_slice(_arg);
            buf.push(0);
        }
        for (i, e) in env.iter().enumerate() {
            buf.extend_from_slice(e);
            if i < env.len() - 1 {
                buf.push(0);
            }
        }
        buf
    }

    #[test]
    fn basic_parse() {
        let buf = build_procargs2(
            2,
            b"/usr/bin/rustain",
            3,
            &[b"rustain", b"daemon"],
            &[b"HOME=/root", b"PATH=/usr/bin"],
        );
        let pa = parse_procargs2(&buf).unwrap();
        assert_eq!(pa.exec_path, "/usr/bin/rustain");
        assert_eq!(pa.argv, vec!["rustain", "daemon"]);
        assert_eq!(pa.env, vec!["HOME=/root", "PATH=/usr/bin"]);
    }

    #[test]
    fn zero_padding() {
        let buf = build_procargs2(1, b"/bin/sh", 0, &[b"sh"], &[]);
        let pa = parse_procargs2(&buf).unwrap();
        assert_eq!(pa.exec_path, "/bin/sh");
        assert_eq!(pa.argv, vec!["sh"]);
        assert!(pa.env.is_empty());
    }

    #[test]
    fn large_padding() {
        let buf = build_procargs2(1, b"/bin/ls", 64, &[b"ls"], &[b"TERM=xterm"]);
        let pa = parse_procargs2(&buf).unwrap();
        assert_eq!(pa.exec_path, "/bin/ls");
        assert_eq!(pa.argv, vec!["ls"]);
        assert_eq!(pa.env, vec!["TERM=xterm"]);
    }

    #[test]
    fn argc_zero() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&0i32.to_le_bytes());
        buf.extend_from_slice(b"/usr/bin/daemon");
        buf.push(0);
        buf.push(0); // padding
        let pa = parse_procargs2(&buf).unwrap();
        assert_eq!(pa.exec_path, "/usr/bin/daemon");
        assert!(pa.argv.is_empty());
    }

    #[test]
    fn truncated_buffer_too_short() {
        assert!(parse_procargs2(&[]).is_none());
        assert!(parse_procargs2(&[1, 0, 0]).is_none());
    }

    #[test]
    fn truncated_buffer_missing_argv() {
        // argc=2 but only 1 arg present before buffer ends.
        let buf = build_procargs2(2, b"/bin/x", 0, &[b"x"], &[]);
        // The second argv entry is missing — should return None.
        assert!(parse_procargs2(&buf).is_none());
    }

    #[test]
    fn env_present_with_multiple_entries() {
        let buf = build_procargs2(1, b"/a", 1, &[b"a"], &[b"K1=V1", b"K2=V2", b"K3=V3"]);
        let pa = parse_procargs2(&buf).unwrap();
        assert_eq!(pa.argv, vec!["a"]);
        assert_eq!(pa.env, vec!["K1=V1", "K2=V2", "K3=V3"]);
    }

    #[test]
    fn env_absent() {
        let buf = build_procargs2(2, b"/usr/bin/cat", 2, &[b"cat", b"file.txt"], &[]);
        let pa = parse_procargs2(&buf).unwrap();
        assert_eq!(pa.argv, vec!["cat", "file.txt"]);
        assert!(pa.env.is_empty());
    }

    #[test]
    fn embedded_nul_in_exec_path_truncates() {
        // exec_path with an embedded NUL: the parser sees the first NUL as terminator.
        let mut buf = Vec::new();
        buf.extend_from_slice(&1i32.to_le_bytes());
        buf.extend_from_slice(b"/bin/da");
        buf.push(0); // embedded NUL (treated as exec end)
        buf.extend_from_slice(b"emon");
        buf.push(0); // real end
        buf.push(0); // padding
        buf.extend_from_slice(b"arg0");
        buf.push(0);
        let pa = parse_procargs2(&buf).unwrap();
        assert_eq!(pa.exec_path, "/bin/da");
    }

    #[test]
    fn non_utf8_bytes_in_argv() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&1i32.to_le_bytes());
        buf.extend_from_slice(b"/bin/x");
        buf.push(0);
        // argv[0] with non-UTF8 byte 0xFF
        buf.extend_from_slice(&[0xFF, 0xFE, b'a']);
        buf.push(0);
        let pa = parse_procargs2(&buf).unwrap();
        assert_eq!(pa.argv.len(), 1);
        assert!(pa.argv[0].contains('\u{FFFD}')); // replacement character
    }

    #[test]
    fn negative_argc_returns_none() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(-1i32).to_le_bytes());
        buf.extend_from_slice(b"/bin/x");
        buf.push(0);
        assert!(parse_procargs2(&buf).is_none());
    }
}
