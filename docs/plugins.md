# Source plugins (design)

> **Status: implemented (protocol 1).** This document specifies how
> ricercar gains catalogues (streaming services, remote libraries, podcast
> directories…) without the core ever containing service-specific code or
> credentials. It is the reference for both the host implementation in this
> repository and third-party plugin authors. A reference plugin serving
> generated files from 127.0.0.1 lives in
> `crates/ricercar-core/tests/fixtures/demo-plugin`.

## Goals

- **Add sources, not code.** A plugin brings a catalogue (browse, search,
  favourites) and turns its items into something the engine can play. The
  core stays generic: no service names, endpoints, keys or tokens in this
  repository.
- **Keep the signal path honest.** A plugin never delivers PCM. It hands the
  host a URL (or a local path); `ricercar-audio` fetches, decodes and plays it
  exactly like any HTTP stream. Output stays bit-perfect when the item allows
  it, and the signal path keeps showing each hop. A plugin may relay the
  stream itself (rebuild a file from segments, decrypt a protected stream
  locally, then serve it on `127.0.0.1`), as long as it delivers the
  original codec unchanged: no re-encoding, no signal processing. It then
  says so with [`delivery: "proxied"`](#resolving), and the signal path
  shows the relay.
- **Isolation.** A plugin is a separate process, written in any language. It
  can crash, hang or be slow without taking the player down.
- **Explicit user choice.** ricercar bundles no plugin and never installs or
  runs one on its own. The user either declares an executable in the config,
  or installs one from the [community hub](#community-hub) on the Plugins
  page, one explicit confirmation per plugin.

### Non-goals (v1)

- DSP or output plugins (anything touching samples).
- Plugin-defined UI widgets. The host renders every page from generic item
  lists.
- In-process plugins (dynamic libraries, WASM).
- Sandboxing (see [Security](#security)).

## Overview

```
┌──────────── ricercar ─────────────┐        ┌──────── plugin process ────────┐
│ UI ─ Controller ─ PlayerHandle    │ stdio  │ auth, catalogue, resolve       │
│          │                        │◄──────►│ (talks to its service)         │
│    PluginHost (ricercar-core)     │JSON-RPC│                                │
└──────────┼────────────────────────┘        └────────────────────────────────┘
           │ resolved URL
           ▼
   ricercar-audio ──► HTTP fetch ──► decode ──► hw: device
```

A queue item from a plugin carries a **plugin URI**,
`plugin://<plugin-id>/<ref>`. Just before the engine needs the item, the host
asks the plugin to *resolve* it into a playable URL.

## Community hub

[github.com/ricercar-player/ricercar-plugins](https://github.com/ricercar-player/ricercar-plugins)
is an index of third-party plugins, open to anyone through a pull request.
It hosts no plugin code: each entry points at its author's repository and at
prebuilt binaries the author publishes, with their SHA-256 pinned in the
entry. Listing is not a review or an endorsement.

The hub's CI turns the entries into `index.json`, which the app reads when
the Plugins page opens, and once at startup when a plugin installed from the
hub is declared (to count available updates). It is never read when
Settings → Online extras → Plugin catalogue is off:

```jsonc
{"version": 1, "plugins": [{
  "id": "example", "name": "Example Music",
  "description": "…", "author": "someone", "license": "MIT",
  "repository": "https://github.com/someone/ricercar-example",
  "version": "1.2.0", "protocol": 1,
  "capabilities": ["auth", "browse", "search", "resolve"],
  "args": [],
  "assets": [
    {"arch": "x86_64", "url": "https://github.com/someone/…/example-x86_64", "sha256": "…"},
    {"arch": "aarch64", "url": "https://github.com/someone/…/example-aarch64", "sha256": "…"}
  ]
}]}
```

- Entries with a bad id, a non-`https` repository or asset, or a malformed
  digest are ignored. An entry without an asset for this computer (or for
  another protocol version) is shown but cannot be installed.
- **Install** asks for confirmation (what, by whom, from which host, and
  what is and is not checked), downloads the binary for this architecture
  (256 MB at most), checks its SHA-256, puts it in
  `$XDG_DATA_HOME/ricercar/plugin-bin/<id>/<version>/<id>` and adds a
  `[[plugins]]` table with `version` and `host` (where the binary came
  from) set. **Update** is offered only when the index version is strictly
  newer than the installed one (a pre-release is older than its release);
  if the binary now comes from another host, the app asks again before
  installing. It does the same as Install with the newer version and removes
  the old one. **Remove** deletes the table and,
  for hub installs only, the binaries; the plugin's data directory stays.
- `RICERCAR_PLUGIN_INDEX` points the app at another index (a URL or a local
  file, whose assets may then be `file://`), for tests and the screenshot
  tour.

## Declaring a plugin

Plugins are declared in `config.toml`, one table per plugin:

```toml
[[plugins]]
id = "example"                 # [a-z0-9-]+, unique; used in plugin:// URIs
command = "/usr/local/bin/example-plugin"
args = ["--serve"]
enabled = true
# version = "1.2.0"            # set by installs from the hub only
# host = "github.com"          # likewise: where the binary was downloaded

[plugins.settings]             # optional: values of the plugin's settings
quality = "lossless"
page_size = 50
```

- The host passes nothing secret on the command line and does not expand
  environment variables.
- The environment is cleared, then only these variables are passed on:
  `HOME`, `USER`, `LOGNAME`, `PATH`, `LANG`, `LANGUAGE`, `LC_*`, `TZ`,
  `XDG_*`, `TMPDIR`, `http_proxy`, `https_proxy`, `no_proxy` (and their
  upper-case forms), `SSL_CERT_FILE`, `SSL_CERT_DIR` and
  `DBUS_SESSION_BUS_ADDRESS`.
- Each plugin gets its own directories, created by the host and sent in
  `initialize`:
  - `$XDG_DATA_HOME/ricercar/plugins/<id>/`: credentials and state, owned by
    the plugin;
  - `$XDG_CACHE_HOME/ricercar/plugins/<id>/`: disposable data.
- The host never reads either directory.

## Lifecycle

- `ricercar-daemon` (the `AppContext`, shared by the daemon and the desktop
  app) spawns every enabled plugin at startup, one process each.
  `ricercar-cli` does not.
- Each plugin runs in its own process group; stopping it kills the whole
  group, helpers it started included.
- `stdin`/`stdout` carry the protocol. Each `stderr` line goes to the ricercar
  log, prefixed with `plugin[<id>]`.
- **Crash or exit:** the host restarts the plugin with backoff (1 s, 2 s, 5 s,
  then every 30 s) and marks its items unavailable meanwhile. Queue entries
  stay in place and resolve again once the plugin is back.
- **Shutdown:** the host sends `shutdown`, waits 2 s, then kills the process
  group.
- **Back-pressure:** messages to a plugin go through a bounded queue. If the
  plugin stops reading its `stdin`, notifications are dropped and calls fail
  at once with a timeout instead of blocking the player.
- **Config changes** (plugin added, removed, toggled, from the settings or
  by editing `config.toml`) apply without a restart, like the network
  settings. A change of `[plugins.settings]` alone keeps the process
  running and sends it `settings.changed` (see [Settings](#settings-optional)).

## Transport

- JSON-RPC 2.0, one JSON object per line (UTF-8, `\n`-terminated) in each
  direction.
- Both sides may send requests; ids are unique per sender.
- **Host → plugin timeouts:**
  - 10 s by default;
  - 30 s for `auth.complete`;
  - `track.resolve` is expected to answer within 5 s, and the UI shows a
    loading state after 300 ms.
- Unknown methods answer `-32601`. Unknown fields are ignored, so fields can
  be added without bumping the protocol version.

## Handshake

```jsonc
// host → plugin
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{
  "protocol": 1,
  "host": {"name":"ricercar","version":"0.7.0"},
  "data_dir": "/home/u/.local/share/ricercar/plugins/example",
  "cache_dir": "/home/u/.cache/ricercar/plugins/example",
  "locale": "fr-FR",
  "output": {                       // what the current DAC accepts natively
    "device": "hw:3,0", "bit_perfect": true,
    "max_rate": 192000, "max_bits": 24, "rates": [44100,48000,88200,96000,176400,192000]
  },
  "settings": {"quality": "hires"}  // stored setting values, see Settings
}}
// plugin → host
{"jsonrpc":"2.0","id":1,"result":{
  "protocol": 1,
  "plugin": {"id":"example","name":"Example Music","version":"1.2.0"},
  "capabilities": {
    "auth": true, "browse": true, "search": true, "resolve": true,
    "favorites": true, "reporting": false, "remote_control": false,
    "library": true, "lyrics": true, "playlist_edit": false,
    "details": true, "radio": true
  },
  "settings": [ … ]                 // optional, see Settings
}}
```

- A plugin whose `protocol` differs from the host's is disabled with a clear
  message.
- A missing capability is `false`. Each optional part of the protocol has
  its own:

| Capability | Methods | Section |
|---|---|---|
| `auth` | `auth.*` | [Authentication](#authentication) |
| `browse`, `search` | `browse.*`, `search`, `item.get` | [Browsing and search](#browsing-and-search) |
| `resolve` | `track.resolve` | [Resolving](#resolving) |
| `favorites` | `favorites.set` | [Browsing and search](#browsing-and-search) |
| `reporting` | `playback.*` (notifications) | [Playback reporting](#playback-reporting-optional) |
| `remote_control` | `player.*` (plugin → host) | [Remote control](#remote-control-optional) |
| `library` | `library.*` | [Library](#library-optional) |
| `lyrics` | `lyrics.get` | [Lyrics](#lyrics-optional) |
| `playlist_edit` | `playlists.*` | [Playlist editing](#playlist-editing-optional) |
| `details` | `item.details` | [Details](#details-optional) |
| `radio` | `radio.next` | [Radio](#radio-optional) |
- `locale` is ricercar's interface language (Settings → Appearance, `[ui]
  language`), a BCP 47 tag: the system's locale when it is the same
  language (`fr-BE`), else the bare language (`fr`); the system's locale
  when the user has not picked one. Use it for every label you send
  (settings, actions, sections, details). When the user switches language,
  running plugins get `locale.changed {locale}`; a plugin that ignores it
  picks the new language up at its next start.
- `output` is sent again with `output.changed` when the user switches device.
  Plugins use it to pick a stream format the DAC plays natively, since the
  engine refuses formats it would have to convert.

## Authentication

Credentials never pass through the host UI as passwords. The flow is
browser-based, and the host only relays what the user pastes.

| Method | Direction | Purpose |
|---|---|---|
| `auth.status` | host → plugin | `{state: "signed_out" \| "signed_in" \| "expired", account?: {display_name, detail?}}` |
| `auth.begin` | host → plugin | `{url, instructions?, expects_input: bool}`. The host opens `url` in the system browser, and also shows it as text and a QR code for signing in on another device. |
| `auth.complete` | host → plugin | `{input}`: the address or code the user pasted. Returns the new `auth.status`. |
| `auth.sign_out` | host → plugin | Forget stored credentials. |
| `auth.changed` | plugin → host (notification) | State changed on the plugin's own initiative (token expired, refreshed, completed through the plugin's own loopback listener). |

- With `expects_input: false`, the plugin completes the sign-in itself (for
  example through a loopback redirect) and sends `auth.changed`.
- The host shows a paste field either way. It is harmless, and helps when the
  browser runs on another machine.

## Items

Every entry a plugin returns is an **item**:

```jsonc
{
  "ref": "track/8812",            // opaque to the host, stable, ≤ 1 KiB
  "kind": "track",                // track | album | artist | playlist | folder
  "title": "…",
  "subtitle": "…",                // free text for lists (e.g. "Album · 2019")
  "artist": "…", "album": "…", "album_artist": "…",
  "track_no": 3, "disc_no": 1, "year": 2019, "genre": "…",
  "duration_ms": 245000,
  "track_count": 12,              // albums, playlists: number of tracks (artist page total)
  "art": "https://…/cover.jpg",   // http(s) image, fetched by the host's cover cache
  "format": {"sample_rate": 96000, "bits": 24, "codec": "flac"},  // best available, informative
  "playable": true,               // tracks: false when region/subscription forbids it
  "browsable": false,             // albums, artists, playlists, folders: true
  // all optional from here
  "album_ref": "album/77",        // tracks: their album
  "artist_ref": "artist/12",      // tracks, albums: their artist
  "label_ref": "label/3",         // albums, tracks: their label or publisher
  "favorite": true,               // in the user's favourites; absent: unknown
  "actions": [
    {"id": "radio",   "label": "Artist radio",   "ref": "radio/artist/12", "kind": "play"},
    {"id": "similar", "label": "Similar albums", "ref": "similar/album/77", "kind": "browse"}
  ],
  "entry_id": "e-5531",           // tracks listed by a playlist, see Playlist editing
  "editable": false               // playlists, see Playlist editing
}
```

- In the queue and in saved sessions, a track is stored as
  `plugin://<id>/<percent-encoded ref>`, together with the metadata above
  mapped onto `TrackInfo`.
- The resolved URL is **never** persisted. It is short-lived.
- `album_ref`, `artist_ref` and `label_ref` are refs like `ref` (opaque,
  ≤ 1 KiB, otherwise ignored), opened with `browse.list` in the host's
  generic page: "Go to album", "Go to artist" and a link to the label.
  Without them the host searches by name, which can pick a namesake. A
  queued track keeps its `album_ref` and `artist_ref`, saved sessions
  included.
- `favorite` lets the host show a filled or empty heart for plugins that
  declare `favorites`, and update it after `favorites.set`. Absent, the
  host shows no state; a plugin that knows an item is not a favourite sends
  `false`, so the empty heart shows.
- `actions` opens related content without a method per case: an artist's
  radio, similar albums, a label's catalogue. `kind: "play"`: the host reads
  `browse.list` on `ref` (up to 500 playable tracks) and queues them,
  replacing the queue or adding to it as the user chose. `kind: "browse"`:
  the host opens `ref` in the generic page. `label` is short plain text in
  the `locale` of `initialize`; `id` names the action (≤ 64 characters).
  The host shows at most 8 actions per item, below its own, and drops
  entries with an unknown `kind`, an empty or overlong label (80
  characters) or a bad `ref`. An `actions` that is not a list (a string,
  an object) counts as no actions, without a warning.

## Browsing and search

| Method | Result |
|---|---|
| `browse.root` | `{sections: [item], home?: [item]}`. `sections`: top-level entries shown under the plugin's name in the sidebar (e.g. "Favourites", "Playlists", "New releases"). `home` (optional, used with `library`): entries shown as shelves on the host's Home page (e.g. "New releases"); the host shows the first page of each. |
| `browse.list {ref, offset, limit}` | `{items: [item], total?, has_more}`. Children of a browsable item. Pages of at most 200. |
| `search {query, kinds?, offset, limit}` | `{groups: [{kind, items, total?, has_more}]}` |
| `item.get {ref}` | One item, fresh. Used to refresh metadata of restored sessions, and to check that a playlist is editable. The host only calls it with the `ref` of items the plugin listed (tracks, albums, artists, playlists); refs that only appear in `actions`, `album_ref`, `artist_ref` or `label_ref` are opened with `browse.list` and need not work with `item.get`. |
| `favorites.set {ref, on}` | Only with the `favorites` capability. |

The host caches nothing beyond the current page and the cover images,
except the library lists below and lyrics.

## Library (optional)

With the `library` capability, the plugin's music joins the host's own
**Albums**, **Artists** and **Tracks** pages and its search results, next to
the local library and marked with the plugin's name. Its playlists join the
sidebar's **Playlists** list, its `home` entries become shelves on the Home
page, and it has no sidebar section of its own. A plugin that only declares
`browse` keeps its sections in the sidebar.

| Method | Result |
|---|---|
| `library.albums {offset, limit}` | `{items: [item], total?, has_more}`: every album of the user's library on the service (kind `album`, browsable: its children are its tracks). |
| `library.artists {offset, limit}` | Same, kind `artist` (browsable: its children are its albums). |
| `library.tracks {offset, limit}` | Same, kind `track`. |
| `library.playlists {offset, limit}` | Optional. Same, kind `playlist` (browsable: its children are its tracks): the user's playlists on the service. A plugin without it answers `-32601`. |

- "The user's library" is what the service considers theirs: all the
  music of a personal server, the saved albums and tracks of a streaming
  account. Not the whole catalogue.
- Pages of at most 200. The order does not matter: the host sorts.
- The host reads every page (up to 20 000 items per list) after sign-in
  and keeps the lists **in memory** for the session, to sort and merge them
  with the local library; they are read again after a new sign-in or when
  the user asks to refresh. Nothing is written to disk.
- For the host's album and artist pages, an album's `artist`, `year` and
  `art`, and an artist's `art`, should be filled in; an album's tracks
  come from `browse.list` on its `ref`, an artist's albums likewise.

## Resolving

```jsonc
// host → plugin
{"method":"track.resolve","params":{"ref":"track/8812","purpose":"play"}}   // or "preload"
// plugin → host
{"result":{
  "url": "https://cdn.example/…/8812.flac",
  "expires_at": 1790670000,       // unix seconds, optional
  "duration_ms": 245000,
  "format": {"sample_rate": 96000, "bits": 24, "channels": 2, "codec": "flac"},
  "replaygain": {"track_gain": -7.2, "track_peak": 0.98},   // optional
  "live": false,
  "delivery": "direct"            // optional: "direct" (default) or "proxied"
}}
```

**Rules for plugins:**
- `url` is `http(s)://` or `file://`. HTTP responses must carry
  `Content-Length`, so the engine's spooled source can seek.
- If the server answers with `Accept-Ranges: bytes` and a
  `Content-Length`, the player uses `Range` requests: a seek far beyond what
  has been downloaded starts a new request at that point instead of waiting
  for the whole file. The remaining parts are fetched afterwards, so seeking
  back never needs the network. A range request must be answered with
  `206 Partial Content` and a matching `Content-Range`; a server that
  ignores `Range` still works, but falls back to one sequential download.
  Relays that produce the stream on demand should only advertise
  `Accept-Ranges` if they can really serve arbitrary offsets.
- `format` describes what the URL delivers. Pick the best format within
  `output`, and return `unavailable` if none fits.
- `delivery: "proxied"` when the plugin serves the stream itself (usually
  on `http://127.0.0.1:<port>/…`) instead of pointing at where the service
  serves it. Allowed only if the bytes are the original codec, unchanged:
  no re-encoding, no resampling, no processing. The signal path then reads
  "Source: <plugin> (relayed locally, original codec unchanged)". A relay
  URL that has expired is handled like any other: the host resolves again
  after an HTTP error. Unknown values count as `direct`.

**When the host resolves:**
- on load (`purpose: "play"`);
- when arming the gapless successor (`purpose: "preload"`), as soon as the
  current track starts;
- again when an armed URL has `expires_at` in the past;
- once more, and only once, when the engine gets HTTP 401/403/404/410 on a
  resolved URL.

**While resolving:**
- It happens off the controller lock and off the UI thread. The item shows a
  loading state.
- If the user skipped to another item meanwhile, the result is dropped.

## Playback reporting (optional)

With the `reporting` capability, the host sends these notifications:
- `playback.started {ref}`
- `playback.progress {ref, pos_ms}` (every 30 s)
- `playback.ended {ref, listened_ms, reason}`: `reason` is `ended` when
  the item played to within 3 s of its end (also when the queue stops
  there), `skipped` when the user moved on earlier, `stopped` when playback
  was stopped earlier.

Some services require them for royalty accounting. Scrobbling stays a host
feature and works for plugin tracks from their metadata.

## Remote control (optional)

With the `remote_control` capability, a plugin can drive the player. This is
for plugins that expose ricercar to an external control protocol.

**Plugin → host requests** (all go through `Controller`):
- `player.play {items, start}`: replaces the queue; `items` are plugin items;
- `player.enqueue {items, at}`;
- `player.pause`, `player.resume`, `player.stop`;
- `player.seek {ms}`;
- `player.next`, `player.previous`;
- `player.set_volume {percent}`, `player.set_mute {on}`.

**Host → plugin notifications:**
- `player.state {status, item_ref?, pos_ms, dur_ms, volume, muted}`, sent on
  every `CtlEvent` and every 1 s while playing;
- `player.taken_over`, sent when the user or another control point replaces a
  queue the plugin had set.

The queue set this way uses a new `Origin` value, `Plugin(id)`, and the UI
shows "Playing from <plugin name>".

## Lyrics (optional)

With the `lyrics` capability, the host asks the plugin for the lyrics of its
tracks before LRCLIB (tags and `.lrc` files are for local files only):

```jsonc
// host → plugin
{"method":"lyrics.get","params":{"ref":"track/8812"}}
// plugin → host
{"result":{
  "synced": [{"time_ms": 12340, "text": "…"}, {"time_ms": 15020, "text": "…"}],  // optional
  "plain": "…",                   // optional, lines separated by \n
  "instrumental": false           // optional
}}
```

- No lyrics: the error `not_found` (-32002). The host then asks LRCLIB when
  the user allows it, and remembers the `not_found` for a day.
- Answer within 5 s. On a timeout or any other error the host goes on with
  LRCLIB and asks again next time.
- With both `synced` and `plain`, the host shows `synced`; `plain` is only
  used when there are no synced lines. Sending both is fine.
- The host sorts `synced` by time and keeps at most 5000 lines of 500
  characters. Text is plain text. Lyrics found are cached on disk under the
  track's `plugin://` URI.
- The lyrics panel names the plugin as the source.

## Playlist editing (optional)

With the `playlist_edit` capability, the user can create, rename and delete
playlists on the service, and add or remove tracks, from ricercar.

| Method | Params | Result |
|---|---|---|
| `playlists.create` | `{name, description?, public?}` | the new playlist, an item |
| `playlists.rename` | `{ref, name}` | `null` |
| `playlists.delete` | `{ref}` | `null` |
| `playlists.add` | `{ref, items: [ref]}` | `null`: tracks appended |
| `playlists.remove` | `{ref, entries: [entry_id]}` | `null` |
| `playlists.move` (optional) | `{ref, entry, to}` | `null`: the entry is taken out, then put back at position `to`, 0-based and counted once the entry is out; a `to` at or past the end puts it last. `[A,B,C,D]`, A to 2: `[B,C,A,D]` |

- `editable: true` marks the playlists the user owns, as opposed to those
  they follow. The host offers editing on those only, never calls an edit
  method on another one, and the plugin refuses them anyway (`-32602`).
- `entry_id`, on each track of a playlist's `browse.list`, names the entry
  rather than the track: a track can appear twice, and many services remove
  by entry. `playlists.remove` and `playlists.move` take entry ids.
- `playlists.add` only receives refs of the plugin's own tracks. Local
  tracks go to local playlists, and a track of one plugin never goes to a
  playlist of another.
- A plugin without `playlists.move` answers `-32601`; the host then stops
  offering reordering until the plugin restarts.
- After each successful edit the host reads `library.playlists` again, and
  the open playlist.
- Deleting asks the user for confirmation first, naming the service: it
  cannot be undone there.

## Details (optional)

With the `details` capability, the host's artist and album pages show more
than the item: a biography, shelves of related items, facts.

```jsonc
// host → plugin
{"method":"item.details","params":{"ref":"artist/12"}}
// plugin → host
{"result":{
  "biography": {"text": "…", "source": "…"},                        // optional
  "related": [{"title": "Similar artists", "items": [item, …]}],    // optional
  "facts": [{"label": "Label", "value": "…"}]                       // optional
}}
```

- Asked after the page shows, without holding it up. `not_found`, an
  empty answer (`{}`) and an error all leave the page as it is: `{}` and
  `not_found` are equivalent.
- Everything is plain text: the plugin strips any markup itself. Titles
  and labels come in the `locale` of `initialize`.
- The host keeps a biography of 20 000 characters, 10 shelves of 50 items
  (empty shelves are dropped) and 30 facts.

## Radio (optional)

With the `radio` capability, the plugin suggests tracks that follow others:

```jsonc
// host → plugin
{"method":"radio.next","params":{"seed":"track/8812","exclude":["track/8812","track/901"],"limit":20}}
// plugin → host
{"result":{"items":[item, …]}}
```

- `seed` is a ref of the plugin: a track, and also an album or an artist
  for a radio started from those. Plugins should accept all three.
- `exclude`: refs played recently (at most 50); the plugin leaves them out.
- `limit`: at most 50. The host keeps the playable tracks, up to `limit`.
- **Continuous playback** (Settings → Playback, off by default): when the
  queue reaches its last track and it is a track of a plugin with `radio`,
  the host calls `radio.next` with it as the seed and appends the tracks
  it gets, so playback goes on without a gap. Never after a local track or
  a track of another plugin. An error or an empty answer: playback stops at
  the end of the queue, as without it.
- "Start radio" on a plugin track, album or artist replaces the queue with
  the answer (the track itself first).

## Settings (optional)

A plugin can declare a few settings of its own (streaming quality, what
to report, page sizes…). The host stores the values and shows them in a
dialog opened from the plugin's row on the Plugins page, with the host's
own controls: a plugin still defines no UI of its own.

**Not for credentials.** Sign-in stays in the [authentication](#authentication)
flow. Setting values are stored in plain text in `config.toml` (mode 0600)
and appear in the dialog as typed.

### Declaring

`settings` in the `initialize` result, a list of entries:

```jsonc
"settings": [
  {"key": "report_playback", "type": "bool", "section": "Playback",
   "label": "Report what I play",
   "description": "Tell the service which tracks you listen to.",
   "default": true},
  {"key": "quality", "type": "choice", "section": "Playback",
   "label": "Streaming quality",
   "options": [{"value": "standard", "label": "Standard"},
               {"value": "lossless", "label": "Lossless (CD quality)"}],
   "default": "lossless"},
  {"key": "page_size", "type": "number", "section": "Browsing",
   "label": "Items per page", "integer": true, "min": 10, "max": 200,
   "unit": "items", "default": 100},
  {"key": "greeting", "type": "string", "section": "Browsing",
   "label": "Greeting", "placeholder": "Hello", "max_length": 80,
   "restart": true, "default": "Hello from Demo Music"}
]
```

Every entry has:
- `key`: `[a-z0-9_.-]`, 1 to 64 characters, unique;
- `type`: one of the types below;
- `label`, and optionally `description`: plain text, never markup;
- `default`: required, of the entry's type and acceptable for it;
- `section` (optional): a heading; entries with the same `section` are
  shown together, in declaration order, after those without one;
- `restart` (optional, default `false`): the setting only takes effect when
  the plugin starts. The host then restarts the plugin when the user changes
  it, instead of sending `settings.changed`, and says so next to it.

| `type` | Value | Extra fields |
|---|---|---|
| `bool` | `true` / `false` | none (a switch) |
| `string` | text, one line | `placeholder`; `max_length` (default and upper bound 1024 characters) |
| `number` | a JSON number | `min`, `max`, `step` (> 0), `integer` (bool), `unit` (short text shown after the field) |
| `choice` | the `value` of one option | `options`: 1 to 50 `{value, label}`, values unique |

- The host checks the declaration. An entry that breaks a rule (bad key,
  default of the wrong type or not among the options, `min` above `max`…)
  is dropped with a warning in the log; the others are kept. At most 100
  entries are kept.
- Labels are shown as sent: plugins translate them themselves, from
  `locale` in `initialize`.
- Text longer than these limits is cut, without a warning: `label` and
  option labels 200 characters, `description` 1000, `section` 80,
  `placeholder` 200, `unit` 20, `key` 64 (a longer key drops the entry).
- The plugin may send `settings.declared {"settings": [...]}`
  (notification) at any time to replace its declaration, for example when
  the choices depend on the signed-in account. The dialog follows.

### Values

- The host stores, in the plugin's `[[plugins]]` table as
  `[plugins.settings]`, only the values that differ from their default.
  **Reset to defaults** in the dialog clears them.
- `initialize` carries `"settings": {key: value}` with every stored value.
  The host does not know the declaration yet at that point, so a value may
  belong to a key the plugin no longer declares; the plugin ignores those
  and applies its own defaults to the keys that are missing.
- When the user changes a value, the host checks it against the
  declaration: type, `min`/`max`, `max_length`, membership of the options.
  Numbers are snapped to `step` (counted from `min`, else 0) and rounded
  when `integer`; a value out of range is refused and the dialog says why.
  The value is then saved and sent as the notification
  `settings.changed {"settings": {…}}`, which holds every declared key with
  its value in effect (defaults included). A value stored for an entry with
  `restart: true` restarts the plugin instead (it gets the new values in
  `initialize`).
- Values edited by hand in `config.toml` are applied the same way: a
  changed `[plugins.settings]` table sends `settings.changed` (or restarts
  the plugin, per `restart`) without restarting anything else. A stored
  value that does not fit the declaration is ignored, and the default
  applies.
- While the plugin is not running, the dialog cannot be opened; values
  changed by hand reach it with the next `initialize`.

## Errors

Plugins answer with JSON-RPC errors using these codes. The host maps each code
to a UI message and never shows raw text from the plugin as HTML.

| Code | Name | Host behaviour |
|---|---|---|
| -32001 | `auth_required` | Mark the plugin signed out and offer to sign in. |
| -32002 | `not_found` | Grey the item out. |
| -32003 | `unavailable` | "Not available (region, subscription or format)". Skip to the next track when playing. |
| -32004 | `rate_limited` | `data.retry_after` in seconds (capped at 3600). Back off. |
| -32005 | `network` | Reads are retried once, then an offline state shows. Methods that change something (`playlists.*`, `favorites.set`) are never retried, since the change may have happened: the host reports the failure and the user tries again. |

## Changes in ricercar

| Crate | Change |
|---|---|
| `ricercar-core` | New `plugin` module: config `[[plugins]]`, process supervision, JSON-RPC over stdio, `PluginHost` API. `TrackInfo::from_uri` understands `plugin://`. `Origin::Plugin(id)`. `plugin::settings`: declared settings checked, values stored in `[plugins.settings]`, `settings.changed` on change; `reconcile` ignores settings when deciding restarts. Optional parts: `lyrics_get`, `item_details`, `playlist_*` (playlists read again after each edit), `radio_next_items`, `playable_tracks` for "play" actions; `TrackInfo` keeps `album_ref`/`artist_ref`. |
| `ricercar-core` / `Controller` | Continuous playback (`[audio] continuous_playback`) and `start_radio`, through `radio.next` off the controller lock. `CtlState.delivery` carries `delivery` for the signal path. Resolution step in `load_current` and `rearm`, done asynchronously: load the item once its URL is known, and drop the result if the current item changed meanwhile. **`on_track_started` matches items by URI**, so keep a map `item id → resolved URL` for the loading and armed items and match against it. Session save/restore keeps `plugin://` URIs. ReplayGain from `resolve` feeds `opts_for`. |
| `ricercar-audio` | Surface the HTTP status of failed fetches in `EngineEvent::Error` (or a typed variant), so the controller can re-resolve on 401/403/404/410. |
| `ricercar-daemon` | Start and stop the `PluginHost` in `AppContext`. Send `output.changed` on device switch. Plugin status in the diagnostic report. `set_plugin_setting` / `reset_plugin_settings`: check, save and apply off the calling thread. |
| `ricercar-ui` | Sidebar section per signed-in plugin, from `browse.root`. Generic browse page (grid for albums and playlists, track table for tracks). Plugins with `library` feed the Albums, Artists and Tracks pages (merged in the page's order, with a source badge and a source filter); plugin albums open on the album page. The global search shows local results at once, then each signed-in plugin's (its `search`, or matches in its library list) as they come, marked with their source. Sign-in dialog (open browser, QR code, paste field). A settings row per plugin (status, sign in/out, enable, and a settings button for plugins that declare settings, opening a dialog of switches, text and number fields and choices, grouped by section). "Source: <plugin>" hop in the signal path. Every new string goes through `@tr()`. |
| `ricercar-online` / scrobble | Plugin tracks scrobble from their metadata (`path` is `None`). |
| `ricercar-online` / lyrics | `LyricsSource::Plugin(id)`; for a `plugin://` track, the plugin (with `lyrics`) then LRCLIB, cached under the URI, `not_found` kept a day. The plugin is asked through a callback (`ricercar-daemon::lyrics`). |
| `ricercar-upnp` | No change. ContentDirectory still serves the local library only. Plugin tracks in the queue are shown with their metadata. |
| Docs | README: "Features", "Is this a … client?" FAQ and the promise wording (the core ships no service API; third-party plugins may add catalogues). HACKING.md: plugin host map. |

### Tests

- **Plugin fixture** (`crates/ricercar-core/tests/fixtures/demo-plugin`): a
  small reference plugin, in shell or Rust, that serves a few generated FLAC
  files through a local HTTP server. It implements every method, including
  the error paths.
- **Tests on the `null` sink:**
  - handshake;
  - crash and restart;
  - resolve, then play;
  - gapless preload;
  - URL expiry and re-resolve;
  - `auth_required` handling;
  - session restore with `plugin://` items.
- Lyrics, contextual refs, actions and favourite flags, details, playlist
  editing (with and without `playlists.move`, refusal of non-editable
  playlists and of tracks from elsewhere), `delivery`, and continuous
  playback on the `null` sink (off, on, after a local track, on error).
- Settings: the declaration is checked, values are checked and stored,
  `initialize` carries them, a change sends `settings.changed` without a
  restart, `restart: true` restarts, `settings.declared` replaces the
  declaration.
- A snapshot scenario for the browse page, the sign-in and the settings
  dialogs.

## Security

- A plugin runs with the user's privileges, and v1 has no sandbox. Only
  install plugins you trust, and the docs must say so plainly. A later
  version could launch plugins under `bubblewrap` with network access and
  their two directories only.
- Credentials live in the plugin's data directory and are the plugin's
  responsibility. The host never asks for, stores or logs them.
- `art` URLs and item text are untrusted input. Images go through the cover
  cache's size limits, and text is rendered as plain text.

## Playlists

Plugin tracks can be added to local playlists. They are stored as
`plugin://<id>/<ref>` with their metadata, so a playlist still shows them
when the plugin is stopped or removed (they play again once it is back).
M3U export leaves them out, and the UPnP ContentDirectory does not serve
them.

## Open questions

- Offline caching of resolved tracks: out of scope for v1. It depends on each
  service's terms.
- Several accounts of the same plugin: run the plugin twice with different
  `id`s. That is enough for v1.
