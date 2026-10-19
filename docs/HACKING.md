# Hacking on ricercar

Everything a newcomer needs to implement a feature end to end. Read
[CONTRIBUTING.md](../CONTRIBUTING.md) first for the ground rules.

## Workspace map

| Crate | Role | Key entry points |
|---|---|---|
| `ricercar-audio` | Decoding (symphonia), DSD (`dsd/`), ALSA output, gapless, ReplayGain, HTTP streaming | `PlayerHandle` (`player.rs`), `device.rs` (`list_devices`, `probe_device`), `sink.rs` (`null` / `file:` sinks) |
| `ricercar-core` | Library (SQLite + FTS5), queue controller, config, covers, file watcher | `Library` (`library.rs`), `Controller` / `CtlEvent` / `CtlState` / `QueueItem` (`controller.rs`), `Config` + XDG paths (`config.rs`), `CoverCache` (`covers.rs`) |
| `ricercar-upnp` | UPnP AV + OpenHome renderer, MediaServer, SSDP, GENA | `start_with`, `RendererHandle` (`lib.rs`), one file per service |
| `ricercar-mpris` | MPRIS D-Bus server (zbus) | `lib.rs` |
| `ricercar-online` | Cover Art Archive, lrclib lyrics, radio-browser, ListenBrainz / Last.fm | one module per service, offline queues in `scrobble.rs` |
| `ricercar-daemon` | Wires everything into an `AppContext`; the headless `ricercar-daemon` binary | `AppContext` (`lib.rs`) |
| `ricercar-ui` | Slint desktop app, binary `ricercar` | see below |
| `ricercar-cli` | Command-line control of a running instance | `main.rs` |

Source plugins ([plugins.md](plugins.md)) live in `ricercar-core/src/plugin/`:
`rpc.rs` (JSON-RPC lines over stdio), `host.rs` (`PluginHost`: supervision,
typed calls, `Resolver` for the controller, reporting and remote control)
and `mod.rs` (items, errors, `plugin://` URIs). The controller resolves
`plugin://` items in `Bridge::load_current` / `rearm`; `AppContext` owns the
host; the UI side is `src/plugins.rs` and `ui/plugins.slint`.

Data flow: the UI and daemon call `Controller` methods; the controller drives
the audio `PlayerHandle` and publishes `CtlEvent`s on a bus. Subscribers are
the UI (`player.rs::wire`), MPRIS, UPnP eventing and the scrobblers. Nothing
reads the player directly. Subscribe with `Controller::subscribe()`, read the
current `CtlState` with `Controller::lock()` (keep the guard short).

### Cue sheets

An album ripped as one audio file plus a `.cue` sheet is indexed as its
tracks (`ricercar-core/src/cue.rs` parses and cuts, `Library::scan_roots`
indexes):

- **Key.** A cue track's `path` is `<audio path>#cue=<TRACK number>`
  (`cue::key`, `cue::split_key`, `cue::file_of`). Everything keyed by path
  (stats, favorites, history, playlists, covers, search) works unchanged;
  code that opens the file goes through `meta::on_disk` or `cue::file_of`.
  The audio file itself is not listed. Only a file that a sheet cuts into
  two tracks or more is replaced; a one-file-per-track rip keeps its files.
- **Columns.** `tracks.cue_start` / `cue_end` (CD frames, 1/75 s; `NULL`
  end = end of file) and `cue_sheet` (schema v3). A cue track's change
  signature is the latest mtime of the sheet and the audio file; the file
  size is counted on its first track only.
- **Span.** A track runs from its `INDEX 01` to the next track's `INDEX 01`
  in the same file (the pregap plays at the end of the track before it).
  Metadata: the sheet first (track `TITLE` / `PERFORMER` / `SONGWRITER`,
  disc `TITLE` → album, disc `PERFORMER` → album artist, `REM DATE` /
  `GENRE` / `DISCNUMBER` / ReplayGain), then the audio file's tags.
- **Playback.** `Track::uri` is the file URI plus a media fragment,
  `file:///…/album.flac#t=<start>,<end>` in seconds (9 decimals, so it
  rounds back to the exact sample at any rate). `TrackSource` seeks
  sample-exactly to the start, stops at the end and keeps the samples
  decoded past it. When the next item is the following span of the same
  file, `Engine::continue_same_file` hands those samples on and keeps the
  decoder running: gapless and sample-exact, nothing is reopened. Position,
  duration and seeks are relative to the span.
- **Elsewhere.** M3U export writes a cue track as a
  `#RICERCAR-CUE:<key>` comment, which other players skip and
  `import_m3u` reads back. The UPnP MediaServer lists cue tracks and
  serves each as a WAV of exactly its samples (decoded on the fly, no
  Range requests). The signal path's Source hop says `CUE track <n>`.
  Embedded sheets (a `CUESHEET` tag) are not read.

### DSD

`ricercar-audio/src/dsd/` plays DSF and DFF files:

- `source.rs`: the `DsdSource` trait every reader implements (shared
  contract: interleaved bytes, MSB first, positions in bytes per channel).
  `dsf.rs`, `dff.rs` (plain and DST, through `dst-decoder`; the
  `FrameDecoder` trait lets tests stand in for it) and `id3.rs` (tags and
  cover) read files; `probe` gives the library format, length and tags.
- `play.rs`: `DsdPlayback` turns any `Box<dyn DsdSource>` into the engine's
  i32 samples for a `DsdMode`: DoP payloads (`dop.rs`, 16 bits per
  channel), native 32-bit words (`native.rs`), or PCM through the
  decimator (`pcm.rs`, filter design documented there).
- `TrackSource::open_with(uri, mode)` picks it for `.dsf` / `.dff` and
  SACD tracks (below). Its
  `wire()` (`fmt::Wire`: `Pcm`, `Dop`, `NativeDsd`) tells the engine how to
  open the sink (`open` or `open_dsd`), whether to apply gain (never for
  DoP and native) and to set the DoP markers (`dop::mark`, alternation
  kept across tracks). Sinks split native words into the ALSA DSD format
  they negotiated; `file:` takes `?formats=DSD_U16_LE,…` to emulate a DAC.
- The mode comes from `[audio.dsd]` per device name (`Controller::
  set_dsd_modes`, `PlayerHandle::set_dsd_mode`), and `DsdMode::guarded`
  turns DoP and native into PCM on any shared device. `ChainInfo::dsd`
  feeds the signal path.
- `dsd::fixture` writes DSF and DFF files from a sigma-delta modulator
  (tests, `examples/dsd_tone.rs` for the demo library).

### SACD images

`ricercar-audio/src/dsd/sacd/` reads SACD disc images (ISO): `SacdImage::
open` reads the master and area TOCs only (disc text, the stereo area's
track list and track text); `track_source(n)` streams a track as a
`DsdSource`, sector by sector, through the DST decoder when the area is
DST-coded. The multichannel area is listed, not played.

- **Key.** A track's library `path` is `<image path>#sacd=<n>`
  (`ricercar_core::sacd::key`, `sacd::split_key`, `sacd::file_of`) and its
  URI the image's `file://` URI plus `#sacd=<n>`. As with cue tracks,
  everything keyed by path works unchanged, and `meta::on_disk` maps a key
  to its image (folder covers, show in folder). The image is not listed.
- **Scan.** `Library::scan_roots` looks at `.iso` files: an image whose
  first track's size and mtime are unchanged keeps its rows without being
  opened; a new or modified one has its TOC read (never audio), and an ISO
  without a SACD master TOC is skipped silently. No schema change: the
  cue columns stay `NULL`. Metadata (`sacd::track_tags`): track text
  title / performer / composer, then disc title → album, disc artist →
  album artist, date, genre, disc number in a set; durations from the TOC
  (frames of 1/75 s); codec `SACD`, rate 2 822 400, 1 bit.
- **Playback.** `TrackSource::open_with` sees `#sacd=<n>`, opens the image
  and hands `track_source(n)` to `DsdPlayback`: the device's DSD mode, the
  guard and unity gain apply as for DSF. Position, duration and seeks are
  relative to the track. Consecutive tracks go through the ordinary DSD
  gapless path: the next track is opened on the pre-open worker (the TOC
  again, a cheap seek), the sink stays open, the DoP marker phase and the
  PCM converter's filter state carry on. Tracks are contiguous and whole
  frames of 1/75 s (a multiple of every DoP and native word), so the
  output is the area's stream, sample for sample.
- **Elsewhere.** M3U export writes `#RICERCAR-SACD:<key>`; the UPnP
  MediaServer serves a track as a WAV of its conversion to 24-bit /
  88.2 kHz PCM (like a cue track, no Range requests); the signal path's
  Source hop reads `SACD · DSD64 · 2.8224 MHz · 2 ch · track <n>`.
- `dsd::sacd::fixture` builds synthetic images (tests,
  `examples/sacd_image.rs` for the demo library, `examples/sacd_info.rs`
  to inspect a real one). `RICERCAR_TEST_SACD_ISO=<image>` enables the
  tests on a real image (`ricercar-audio` reader, `ricercar-core`
  `tests/sacd.rs`: index, then 3 s of track 2 to the file sink).

## The UI crate

- `ui/*.slint` holds the markup. `state.slint` declares the `Page` enum and the
  `App` global, which carries all state and callbacks shared with Rust.
  `theme.slint` holds the `Theme` global with the design tokens. Other files
  hold components: `chrome` (sidebar, player bar), `pages`, `lists`,
  `widgets`, `overlays`, `more`, `icons`.
- `src/app.rs`: `Ui` owns the window, the `AppContext` and the list `Models`.
  It is stored in a UI-thread `thread_local`:
  - `with_ui(|ui| …)` runs on the UI thread only;
  - `post(|ui| …)` runs from any thread (worker, bus subscriber) and hops back
    to the event loop.

  Never touch Slint objects off the UI thread. Blocking work (SQLite queries
  on big libraries, network, image decoding) goes to a `std::thread`, then
  `post` the result.
- `src/views.rs`: `load(ui, page, arg)` fills the models for each page.
  `Ui::navigate(page, arg, push)` sets the page, keeps the history and calls
  `load`. Row actions (`track_action`, `album_action`) are dispatched by
  string.
- `src/player.rs`: now-playing state, queue drawer, lyrics and signal path
  (`update_chain`). It is driven by `CtlEvent`s and a tick timer.
- `src/extras.rs`: radio, settings, theme and accent colors. It also provides
  a `debounce` helper used for live text fields.
- `src/images.rs`: cover loading and scaling off-thread. `Ui::refill_covers`
  patches the models in place once images arrive.
- `src/snapshot.rs`: the headless screenshot tour (see below).

### Adding a page

1. Add the variant to `Page` in `state.slint`, and any state or callbacks to
   the `App` global.
2. Build the component in `pages.slint` (or a new file imported from
   `app.slint`) and route it in the page switch of `app.slint`.
3. Handle it in `views::load` and add the sidebar or navigation entry.
4. Put every string in `@tr(...)` and add its translations (see
   *Translations*).
5. Add a step to the tour in `snapshot.rs`.

### Slint pitfalls we already paid for (Slint 1.18)

- Changed-callbacks (`changed prop => {}`) do not work on properties of a
  **global**. Mirror the global into a local `property` of the component and
  react on that.
- `row` is reserved in some contexts. Name model items `item`.
- Never set an explicit `y:` (or `x:`) on a child inside a layout: it silently
  offsets it. Use alignment and padding instead.
- A property named like an enum value shadows it: write `ImageFit.cover`,
  not `cover`.
- Binding loops show up as warnings at build time, or as frozen values. Break
  them with an intermediate property set from a callback.
- Animations on a property that is also re-bound every frame can stall.
  Prefer no animation over a stuck one.
- `slint::set_xdg_app_id` must run after `MainWindow::new()` and before
  `show()`.
- Wayland: winit 0.30 gives neither drag-and-drop of external files nor window
  position. Design features with a keyboard or menu fallback.
- There is no resize event. Poll `window().size()` with a timer if needed.

## Build and run

```sh
cargo build                      # debug; deps are built at opt-level 2 (see Cargo.toml)
cargo build --release
```

**Cap parallelism at 4 jobs.** Create `.cargo/config.toml` (gitignored) with:

```toml
[build]
jobs = 4
```

Run the app against the demo library with a silent sink, without touching the
real user data:

```sh
scripts/demo-library.sh /tmp/rc/music          # 69 tracks, needs ffmpeg + magick
export XDG_CONFIG_HOME=/tmp/rc/config XDG_DATA_HOME=/tmp/rc/data XDG_CACHE_HOME=/tmp/rc/cache
./target/debug/ricercar --device null --no-upnp --no-mpris --library /tmp/rc/music
```

- `--device null` discards the audio. `--device file:/path/out.wav` writes it
  to a file.
- **Never use `hw:` or `plughw:` devices in tests or scripted runs.**
- `RUST_LOG=debug` gives verbose logs.

### Screenshots (headless)

```sh
RICERCAR_SNAPSHOT=/tmp/rc/shots RUST_LOG=error timeout 200 \
  ./target/debug/ricercar --device null --no-upnp --no-mpris --library /tmp/rc/music
```

- This renders a 1440×900 software window and walks the tour in
  `snapshot.rs`, writing one PNG per step.
- The tour runs in a throwaway home: `HOME` and the `XDG_*` directories
  point to a temporary directory, removed at the end, and the `--library`
  folder becomes its `~/Music`. Set `RICERCAR_SNAPSHOT_HOME=<dir>` to keep it
  between runs (the state tour below).
- The docs images live in `docs/screenshots/`, plus `docs/assets/banner.jpg`,
  which is derived from `home.png`. Refresh them when a visible feature
  changes.
- The tours ignore `ui-state.json` (window size, last page, sorts) so they
  always start from the same state. `RICERCAR_SNAPSHOT_TOUR=state` is the
  exception: it prints the restored state, changes it and quits; run it twice
  with the same `RICERCAR_SNAPSHOT_HOME` to check that the state survives a
  restart.
- `RICERCAR_SNAPSHOT_TOUR=stream` with `RICERCAR_SNAPSHOT_URL=<http URL>`
  plays that stream and captures the player bar while it loads, plays, and
  after seeks ahead and back: point it at a local server that answers late
  and slowly to see the loading spinner and the received parts on the seek
  bar (`EngineEvent::Buffering` / `Buffered`, `CtlState::buffering` /
  `buffered`).

### Plugins

- `cargo build -p ricercar-core --bin ricercar-demo-plugin` builds the
  reference plugin used by `crates/ricercar-core/tests/plugins.rs`. It serves
  generated FLAC files from 127.0.0.1 (sign-in code: `DEMO`) and has options
  to exercise the error paths (see the top of its `main.rs`).
- `RICERCAR_SNAPSHOT_TOUR=plugins` installs it through the Plugins page
  from a local catalogue (it must sit next to the `ricercar` binary), then
  captures the catalogue, the "add from a link" dialog, the confirmation of
  an install from a file, the install dialog, the sign-in dialog, the browse
  pages, the plugin library in the Albums, Artists and Tracks pages, a plugin
  album page, a playing plugin track, a search mixing both sources, playlist
  editing on the service (an editable playlist and its entry menu, "Add to
  playlist" grouped by source, the new-playlist location, rename, the delete
  confirmation, a followed playlist), the plugin's details on an artist, an
  album and a browse page, the "Start radio" menu and the radio it starts, the
  continuous playback setting and the plugin's settings dialog (with a
  refused value).
  Headless tours never open a browser nor check for updates. Pass
  `--library` so the tour does not index the default music folder.
- `RICERCAR_PLUGIN_INDEX=<url or file>` replaces the hub's index (local
  files may then use `file://` assets). The hub itself is
  [ricercar-player/ricercar-plugins](https://github.com/ricercar-player/ricercar-plugins);
  `plugin::catalog` reads it and installs from it.
- Declare it by hand to try the UI:

  ```toml
  [[plugins]]
  id = "demo"
  command = "/path/to/target/debug/ricercar-demo-plugin"
  ```

### Performance

- `RICERCAR_PROFILE=1` prints `[profile]` timings to stderr: page loads, the
  frame that shows them, first frame, scans, memory.
- `scripts/perf.sh <dir>` runs the headless perf tour on synthetic libraries
  of 50 000 tracks and 5 000 files. Results and method:
  [docs/perf.md](perf.md).
- Lists that can grow with the library are virtualized (`ListView`) and load
  covers for the rows on screen only (`App.visible`). Patch rows through the
  `Rows` indexes in `app.rs` rather than walking whole models.

## Tests and checks

These must all pass before every commit (this is what CI runs):

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
dbus-run-session -- cargo test --workspace     # MPRIS tests need a session bus
cargo deny check                               # licenses / advisories
```

- UPnP integration tests (`crates/ricercar-upnp/tests`) start a real renderer
  on loopback with the `null` sink. Follow them as the model for network
  tests.
- UI logic that can be tested without a window (formatting, sorting, version
  compare…) goes in plain functions with unit tests. The UI itself is checked
  with the snapshot tour.

## Translations

- Languages: English (the source), French, German, Spanish, Italian and
  Japanese. Each has one table, `crates/ricercar-ui/tools/<code>.json`, keyed
  by the English string, which serves both sides below.
- UI strings: `@tr("English text")` in `.slint`. Add the string to every
  `tools/<code>.json`, then run `python3 crates/ricercar-ui/tools/gen_po.py`,
  which writes the gettext catalogues bundled at build time. Plurals are
  lists, one string per form (`ja` has one form).
- Rust-side user-visible strings (toasts, statuses, chain labels) go through
  `text::t("English")`. A sentence with values in it is a template,
  `text::tf("{name} installed", &[("name", …)])`, so each language can put
  the values where it needs them; never glue a translated word to a value.
- `cargo test -p ricercar-ui` checks that every language has every string
  with the same placeholders, and that every literal passed to `t` or `tf`
  is in the tables.
- Japanese text is drawn with a CJK font found on the system (fontconfig);
  the bundled Inter has no CJK glyphs.
- Adding a language: a `tools/<code>.json` with the same keys, its plural
  rule in `gen_po.py`, and an entry in `text::LANGUAGES`, `CATALOGS` and the
  Language list of the settings (`more.slint`).

## Git conventions

- Subject style: `area: what changed`, all lowercase, with no final period
  (`ui: …`, `upnp: …`, `core: …`, `docs: …`, `release: …`). Add a body
  explaining *why* when it isn't obvious.
- One logical change per commit. Run `cargo fmt` **before** committing, not
  as a follow-up commit.
- Do not commit editor or tool state directories.
- Do not push, tag or publish a release without the maintainer's go-ahead.
- Releases: bump `version` in the root `Cargo.toml` (workspace and
  `workspace.dependencies`), write `docs/releases/vX.Y.Z.md`, then tag
  `vX.Y.Z`. `.github/workflows/release.yml` builds and publishes the packages.
  Use `workflow_dispatch` for a dry run.

## Definition of done

- [ ] The change does what it says; performance-sensitive changes are
      measured with `scripts/perf.sh`.
- [ ] Tests added. fmt, clippy, test and deny are green.
- [ ] Strings are translated (every `tools/<code>.json`).
- [ ] The snapshot tour covers the new UI, and the screenshots are
      regenerated if visible.
- [ ] The README is updated for user-visible features, and the privacy table for any new
      network call.
