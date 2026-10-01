use std::io::{Read, Seek, SeekFrom, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ricercar_audio::Subscriber;
use ricercar_audio::player::{EngineEvent, PlayerHandle, TransportStatus};

#[derive(Clone, Copy)]
enum Mode {
    /// Plain file with Content-Length (spooled, seekable).
    Sized,
    /// No Content-Length (live, forward-only).
    Unsized,
    /// Internet radio: ICY metadata every `metaint` bytes.
    Icy { metaint: usize },
    /// The server refuses: `410 Gone` (an expired signed URL).
    Gone,
    /// Content-Length far beyond what may be spooled to disk.
    Huge,
    /// The connection drops halfway through a sized body.
    Cut,
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name),
    )
    .unwrap()
}

fn icy_interleave(body: &[u8], metaint: usize, titles: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, chunk) in body.chunks(metaint).enumerate() {
        out.extend_from_slice(chunk);
        if chunk.len() < metaint {
            break;
        }
        match titles.get(i) {
            Some(t) => {
                let mut m = format!("StreamTitle='{t}';StreamUrl='';").into_bytes();
                m.resize(m.len().div_ceil(16) * 16, 0);
                out.push((m.len() / 16) as u8);
                out.extend(m);
            }
            None => out.push(0),
        }
    }
    out
}

/// Serve one fixture over plain HTTP/1.1 on an ephemeral port. Returns the
/// URI and the captured request heads.
fn serve(name: &str, mode: Mode) -> (String, Arc<Mutex<Vec<String>>>) {
    let body = fixture(name);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let reqs = requests.clone();
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in l.incoming().flatten() {
            let mut stream = stream;
            let mut req = [0u8; 4096];
            let n = stream.read(&mut req).unwrap_or(0);
            reqs.lock()
                .unwrap()
                .push(String::from_utf8_lossy(&req[..n]).to_lowercase());
            let (head, payload) = match mode {
                Mode::Sized => (
                    format!(
                        "HTTP/1.1 200 OK\r\nCONTENT-TYPE: audio/flac\r\nCONTENT-LENGTH: {}\r\nCONNECTION: close\r\n\r\n",
                        body.len()
                    ),
                    body.clone(),
                ),
                Mode::Unsized => (
                    "HTTP/1.1 200 OK\r\nContent-Type: audio/flac\r\nConnection: close\r\n\r\n"
                        .to_string(),
                    body.clone(),
                ),
                Mode::Huge => (
                    "HTTP/1.1 200 OK\r\nContent-Type: audio/flac\r\nContent-Length: 1000000000000000\r\nConnection: close\r\n\r\n"
                        .to_string(),
                    body.clone(),
                ),
                Mode::Cut => (
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: audio/flac\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    ),
                    body[..body.len() / 2].to_vec(),
                ),
                Mode::Gone => (
                    "HTTP/1.1 410 Gone\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_string(),
                    Vec::new(),
                ),
                Mode::Icy { metaint } => (
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: audio/flac\r\nicy-name: Test FM\r\nicy-metaint: {metaint}\r\nConnection: close\r\n\r\n"
                    ),
                    icy_interleave(&body, metaint, &["Artist - First", "Artist - Second"]),
                ),
            };
            if stream.write_all(head.as_bytes()).is_ok() {
                // Trickle the body so playback overlaps the download.
                for c in payload.chunks(16 * 1024) {
                    if stream.write_all(c).is_err() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(1));
                }
                let _ = stream.flush();
            }
            thread::sleep(Duration::from_millis(200));
        }
    });
    (format!("http://127.0.0.1:{port}/{name}"), requests)
}

fn file_player(tag: &str) -> (PlayerHandle, PathBuf) {
    let out = std::env::temp_dir().join(format!(
        "ricercar-test-http-{tag}-{}.raw",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&out);
    let h = ricercar_audio::player::spawn_player(&format!("file:{}", out.display()));
    (h, out)
}

fn wait_stopped(sub: &Subscriber, mut on: impl FnMut(&EngineEvent)) -> bool {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        match sub.0.recv_timeout(Duration::from_millis(200)) {
            Ok(EngineEvent::Status {
                status: TransportStatus::Stopped,
            }) => return true,
            Ok(EngineEvent::Error { message, .. }) => panic!("engine error: {message}"),
            Ok(ev) => on(&ev),
            Err(_) => {}
        }
    }
    false
}

fn golden() -> Vec<u8> {
    fixture("golden_16_441.raw")
}

#[test]
fn http_source_plays_bit_exact() {
    let (uri, reqs) = serve("tone_16_441.flac", Mode::Sized);
    let (h, out) = file_player("sized");
    let sub = h.subscribe();
    h.load(&uri);
    let mut seekable = None;
    assert!(
        wait_stopped(&sub, |ev| if let EngineEvent::TrackStarted { .. } = ev {
            seekable = Some(h.state.lock().unwrap().seekable);
        }),
        "http track never finished"
    );
    assert_eq!(seekable, Some(true), "Content-Length => seekable");
    assert_eq!(std::fs::read(&out).unwrap(), golden());
    let req = reqs.lock().unwrap()[0].clone();
    assert!(
        req.contains(&format!(
            "user-agent: ricercar/{}",
            env!("CARGO_PKG_VERSION")
        )),
        "{req}"
    );
}

#[test]
fn http_seek_is_sample_exact() {
    let (uri, _) = serve("tone_16_441.flac", Mode::Sized);
    let (h, out) = file_player("seek");
    let sub = h.subscribe();
    h.load(&uri);
    h.seek_ms(1500);
    let mut pos = None;
    assert!(wait_stopped(&sub, |ev| {
        if let EngineEvent::Position { pos_ms, .. } = ev {
            pos.get_or_insert(*pos_ms);
        }
    }));
    let pos = pos.expect("position after seek");
    assert!(pos > 1300 && pos <= 1500, "landed at {pos}");
    let got = std::fs::read(&out).unwrap();
    let g = golden();
    assert!(!got.is_empty() && got.len() < g.len());
    assert_eq!(got, g[g.len() - got.len()..], "output is the golden tail");
    let skipped_ms = ((g.len() - got.len()) / 4) as f64 * 1000.0 / 44100.0;
    assert!(
        (skipped_ms - pos as f64).abs() <= 1.0,
        "pos {pos} skipped {skipped_ms}"
    );
}

#[test]
fn http_live_stream_is_forward_only_and_bit_exact() {
    let (uri, _) = serve("tone_16_441.flac", Mode::Unsized);
    let (h, out) = file_player("live");
    let sub = h.subscribe();
    h.load(&uri);
    let mut seekable = None;
    assert!(wait_stopped(&sub, |ev| {
        if let EngineEvent::TrackStarted { .. } = ev {
            seekable = Some(h.state.lock().unwrap().seekable);
            h.seek_ms(1000); // ignored on a live stream
        }
    }));
    assert_eq!(seekable, Some(false));
    assert_eq!(std::fs::read(&out).unwrap(), golden());
}

#[test]
fn icy_metadata_is_stripped_and_published() {
    let (uri, reqs) = serve("tone_16_441.flac", Mode::Icy { metaint: 8192 });
    let (h, out) = file_player("icy");
    let sub = h.subscribe();
    h.load(&uri);
    let mut titles = Vec::new();
    assert!(wait_stopped(&sub, |ev| {
        if let EngineEvent::StreamTitle { title } = ev {
            titles.push(title.clone());
        }
    }));
    assert_eq!(
        std::fs::read(&out).unwrap(),
        golden(),
        "metadata leaked into audio"
    );
    assert!(!titles.is_empty(), "no StreamTitle published");
    assert!(
        titles.iter().all(|t| t.starts_with("Artist - ")),
        "{titles:?}"
    );
    assert!(reqs.lock().unwrap()[0].contains("icy-metadata: 1"));
}

#[test]
fn http_error_status_is_reported() {
    let (uri, _) = serve("tone_16_441.flac", Mode::Gone);
    let (h, _out) = file_player("gone");
    let sub = h.subscribe();
    h.load(&uri);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut got = None;
    while Instant::now() < deadline && got.is_none() {
        if let Ok(EngineEvent::Error {
            uri: u,
            http_status,
            ..
        }) = sub.0.recv_timeout(Duration::from_millis(200))
        {
            got = Some((u, http_status));
        }
    }
    assert_eq!(got, Some((Some(uri), Some(410))));
}

#[test]
fn oversized_body_is_streamed_not_spooled() {
    let (uri, _) = serve("tone_16_441.flac", Mode::Huge);
    let (h, out) = file_player("huge");
    let sub = h.subscribe();
    h.load(&uri);
    let mut seekable = None;
    assert!(wait_stopped(&sub, |ev| {
        if let EngineEvent::TrackStarted { .. } = ev {
            seekable = Some(h.state.lock().unwrap().seekable);
        }
    }));
    assert_eq!(seekable, Some(false));
    assert_eq!(std::fs::read(&out).unwrap(), golden());
}

#[test]
fn dropped_connection_is_an_error_not_an_end() {
    let (uri, _) = serve("tone_16_441.flac", Mode::Cut);
    let (h, _out) = file_player("cut");
    let sub = h.subscribe();
    h.load(&uri);
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut error = None;
    let mut reason = None;
    while Instant::now() < deadline && reason.is_none() {
        match sub.0.recv_timeout(Duration::from_millis(200)) {
            Ok(EngineEvent::Error { message, .. }) => error = Some(message),
            Ok(EngineEvent::TrackEnded { reason: r, .. }) => reason = Some(r),
            _ => {}
        }
    }
    assert_eq!(reason, Some(ricercar_audio::EndReason::Error));
    assert!(error.is_some_and(|m| m.contains("http read")));
}

// ---------------------------------------------------------------- ranges

#[derive(Clone, Copy, PartialEq)]
enum Ranges {
    /// `Accept-Ranges: bytes`, ranges answered with 206.
    Honour,
    /// Advertises ranges but answers every request with the full body.
    Ignore,
    /// No `Accept-Ranges` header.
    Absent,
    /// Honours ranges, but a range response stops halfway.
    Cut,
}

/// One served request: its `Range` start, and the body bytes written.
#[derive(Clone, Debug)]
struct Served {
    from: Option<u64>,
    sent: u64,
}

const MIB: usize = 1024 * 1024;

fn pattern(n: usize) -> Vec<u8> {
    (0..n as u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect()
}

fn read_head(stream: &mut std::net::TcpStream) -> String {
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") && stream.read(&mut b).unwrap_or(0) == 1 {
        head.push(b[0]);
    }
    String::from_utf8_lossy(&head).to_lowercase()
}

/// Serve `body` with one thread per connection (a range request arrives
/// while the first response is still open). The body is trickled at about
/// 3 MB/s so that a seek lands beyond the downloaded data.
fn serve_ranges(body: Vec<u8>, mode: Ranges) -> (String, Arc<Mutex<Vec<Served>>>) {
    let body = Arc::new(body);
    let log = Arc::new(Mutex::new(Vec::<Served>::new()));
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let lg = log.clone();
    thread::spawn(move || {
        for stream in l.incoming().flatten() {
            let (body, log) = (body.clone(), lg.clone());
            thread::spawn(move || {
                let mut stream = stream;
                let head = read_head(&mut stream);
                let from = head
                    .lines()
                    .find_map(|l| l.strip_prefix("range: bytes="))
                    .and_then(|r| r.trim().trim_end_matches('-').parse::<u64>().ok());
                let idx = {
                    let mut log = log.lock().unwrap();
                    log.push(Served { from, sent: 0 });
                    log.len() - 1
                };
                let len = body.len() as u64;
                let accept = if mode == Ranges::Absent {
                    ""
                } else {
                    "Accept-Ranges: bytes\r\n"
                };
                let (head, part) = match from {
                    Some(f) if mode == Ranges::Honour || mode == Ranges::Cut => {
                        let part = &body[f as usize..];
                        let shown = if mode == Ranges::Cut {
                            &part[..part.len() / 2]
                        } else {
                            part
                        };
                        (
                            format!(
                                "HTTP/1.1 206 Partial Content\r\n{accept}Content-Range: bytes {f}-{}/{len}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                len - 1,
                                part.len()
                            ),
                            shown,
                        )
                    }
                    _ => (
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\n{accept}Content-Length: {len}\r\nConnection: close\r\n\r\n"
                        ),
                        &body[..],
                    ),
                };
                if stream.write_all(head.as_bytes()).is_err() {
                    return;
                }
                for c in part.chunks(16 * 1024) {
                    if stream.write_all(c).is_err() {
                        return;
                    }
                    log.lock().unwrap()[idx].sent += c.len() as u64;
                    thread::sleep(Duration::from_millis(5));
                }
                let _ = stream.flush();
            });
        }
    });
    (format!("http://127.0.0.1:{port}/big.wav"), log)
}

fn served(log: &Arc<Mutex<Vec<Served>>>) -> Vec<Served> {
    log.lock().unwrap().clone()
}

#[test]
fn range_seek_fetches_from_the_seek_point() {
    let data = pattern(4 * MIB);
    let seek = 3 * MIB as u64;
    let (uri, log) = serve_ranges(data.clone(), Ranges::Honour);
    let mut src = ricercar_audio::http::open(&uri).unwrap().source;
    src.seek(SeekFrom::Start(seek)).unwrap();
    let mut tail = Vec::new();
    src.read_to_end(&mut tail).unwrap();
    assert!(tail == data[seek as usize..], "tail differs");
    let s = served(&log);
    assert_eq!(s[0].from, None);
    assert!(s[0].sent < seek, "first response ran to the seek: {s:?}");
    assert_eq!(s[1].from, Some(seek), "{s:?}");
    assert!(s.iter().all(|r| r.from.is_none_or(|f| f <= seek)), "{s:?}");

    // Back into the downloaded tail: served from disk, no request there.
    src.seek(SeekFrom::Start(seek + 12_345)).unwrap();
    let mut again = vec![0u8; 100_000];
    src.read_exact(&mut again).unwrap();
    assert!(again == data[seek as usize + 12_345..][..100_000]);
    assert!(
        served(&log)
            .iter()
            .all(|r| r.from.is_none_or(|f| f <= seek)),
        "{:?}",
        served(&log)
    );

    // The hole before the seek point is filled; the file ends up whole.
    src.seek(SeekFrom::Start(0)).unwrap();
    let mut all = Vec::new();
    src.read_to_end(&mut all).unwrap();
    assert!(all == data, "whole file differs");
    let s = served(&log);
    assert_eq!(s.len(), 3, "{s:?}");
    assert!(s[2].from.is_some_and(|f| f < seek), "{s:?}");
    let total: u64 = s.iter().map(|r| r.sent).sum();
    assert!(total <= data.len() as u64 + MIB as u64, "{s:?}");
}

#[test]
fn range_seek_back_into_the_downloaded_head_needs_no_request() {
    let data = pattern(2 * MIB);
    let (uri, log) = serve_ranges(data.clone(), Ranges::Honour);
    let mut src = ricercar_audio::http::open(&uri).unwrap().source;
    let mut head = vec![0u8; 200_000];
    src.read_exact(&mut head).unwrap();
    src.seek(SeekFrom::Start(1000)).unwrap();
    src.read_exact(&mut head).unwrap();
    assert!(head == data[1000..201_000]);
    let mut rest = Vec::new();
    src.read_to_end(&mut rest).unwrap();
    assert_eq!(served(&log).len(), 1, "{:?}", served(&log));
}

#[test]
fn range_refused_falls_back_to_sequential() {
    let data = pattern(3 * MIB);
    let seek = 2 * MIB as u64;
    let (uri, log) = serve_ranges(data.clone(), Ranges::Ignore);
    let mut src = ricercar_audio::http::open(&uri).unwrap().source;
    src.seek(SeekFrom::Start(seek)).unwrap();
    let mut tail = Vec::new();
    src.read_to_end(&mut tail).unwrap();
    assert!(tail == data[seek as usize..], "tail differs");
    src.seek(SeekFrom::Start(0)).unwrap();
    let mut all = Vec::new();
    src.read_to_end(&mut all).unwrap();
    assert!(all == data, "whole file differs");
    let s = served(&log);
    assert_eq!(s.len(), 2, "one range attempt, then none: {s:?}");
    assert_eq!(s[1].from, Some(seek));
    assert_eq!(s[0].sent, data.len() as u64, "first response kept: {s:?}");
}

#[test]
fn no_accept_ranges_keeps_one_sequential_request() {
    let data = pattern(3 * MIB);
    let seek = 2 * MIB as u64;
    let (uri, log) = serve_ranges(data.clone(), Ranges::Absent);
    let mut src = ricercar_audio::http::open(&uri).unwrap().source;
    src.seek(SeekFrom::Start(seek)).unwrap();
    let mut tail = Vec::new();
    src.read_to_end(&mut tail).unwrap();
    assert!(tail == data[seek as usize..], "tail differs");
    let s = served(&log);
    assert_eq!(s.len(), 1, "{s:?}");
    assert_eq!(s[0].from, None);
}

#[test]
fn range_cut_midway_is_an_error() {
    let data = pattern(4 * MIB);
    let seek = 3 * MIB as u64;
    let (uri, _log) = serve_ranges(data.clone(), Ranges::Cut);
    let mut src = ricercar_audio::http::open(&uri).unwrap().source;
    src.seek(SeekFrom::Start(seek)).unwrap();
    let mut got = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    let err = loop {
        match src.read(&mut buf) {
            Ok(0) => panic!("clean end after {} bytes", got.len()),
            Ok(n) => got.extend_from_slice(&buf[..n]),
            Err(e) => break e,
        }
    };
    assert!(err.to_string().contains("http read"), "{err}");
    assert!(got.len() >= MIB / 2 - 64 * 1024, "{}", got.len());
    assert!(got == data[seek as usize..][..got.len()]);
}

/// 16-bit stereo 44.1 kHz WAV around `pcm`.
fn wav(pcm: &[u8]) -> Vec<u8> {
    let mut w = Vec::with_capacity(pcm.len() + 44);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&44_100u32.to_le_bytes());
    w.extend_from_slice(&(44_100u32 * 4).to_le_bytes());
    w.extend_from_slice(&4u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    w.extend_from_slice(pcm);
    w
}

#[test]
fn range_seek_plays_bit_exact() {
    let pcm = pattern(44_100 * 4 * 24);
    let (uri, log) = serve_ranges(wav(&pcm), Ranges::Honour);
    let (h, out) = file_player("range");
    let sub = h.subscribe();
    h.load(&uri);
    h.seek_ms(20_000);
    assert!(wait_stopped(&sub, |_| {}), "range track never finished");
    let got = std::fs::read(&out).unwrap();
    assert!(!got.is_empty() && got.len() < pcm.len());
    assert!(
        got == pcm[pcm.len() - got.len()..],
        "output is not the tail"
    );
    let skipped_ms = ((pcm.len() - got.len()) / 4) as f64 * 1000.0 / 44_100.0;
    // Resumes on the packet boundary at or before the request.
    assert!(
        (19_900.0..=20_000.0).contains(&skipped_ms),
        "skipped {skipped_ms}"
    );
    let s = served(&log);
    let seek_at = 44 + (pcm.len() - got.len()) as u64;
    assert!(s[0].sent < seek_at, "{s:?}");
    assert!(
        s.iter()
            .any(|r| r.from.is_some_and(|f| f <= seek_at && f > 2 * MIB as u64)),
        "{s:?}"
    );
}

// ---------------------------------------------------------------- client

/// Answer every connection with `respond(path, request head)`.
fn serve_fn(respond: impl Fn(&str, &str) -> Vec<u8> + Send + Sync + 'static) -> String {
    let respond = Arc::new(respond);
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in l.incoming().flatten() {
            let respond = respond.clone();
            thread::spawn(move || {
                let mut stream = stream;
                let head = read_head(&mut stream);
                let path = head.split(' ').nth(1).unwrap_or("/").to_string();
                let out = respond(&path, &head);
                for c in out.chunks(16 * 1024) {
                    if stream.write_all(c).is_err() {
                        return;
                    }
                }
                let _ = stream.flush();
            });
        }
    });
    format!("http://127.0.0.1:{port}")
}

fn with_headers(status: &str, extra: &[String], body: &[u8]) -> Vec<u8> {
    let mut out = format!("{status}\r\n").into_bytes();
    for h in extra {
        out.extend(h.as_bytes());
        out.extend(b"\r\n");
    }
    out.extend(b"Connection: close\r\n\r\n");
    out.extend(body);
    out
}

fn debug_headers(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("X-Debug: {i}")).collect()
}

fn engine_error(uri: &str) -> (String, Option<String>, Option<u16>) {
    let (h, _out) = file_player("err");
    let sub = h.subscribe();
    h.load(uri);
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if let Ok(EngineEvent::Error {
            message,
            uri,
            http_status,
        }) = sub.0.recv_timeout(Duration::from_millis(200))
        {
            return (message, uri, http_status);
        }
    }
    panic!("no engine error for {uri}");
}

#[test]
fn many_header_lines_are_accepted_and_seekable() {
    let data = wav(&pattern(300_000));
    let len = data.len();
    let base = serve_fn(move |_, _| {
        let mut h = debug_headers(150);
        h.push(format!("Content-Length: {len}"));
        h.push("Accept-Ranges: bytes".into());
        h.push("Content-Type: audio/wav".into());
        with_headers("HTTP/1.1 200 OK", &h, &data)
    });
    let data = wav(&pattern(300_000));
    let src = ricercar_audio::http::open(&format!("{base}/many.wav")).unwrap();
    assert!(src.seekable);
    let mut src = src.source;
    src.seek(SeekFrom::End(-5000)).unwrap();
    let mut tail = Vec::new();
    src.read_to_end(&mut tail).unwrap();
    assert!(tail == data[data.len() - 5000..], "tail differs");
    src.seek(SeekFrom::Start(0)).unwrap();
    let mut all = Vec::new();
    src.read_to_end(&mut all).unwrap();
    assert!(all == data, "whole file differs");
}

#[test]
fn too_many_header_lines_is_a_clear_error() {
    let base = serve_fn(|_, _| {
        let mut h = debug_headers(600);
        h.push("Content-Length: 4".into());
        with_headers("HTTP/1.1 200 OK", &h, b"RIFF")
    });
    let uri = format!("{base}/x.flac");
    let e = ricercar_audio::http::open(&uri).err().unwrap().to_string();
    // 600 + Content-Length + Connection.
    assert_eq!(e, "response from 127.0.0.1 has too many headers (602)");
    let (message, _, status) = engine_error(&uri);
    assert!(message.contains("too many headers (602)"), "{message}");
    assert!(!message.contains("unsupported source"), "{message}");
    assert_eq!(status, None);
}

#[test]
fn malformed_response_is_a_clear_error() {
    let base = serve_fn(|_, _| b"SPEAK 200 PLEASE\r\n\r\n".to_vec());
    let e = ricercar_audio::http::open(&format!("{base}/x.flac"))
        .err()
        .unwrap()
        .to_string();
    assert!(
        e.starts_with("malformed HTTP response from 127.0.0.1"),
        "{e}"
    );
}

/// `Transfer-Encoding: chunked` with chunks of varied sizes.
fn chunked(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rest = body;
    let mut size = 1;
    while !rest.is_empty() {
        let n = size.min(rest.len());
        out.extend(format!("{n:x};x=y\r\n").as_bytes());
        out.extend(&rest[..n]);
        out.extend(b"\r\n");
        rest = &rest[n..];
        size = size * 7 % 40_000 + 1;
    }
    out.extend(b"0\r\nX-Trailer: 1\r\n\r\n");
    out
}

#[test]
fn chunked_stream_of_unknown_length_plays_bit_exact() {
    let body = fixture("tone_16_441.flac");
    let base = serve_fn(move |_, _| {
        with_headers(
            "HTTP/1.1 200 OK",
            &[
                "Content-Type: audio/flac".into(),
                "Transfer-Encoding: chunked".into(),
            ],
            &chunked(&body),
        )
    });
    let (h, out) = file_player("chunked");
    let sub = h.subscribe();
    h.load(format!("{base}/tone.flac"));
    let mut seekable = None;
    assert!(wait_stopped(&sub, |ev| {
        if let EngineEvent::TrackStarted { .. } = ev {
            seekable = Some(h.state.lock().unwrap().seekable);
        }
    }));
    assert_eq!(seekable, Some(false));
    assert_eq!(std::fs::read(&out).unwrap(), golden());
}

#[test]
fn icy_status_line_is_accepted() {
    let body = fixture("tone_16_441.flac");
    let base = serve_fn(move |_, head| {
        assert!(head.contains("icy-metadata: 1"));
        with_headers(
            "ICY 200 OK",
            &["icy-name: Test FM".into(), "icy-metaint: 4096".into()],
            &icy_interleave(&body, 4096, &["Artist - Only"]),
        )
    });
    let (h, out) = file_player("icyline");
    let sub = h.subscribe();
    h.load(format!("{base}/stream"));
    let mut titles = Vec::new();
    assert!(wait_stopped(&sub, |ev| {
        if let EngineEvent::StreamTitle { title } = ev {
            titles.push(title.clone());
        }
    }));
    assert_eq!(std::fs::read(&out).unwrap(), golden());
    assert_eq!(titles, ["Artist - Only"]);
}

#[test]
fn redirect_chain_is_followed() {
    let body = fixture("tone_16_441.flac");
    let port = Arc::new(Mutex::new(String::new()));
    let p = port.clone();
    let base = serve_fn(move |path, _| {
        let to =
            |status: &str, loc: String| with_headers(status, &[format!("Location: {loc}")], b"");
        match path {
            "/a" => to("HTTP/1.1 302 Found", "b".into()),
            "/b" => to("HTTP/1.1 301 Moved", format!("{}/c?k=v", p.lock().unwrap())),
            "/c?k=v" => to("HTTP/1.1 307 Temporary", "/sub/d".into()),
            "/sub/d" => to("HTTP/1.1 308 Permanent", "../final.flac".into()),
            "/final.flac" => with_headers(
                "HTTP/1.1 200 OK",
                &[format!("Content-Length: {}", body.len())],
                &body,
            ),
            "/loop" => to("HTTP/1.1 302 Found", "/loop".into()),
            _ => with_headers("HTTP/1.1 404 Not Found", &["Content-Length: 0".into()], b""),
        }
    });
    *port.lock().unwrap() = base.clone();
    let src = ricercar_audio::http::open(&format!("{base}/a")).unwrap();
    assert!(src.seekable);
    let mut got = Vec::new();
    { src.source }.read_to_end(&mut got).unwrap();
    assert!(got == fixture("tone_16_441.flac"));
    let e = ricercar_audio::http::open(&format!("{base}/loop"))
        .err()
        .unwrap()
        .to_string();
    assert_eq!(e, "more than 5 redirects");
}

#[test]
fn url_secrets_never_reach_error_messages() {
    let base = serve_fn(|path, _| {
        let status = if path.starts_with("/gone") {
            "HTTP/1.1 410 Gone"
        } else {
            "HTTP/1.1 200 OK"
        };
        with_headers(status, &debug_headers(700), b"")
    });
    for path in ["/gone?token=secret", "/x?token=secret&a=secret#secret"] {
        let uri = format!("{base}{path}");
        let e = ricercar_audio::http::open(&uri).err().unwrap().to_string();
        assert!(!e.contains("secret"), "{e}");
        let (message, u, _) = engine_error(&uri);
        assert!(!message.contains("secret"), "{message}");
        assert!(message.contains("token=…"), "{message}");
        // The event still carries the real URL for matching.
        assert_eq!(u.as_deref(), Some(uri.as_str()));
    }
    // Nothing listening there.
    let dead = TcpListener::bind("127.0.0.1:0").unwrap();
    let uri = format!(
        "http://user:secret@{}/x?token=secret",
        dead.local_addr().unwrap()
    );
    drop(dead);
    let (message, _, _) = engine_error(&uri);
    assert!(!message.contains("secret"), "{message}");
}
