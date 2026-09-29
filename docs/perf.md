# Performance on large libraries

Load test: the app must stay fluid
with 50 000 tracks / 4 000 albums.

## How to measure

```sh
scripts/perf.sh /tmp/rc/perf          # 50 000 tracks (database) + 5 000 files
```

The script builds the release binaries, generates two synthetic libraries
with `cargo run -p ricercar-core --example synth_library` (deterministic, same
seed every time), then runs the headless perf tour
(`RICERCAR_SNAPSHOT_TOUR=perf`, `RICERCAR_PROFILE=1`) four times:

| Run | Library | What it shows |
|---|---|---|
| `big-cold` | 50 000 tracks written straight into the database, empty cover cache | UI costs, first cover thumbnails |
| `big-warm` | same, cover cache filled by the previous run | UI costs |
| `files-scan` | 5 000 very short FLAC files (416 albums, `cover.jpg` each), empty database | initial scan |
| `files-rescan` | same files, same database | rescan with nothing changed |

The tour waits for the startup scan, then opens Albums (and sorts it), Artists,
Tracks (and sorts it), an artist, an album, Genres, runs 40 searches typed
query by query (single letters included), queues 5 000 tracks, opens the
queue and changes track. Every line printed is `[profile] <what>: <value>`:

- `page X: load` is the time spent in `views::load`; `page X: frame` is the
  time from the navigation to the frame that shows it.
- `startup: first frame` counts from the start of `main`.
- `search (library)` times `Library::search` alone, `search (page)` the whole
  search page update.
- Memory is the resident set (`VmRSS`, peak `VmHWM`) from `/proc/self/status`.

`RICERCAR_PROFILE=1` also works in a normal run (first frame and page timings
use Slint's rendering notifier, when the renderer offers one).

The synthetic vocabulary is small (24 adjectives × 24 nouns for titles), so a
word like "blue" matches ~11 % of the library, far more than in a real one:
search timings are pessimistic.

## Results

Release build, headless software renderer (1440×900), laptop with an AMD
Ryzen 7 PRO 6850U (8 cores, 16 threads). `before` is commit 4c49988
(tooling only), `after` the fixes below. For the 50 000-track runs both
values are given (cold / warm cover cache).

| Measurement | Before | After | Target |
|---|---|---|---|
| Startup to first frame (50 000 tracks) | 486 / 657 ms | 354 / 349 ms | < 1.5 s |
| Search, p50 / p95 (library query) | 86 / 154 ms, 91 / 137 ms | 9 / 41 ms, 9 / 48 ms | p95 < 50 ms |
| Tracks page: load + frame | 396 + 23 ms, 701 + 53 ms | 50 + 16 ms, 52 + 16 ms | < 300 ms |
| Tracks page: all 50 000 rows in (background) | — | 821 / 854 ms | — |
| Tracks sort by title, to the frame | 590 / 792 ms | 105 / 124 ms | — |
| Albums page: load + frame | 72 + 13 ms, 148 + 32 ms | 61 + 10 ms, 81 + 13 ms | — |
| Artists page: load + frame | 42 + 13 ms, 66 + 18 ms | 40 + 12 ms, 40 + 11 ms | — |
| Cover refill, worst call | 25 / 40 ms | 3.2 / 3.5 ms | — |
| Change track with a 5 000-item queue: rebuild queue | 28 ms | 4.6 ms | — |
| Memory at idle | 44 / 45 MiB | 44 / 42 MiB | — |
| Memory after the whole tour | 290 / 292 MiB | 294 / 296 MiB | < 400 MiB |
| Initial scan, 5 000 files | 1.6 s | 0.9 s | — |
| Rescan, 5 000 unchanged files | 82 ms | 34 ms | < 3 s |

The scan code did not change; its difference between the two runs is noise
(page cache, machine load).

## What changed

- **Covers**: every list model keeps an index from cover key to rows
  (`app::Rows`). A decoded cover patches only the rows waiting for it; before,
  each refill (every 60 ms while covers arrive) walked every row of every
  model.
- **"Playing" and favourite markers**: the same models index rows by track
  path, so a track change touches the rows of the old and new track only.
- **Tracks page**: the first 2 000 rows are queried and shown at once
  (`Library::tracks_range`); the rest is queried on a worker thread and
  appended 2 000 rows per event-loop turn. Sorts now end with the row id so
  that pages of the same sort always line up.
- **Search**: ranking (`bm25`) runs inside the FTS index and only the rows
  kept are joined to `tracks`; albums are ranked from the best 2 000 matching
  tracks; the artists found are counted with one filtered query (new
  expression index `idx_tracks_name`) instead of computing every artist.
- **Queue**: the drawer is a virtualized `ListView` of the upcoming items;
  their covers load as rows scroll in. The current item sits above it.
- **Playlist lookup**: `Library::playlist(id)` queries one row instead of
  loading every playlist.

## Non-regression bench

`crates/ricercar-core/tests/perf.rs` runs with `cargo test --workspace` (so
in CI): it builds a 20 000-track library and times the queries behind the
heavy views. Typical debug-build timings and the limits that fail the test:

| Query | Typical | Limit |
|---|---|---|
| Tracks page, first chunk | 37 ms | 120 ms |
| All tracks | 226 ms | 1 000 ms |
| Albums | 33 ms | 200 ms |
| Artists | 16 ms | 100 ms |
| Artist page | 7 ms | 50 ms |
| One playlist | 0.2 ms | 10 ms |
| Library stats | 18 ms | 100 ms |
| Search p95 (18 queries) | 18 ms | 60 ms |

## Still open

- The player tick peaked at 140–150 ms once per 50 000-track run: it reloads
  the current page when the library revision changes (here after the queue
  step), and a reload is as costly as opening the page.
- The Home page takes ~120 ms to load on 50 000 tracks (recently added and
  most played albums, library stats); acceptable, not optimised yet.
- Measurements use the headless software renderer. On a real window, first
  frame and page frames come from the GPU renderer; run the app with
  `RICERCAR_PROFILE=1` to get them.
