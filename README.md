# ricercar

A native, bit-perfect Linux music player that is also a **UPnP AV
MediaRenderer** — so phone-based control points (BubbleUPnP, Symfonium,
Kazoo…) can push lossless streams to your machine and play them through a
clean, exclusive, no-resampling ALSA path.

ricercar is **inspired by QBZ** but is a clean-room, from-scratch project:
no QBZ code, no shared history. Like QBZ, it deliberately contains **no
private or unofficial API clients for any streaming service**. a streaming service (or any
other streaming catalog) reaches ricercar the same way a Sonos or a
network streamer gets it: the control point on your phone authenticates to
the service with *your own* subscription and hands ricercar a plain HTTP
stream URL.

## What it does (0.1 alpha)

- **Audio engine** — symphonia decode (FLAC/WAV/MP3/OGG/M4A), bit-perfect
  output to `hw:` ALSA devices (no mixer, no resample, native sample-rate
  switching), gapless playback with pre-primed next track via
  `SetNextAVTransportURI`, position/seek, software volume (disables
  bit-perfect only when you lower it).
- **UPnP AV MediaRenderer** — SSDP advertisement, device/service
  descriptions, AVTransport (incl. `SetNextAVTransportURI`),
  RenderingControl, ConnectionManager, GENA events. Works with BubbleUPnP /
  Symfonium / Hi-Fi cast style apps.
- **MPRIS** — appears as `org.mpris.MediaPlayer2.ricercar`; controllable by
  media keys, GNOME/KDE media controls, `playerctl`, and `ricercar-cli`.
- **Library** — SQLite index of your FLAC/WAV tags (lofty), album list,
  search, filesystem watcher for incremental rescans.
- **UI** — minimal slint desktop app (now-playing, transport, volume,
  library list with search), plus `--headless` for a daemon-only run.

## Quick start

```sh
cargo build --release

# list audio devices
./target/release/ricercar-daemon --print-devices

# run the renderer (UPnP + MPRIS) on a hardware device, indexing ~/Music
./target/release/ricercar-daemon --device hw:1,0 --library ~/Music

# desktop UI (same engine, plus window)
./target/release/ricercar-ui --device hw:1,0 --library ~/Music

# control it
./target/release/ricercar-cli status
./target/release/ricercar-cli open "file:///Music/album/track.flac"
./target/release/ricercar-cli volume 0.8
```

Then open BubbleUPnP (or any UPnP control point): "ricercar" appears as a
renderer; select it, press play in your streaming app, and the stream plays
on your DAC.

## Bit-perfect policy

When the output device is a `hw:` device and volume is 100 %, samples go to
the kernel **untouched** (native bit depth container, native sample rate).
A rate change between tracks drains and reopens the device (a few ms of
silence); gapless within the same rate is sample-exact. The exact-pipeline
tests in `crates/ricercar-audio/tests` compare decoded output byte-for-byte
against ffmpeg references.

## Layout

| crate | role |
|---|---|
| `ricercar-audio` | decode + sink pipeline, engine thread |
| `ricercar-core` | library DB, tags, controller, watcher |
| `ricercar-upnp` | SSDP / HTTP / SOAP / GENA renderer |
| `ricercar-mpris` | org.mpris.MediaPlayer2 on the session bus |
| `ricercar-ui` | slint desktop app |
| `ricercar-daemon` | headless player binary |
| `ricercar-cli` | MPRIS client for scripts |

## Development

```sh
cargo test --workspace                  # all crates
dbus-run-session -- cargo test -p ricercar-mpris
cargo clippy --workspace -- -D warnings
```

Tests only use `null` and `file:` sinks — no audio is ever sent to your
hardware during `cargo test`.

## License

MIT. See `LICENSE` and `AUTHORS`. ricercar is not affiliated with a streaming service;
streaming playback depends on your own subscription and a third-party
control point.
