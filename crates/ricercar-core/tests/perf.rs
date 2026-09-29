//! Non-regression bench: the library queries behind the heavy views, on a
//! 20 000-track synthetic library (database only). Runs with the normal test
//! suite (debug build, CI runners); the limits are 3 to 5 times the usual
//! timings, so noise passes and real regressions (a full-table query on the
//! Tracks page, slow search ranking) fail. See docs/perf.md.

use std::path::Path;
use std::time::{Duration, Instant};

use ricercar_core::profile::percentiles;
use ricercar_core::{AlbumSort, Library, TrackSort, synth};

const TRACKS: usize = 20_000;

fn timed<T>(f: impl FnOnce() -> T) -> (T, Duration) {
    let t = Instant::now();
    let out = f();
    (out, t.elapsed())
}

#[test]
fn big_library_queries_stay_fast() {
    let dir = tempfile::tempdir().unwrap();
    let lib = Library::open(&dir.path().join("library.db")).unwrap();
    let albums = synth::albums(Path::new("/synth"), TRACKS);
    assert_eq!(synth::fill(&lib, &albums), TRACKS);
    for (i, album) in albums.iter().take(40).enumerate() {
        let id = lib.create_playlist(&format!("Playlist {i}"));
        let paths: Vec<String> = album
            .tracks
            .iter()
            .map(|t| t.0.display().to_string())
            .collect();
        lib.add_to_playlist(id, &paths);
    }
    let playlist = lib.playlists()[20].id;

    let mut results: Vec<(&str, Duration, Duration)> = Vec::new();
    let mut check = |what: &'static str, took: Duration, limit_ms: u64| {
        results.push((what, took, Duration::from_millis(limit_ms)));
    };

    let (first, took) = timed(|| lib.tracks_range(TrackSort::Artist, 0, 2000));
    assert_eq!(first.len(), 2000);
    check("tracks page, first chunk", took, 120);
    let (all, took) = timed(|| lib.tracks(TrackSort::Artist));
    assert_eq!(all.len(), TRACKS);
    assert_eq!(all[..2000], first[..]);
    check("tracks, all", took, 1000);
    let (list, took) = timed(|| lib.albums(AlbumSort::Title));
    // Same title and album artist twice make one album.
    assert!(list.len() > albums.len() * 9 / 10);
    check("albums", took, 200);
    let (artists, took) = timed(|| lib.artists());
    assert!(artists.len() > 100);
    check("artists", took, 100);
    let name = artists[artists.len() / 2].name.clone();
    let (_, took) = timed(|| lib.artist_albums(&name));
    check("artist page", took, 50);
    let (_, took) = timed(|| lib.playlist(playlist));
    check("one playlist", took, 10);
    let (_, took) = timed(|| lib.stats());
    check("stats", took, 100);

    let queries = [
        "a",
        "m",
        "bl",
        "an",
        "blue",
        "rain",
        "anna",
        "moreau",
        "jazz",
        "sil",
        "quartet",
        "deluxe",
        "blue rain",
        "anna mor",
        "velvet garden",
        "fug",
        "björn",
        "lunar tide",
    ];
    let mut samples: Vec<Duration> = queries.iter().map(|q| timed(|| lib.search(q)).1).collect();
    let (_, p95) = percentiles(&mut samples);
    check("search p95", p95, 60);

    let mut failed = Vec::new();
    for (what, took, limit) in &results {
        eprintln!(
            "{what:<26} {:>8.1} ms  (limit {} ms)",
            took.as_secs_f64() * 1e3,
            limit.as_millis()
        );
        if took > limit {
            failed.push(*what);
        }
    }
    assert!(failed.is_empty(), "too slow: {failed:?}");
}
