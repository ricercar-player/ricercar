use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use ricercar_audio::player::{EngineEvent, TransportStatus};

/// Serve one fixture over plain HTTP/1.1 on an ephemeral port; returns the URI.
fn http_serve_fixture(name: &str) -> String {
    let body = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name),
    )
    .unwrap();
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in l.incoming().flatten() {
            let mut stream = stream;
            let mut req = [0u8; 2048];
            let _ = stream.read(&mut req);
            let head = format!(
                "HTTP/1.1 200 OK\r\nCONTENT-TYPE: audio/flac\r\nCONTENT-LENGTH: {}\r\nCONNECTION: close\r\n\r\n",
                body.len()
            );
            if stream.write_all(head.as_bytes()).is_ok() {
                let _ = stream.write_all(&body);
                let _ = stream.flush();
            }
            // keep the stream open briefly so the client can drain it, then close
            thread::sleep(Duration::from_millis(200));
        }
    });
    format!("http://127.0.0.1:{port}/tone.flac")
}

#[test]
fn http_source_plays_bit_exact() {
    let uri = http_serve_fixture("tone_16_441.flac");
    let out: PathBuf = std::env::temp_dir().join("ricercar-test-http.raw");
    let _ = std::fs::remove_file(&out);
    let h = ricercar_audio::player::spawn_player(&format!("file:{}", out.display()));
    std::thread::sleep(Duration::from_millis(50));
    let sub = h.subscribe();
    h.load(&uri);

    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let mut finished = false;
    while std::time::Instant::now() < deadline {
        match sub.0.recv_timeout(Duration::from_millis(200)) {
            Ok(EngineEvent::Status {
                status: TransportStatus::Stopped,
            }) => {
                finished = true;
                break;
            }
            Ok(EngineEvent::Error { message }) => panic!("engine error: {message}"),
            Ok(_) => {}
            Err(_) => {}
        }
    }
    assert!(finished, "http track never finished");
    let got = std::fs::read(&out).unwrap();
    let golden = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join("golden_16_441.raw"),
    )
    .unwrap();
    assert_eq!(got, golden, "http-decoded output must match golden");
    h.stop();
}
