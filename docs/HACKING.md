# Hacking on ricercar

Everything a newcomer needs to implement a feature end to end. Read
[CONTRIBUTING.md](../CONTRIBUTING.md) first for the ground rules.

## Workspace map

| Crate | Role | Key entry points |
|---|---|---|
| `ricercar-audio` | Decoding (symphonia), ALSA output, gapless, ReplayGain, HTTP streaming | `PlayerHandle` (`player.rs`), `device.rs` (`list_devices`, `probe_device`), `sink.rs` (`null` / `file:` sinks) |
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
4. Put every string in `@tr(...)` and add the French string (see
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
scripts/demo-library.sh /tmp/rc/music          # 59 tracks, needs ffmpeg + magick
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
- The tour seeds plays, favorites and playlists into the database, so always
  use the throwaway `XDG_*` directories above.
- The docs images live in `docs/screenshots/`, plus `docs/assets/banner.jpg`,
  which is derived from `home.png`. Refresh them when a visible feature
  changes.
- The tours ignore `ui-state.json` (window size, last page, sorts) so they
  always start from the same state. `RICERCAR_SNAPSHOT_TOUR=state` is the
  exception: it prints the restored state, changes it and quits; run it twice
  to check that the state survives a restart.

### Plugins

- `cargo build -p ricercar-core --bin ricercar-demo-plugin` builds the
  reference plugin used by `crates/ricercar-core/tests/plugins.rs`. It serves
  generated FLAC files from 127.0.0.1 (sign-in code: `DEMO`) and has options
  to exercise the error paths (see the top of its `main.rs`).
- `RICERCAR_SNAPSHOT_TOUR=plugins` installs it through the Plugins page
  from a local catalogue (it must sit next to the `ricercar` binary), then
  captures the catalogue, the install dialog, the sign-in dialog, the browse
  pages, the plugin library in the Albums, Artists and Tracks pages, a plugin
  album page, a playing plugin track and a search mixing both sources. Headless tours
  never open a browser. Pass `--library` so the tour does not index the
  default music folder.
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

- UI strings: `@tr("English text")` in `.slint`.
- Add the French string to `crates/ricercar-ui/tools/fr.json`, then run
  `python3 crates/ricercar-ui/tools/gen_po.py`. The catalogs are bundled at
  build time.
- Rust-side user-visible strings (toasts, statuses, chain labels) go through
  `text::t("English")`, whose French table lives in `src/text.rs` itself.

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
- [ ] Strings are translated (FR).
- [ ] The snapshot tour covers the new UI, and the screenshots are
      regenerated if visible.
- [ ] The README is updated for user-visible features, and the privacy table for any new
      network call.
