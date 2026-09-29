//! Logging: stderr, a rotating file (`$XDG_STATE_HOME/ricercar/ricercar.log`,
//! three files of 2 MB) and the last lines in memory for diagnostic reports.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, fmt};

/// Size at which the log file is rotated.
pub const MAX_BYTES: u64 = 2 * 1024 * 1024;
/// `ricercar.log`, `ricercar.log.1`, `ricercar.log.2`.
pub const FILES: usize = 3;
/// Lines kept in memory.
pub const MEMORY_LINES: usize = 500;

pub fn log_dir() -> PathBuf {
    ricercar_core::config::state_dir()
}

pub fn log_path() -> PathBuf {
    log_dir().join("ricercar.log")
}

/// A log file that rotates itself once it passes `max` bytes.
pub struct Rotating {
    path: PathBuf,
    file: Option<File>,
    size: u64,
    max: u64,
    keep: usize,
}

impl Rotating {
    pub fn open(path: &Path, max: u64, keep: usize) -> std::io::Result<Rotating> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Rotating {
            path: path.to_path_buf(),
            file: Some(file),
            size,
            max,
            keep: keep.max(1),
        })
    }

    fn numbered(&self, n: usize) -> PathBuf {
        let mut p = self.path.clone().into_os_string();
        p.push(format!(".{n}"));
        p.into()
    }

    fn rotate(&mut self) {
        self.file = None;
        let _ = std::fs::remove_file(self.numbered(self.keep - 1));
        for n in (1..self.keep - 1).rev() {
            let _ = std::fs::rename(self.numbered(n), self.numbered(n + 1));
        }
        if self.keep > 1 {
            let _ = std::fs::rename(&self.path, self.numbered(1));
        } else {
            let _ = std::fs::remove_file(&self.path);
        }
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .ok();
        self.size = 0;
    }

    pub fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        if self.size > 0 && self.size + buf.len() as u64 > self.max {
            self.rotate();
        }
        if let Some(f) = self.file.as_mut() {
            f.write_all(buf)?;
            self.size += buf.len() as u64;
        }
        Ok(())
    }
}

struct FileWriter(Arc<Mutex<Rotating>>);

impl Write for FileWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write_all(buf)?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The last lines logged, oldest first.
#[derive(Default)]
pub struct Memory {
    lines: VecDeque<String>,
    cap: usize,
}

impl Memory {
    pub fn with_capacity(cap: usize) -> Memory {
        Memory {
            lines: VecDeque::with_capacity(cap),
            cap,
        }
    }

    pub fn push(&mut self, text: &str) {
        for line in text.lines().filter(|l| !l.is_empty()) {
            if self.lines.len() == self.cap {
                self.lines.pop_front();
            }
            self.lines.push_back(line.to_string());
        }
    }

    pub fn last(&self, n: usize) -> Vec<String> {
        let skip = self.lines.len().saturating_sub(n);
        self.lines.iter().skip(skip).cloned().collect()
    }
}

fn memory() -> &'static Mutex<Memory> {
    static M: OnceLock<Mutex<Memory>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(Memory::with_capacity(MEMORY_LINES)))
}

struct MemoryWriter;

impl Write for MemoryWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        memory().lock().unwrap().push(&String::from_utf8_lossy(buf));
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The last `n` lines logged by this process.
pub fn recent_lines(n: usize) -> Vec<String> {
    memory().lock().unwrap().last(n)
}

/// stderr, the log file and the in-memory tail, all with the same filter
/// (`RUST_LOG`, default `info,symphonia=warn`).
pub fn init() {
    let filter =
        || EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,symphonia=warn".into());
    let stderr = fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(filter());
    let tail = fmt::layer()
        .with_ansi(false)
        .with_writer(|| MemoryWriter)
        .with_filter(filter());
    let file = match Rotating::open(&log_path(), MAX_BYTES, FILES) {
        Ok(r) => {
            let r = Arc::new(Mutex::new(r));
            Some(
                fmt::layer()
                    .with_ansi(false)
                    .with_writer(move || FileWriter(r.clone()))
                    .with_filter(filter()),
            )
        }
        Err(e) => {
            eprintln!("ricercar: no log file ({}): {e}", log_path().display());
            None
        }
    };
    let _ = tracing_subscriber::registry()
        .with(stderr)
        .with(tail)
        .with(file)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_rotates_and_keeps_three() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("logs/ricercar.log");
        let mut r = Rotating::open(&p, 100, 3).unwrap();
        for i in 0..20 {
            r.write_all(format!("line {i:02} ........................\n").as_bytes())
                .unwrap();
        }
        let read = |s: &str| std::fs::read_to_string(dir.path().join("logs").join(s)).unwrap();
        assert!(read("ricercar.log").contains("line 19"));
        assert!(read("ricercar.log.1").contains("line 17"));
        assert!(read("ricercar.log.2").contains("line 14"));
        assert!(!dir.path().join("logs/ricercar.log.3").exists());
        assert!(std::fs::metadata(&p).unwrap().len() <= 100);
        // Reopening appends.
        drop(r);
        let mut r = Rotating::open(&p, 100, 3).unwrap();
        r.write_all(b"again\n").unwrap();
        assert!(read("ricercar.log").ends_with("again\n"));
    }

    #[test]
    fn memory_keeps_the_last_lines() {
        let mut m = Memory::with_capacity(3);
        m.push("a\nb\n");
        m.push("c\n");
        m.push("d\n\n");
        assert_eq!(m.last(10), ["b", "c", "d"]);
        assert_eq!(m.last(2), ["c", "d"]);
    }
}
