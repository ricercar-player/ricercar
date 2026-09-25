//! HTTP(S) media sources.
//!
//! * Known `Content-Length`: the body is spooled to a temp file by a
//!   background thread; the reader is seekable and blocks until the bytes it
//!   needs have arrived. Memory stays bounded whatever the file size.
//! * Unknown length (internet radio): a forward-only reader fed through a
//!   bounded channel (backpressure on the socket), with ICY metadata
//!   stripped from the audio and stream titles published separately.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};
use symphonia::core::io::MediaSource;

use crate::error::{AudioError, Result};

const CHUNK: usize = 64 * 1024;
/// Live streams: at most this much read ahead of the decoder.
const LIVE_BUFFER: usize = 8 * 1024 * 1024;

pub struct HttpSource {
    pub source: Box<dyn MediaSource + Send + 'static>,
    pub seekable: bool,
    /// Stream titles parsed from ICY metadata (internet radio).
    pub titles: Option<Receiver<String>>,
}

pub fn open(uri: &str) -> Result<HttpSource> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(30))
        .user_agent(concat!("ricercar/", env!("CARGO_PKG_VERSION")))
        .build();
    let resp = agent
        .get(uri)
        .set("Icy-MetaData", "1")
        .call()
        .map_err(|e| AudioError::UnsupportedSource(format!("http fetch {uri}: {e}")))?;

    let metaint = resp
        .header("icy-metaint")
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n > 0);
    // A transfer-encoded body makes Content-Length meaningless for offsets.
    let identity = resp
        .header("content-encoding")
        .is_none_or(|e| e.eq_ignore_ascii_case("identity"));
    let len = resp
        .header("content-length")
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|_| identity && metaint.is_none());
    let body: Box<dyn Read + Send> = Box::new(resp.into_reader());

    match len {
        Some(len) => Ok(HttpSource {
            source: Box::new(SpoolReader::start(body, len)?),
            seekable: true,
            titles: None,
        }),
        None => {
            let (source, titles) = LiveReader::start(body, metaint);
            Ok(HttpSource {
                source: Box::new(source),
                seekable: false,
                titles,
            })
        }
    }
}

// ---------------------------------------------------------------- spool

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn spool_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")));
    if let Some(dir) = base.map(|b| b.join("ricercar").join("stream"))
        && std::fs::create_dir_all(&dir).is_ok()
    {
        return dir;
    }
    std::env::temp_dir()
}

/// Remove spool files left behind by processes that no longer exist
/// (crash / kill); files of live processes are left alone.
fn sweep_stale(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|n| n.strip_prefix("ricercar-spool-"))
            .and_then(|n| n.split('-').next())
            .and_then(|p| p.parse::<u32>().ok())
        else {
            continue;
        };
        if !Path::new(&format!("/proc/{pid}")).exists() {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

#[derive(Default)]
struct Progress {
    downloaded: u64,
    done: bool,
    error: Option<String>,
}

struct Shared {
    progress: Mutex<Progress>,
    cv: Condvar,
    cancel: AtomicBool,
}

pub struct SpoolReader {
    file: File,
    path: PathBuf,
    pos: u64,
    len: u64,
    shared: Arc<Shared>,
}

static SPOOL_SEQ: AtomicU64 = AtomicU64::new(0);

impl SpoolReader {
    fn start(mut body: Box<dyn Read + Send>, len: u64) -> Result<SpoolReader> {
        let dir = spool_dir();
        sweep_stale(&dir);
        let path = dir.join(format!(
            "ricercar-spool-{}-{}",
            std::process::id(),
            SPOOL_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let mut writer = File::create(&path)?;
        let file = File::open(&path)?;
        let shared = Arc::new(Shared {
            progress: Mutex::new(Progress::default()),
            cv: Condvar::new(),
            cancel: AtomicBool::new(false),
        });
        let sh = shared.clone();
        std::thread::Builder::new()
            .name("ricercar-spool".into())
            .spawn(move || {
                use std::io::Write;
                let mut buf = vec![0u8; CHUNK];
                let outcome = loop {
                    if sh.cancel.load(Ordering::Relaxed) {
                        break Ok(());
                    }
                    match body.read(&mut buf) {
                        Ok(0) => break Ok(()),
                        Ok(n) => {
                            if let Err(e) = writer.write_all(&buf[..n]) {
                                break Err(format!("spool write: {e}"));
                            }
                            lock(&sh.progress).downloaded += n as u64;
                            sh.cv.notify_all();
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(e) => break Err(format!("http read: {e}")),
                    }
                };
                let mut p = lock(&sh.progress);
                p.done = true;
                p.error = outcome.err();
                sh.cv.notify_all();
            })?;
        Ok(SpoolReader {
            file,
            path,
            pos: 0,
            len,
            shared,
        })
    }

    /// Block until `pos` is downloaded (or the download ended); returns the
    /// number of bytes readable at `pos`.
    fn wait_for(&self, pos: u64) -> io::Result<u64> {
        let mut p = lock(&self.shared.progress);
        loop {
            if p.downloaded > pos {
                return Ok(p.downloaded - pos);
            }
            if p.done {
                return match &p.error {
                    Some(e) if pos < self.len => Err(io::Error::other(e.clone())),
                    _ => Ok(0),
                };
            }
            p = self
                .shared
                .cv
                .wait_timeout(p, Duration::from_millis(500))
                .map(|(g, _)| g)
                .unwrap_or_else(|e| e.into_inner().0);
        }
    }
}

impl Drop for SpoolReader {
    fn drop(&mut self) {
        self.shared.cancel.store(true, Ordering::Relaxed);
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Read for SpoolReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let avail = self.wait_for(self.pos)?;
        let n = (out.len() as u64).min(avail) as usize;
        let n = self.file.read_at(&mut out[..n], self.pos)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for SpoolReader {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let target = match to {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::End(d) => self.len.checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        self.pos =
            target.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek before 0"))?;
        Ok(self.pos)
    }
}

impl MediaSource for SpoolReader {
    fn is_seekable(&self) -> bool {
        true
    }
    fn byte_len(&self) -> Option<u64> {
        Some(self.len)
    }
}

// ---------------------------------------------------------------- live

/// Splits an ICY stream into audio bytes and metadata blocks.
pub struct IcyDemux {
    metaint: usize,
    /// Audio bytes left before the next metadata length byte.
    until_meta: usize,
    /// Metadata bytes still expected, and the block being collected.
    meta_left: usize,
    meta: Vec<u8>,
    in_meta: bool,
}

impl IcyDemux {
    pub fn new(metaint: usize) -> IcyDemux {
        IcyDemux {
            metaint,
            until_meta: metaint,
            meta_left: 0,
            meta: Vec::new(),
            in_meta: false,
        }
    }

    /// Append the audio part of `input` to `audio`; returns completed titles.
    pub fn feed(&mut self, mut input: &[u8], audio: &mut Vec<u8>) -> Vec<String> {
        let mut titles = Vec::new();
        while !input.is_empty() {
            if self.in_meta {
                let n = self.meta_left.min(input.len());
                self.meta.extend_from_slice(&input[..n]);
                input = &input[n..];
                self.meta_left -= n;
                if self.meta_left == 0 {
                    self.in_meta = false;
                    self.until_meta = self.metaint;
                    if let Some(t) = parse_stream_title(&self.meta) {
                        titles.push(t);
                    }
                    self.meta.clear();
                }
            } else if self.until_meta == 0 {
                let len = input[0] as usize * 16;
                input = &input[1..];
                if len == 0 {
                    self.until_meta = self.metaint;
                } else {
                    self.in_meta = true;
                    self.meta_left = len;
                }
            } else {
                let n = self.until_meta.min(input.len());
                audio.extend_from_slice(&input[..n]);
                input = &input[n..];
                self.until_meta -= n;
            }
        }
        titles
    }
}

/// `StreamTitle='Artist - Song';StreamUrl='...';` (padded with NULs).
pub fn parse_stream_title(block: &[u8]) -> Option<String> {
    let end = block.iter().position(|&b| b == 0).unwrap_or(block.len());
    let block = &block[..end];
    let text = match std::str::from_utf8(block) {
        Ok(s) => s.to_string(),
        // Many stations still send Latin-1.
        Err(_) => block.iter().map(|&b| b as char).collect(),
    };
    let start = text.find("StreamTitle='")? + "StreamTitle='".len();
    let rest = &text[start..];
    // Titles may contain quotes: the field ends at the `';` delimiter.
    let stop = rest.find("';").or_else(|| rest.rfind('\''))?;
    Some(rest[..stop].trim().to_string())
}

pub struct LiveReader {
    rx: Receiver<io::Result<Vec<u8>>>,
    buf: Vec<u8>,
    off: usize,
}

impl LiveReader {
    fn start(
        mut body: Box<dyn Read + Send>,
        metaint: Option<usize>,
    ) -> (LiveReader, Option<Receiver<String>>) {
        let (tx, rx) = crossbeam_channel::bounded::<io::Result<Vec<u8>>>(LIVE_BUFFER / CHUNK);
        let (title_tx, title_rx): (Option<Sender<String>>, _) = match metaint {
            Some(_) => {
                let (t, r) = crossbeam_channel::unbounded();
                (Some(t), Some(r))
            }
            None => (None, None),
        };
        std::thread::Builder::new()
            .name("ricercar-live".into())
            .spawn(move || {
                let mut demux = metaint.map(IcyDemux::new);
                let mut buf = vec![0u8; CHUNK];
                loop {
                    match body.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let chunk = match &mut demux {
                                Some(d) => {
                                    let mut audio = Vec::with_capacity(n);
                                    for t in d.feed(&buf[..n], &mut audio) {
                                        if let Some(tt) = &title_tx {
                                            let _ = tt.send(t);
                                        }
                                    }
                                    audio
                                }
                                None => buf[..n].to_vec(),
                            };
                            // Blocks when the decoder is far behind; errors
                            // once the reader is gone.
                            if !chunk.is_empty() && tx.send(Ok(chunk)).is_err() {
                                break;
                            }
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(e) => {
                            let _ = tx.send(Err(e));
                            break;
                        }
                    }
                }
            })
            .ok();
        (
            LiveReader {
                rx,
                buf: Vec::new(),
                off: 0,
            },
            title_rx,
        )
    }
}

impl Read for LiveReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.off >= self.buf.len() {
            match self.rx.recv() {
                Ok(Ok(chunk)) => {
                    self.buf = chunk;
                    self.off = 0;
                }
                Ok(Err(e)) => return Err(e),
                Err(_) => return Ok(0), // producer gone => EOF
            }
        }
        let n = out.len().min(self.buf.len() - self.off);
        out[..n].copy_from_slice(&self.buf[self.off..self.off + n]);
        self.off += n;
        Ok(n)
    }
}

impl Seek for LiveReader {
    fn seek(&mut self, _p: SeekFrom) -> io::Result<u64> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "live stream"))
    }
}

impl MediaSource for LiveReader {
    fn is_seekable(&self) -> bool {
        false
    }
    fn byte_len(&self) -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn icy_block(meta: &str) -> Vec<u8> {
        let mut m = meta.as_bytes().to_vec();
        m.resize(m.len().div_ceil(16) * 16, 0);
        let mut out = vec![(m.len() / 16) as u8];
        out.extend(m);
        out
    }

    #[test]
    fn icy_demux_strips_metadata_across_chunk_boundaries() {
        let metaint = 5;
        let mut wire = b"AAAAA".to_vec();
        wire.extend(icy_block("StreamTitle='It's - Me';StreamUrl='';"));
        wire.extend(b"BBBBB");
        wire.push(0); // empty metadata block
        wire.extend(b"CCCCC");
        wire.extend(icy_block("StreamTitle='Next';"));
        wire.extend(b"DD");
        for split in 1..wire.len() {
            let mut d = IcyDemux::new(metaint);
            let mut audio = Vec::new();
            let mut titles = Vec::new();
            for part in wire.chunks(split) {
                titles.extend(d.feed(part, &mut audio));
            }
            assert_eq!(audio, b"AAAAABBBBBCCCCCDD", "split {split}");
            assert_eq!(titles, ["It's - Me", "Next"], "split {split}");
        }
    }

    #[test]
    fn stream_title_latin1_and_missing() {
        assert_eq!(
            parse_stream_title(b"StreamTitle='Caf\xe9';\0\0").as_deref(),
            Some("Café")
        );
        assert_eq!(parse_stream_title(b"StreamUrl='x';"), None);
    }

    #[test]
    fn spool_blocks_until_bytes_arrive_and_seeks() {
        struct Slow(Vec<u8>, usize);
        impl Read for Slow {
            fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
                std::thread::sleep(Duration::from_millis(5));
                let n = out.len().min(1000).min(self.0.len() - self.1);
                out[..n].copy_from_slice(&self.0[self.1..self.1 + n]);
                self.1 += n;
                Ok(n)
            }
        }
        let data: Vec<u8> = (0..20_000u32).map(|i| (i % 251) as u8).collect();
        let mut r = SpoolReader::start(Box::new(Slow(data.clone(), 0)), data.len() as u64).unwrap();
        let path = r.path.clone();
        r.seek(SeekFrom::End(-100)).unwrap();
        let mut tail = Vec::new();
        r.read_to_end(&mut tail).unwrap();
        assert_eq!(tail, &data[data.len() - 100..]);
        r.seek(SeekFrom::Start(0)).unwrap();
        let mut all = Vec::new();
        r.read_to_end(&mut all).unwrap();
        assert_eq!(all, data);
        drop(r);
        assert!(!path.exists(), "spool file removed on drop");
    }

    #[test]
    fn truncated_download_is_an_error() {
        let mut r = SpoolReader::start(Box::new(io::Cursor::new(vec![1u8; 10])), 20).unwrap();
        let mut v = Vec::new();
        // Short body: EOF before Content-Length; reads past it end cleanly
        // (the decoder decides whether the data is usable).
        assert_eq!(r.read_to_end(&mut v).unwrap(), 10);
    }
}
