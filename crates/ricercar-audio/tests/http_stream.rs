use std::io::{Read, Write};
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
            Ok(EngineEvent::Error { message }) => panic!("engine error: {message}"),
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
