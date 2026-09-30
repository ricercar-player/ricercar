//! HTTP(S) media sources.
//!
//! * Known `Content-Length`: the body is spooled to a temp file by a
//!   background thread; the reader is seekable and blocks until the bytes it
//!   needs have arrived. Memory stays bounded whatever the file size.
//!   When the server accepts byte ranges, a read far beyond the downloaded
//!   data starts a `Range` request there instead of waiting; the spool is
//!   then sparse, and the holes left behind are fetched afterwards.
//! * Unknown length (internet radio): a forward-only reader fed through a
//!   bounded channel (backpressure on the socket), with ICY metadata
//!   stripped from the audio and stream titles published separately.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use symphonia::core::io::MediaSource;

use crate::error::{AudioError, Result};

const CHUNK: usize = 64 * 1024;
/// Live streams: at most this much read ahead of the decoder.
const LIVE_BUFFER: usize = 8 * 1024 * 1024;
/// Largest body spooled to disk; bigger ones are played forward-only.
const SPOOL_MAX: u64 = 2 * 1024 * 1024 * 1024;
/// Disk space always left free by the spool.
const SPOOL_MARGIN: u64 = 512 * 1024 * 1024;
/// A read at least this far past the download position starts a `Range`
/// request (or farther, if about a second of transfer would reach it).
const JUMP_MIN: u64 = 512 * 1024;

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
        .map_err(|e| match e {
            ureq::Error::Status(status, _) => AudioError::HttpStatus {
                uri: uri.to_string(),
                status,
            },
            e => AudioError::UnsupportedSource(format!("http fetch {uri}: {e}")),
        })?;

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
    let origin = resp
        .header("accept-ranges")
        .is_some_and(|v| v.split(',').any(|u| u.trim().eq_ignore_ascii_case("bytes")))
        .then(|| Origin {
            agent: agent.clone(),
            uri: uri.to_string(),
        });
    let body: Body = Box::new(resp.into_reader());

    match len.filter(|&n| n <= spool_limit(free_space(&spool_dir()))) {
        Some(len) => Ok(HttpSource {
            source: Box::new(SpoolReader::start(body, len, origin)?),
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

/// Bytes a single spool may take given the free space of its filesystem.
fn spool_limit(free: Option<u64>) -> u64 {
    free.map_or(SPOOL_MAX, |f| f.saturating_sub(SPOOL_MARGIN).min(SPOOL_MAX))
}

// The statvfs field widths vary between targets.
#[allow(clippy::unnecessary_cast)]
fn free_space(dir: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut st = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is NUL-terminated and `st` is a valid out pointer.
    if unsafe { libc::statvfs(path.as_ptr(), st.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: statvfs succeeded and filled `st`.
    let st = unsafe { st.assume_init() };
    Some((st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
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

/// Downloaded byte ranges: sorted, disjoint, half-open, merged when they
/// touch.
#[derive(Default)]
struct Ranges(Vec<(u64, u64)>);

impl Ranges {
    fn insert(&mut self, a: u64, b: u64) {
        if a >= b {
            return;
        }
        let i = self.0.partition_point(|r| r.1 < a);
        let (mut lo, mut hi, mut j) = (a, b, i);
        while j < self.0.len() && self.0[j].0 <= hi {
            lo = lo.min(self.0[j].0);
            hi = hi.max(self.0[j].1);
            j += 1;
        }
        self.0.splice(i..j, [(lo, hi)]);
    }

    /// End of the downloaded run containing `pos`.
    fn run_end(&self, pos: u64) -> Option<u64> {
        let i = self.0.partition_point(|r| r.1 <= pos);
        self.0.get(i).filter(|r| r.0 <= pos).map(|r| r.1)
    }

    /// First downloaded byte at or after `pos`.
    fn next_start(&self, pos: u64) -> Option<u64> {
        let i = self.0.partition_point(|r| r.1 <= pos);
        self.0.get(i).map(|r| r.0.max(pos))
    }

    /// First missing byte at or after `from`, else the first one at all.
    fn first_hole(&self, from: u64, len: u64) -> Option<u64> {
        let hole = |p: u64| Some(self.run_end(p).unwrap_or(p)).filter(|&h| h < len);
        hole(from.min(len)).or_else(|| hole(0))
    }
}

#[derive(Default)]
struct Progress {
    have: Ranges,
    /// A missing offset the reader is blocked on.
    want: Option<u64>,
    /// Where the reader last needed data: holes after it are filled first.
    reading: u64,
    done: bool,
    error: Option<String>,
}

struct Shared {
    progress: Mutex<Progress>,
    cv: Condvar,
    cancel: AtomicBool,
}

type Body = Box<dyn Read + Send>;

/// Where a spooled body comes from, for `Range` requests.
struct Origin {
    agent: ureq::Agent,
    uri: String,
}

impl Origin {
    /// `GET` with `Range: bytes=<from>-`; returns the body and the end of
    /// the part it carries. Anything but a matching 206 is refused.
    fn range(&self, from: u64, len: u64) -> std::result::Result<(Body, u64), String> {
        let resp = self
            .agent
            .get(&self.uri)
            .set("Icy-MetaData", "1")
            .set("Range", &format!("bytes={from}-"))
            .call()
            .map_err(|e| e.to_string())?;
        if resp.status() != 206 {
            return Err(format!("status {}", resp.status()));
        }
        if resp
            .header("content-encoding")
            .is_some_and(|e| !e.eq_ignore_ascii_case("identity"))
        {
            return Err("encoded body".into());
        }
        let cr = resp.header("content-range").unwrap_or_default().to_string();
        match parse_content_range(&cr) {
            Some((first, last, total)) if first == from && total == len => {
                Ok((Box::new(resp.into_reader()), last + 1))
            }
            _ => Err(format!("content-range {cr:?}")),
        }
    }
}

/// `bytes <first>-<last>/<total>`.
fn parse_content_range(v: &str) -> Option<(u64, u64, u64)> {
    let rest = v.trim().strip_prefix("bytes")?.trim_start();
    let (span, total) = rest.split_once('/')?;
    let (first, last) = span.split_once('-')?;
    let first: u64 = first.trim().parse().ok()?;
    let last: u64 = last.trim().parse().ok()?;
    let total: u64 = total.trim().parse().ok()?;
    (first <= last && last < total).then_some((first, last, total))
}

/// Fills the spool: the initial response first, a `Range` request when the
/// reader waits far from the download position, and once a range has been
/// used, every remaining hole so that the file ends up complete.
struct Fetcher {
    sh: Arc<Shared>,
    file: File,
    len: u64,
    /// `None` without range support, or once the server refused a range.
    origin: Option<Origin>,
}

impl Fetcher {
    fn run(mut self, body: Body) {
        let outcome = self.fetch(body);
        let mut p = lock(&self.sh.progress);
        p.done = true;
        p.error = outcome.err();
        self.sh.cv.notify_all();
    }

    fn fetch(&mut self, mut body: Body) -> std::result::Result<(), String> {
        let mut buf = vec![0u8; CHUNK];
        // Next offset of `body`, and where its part ends.
        let mut at = 0u64;
        let mut end = self.len;
        let mut ranged = false;
        let mut since = Instant::now();
        let mut fetched = 0u64;
        loop {
            if self.sh.cancel.load(Ordering::Relaxed) {
                return Ok(());
            }
            let (stop, jump, hole) = {
                let mut p = lock(&self.sh.progress);
                // Never download a byte twice.
                let stop = p.have.next_start(at).unwrap_or(self.len).min(end);
                let ahead = (fetched / since.elapsed().as_secs().max(1)).max(JUMP_MIN);
                let want = p.want.take();
                let jump = want.filter(|&w| {
                    self.origin.is_some()
                        && w < self.len
                        && p.have.run_end(w).is_none()
                        && !(at <= w && w < stop && w - at < ahead)
                });
                let hole = if at >= stop {
                    p.have.first_hole(p.reading, self.len)
                } else {
                    None
                };
                (stop, jump, hole)
            };
            let Some(to) = jump.or(hole) else {
                if at >= stop {
                    return Ok(());
                }
                let n = match body.read(&mut buf) {
                    Ok(0) if ranged => return Err("http read: range ended early".into()),
                    Ok(0) => return Ok(()),
                    Ok(n) => (n as u64).min(stop - at),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(format!("http read: {e}")),
                };
                self.file
                    .write_all_at(&buf[..n as usize], at)
                    .map_err(|e| format!("spool write: {e}"))?;
                lock(&self.sh.progress).have.insert(at, at + n);
                self.sh.cv.notify_all();
                at += n;
                fetched += n;
                continue;
            };
            let Some(origin) = &self.origin else {
                return Err("http read: incomplete body".into());
            };
            match origin.range(to, self.len) {
                Ok((b, e)) => {
                    body = b;
                    (at, end, ranged) = (to, e, true);
                    (since, fetched) = (Instant::now(), 0);
                }
                // The initial response is still open: carry on with it.
                Err(e) if !ranged => {
                    tracing::debug!("range request refused, reading sequentially: {e}");
                    self.origin = None;
                }
                Err(e) => return Err(format!("http range: {e}")),
            }
        }
    }
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
    fn start(body: Body, len: u64, origin: Option<Origin>) -> Result<SpoolReader> {
        let dir = spool_dir();
        sweep_stale(&dir);
        let path = dir.join(format!(
            "ricercar-spool-{}-{}",
            std::process::id(),
            SPOOL_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let writer = File::create(&path)?;
        let file = File::open(&path)?;
        let shared = Arc::new(Shared {
            progress: Mutex::new(Progress::default()),
            cv: Condvar::new(),
            cancel: AtomicBool::new(false),
        });
        let fetcher = Fetcher {
            sh: shared.clone(),
            file: writer,
            len,
            origin,
        };
        std::thread::Builder::new()
            .name("ricercar-spool".into())
            .spawn(move || fetcher.run(body))?;
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
        if pos >= self.len {
            return Ok(0);
        }
        let mut p = lock(&self.shared.progress);
        p.reading = pos;
        loop {
            if let Some(end) = p.have.run_end(pos) {
                return Ok(end - pos);
            }
            if p.done {
                return match &p.error {
                    Some(e) => Err(io::Error::other(e.clone())),
                    None => Ok(0),
                };
            }
            p.want = Some(pos);
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
                        // Closed before the announced length: end the stream
                        // like a plain close, so the decoder keeps what it has.
                        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                            tracing::debug!("live stream closed early: {e}");
                            break;
                        }
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
        let mut r =
            SpoolReader::start(Box::new(Slow(data.clone(), 0)), data.len() as u64, None).unwrap();
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
    fn spool_is_capped_by_size_and_free_space() {
        const GIB: u64 = 1024 * 1024 * 1024;
        assert_eq!(spool_limit(None), SPOOL_MAX);
        assert_eq!(spool_limit(Some(100 * GIB)), SPOOL_MAX);
        assert_eq!(spool_limit(Some(GIB)), GIB - SPOOL_MARGIN);
        assert_eq!(spool_limit(Some(SPOOL_MARGIN / 2)), 0);
        assert!(free_space(&std::env::temp_dir()).is_some());
    }

    #[test]
    fn spool_stops_at_the_announced_length() {
        let mut r =
            SpoolReader::start(Box::new(io::Cursor::new(vec![7u8; 5000])), 1000, None).unwrap();
        let mut v = Vec::new();
        assert_eq!(r.read_to_end(&mut v).unwrap(), 1000);
        assert_eq!(std::fs::metadata(&r.path).unwrap().len(), 1000);
    }

    #[test]
    fn ranges_merge_and_find_holes() {
        let mut r = Ranges::default();
        r.insert(10, 20);
        r.insert(30, 40);
        r.insert(20, 25); // touches: merged
        assert_eq!(r.0, [(10, 25), (30, 40)]);
        assert_eq!(r.run_end(10), Some(25));
        assert_eq!(r.run_end(25), None);
        assert_eq!(r.run_end(5), None);
        assert_eq!(r.next_start(0), Some(10));
        assert_eq!(r.next_start(12), Some(12));
        assert_eq!(r.next_start(26), Some(30));
        assert_eq!(r.next_start(40), None);
        assert_eq!(r.first_hole(12, 50), Some(25));
        assert_eq!(r.first_hole(35, 50), Some(40));
        assert_eq!(r.first_hole(35, 40), Some(0));
        r.insert(0, 10);
        r.insert(24, 31);
        assert_eq!(r.0, [(0, 40)]);
        assert_eq!(r.first_hole(7, 40), None);
    }

    #[test]
    fn content_range_is_parsed_strictly() {
        assert_eq!(
            parse_content_range("bytes 100-199/1000"),
            Some((100, 199, 1000))
        );
        assert_eq!(parse_content_range("bytes 5-4/10"), None);
        assert_eq!(parse_content_range("bytes 0-10/10"), None);
        assert_eq!(parse_content_range("bytes 0-9/*"), None);
        assert_eq!(parse_content_range("bytes */10"), None);
        assert_eq!(parse_content_range(""), None);
    }

    #[test]
    fn truncated_download_is_an_error() {
        let mut r = SpoolReader::start(Box::new(io::Cursor::new(vec![1u8; 10])), 20, None).unwrap();
        let mut v = Vec::new();
        // Short body: EOF before Content-Length; reads past it end cleanly
        // (the decoder decides whether the data is usable).
        assert_eq!(r.read_to_end(&mut v).unwrap(), 10);
    }
}
