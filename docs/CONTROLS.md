# Control-point compatibility matrix

ricercar exposes two UPnP root devices from one HTTP server:

- **MediaRenderer** with the UPnP AV services
  `urn:schemas-upnp-org:service:AVTransport:1` (incl. `SetNextAVTransportURI`),
  `RenderingControl:1` and `ConnectionManager:1`, **and** the OpenHome services
  `urn:av-openhome-org:service:Product:1`, `Playlist:1`, `Info:1`, `Time:1`,
  `Volume:1` on the same device (as upmpdcli does). Both service families are
  views of the same play queue: an OpenHome playlist shows up in AVTransport
  (`NrTracks`, `CurrentTrack`, transport state) and vice versa.
- **MediaServer** with `ContentDirectory:1` and `ConnectionManager:1`,
  browsing the local library.

Only open standards are implemented (UPnP AV / DLNA, OpenHome). No streaming
service private APIs.

| Control point | Status | Notes |
|---|---|---|
| Own SOAP/HTTP test-suite (`cargo test -p ricercar-upnp`) | ✅ automated | See "What the tests prove" below |
| HTTP-streamed sources | ✅ automated | `http://` URLs decoded on the fly, bit-exact |
| OpenHome control points (Kazoo, BubbleUPnP "OpenHome" renderer, Lumin…) | 🟡 protocol tested, app untested | Playlist/Info/Time/Volume/Product per the OpenHome specs; no real-app report yet |
| BubbleUPnP | ⬜ untested | Expected to work (AVTransport or OpenHome); ContentDirectory search implements the criteria BubbleUPnP sends |
| Symfonium | ⬜ untested | "Push to renderer" uses AVTransport |
| Hi-Fi Cast / Linn Kazoo | ⬜ untested | Kazoo needs OpenHome (now implemented) |
| mConnect / Twonky "PlayTo" | ⬜ untested | AVTransport |

## What the tests prove

Renderer, UPnP AV (`tests/renderer.rs`, `tests/openhome.rs`):
- SetAVTransportURI → SetNext → Play → gapless switch → STOPPED, both tracks
  written bit-exact to a `file:` sink; `http://` sources.
- `SetPlayMode` NORMAL / REPEAT_ALL / REPEAT_ONE / SHUFFLE mapped onto the
  controller and read back by `GetTransportSettings`; unsupported modes → 712.
- `GetDeviceCapabilities` → `PlayMedia=NETWORK`; `GetMediaInfo` `NrTracks`
  follows the real queue; control-point DIDL is parsed (title, artist, genre,
  track number, `res@duration`, `sampleFrequency`, `bitsPerSample`,
  protocolInfo MIME → codec) and returned verbatim.
- GENA: SUBSCRIBE / renew / UNSUBSCRIBE, initial event (SEQ 0) with every
  evented variable, `LastChange` documents escaped inside the property set,
  later events carrying only changed variables (e.g. `CurrentPlayMode`).

Renderer, OpenHome (`tests/openhome.rs`):
- Description lists the five OpenHome services; every SCPD is served.
- Playlist: `Insert` ×3 (no autoplay) → `IdArray` (base64 big-endian ids) →
  `IdArrayChanged` → `Read` returns the inserted DIDL verbatim → `ReadList` →
  `SeekId` plays through to the last id → `TransportState`/`Id` → `DeleteId`
  → unknown ids fault 800 → `DeleteAll`; AVTransport reports the same queue
  and state throughout.
- Info `Details` (sample rate, bit depth, codec, lossless, duration),
  `Counters`, `Track`; Time `Time`.
- Volume `SetVolume`/`VolumeInc`/`SetMute` shared with RenderingControl;
  out-of-range volume → 801. Product source list/index/attributes/standby.
- `SetRepeat`/`SetShuffle` visible through AVTransport `GetTransportSettings`.
- Eventing: initial Playlist event (SEQ 0), then an `IdArray` change after
  `Insert` without unchanged variables.

MediaServer (`tests/dms.rs`):
- Root containers Albums, Artists, Genres, All tracks, Playlists, Recently
  added with correct `childCount`; album → tracks with stable ids and
  `parentID`; `BrowseMetadata` on the root, a top container, an album and a
  track; paging (`StartingIndex`/`RequestCount`, `TotalMatches`); unknown
  ids → 701, children of an item → 710.
- `res` carries `duration`, `size`, `bitsPerSample`, `sampleFrequency`,
  `nrAudioChannels` and the MIME per extension (flac → `audio/flac`);
  `upnp:albumArtURI` on albums and tracks.
- `Search` with `upnp:class derivedfrom "object.item.audioItem" and
  dc:title contains "…"`, class-only criteria, album class; malformed
  criteria → 708. `SystemUpdateID` follows the library revision.
- HTTP: streamed from disk, `Range` (first bytes, suffix), 416 when
  unsatisfiable, `HEAD` without body, `Accept-Ranges`,
  `transferMode.dlna.org`, `contentFeatures.dlna.org`. Paths outside the
  library → 404.
- Art: `/art` never fetches remote URLs (a queued item's http cover hint is
  refused), `file:///etc/passwd` and unknown albums → 404.

SSDP (soft check, port 1900 may be busy): unicast M-SEARCH answered with a
LOCATION on the requester's subnet (loopback here), `BOOTID.UPNP.ORG` and
`CONFIGID.UPNP.ORG`.

## Protocol features

- SSDP on 239.255.255.250:1900, on every IPv4 interface; M-SEARCH answered
  after the MX-random delay; LOCATION uses the interface on the requester's
  subnet; re-announce when interfaces change; `ssdp:byebye` when the renderer
  handle is dropped. OpenHome service types are advertised.
- HTTP: at most 64 concurrent connections (503 beyond), read timeout 10 s,
  write timeout 30 s, one request per connection (`Connection: close`).
- Cover art is served only for library tracks/albums (resized 500 px JPEG
  thumbnails via the cover cache when the image decodes) or local files
  already in the play queue.

Implemented but covered only by unit tests or not at all: the MX delay
(unit-tested parsing/jitter), subnet choice (unit-tested), interface-change
re-announce, byebye on drop, the connection cap and the socket timeouts.

## Not implemented / known limits

- OpenHome `Radio`, `Receiver`, `Sender`, `Credentials`, `Transport`
  services; Product exposes a single "Playlist" source.
- OpenHome shuffle reorders the queue (the controller's shuffle) instead of
  only randomising the play order, so `IdArray` changes on `SetShuffle`.
- Balance / fade are fixed at 0 (bit-perfect output).
- ContentDirectory `SortCriteria` is ignored (containers have a fixed order);
  no `Filter` pruning; no `ContainerUpdateIDs`.
- HTTP keep-alive, multi-range requests, transcoding.
- Multi-room / grouped playback.
- Serving resized thumbnails is not covered by an automated test (the
  fixtures carry no artwork).

To add your app to this table, open an issue with the app version +
what worked/broke; we keep this matrix honest with real reports.
