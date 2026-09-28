//! Build a synthetic music library for load tests.
//!
//! ```sh
//! # 50 000 tracks written straight into <out>/library.db (UI measurements);
//! # folders with a cover.jpg each, no audio files.
//! cargo run --release -p ricercar-core --example synth_library -- --tracks 50000 --out /tmp/rc/big
//! # ~5 000 very short FLAC files under <out>/music (scan measurements).
//! cargo run --release -p ricercar-core --example synth_library -- --files --tracks 5000 --out /tmp/rc/files
//! ```
//!
//! Run the app on the database with `--db <out>/library.db` and a config
//! without library roots, so the fake paths are never pruned.

use std::path::PathBuf;
use std::time::Instant;

use rayon::prelude::*;
use ricercar_core::Library;
use ricercar_core::synth;

fn main() {
    let mut tracks = 50_000usize;
    let mut out: Option<PathBuf> = None;
    let mut files = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--tracks" => tracks = args.next().and_then(|v| v.parse().ok()).unwrap_or(tracks),
            "--out" => out = args.next().map(PathBuf::from),
            "--files" => files = true,
            _ => {
                eprintln!("usage: synth_library [--files] [--tracks N] --out DIR");
                std::process::exit(2);
            }
        }
    }
    let Some(out) = out else {
        eprintln!("--out DIR is required");
        std::process::exit(2);
    };
    let music = out.join("music");
    let albums = synth::albums(&music, tracks);
    let t = Instant::now();

    if files {
        albums.par_iter().for_each(|a| {
            synth::write_cover(a).expect("write cover");
            for (path, tags, _) in &a.tracks {
                synth::write_flac(&path.with_extension("flac"), tags).expect("write flac");
            }
        });
        println!(
            "{tracks} FLAC files in {} albums under {} ({:.1?})",
            albums.len(),
            music.display(),
            t.elapsed()
        );
        return;
    }

    albums.par_iter().for_each(|a| {
        synth::write_cover(a).expect("write cover");
    });
    let db = out.join("library.db");
    for ext in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{ext}", db.display()));
    }
    let lib = Library::open(&db).expect("open library");
    let added = synth::fill(&lib, &albums);
    println!(
        "{added} tracks in {} albums written to {} ({:.1?})",
        albums.len(),
        db.display(),
        t.elapsed()
    );
}
