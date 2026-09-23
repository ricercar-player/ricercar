use std::path::{Path, PathBuf};
use std::time::Duration;

use ricercar_audio::player::{EngineEvent, PlayerHandle, TransportStatus};

fn fix(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn fixture_uri(name: &str) -> String {
    format!("file://{}", fix(name).display())
}

fn tmp_out(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("ricercar-test-{tag}.raw"));
    let _ = std::fs::remove_file(&p);
    p
}

fn spawn_file_player(tag: &str) -> (PlayerHandle, PathBuf) {
    let out = tmp_out(tag);
    let h = ricercar_audio::player::spawn_player(&format!("file:{}", out.display()));
    std::thread::sleep(Duration::from_millis(50));
    (h, out)
}

/// Run until status becomes Stopped (or error), return true if finished.
fn wait_finished(_h: &PlayerHandle, sub: &ricercar_audio::Subscriber, secs: u64) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    loop {
        match sub.0.recv_timeout(Duration::from_millis(200)) {
            Ok(EngineEvent::Status {
                status: TransportStatus::Stopped,
            }) => return true,
            Ok(EngineEvent::Error { message }) => {
                panic!("engine error: {message}")
            }
            Ok(_) => {}
            Err(_) => {
                if std::time::Instant::now() > deadline {
                    return false;
                }
            }
        }
    }
}

fn assert_eq_bytes(got: &Path, golden: &str) {
    let g = std::fs::read(got).unwrap_or_else(|e| panic!("read {}: {e}", got.display()));
    let w = std::fs::read(fix(golden)).expect("golden");
    assert_eq!(
        g.len(),
        w.len(),
        "length mismatch vs {golden}: got {} expected {}",
        g.len(),
        w.len()
    );
    let first_diff = g.iter().zip(w.iter()).position(|(a, b)| a != b);
    assert_eq!(
        first_diff, None,
        "first difference vs {golden} at {first_diff:?}"
    );
}

#[test]
fn bitexact_16_441() {
    let (h, out) = spawn_file_player("16");
    let sub = h.subscribe();
    h.load(fixture_uri("tone_16_441.flac"));
    assert!(wait_finished(&h, &sub, 20), "track did not finish");
    assert_eq_bytes(&out, "golden_16_441.raw");
    let st = h.state.lock().unwrap();
    assert!(st.chain.bit_perfect, "file sink must report bit-perfect");
}

#[test]
fn bitexact_24_96() {
    let (h, out) = spawn_file_player("24");
    let sub = h.subscribe();
    h.load(fixture_uri("tone_24_96.flac"));
    assert!(wait_finished(&h, &sub, 20), "track did not finish");
    assert_eq_bytes(&out, "golden_24_96.raw");
    let fmt = h.state.lock().unwrap().chain.format.unwrap();
    assert_eq!((fmt.sample_rate, fmt.channels, fmt.bits), (96000, 2, 24));
}

#[test]
fn gapless_same_rate() {
    let (h, out) = spawn_file_player("gapless");
    let sub = h.subscribe();
    h.load(fixture_uri("gap_a.flac"));
    h.enqueue_next(fixture_uri("gap_b.flac"));
    assert!(wait_finished(&h, &sub, 20), "playlist did not finish");
    // Concatenated output must equal the independent ffmpeg decode of both
    // halves: nothing added (silence) and nothing lost at the seam.
    assert_eq_bytes(&out, "golden_gap.raw");
}

#[test]
fn rate_switch_concat() {
    let (h, out) = spawn_file_player("rate");
    let sub = h.subscribe();
    h.load(fixture_uri("tone_16_441.flac"));
    h.enqueue_next(fixture_uri("tone_24_96.flac"));
    assert!(wait_finished(&h, &sub, 20), "playlist did not finish");
    let got = std::fs::read(&out).unwrap();
    let mut want = std::fs::read(fix("golden_16_441.raw")).unwrap();
    want.extend(std::fs::read(fix("golden_24_96.raw")).unwrap());
    assert_eq!(got.len(), want.len());
    assert_eq!(got, want, "sample-exact concatenation across rate change");
}

#[test]
fn seek_file_source() {
    let (h, _out) = spawn_file_player("seek");
    let sub = h.subscribe();
    h.load(fixture_uri("tone_16_441.flac"));
    // Queued right behind Load so the engine seeks the freshly opened track.
    h.seek_ms(1500);
    let mut saw_pos = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if let Ok(EngineEvent::Position { pos_ms, .. }) =
            sub.0.recv_timeout(Duration::from_millis(300))
            && pos_ms >= 1400
        {
            saw_pos = true;
            break;
        }
    }
    assert!(saw_pos, "position should pass the seek target");
    h.stop();
    wait_finished(&h, &sub, 10);
}

#[test]
fn lossy_decodes_and_completes() {
    let (h, out) = spawn_file_player("mp3");
    let sub = h.subscribe();
    h.load(fixture_uri("tone.mp3"));
    assert!(wait_finished(&h, &sub, 20), "mp3 did not finish");
    let got = std::fs::read(&out).unwrap();
    assert!(got.len() > 100_000, "mp3 output unexpectedly short");
}

#[test]
fn volume_is_not_bitperfect() {
    let (h, _out) = spawn_file_player("vol");
    let sub = h.subscribe();
    h.set_volume(50);
    h.load(fixture_uri("tone_16_441.flac"));
    std::thread::sleep(Duration::from_millis(300));
    {
        let st = h.state.lock().unwrap();
        assert!(!st.chain.bit_perfect, "volume processing must clear flag");
    }
    h.stop();
    assert!(wait_finished(&h, &sub, 10));
}
