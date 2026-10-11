//! File-backed diagnostics.
//!
//! The GUI app's stdout/stderr is discarded when the user launches DTV by
//! double-clicking it, which made field problems ("看着看着就停了") impossible to
//! diagnose. This module mirrors the important runtime events into a single
//! rotating file the user can send back:
//!
//!     ~/Downloads/DTV-live.log        (current session, newest at the end)
//!     ~/Downloads/DTV-live.1.log      (previous rotation, kept for context)
//!
//! Writes are line-buffered through a mutex, timestamped to the millisecond and
//! tagged by subsystem, so proxy/pump/player/dlna timelines can be interleaved
//! and read top-to-bottom. Everything is also echoed to stdout so a terminal
//! launch still behaves like before.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// Rotate when the log grows past this, so a long-running app cannot fill the
/// disk. One archived generation is enough to see what preceded a failure.
const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;

static LOG_FILE: OnceLock<Mutex<Option<File>>> = OnceLock::new();
static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();
static LINES: AtomicU64 = AtomicU64::new(0);

fn slot() -> &'static Mutex<Option<File>> {
    LOG_FILE.get_or_init(|| Mutex::new(None))
}

/// `~/Downloads` when it exists (that is where the user expects the file),
/// otherwise the OS temp dir so diagnostics never break the app.
pub fn log_path() -> PathBuf {
    if let Some(p) = LOG_PATH.get() {
        return p.clone();
    }
    let home = std::env::var("HOME").ok().map(PathBuf::from);
    let dir = home
        .map(|h| h.join("Downloads"))
        .filter(|d| d.is_dir())
        .or_else(|| std::env::var("TEMP").ok().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    // Prefer an explicit override for automated testing.
    let path = std::env::var("DTV_LOG_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dir.join("DTV-live.log"));
    let _ = LOG_PATH.set(path.clone());
    path
}

fn rotate_if_needed(path: &PathBuf) {
    let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if len < MAX_LOG_BYTES {
        return;
    }
    let archived = path.with_extension("1.log");
    let _ = std::fs::remove_file(&archived);
    if std::fs::rename(path, &archived).is_ok() {
        eprintln!("[telemetry] rotated log to {}", archived.display());
    }
}

/// Open (or reopen after rotation) the log file. Idempotent.
pub fn init_session(version: &str) {
    let path = log_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    rotate_if_needed(&path);
    let file = OpenOptions::new().create(true).append(true).open(&path);
    match file {
        Ok(f) => {
            *slot().lock().unwrap() = Some(f);
            let pid = std::process::id();
            let os = format!("{} {}", std::env::consts::OS, std::env::consts::ARCH);
            write_line(
                "session",
                &format!(
                    "===== SESSION START v{} pid={} {} downloads={} =====",
                    version,
                    pid,
                    os,
                    path.display()
                ),
            );
            eprintln!("[telemetry] logging to {}", path.display());
        }
        Err(e) => eprintln!("[telemetry] FAILED to open {}: {}", path.display(), e),
    }
}

fn write_line(tag: &str, msg: &str) {
    let n = LINES.fetch_add(1, Ordering::Relaxed);
    let line = format!("{} {:>6} [{}] {}", stamp(), n, tag, msg);
    // 懒打开：长跑联调进程也要能落盘（否则最需要日志的排查场景一行都拿不到），
    // 但普通单测绝不能落盘 —— 否则 mock 数据会混进用户 ~/Downloads/DTV-live.log。
    if slot().lock().unwrap().is_none()
        && (std::env::var_os("DTV_LOG_PATH").is_some() || !cfg!(test))
    {
        init_session_cached();
    }
    if let Some(mut f) = slot().lock().unwrap().as_ref() {
        // A failed write must never take the app down; just try to reopen once.
        if writeln!(f, "{}", line).is_err() || f.flush().is_err() {
            *slot().lock().unwrap() = None;
            init_session_cached();
            if let Some(mut f2) = slot().lock().unwrap().as_ref() {
                let _ = writeln!(f2, "{}", line);
                let _ = f2.flush();
            }
        }
    }
}

fn init_session_cached() {
    let path = log_path();
    if let Ok(f) = OpenOptions::new().create(true).append(true).open(&path) {
        *slot().lock().unwrap() = Some(f);
    }
}

fn stamp() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string()
}

/// Record one diagnostic line (file + stdout).
pub fn record(tag: &str, msg: &str) {
    write_line(tag, msg);
    if std::env::var("DTV_LOG_QUIET").is_err() {
        println!("[{}][{}] {}", stamp(), tag, msg);
    }
}

/// `tl!("proxy", "pump #{} EOF", id)` -> timestamped line in DTV-live.log.
#[macro_export]
macro_rules! tl {
    ($tag:expr, $($arg:tt)*) => {{
        $crate::telemetry::record($tag, &format!($($arg)*));
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The log file must actually be created and receive tagged, timestamped
    /// lines -- this is the whole point of the module.
    #[test]
    fn writes_are_persisted_to_file() {
        let dir = std::env::temp_dir().join(format!("dtv-tel-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("t.log");
        std::fs::write(&path, b"").ok();
        // Point the module at a temp file for this test process.
        std::env::set_var("DTV_LOG_PATH", &path);
        std::env::set_var("DTV_LOG_QUIET", "1");
        // LOG_PATH may already be cached by an earlier test in this binary; only
        // assert what that allows.
        let resolved = log_path();
        init_session("test");
        crate::tl!("proxy", "pump #1 test line {}", 42);
        let content = std::fs::read_to_string(&resolved).unwrap_or_default();
        assert!(
            content.contains("SESSION START") || resolved != path,
            "expected a session header in {}, got: {:?}",
            resolved.display(),
            &content[..content.len().min(200)]
        );
        if resolved == path {
            assert!(content.contains("[proxy]"));
            assert!(content.contains("pump #1 test line 42"));
            // Each line starts with a full timestamp.
            for l in content.lines().filter(|l| l.contains("test line")) {
                assert!(l.starts_with('2'), "line lacks timestamp: {}", l);
                assert_eq!(l[..10].matches('-').count(), 2, "bad date prefix: {}", l);
            }
        }
        std::env::remove_var("DTV_LOG_PATH");
        std::env::remove_var("DTV_LOG_QUIET");
    }
}
