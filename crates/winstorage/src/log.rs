//! File + sink logger and environment helpers.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use windows::core::w;
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ,
};
use windows::Win32::System::SystemInformation::GetLocalTime;

type Sink = Box<dyn Fn(&str) + Send>;

struct Inner {
    file: File,
    sink: Option<Sink>,
}

/// Appends timestamped lines to a log file and optionally forwards each line
/// to a sink callback (used by the GUI).
pub struct Logger {
    path: PathBuf,
    inner: Mutex<Inner>,
}

fn timestamp() -> String {
    let t = unsafe { GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

fn file_stamp() -> String {
    let t = unsafe { GetLocalTime() };
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

impl Logger {
    /// Creates `dir` (if needed) and opens `<prefix>-<YYYYMMDD-HHMMSS>.log` for appending.
    pub fn open(dir: &Path, prefix: &str) -> io::Result<Logger> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("{prefix}-{}.log", file_stamp()));
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Logger {
            path,
            inner: Mutex::new(Inner { file, sink: None }),
        })
    }

    /// Opens an existing (or new) log file at exactly `path` for appending. Used to log
    /// from places that cannot hold a borrowed [`Logger`], such as `Drop` impls.
    pub fn append(path: &Path) -> io::Result<Logger> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Logger {
            path: path.to_path_buf(),
            inner: Mutex::new(Inner { file, sink: None }),
        })
    }

    /// Path of the log file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn info(&self, msg: &str) {
        self.write("INFO", msg);
    }

    pub fn warn(&self, msg: &str) {
        self.write("WARN", msg);
    }

    pub fn error(&self, msg: &str) {
        self.write("ERROR", msg);
    }

    /// Installs a callback that receives every logged line (without newline).
    pub fn set_sink(&self, f: Box<dyn Fn(&str) + Send>) {
        if let Ok(mut g) = self.inner.lock() {
            g.sink = Some(f);
        }
    }

    fn write(&self, level: &str, msg: &str) {
        // Format: "YYYY-MM-DD HH:MM:SS  LEVEL  message" (two spaces around LEVEL).
        let line = format!("{}  {}  {}", timestamp(), level, msg);
        if let Ok(mut g) = self.inner.lock() {
            // Logging must never fail the caller; I/O errors are deliberately dropped.
            let _ = writeln!(g.file, "{line}");
            let _ = g.file.flush();
            if let Some(s) = &g.sink {
                s(&line);
            }
        }
    }
}

/// True when running under WinPE (`HKLM\SYSTEM\CurrentControlSet\Control\MiniNT` exists).
pub fn is_winpe() -> bool {
    let mut key = HKEY::default();
    let rc = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            w!(r"SYSTEM\CurrentControlSet\Control\MiniNT"),
            0,
            KEY_READ,
            &mut key,
        )
    };
    if rc == ERROR_SUCCESS {
        let _ = unsafe { RegCloseKey(key) };
        true
    } else {
        false
    }
}

/// `X:\rescue-logs` in WinPE, `%LOCALAPPDATA%\WoARescue\logs` otherwise.
pub fn default_log_dir() -> PathBuf {
    if is_winpe() {
        return PathBuf::from(r"X:\rescue-logs");
    }
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("WoARescue").join("logs")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_match_format() {
        let dir = tempfile::tempdir().unwrap();
        let log = Logger::open(dir.path(), "t").unwrap();
        log.info("hello");
        log.warn("careful");
        log.error("boom");
        let text = std::fs::read_to_string(log.path()).unwrap();
        let re =
            regex::Regex::new(r"^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}  (INFO|WARN|ERROR) ").unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        for l in &lines {
            assert!(re.is_match(l), "bad line: {l:?}");
        }
        assert!(lines[0].contains("  INFO  hello"));
        assert!(lines[1].contains("  WARN  careful"));
        assert!(lines[2].contains("  ERROR  boom"));
    }

    #[test]
    fn sink_receives_lines() {
        use std::sync::{Arc, Mutex};
        let dir = tempfile::tempdir().unwrap();
        let log = Logger::open(dir.path(), "t").unwrap();
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let s2 = seen.clone();
        log.set_sink(Box::new(move |l| s2.lock().unwrap().push(l.to_string())));
        log.info("x");
        let v = seen.lock().unwrap();
        assert_eq!(v.len(), 1);
        assert!(v[0].ends_with("INFO  x"));
    }

    #[test]
    fn default_dir_not_winpe_is_localappdata() {
        if !is_winpe() {
            let d = default_log_dir();
            assert!(d.ends_with(r"WoARescue\logs"));
        }
    }
}
