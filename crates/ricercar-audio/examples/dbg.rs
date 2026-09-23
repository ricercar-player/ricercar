use ricercar_audio::player::spawn_player;

fn main() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let out = std::env::temp_dir().join("ricercar-dbg2.raw");
    let _ = std::fs::remove_file(&out);
    let h = spawn_player(&format!("file:{}", out.display()));
    let sub = h.subscribe();
    h.load(format!("file://{}", dir.join("tone_16_441.flac").display()));
    h.enqueue_next(format!("file://{}", dir.join("tone_24_96.flac").display()));
    let start = std::time::Instant::now();
    while start.elapsed().as_secs() < 3 {
        match sub.0.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(ev) => println!("ev: {ev:?}"),
            Err(_) => {}
        }
    }
    println!("out len: {:?}", std::fs::metadata(&out).map(|m| m.len()));
}
