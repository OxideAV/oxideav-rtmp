//! Shared plumbing for the live black-box interop tests.
//!
//! Everything here binds 127.0.0.1 on OS-assigned ports, bounds every
//! wait with a hard timeout, and only ever kills child processes it
//! spawned itself (through the owned [`std::process::Child`] handle).
//! When the black-box tool is not installed every test using this
//! module skips at runtime.

#![allow(dead_code)]

use std::fs;
use std::io::Read;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// Executable name of the black-box RTMP / FLV tool.
pub const TOOL: &str = "ffmpeg";

/// FLV / RTMP message type ids.
pub const KIND_AUDIO: u8 = 8;
pub const KIND_VIDEO: u8 = 9;
pub const KIND_SCRIPT: u8 = 18;

/// `true` when the black-box tool can be executed on this machine and
/// the interop tests have not been disabled through the environment.
pub fn tool_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        if std::env::var_os("OXIDEAV_RTMP_SKIP_INTEROP").is_some() {
            return false;
        }
        Command::new(TOOL)
            .arg("-version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

/// A scratch directory under the system temp dir, removed on drop.
pub struct Scratch {
    path: PathBuf,
}

impl Scratch {
    pub fn new(label: &str) -> Self {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "oxideav-rtmp-interop-{}-{}-{}",
            std::process::id(),
            n,
            label
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create scratch dir");
        Scratch { path }
    }

    pub fn file(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// A child process of the black-box tool. Killed (by its own handle)
/// and reaped on drop, so a failing test never leaves it behind.
pub struct Proc {
    child: Child,
    log: PathBuf,
}

impl Proc {
    /// Spawn the tool with `args`; stderr goes to `log`.
    pub fn spawn(args: &[String], log: &Path) -> Proc {
        let log_file = fs::File::create(log).expect("create tool log");
        let child = Command::new(TOOL)
            .args(["-hide_banner", "-nostdin", "-loglevel", "warning", "-y"])
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log_file))
            .spawn()
            .expect("spawn black-box tool");
        Proc {
            child,
            log: log.to_path_buf(),
        }
    }

    /// Wait for the process to exit; `None` when `timeout` elapses
    /// first (the process is left running — drop kills it).
    pub fn wait(&mut self, timeout: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) => {}
                Err(_) => return None,
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(15));
        }
    }

    /// Hard-kill the process (models an abrupt peer disconnect).
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Whatever the tool wrote to stderr so far.
    pub fn log(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Run the tool to completion. `Ok(log)` on a zero exit status,
/// `Err(log)` on failure or timeout.
pub fn run_tool(
    args: &[String],
    log: &Path,
    timeout: Duration,
) -> std::result::Result<String, String> {
    let mut p = Proc::spawn(args, log);
    match p.wait(timeout) {
        Some(st) if st.success() => Ok(p.log()),
        Some(st) => Err(format!("exit {st}: {}", p.log())),
        None => {
            p.kill();
            Err(format!("timeout: {}", p.log()))
        }
    }
}

/// Split a whitespace-separated argument string.
pub fn args(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_string).collect()
}

/// Reserve a loopback port for a tool that must be told its listen
/// port: bind port 0, read the OS-assigned number, release it.
pub fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind probe");
    l.local_addr().expect("probe addr").port()
}

/// One FLV tag / RTMP media message: type, timestamp, body bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct Tag {
    pub kind: u8,
    pub ts: u32,
    pub data: Vec<u8>,
}

impl std::fmt::Debug for Tag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let head: Vec<String> = self
            .data
            .iter()
            .take(12)
            .map(|b| format!("{b:02x}"))
            .collect();
        write!(
            f,
            "Tag(kind={} ts={} len={} [{}])",
            self.kind,
            self.ts,
            self.data.len(),
            head.join(" ")
        )
    }
}

/// Minimal FLV file walker, independent of the crate under test:
/// 9-byte header, then `PreviousTagSize(4) | TagType(1) DataSize(3)
/// Timestamp(3) TimestampExtended(1) StreamID(3) | Data` records
/// (FLV v10.1 Annex E.2 – E.4.1).
pub fn read_flv(path: &Path) -> Vec<Tag> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .unwrap_or_else(|e| panic!("open {}: {e}", path.display()))
        .read_to_end(&mut bytes)
        .expect("read flv");
    assert!(bytes.len() >= 9 && &bytes[..3] == b"FLV", "not an FLV file");
    let mut pos = u32::from_be_bytes([bytes[5], bytes[6], bytes[7], bytes[8]]) as usize;
    let mut tags = Vec::new();
    loop {
        pos += 4; // PreviousTagSize
        if pos + 11 > bytes.len() {
            break;
        }
        let kind = bytes[pos] & 0x1F;
        let size = u32::from_be_bytes([0, bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]]) as usize;
        let ts = u32::from_be_bytes([
            bytes[pos + 7],
            bytes[pos + 4],
            bytes[pos + 5],
            bytes[pos + 6],
        ]);
        pos += 11;
        if pos + size > bytes.len() {
            break;
        }
        tags.push(Tag {
            kind,
            ts,
            data: bytes[pos..pos + size].to_vec(),
        });
        pos += size;
    }
    tags
}

/// Audio + video tags only, in file order.
pub fn media(tags: &[Tag]) -> Vec<Tag> {
    tags.iter()
        .filter(|t| t.kind == KIND_AUDIO || t.kind == KIND_VIDEO)
        .cloned()
        .collect()
}

/// Describe the first difference between two tag sequences, or `None`
/// when they are identical (bytes, timestamps and order).
pub fn diff(expected: &[Tag], got: &[Tag]) -> Option<String> {
    for (i, (e, g)) in expected.iter().zip(got.iter()).enumerate() {
        if e != g {
            return Some(format!(
                "tag {i} differs: expected {e:?}, got {g:?} (expected {} tags, got {})",
                expected.len(),
                got.len()
            ));
        }
    }
    if expected.len() != got.len() {
        return Some(format!(
            "tag count differs: expected {}, got {} (first extra: {:?})",
            expected.len(),
            got.len(),
            expected.get(got.len()).or_else(|| got.get(expected.len()))
        ));
    }
    None
}
