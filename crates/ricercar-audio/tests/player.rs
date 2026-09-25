//! End-to-end engine tests on `file:` sinks (and ALSA's `null` plugin), never
//! on real hardware.
//!
//! Golden references are ffmpeg decodes: `golden_16_*` is `-f s16le`, the
//! 24-bit ones are `-f s32le` (content MSB-aligned, low byte zero). The file
//! sink writes exactly what ALSA would receive for the negotiated container,
//! so each test re-encodes the golden *content* into that container with an
//! independent encoder below.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ricercar_audio::player::{EndReason, EngineEvent, PlayerHandle, TransportStatus};
use ricercar_audio::sink::device_info;
use ricercar_audio::{AudioError, AudioSink, Container, DeviceInfo, PcmFormat, TrackOpts};

fn fix(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn fixture_uri(name: &str) -> String {
    format!("file://{}", fix(name).display())
}

fn tmp_out(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("ricercar-test-{tag}-{}.raw", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

fn spawn_file_player_fmt(tag: &str, formats: Option<&str>) -> (PlayerHandle, PathBuf) {
    let out = tmp_out(tag);
    let spec = match formats {
        Some(f) => format!("file:{}?formats={f}", out.display()),
        None => format!("file:{}", out.display()),
    };
    (ricercar_audio::player::spawn_player(&spec), out)
}

fn spawn_file_player(tag: &str) -> (PlayerHandle, PathBuf) {
    spawn_file_player_fmt(tag, None)
}

/// Run until status becomes Stopped (panics on engine error).
fn wait_finished(sub: &ricercar_audio::Subscriber, secs: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        match sub.0.recv_timeout(Duration::from_millis(200)) {
            Ok(EngineEvent::Status {
                status: TransportStatus::Stopped,
            }) => return true,
            Ok(EngineEvent::Error { message }) => panic!("engine error: {message}"),
            _ => {}
        }
    }
    false
}

/// Wait for the first event matching `f`, returning it.
fn wait_for<T>(
    sub: &ricercar_audio::Subscriber,
    secs: u64,
    mut f: impl FnMut(&EngineEvent) -> Option<T>,
) -> Option<T> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if let Ok(ev) = sub.0.recv_timeout(Duration::from_millis(100))
            && let Some(v) = f(&ev)
        {
            return Some(v);
        }
    }
    None
}

/// Content values (LSB-aligned) of an ffmpeg golden reference.
fn golden_content(name: &str, bits: u8) -> Vec<i32> {
    let raw = std::fs::read(fix(name)).expect("golden");
    match bits {
        16 => raw
            .chunks(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]) as i32)
            .collect(),
        24 => raw
            .chunks(4)
            .map(|b| {
                assert_eq!(b[0], 0, "24-bit golden must be MSB-aligned s32le");
                i32::from_le_bytes([b[0], b[1], b[2], b[3]]) >> 8
            })
            .collect(),
        _ => unreachable!(),
    }
}

/// Independent reference encoder for ALSA container layouts.
fn encode(content: &[i32], bits: u8, c: Container) -> Vec<u8> {
    let mut out = Vec::new();
    for &v in content {
        match c {
            Container::S16 => out.extend(((v << (16 - bits)) as i16).to_le_bytes()),
            // LSB-aligned in a 32-bit word, sign-extended.
            Container::S24 => out.extend((v << (24 - bits)).to_le_bytes()),
            Container::S24_3 => out.extend(&(v << (24 - bits)).to_le_bytes()[..3]),
            // MSB-aligned.
            Container::S32 => out.extend((v << (32 - bits)).to_le_bytes()),
        }
    }
    out
}

fn assert_same(got: &[u8], want: &[u8], what: &str) {
    assert_eq!(
        got.len(),
        want.len(),
        "length mismatch ({what}): got {} expected {}",
        got.len(),
        want.len()
    );
    let first_diff = got.iter().zip(want).position(|(a, b)| a != b);
    assert_eq!(first_diff, None, "first difference ({what})");
}

fn play_to_end(tag: &str, formats: Option<&str>, uris: &[&str]) -> (PlayerHandle, Vec<u8>) {
    let (h, out) = spawn_file_player_fmt(tag, formats);
    let sub = h.subscribe();
    h.load(fixture_uri(uris[0]));
    for u in &uris[1..] {
        h.enqueue_next(fixture_uri(u));
    }
    assert!(wait_finished(&sub, 20), "playback did not finish");
    let got = std::fs::read(&out).unwrap();
    let _ = std::fs::remove_file(&out);
    (h, got)
}

#[test]
fn bitexact_16_441() {
    let (h, got) = play_to_end("16", None, &["tone_16_441.flac"]);
    assert_same(
        &got,
        &std::fs::read(fix("golden_16_441.raw")).unwrap(),
        "S16",
    );
    let st = h.state.lock().unwrap();
    assert!(st.chain.bit_perfect, "file sink must report bit-perfect");
    assert_eq!(st.chain.container, Some("S16_LE"));
}

#[test]
fn bitexact_24_96_native_s24_le() {
    let (h, got) = play_to_end("24", None, &["tone_24_96.flac"]);
    let want = encode(&golden_content("golden_24_96.raw", 24), 24, Container::S24);
    assert_same(&got, &want, "S24_LE is LSB-aligned");
    let st = h.state.lock().unwrap();
    let fmt = st.chain.format.unwrap();
    assert_eq!((fmt.sample_rate, fmt.channels, fmt.bits), (96000, 2, 24));
    assert_eq!(st.chain.container, Some("S24_LE"));
}

#[test]
fn bitexact_24_96_on_s32_only_device() {
    let (h, got) = play_to_end("24s32", Some("S32_LE,S16_LE"), &["tone_24_96.flac"]);
    // S32 MSB-aligned is byte-identical to ffmpeg's s32le decode.
    assert_same(
        &got,
        &std::fs::read(fix("golden_24_96.raw")).unwrap(),
        "S32",
    );
    assert_eq!(h.state.lock().unwrap().chain.container, Some("S32_LE"));
}

#[test]
fn bitexact_24_96_on_packed_only_device() {
    let (h, got) = play_to_end("24p", Some("S24_3LE,S16_LE,S32_LE"), &["tone_24_96.flac"]);
    let want = encode(
        &golden_content("golden_24_96.raw", 24),
        24,
        Container::S24_3,
    );
    assert_same(&got, &want, "S24_3LE");
    assert_eq!(h.state.lock().unwrap().chain.container, Some("S24_3LE"));
}

#[test]
fn bitexact_16_padded_into_s32() {
    let (h, got) = play_to_end("16s32", Some("S32_LE"), &["tone_16_441.flac"]);
    let want = encode(&golden_content("golden_16_441.raw", 16), 16, Container::S32);
    assert_same(&got, &want, "16 in S32");
    let st = h.state.lock().unwrap();
    assert_eq!(st.chain.container, Some("S32_LE"));
    assert!(st.chain.bit_perfect, "zero-padding is still bit-perfect");
}

#[test]
fn device_without_lossless_container_refuses() {
    let (h, out) = spawn_file_player_fmt("refuse", Some("S16_LE"));
    let sub = h.subscribe();
    h.load(fixture_uri("tone_24_96.flac"));
    let msg = wait_for(&sub, 10, |e| match e {
        EngineEvent::Error { message } => Some(message.clone()),
        _ => None,
    })
    .expect("24-bit on a 16-bit-only device must be refused");
    assert!(msg.contains("24-bit"), "{msg}");
    assert!(std::fs::metadata(&out).map_or(0, |m| m.len()) == 0);
}

#[test]
fn gapless_same_rate() {
    let (_h, got) = play_to_end("gapless", None, &["gap_a.flac", "gap_b.flac"]);
    // Concatenated output must equal the independent ffmpeg decode of both
    // halves: nothing added (silence) and nothing lost at the seam.
    let want = encode(&golden_content("golden_gap.raw", 24), 24, Container::S24);
    assert_same(&got, &want, "gapless");
}

#[test]
fn rate_switch_concat() {
    let (_h, got) = play_to_end("rate", None, &["tone_16_441.flac", "tone_24_96.flac"]);
    let mut want = std::fs::read(fix("golden_16_441.raw")).unwrap();
    want.extend(encode(
        &golden_content("golden_24_96.raw", 24),
        24,
        Container::S24,
    ));
    assert_same(&got, &want, "sample-exact concatenation across rate change");
}

/// After `load` + `seek` the output must be exactly the golden from the
/// reported (actual) seek position onward.
fn check_seek_suffix(h: &PlayerHandle, out: &Path, uri: &str, golden: &[u8], frame: usize) {
    let sub = h.subscribe();
    h.load(uri);
    h.seek_ms(1234);
    let pos = wait_for(&sub, 10, |e| match e {
        EngineEvent::Position { pos_ms, .. } => Some(*pos_ms),
        _ => None,
    })
    .expect("position after seek");
    assert!(pos <= 1234 && pos > 1000, "actual seek position {pos}");
    assert!(wait_finished(&sub, 20));
    let got = std::fs::read(out).unwrap();
    assert!(got.len() < golden.len());
    assert_eq!(got.len() % frame, 0);
    let skipped_frames = (golden.len() - got.len()) / frame;
    let skipped_ms = skipped_frames as f64 * 1000.0 / 44100.0;
    assert!(
        (skipped_ms - pos as f64).abs() <= 1.0,
        "reported position {pos} is the actual landing point {skipped_ms}"
    );
    assert_same(&got, &golden[golden.len() - got.len()..], "seek suffix");
}

#[test]
fn seek_file_source_is_sample_exact() {
    let (h, out) = spawn_file_player("seek");
    let golden = std::fs::read(fix("golden_16_441.raw")).unwrap();
    check_seek_suffix(&h, &out, &fixture_uri("tone_16_441.flac"), &golden, 4);
}

#[test]
fn pause_resume_keeps_position_and_bits() {
    let (h, out) = spawn_file_player("pause");
    let sub = h.subscribe();
    h.load(fixture_uri("tone_16_441.flac"));
    h.pause();
    assert!(
        wait_for(&sub, 5, |e| matches!(
            e,
            EngineEvent::Status {
                status: TransportStatus::Paused
            }
        )
        .then_some(()))
        .is_some()
    );
    let paused_pos = h.state.lock().unwrap().pos_ms;
    let len1 = std::fs::metadata(&out).map_or(0, |m| m.len());
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        std::fs::metadata(&out).map_or(0, |m| m.len()),
        len1,
        "nothing written while paused"
    );
    assert_eq!(h.state.lock().unwrap().pos_ms, paused_pos);
    h.resume();
    let resumed = wait_for(&sub, 5, |e| match e {
        EngineEvent::Position { pos_ms, .. } => Some(*pos_ms),
        _ => None,
    })
    .unwrap();
    assert!(resumed >= paused_pos && resumed < paused_pos + 200);
    assert!(wait_finished(&sub, 20));
    assert_same(
        &std::fs::read(&out).unwrap(),
        &std::fs::read(fix("golden_16_441.raw")).unwrap(),
        "pause/resume",
    );
}

#[test]
fn track_ended_reasons() {
    let (h, _out) = spawn_file_player("reasons");
    let sub = h.subscribe();
    h.load(fixture_uri("tone_16_441.flac"));
    h.load(fixture_uri("gap_a.flac"));
    h.stop();
    let mut reasons = Vec::new();
    wait_for(&sub, 10, |e| {
        if let EngineEvent::TrackEnded { reason, .. } = e {
            reasons.push(*reason);
        }
        (reasons.len() == 2).then_some(())
    });
    assert_eq!(reasons, [EndReason::Replaced, EndReason::Stopped]);
    h.load(fixture_uri("tone_16_441.aiff"));
    let r = wait_for(&sub, 10, |e| match e {
        EngineEvent::TrackEnded { reason, .. } => Some(*reason),
        _ => None,
    });
    assert_eq!(r, Some(EndReason::Finished));
}

#[test]
fn lossy_decodes_and_completes() {
    for (i, name) in ["tone.mp3", "tone.aac", "tone.m4a"].iter().enumerate() {
        let (_h, got) = play_to_end(&format!("lossy{i}"), None, &[name]);
        assert!(got.len() > 100_000, "{name}: output unexpectedly short");
    }
}

#[test]
fn aiff_is_bit_exact() {
    let (_h, got) = play_to_end("aiff", None, &["tone_16_441.aiff"]);
    let golden = std::fs::read(fix("golden_16_441.raw")).unwrap();
    // The AIFF fixture is the first 0.5 s of the FLAC tone (16-bit big-endian
    // PCM: symphonia hands it over as an S16 buffer).
    assert_same(&got, &golden[..88200], "aiff");
}

#[test]
fn wav_24_is_bit_exact() {
    let (_h, got) = play_to_end("wav24", None, &["tone_24_96.wav"]);
    // First 0.25 s of the 24/96 FLAC tone, as packed 24-bit PCM.
    let content = golden_content("golden_24_96.raw", 24);
    assert_same(
        &got,
        &encode(&content[..48000], 24, Container::S24),
        "wav24",
    );
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
    assert!(wait_finished(&sub, 10));
}

#[test]
fn track_gain_is_applied_and_clears_bit_perfect() {
    let (h, out) = spawn_file_player("gain");
    let sub = h.subscribe();
    let opts = TrackOpts {
        gain_db: Some(-6.0),
        peak: None,
    };
    h.load_with(fixture_uri("tone_16_441.flac"), opts);
    assert!(wait_finished(&sub, 20));
    let f = 10f64.powf(-6.0 / 20.0);
    let want: Vec<i32> = golden_content("golden_16_441.raw", 16)
        .iter()
        .map(|&v| (v as f64 * f).round() as i32)
        .collect();
    assert_same(
        &std::fs::read(&out).unwrap(),
        &encode(&want, 16, Container::S16),
        "-6 dB",
    );
    let st = h.state.lock().unwrap();
    assert!(!st.chain.bit_perfect);
    assert_eq!(st.gain.track, opts);
}

#[test]
fn preamp_is_capped_by_peak() {
    let (h, out) = spawn_file_player("peak");
    let sub = h.subscribe();
    let content = golden_content("golden_16_441.raw", 16);
    let max_in = content.iter().map(|v| v.unsigned_abs()).max().unwrap();
    let peak = max_in as f32 / 32767.0;
    // +30 dB would clip most of this quiet tone; the peak caps it to exactly
    // full scale.
    h.set_gain(30.0);
    h.load_with(
        fixture_uri("tone_16_441.flac"),
        TrackOpts {
            gain_db: None,
            peak: Some(peak),
        },
    );
    assert!(wait_finished(&sub, 20));
    let got: Vec<i32> = std::fs::read(&out)
        .unwrap()
        .chunks(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]) as i32)
        .collect();
    let max_out = got.iter().map(|v| v.unsigned_abs()).max().unwrap();
    assert!(
        (32766..=32768).contains(&max_out),
        "peak after gain {max_out}"
    );
    let at_peak_in = content
        .iter()
        .filter(|v| v.unsigned_abs() == max_in)
        .count();
    let at_full_scale = got.iter().filter(|v| v.unsigned_abs() >= 32767).count();
    assert!(
        at_full_scale <= at_peak_in,
        "{at_full_scale} samples at full scale, only {at_peak_in} peaks in the source"
    );
    let st = h.state.lock().unwrap();
    assert!((st.gain.effective_db() - 20.0 * (1.0 / peak).log10()).abs() < 1e-3);
}

#[test]
fn mute_writes_silence() {
    let (h, out) = spawn_file_player("mute");
    let sub = h.subscribe();
    h.set_mute(true);
    h.load(fixture_uri("tone_16_441.flac"));
    assert!(wait_finished(&sub, 20));
    let got = std::fs::read(&out).unwrap();
    assert_eq!(got.len(), 352800, "device keeps running while muted");
    assert!(got.iter().all(|&b| b == 0));
    assert!(!h.state.lock().unwrap().chain.bit_perfect);
}

/// Sink that dies like an unplugged USB DAC after a few writes.
struct DyingSink {
    dev: DeviceInfo,
    fmt: Option<PcmFormat>,
    writes: Arc<Mutex<u32>>,
}

impl AudioSink for DyingSink {
    fn device(&self) -> &DeviceInfo {
        &self.dev
    }
    fn opened_format(&self) -> Option<PcmFormat> {
        self.fmt
    }
    fn opened_container(&self) -> Option<Container> {
        self.fmt.map(|f| Container::for_bits(f.bits))
    }
    fn open(&mut self, fmt: PcmFormat) -> ricercar_audio::Result<()> {
        self.fmt = Some(fmt);
        Ok(())
    }
    fn write_i32(&mut self, _s: &[i32]) -> ricercar_audio::Result<()> {
        let mut w = self.writes.lock().unwrap();
        *w += 1;
        if *w > 3 {
            return Err(AudioError::DeviceGone {
                device: self.dev.name.clone(),
            });
        }
        Ok(())
    }
    fn drain(&mut self) -> ricercar_audio::Result<()> {
        Ok(())
    }
    fn close(&mut self) {
        self.fmt = None;
    }
}

#[test]
fn unplugged_device_stops_cleanly() {
    let writes = Arc::new(Mutex::new(0));
    let h = ricercar_audio::spawn_player_with_sink(Box::new(DyingSink {
        dev: device_info("hw:9,0"),
        fmt: None,
        writes: writes.clone(),
    }));
    let sub = h.subscribe();
    h.load(fixture_uri("tone_16_441.flac"));
    let mut error = None;
    let mut reason = None;
    let stopped = wait_for(&sub, 10, |e| {
        match e {
            EngineEvent::Error { message } => error = Some(message.clone()),
            EngineEvent::TrackEnded { reason: r, .. } => reason = Some(*r),
            EngineEvent::Status {
                status: TransportStatus::Stopped,
            } => return Some(()),
            _ => {}
        }
        None
    });
    assert!(stopped.is_some());
    let error = error.expect("error event");
    assert!(
        error.contains("hw:9,0") && error.contains("unplugged"),
        "{error}"
    );
    assert_eq!(reason, Some(EndReason::Error));
    assert_eq!(*writes.lock().unwrap(), 4, "no writes after the failure");
    std::thread::sleep(Duration::from_millis(100));
    assert!(h.is_alive(), "engine survives the device loss");
    assert!(h.state.lock().unwrap().track_uri.is_none());
}

/// ALSA's `null` plugin: exercises the real ALSA sink (negotiation, pause,
/// resume, seek discard) without any hardware involved.
#[test]
fn alsa_null_plugin_pause_resume() {
    let h = ricercar_audio::player::spawn_player("null");
    let sub = h.subscribe();
    h.load(fixture_uri("tone_24_96.flac"));
    let started = wait_for(&sub, 5, |e| match e {
        EngineEvent::TrackStarted { .. } => Some(Ok(())),
        EngineEvent::Error { message } => Some(Err(message.clone())),
        _ => None,
    });
    match started {
        Some(Ok(())) => {}
        Some(Err(m)) if m.contains("null") => {
            eprintln!("ALSA null plugin unavailable, skipping: {m}");
            return;
        }
        other => panic!("unexpected: {other:?}"),
    }
    assert_eq!(h.state.lock().unwrap().chain.container, Some("S24_LE"));
    h.pause();
    h.seek_ms(500);
    h.resume();
    h.stop();
    assert!(wait_finished(&sub, 10));
}
