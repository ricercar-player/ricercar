//! Reference source plugin for tests and screenshots (docs/plugins.md).
//! It serves a few generated FLAC files over a local HTTP server and
//! implements every method of protocol 1, error paths included. No network
//! beyond 127.0.0.1, no real service.
//!
//! Options:
//!   --protocol N       answer `initialize` with another protocol version
//!   --no-auth          no sign-in (catalogue open)
//!   --expire-preload   the first preload of each track returns an expired URL
//!   --gone-first       the first URL of each track answers 410 Gone
//!   --no-move          no `playlists.move` (answers "method not found")
//!   --radio-fail       `radio.next` answers `unavailable`
//!
//! Everything interesting is appended to `<data_dir>/calls.log`, one line
//! per event, so tests can check what the host did.
//!
//! Settings (docs/plugins.md, "Settings"): `report_playback` gates the
//! `playback.*` lines, `page_size` caps `browse.list` pages, `greeting` is
//! logged at start (it asks for a restart), and signing in adds a choice
//! to `quality`, sent with `settings.declared`.
//!
//! Optional parts: lyrics (synced for track 1, plain for track 2, track 3
//! instrumental), album/artist/label refs, actions (a "play" radio and a
//! "browse" similar albums), favourite flags, two playlists (one editable,
//! with entry ids and every `playlists.*` method), details, `radio.next`,
//! and `delivery: "proxied"` for the tracks of album 2.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use ricercar_core::meta::TagInfo;
use serde_json::{Value, json};

struct Track {
    n: u32,
    title: &'static str,
    album: u32,
    rate: u32,
    bits: u8,
    playable: bool,
}

const ARTIST: &str = "Demo Ensemble";
const ALBUMS: [(u32, &str, i32); 2] = [(1, "Demo Sessions", 2021), (2, "Night Studies", 2024)];
const TRACKS: [Track; 6] = [
    Track {
        n: 1,
        title: "Opening",
        album: 1,
        rate: 44_100,
        bits: 16,
        playable: true,
    },
    Track {
        n: 2,
        title: "Second Take",
        album: 1,
        rate: 44_100,
        bits: 16,
        playable: true,
    },
    Track {
        n: 3,
        title: "Coda",
        album: 1,
        rate: 44_100,
        bits: 16,
        playable: true,
    },
    Track {
        n: 4,
        title: "Blue Hour",
        album: 2,
        rate: 96_000,
        bits: 24,
        playable: true,
    },
    Track {
        n: 5,
        title: "Lanterns",
        album: 2,
        rate: 96_000,
        bits: 24,
        playable: true,
    },
    Track {
        n: 6,
        title: "Locked Track",
        album: 2,
        rate: 96_000,
        bits: 24,
        playable: false,
    },
];

struct Playlist {
    n: u32,
    title: String,
    /// (entry id, track number)
    entries: Vec<(String, u32)>,
    editable: bool,
}

struct Opts {
    protocol: u64,
    auth: bool,
    expire_preload: bool,
    gone_first: bool,
    no_move: bool,
    radio_fail: bool,
}

struct State {
    opts: Opts,
    data_dir: PathBuf,
    port: u16,
    favorites: HashSet<String>,
    preloaded: HashSet<String>,
    resolved: HashSet<String>,
    last_player_status: String,
    next_id: u64,
    settings: serde_json::Map<String, Value>,
    playlists: Vec<Playlist>,
    next_entry: u64,
    next_playlist: u32,
}

impl State {
    fn log(&self, line: &str) {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.data_dir.join("calls.log"))
        {
            let _ = writeln!(f, "{line}");
        }
    }

    fn signed_in(&self) -> bool {
        !self.opts.auth || self.data_dir.join("auth.json").exists()
    }

    fn auth_status(&self) -> Value {
        if self.signed_in() {
            json!({"state": "signed_in", "account": {"display_name": "Demo listener", "detail": "Free plan"}})
        } else {
            json!({"state": "signed_out"})
        }
    }

    fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn playlist(&self, r: &str) -> Option<&Playlist> {
        let n: u32 = r.strip_prefix("playlist/")?.parse().ok()?;
        self.playlists.iter().find(|p| p.n == n)
    }

    /// The playlist `r`, if the user may edit it.
    fn editable(&mut self, r: &str) -> Result<&mut Playlist, Value> {
        let n: Option<u32> = r.strip_prefix("playlist/").and_then(|n| n.parse().ok());
        match self.playlists.iter_mut().find(|p| Some(p.n) == n) {
            Some(p) if p.editable => Ok(p),
            Some(_) => Err(err(-32602, "this playlist is not yours")),
            None => Err(err(-32002, "no such playlist")),
        }
    }

    fn entry(&mut self) -> String {
        self.next_entry += 1;
        format!("e{}", self.next_entry)
    }

    fn setting(&self, key: &str) -> Value {
        self.settings.get(key).cloned().unwrap_or_else(|| {
            schema(true)
                .as_array()
                .and_then(|a| a.iter().find(|s| s["key"] == key))
                .map(|s| s["default"].clone())
                .unwrap_or(Value::Null)
        })
    }
}

/// The settings this plugin declares; signed in, `quality` offers one more
/// choice.
fn schema(signed_in: bool) -> Value {
    let mut quality = vec![
        json!({"value": "standard", "label": "Standard"}),
        json!({"value": "lossless", "label": "Lossless (CD quality)"}),
    ];
    if signed_in {
        quality.push(json!({"value": "hires", "label": "Hi-Res (up to 24-bit)"}));
    }
    json!([
        {"key": "report_playback", "type": "bool", "section": "Playback",
         "label": "Report what I play",
         "description": "Tell the service which tracks you listen to.",
         "default": true},
        {"key": "quality", "type": "choice", "section": "Playback",
         "label": "Streaming quality",
         "description": "The best format to ask for. The DAC's limits still apply.",
         "options": quality, "default": "lossless"},
        {"key": "page_size", "type": "number", "section": "Browsing",
         "label": "Items per page", "integer": true, "min": 10, "max": 200,
         "unit": "items", "default": 100},
        {"key": "greeting", "type": "string", "section": "Browsing",
         "label": "Greeting", "description": "Written to the log when the plugin starts.",
         "placeholder": "Hello", "max_length": 80, "restart": true,
         "default": "Hello from Demo Music"}
    ])
}

fn err(code: i64, message: &str) -> Value {
    json!({"code": code, "message": message})
}

fn track_item(st: &State, t: &Track) -> Value {
    let (_, album, year) = ALBUMS.iter().find(|a| a.0 == t.album).unwrap();
    json!({
        "ref": format!("track/{}", t.n),
        "kind": "track",
        "title": t.title,
        "subtitle": format!("{ARTIST} · {album}"),
        "artist": ARTIST,
        "album": album,
        "album_artist": ARTIST,
        "track_no": (t.n - 1) % 3 + 1,
        "year": year,
        "genre": "Demo",
        "duration_ms": 1000,
        "art": format!("{}/art/{}.jpg", st.base(), t.album),
        "format": {"sample_rate": t.rate, "bits": t.bits, "codec": "flac"},
        "playable": t.playable,
        "album_ref": format!("album/{}", t.album),
        "artist_ref": "artist/1",
        "label_ref": "label/1",
        "favorite": st.favorites.contains(&format!("track/{}", t.n)),
        "actions": [
            {"id": "radio", "label": "Track radio", "ref": format!("radio/track/{}", t.n), "kind": "play"}
        ],
    })
}

fn album_item(st: &State, a: &(u32, &str, i32)) -> Value {
    json!({
        "ref": format!("album/{}", a.0),
        "kind": "album",
        "title": a.1,
        "subtitle": format!("{ARTIST} · {}", a.2),
        "artist": ARTIST,
        "year": a.2,
        "art": format!("{}/art/{}.jpg", st.base(), a.0),
        "browsable": true,
        "artist_ref": "artist/1",
        "label_ref": "label/1",
        "favorite": st.favorites.contains(&format!("album/{}", a.0)),
        "actions": [
            {"id": "radio", "label": "Album radio", "ref": format!("radio/album/{}", a.0), "kind": "play"},
            {"id": "similar", "label": "Similar albums", "ref": format!("similar/album/{}", a.0), "kind": "browse"}
        ],
    })
}

fn artist_item(st: &State) -> Value {
    json!({
        "ref": "artist/1",
        "kind": "artist",
        "title": ARTIST,
        "subtitle": "2 albums",
        "art": format!("{}/art/1.jpg", st.base()),
        "browsable": true,
        "favorite": st.favorites.contains("artist/1"),
        "actions": [
            {"id": "radio", "label": "Artist radio", "ref": "radio/artist/1", "kind": "play"}
        ],
    })
}

fn playlist_item(st: &State, p: &Playlist) -> Value {
    json!({
        "ref": format!("playlist/{}", p.n), "kind": "playlist", "title": p.title,
        "subtitle": format!("{} tracks", p.entries.len()),
        "track_count": p.entries.len(),
        "art": format!("{}/art/1.jpg", st.base()), "browsable": true,
        "editable": p.editable,
    })
}

/// Playable tracks for a radio seeded by `seed`, none of `exclude`.
fn radio_tracks(st: &State, seed: &str, exclude: &[String], limit: usize) -> Vec<Value> {
    let seed_album: Option<u32> = seed.strip_prefix("album/").and_then(|n| n.parse().ok());
    TRACKS
        .iter()
        .filter(|t| t.playable)
        .filter(|t| seed_album.is_none_or(|a| t.album != a))
        .filter(|t| {
            let r = format!("track/{}", t.n);
            r != seed && !exclude.contains(&r)
        })
        .take(limit)
        .map(|t| track_item(st, t))
        .collect()
}

fn children(st: &State, r: &str) -> Option<Vec<Value>> {
    let tracks = |f: &dyn Fn(&Track) -> bool| -> Vec<Value> {
        TRACKS
            .iter()
            .filter(|t| f(t))
            .map(|t| track_item(st, t))
            .collect()
    };
    Some(match r {
        "albums" => ALBUMS.iter().map(|a| album_item(st, a)).collect(),
        "playlists" => st.playlists.iter().map(|p| playlist_item(st, p)).collect(),
        "favorites" => tracks(&|t| st.favorites.contains(&format!("track/{}", t.n))),
        "artist/1" | "label/1" => ALBUMS.iter().map(|a| album_item(st, a)).collect(),
        _ if r.starts_with("playlist/") => st
            .playlist(r)?
            .entries
            .iter()
            .filter_map(|(e, n)| {
                let t = TRACKS.iter().find(|t| t.n == *n)?;
                let mut v = track_item(st, t);
                v["entry_id"] = json!(e);
                Some(v)
            })
            .collect(),
        _ if r.starts_with("radio/") => radio_tracks(st, &r["radio/".len()..], &[], 50),
        _ if r.starts_with("similar/album/") => {
            let n: u32 = r["similar/album/".len()..].parse().ok()?;
            ALBUMS
                .iter()
                .filter(|a| a.0 != n)
                .map(|a| album_item(st, a))
                .collect()
        }
        _ => {
            let n: u32 = r.strip_prefix("album/")?.parse().ok()?;
            ALBUMS.iter().find(|a| a.0 == n)?;
            tracks(&|t| t.album == n)
        }
    })
}

fn find_item(st: &State, r: &str) -> Option<Value> {
    if let Some(n) = r.strip_prefix("track/").and_then(|n| n.parse::<u32>().ok()) {
        return TRACKS.iter().find(|t| t.n == n).map(|t| track_item(st, t));
    }
    if let Some(n) = r.strip_prefix("album/").and_then(|n| n.parse::<u32>().ok()) {
        return ALBUMS.iter().find(|a| a.0 == n).map(|a| album_item(st, a));
    }
    if r == "artist/1" {
        return Some(artist_item(st));
    }
    st.playlist(r).map(|p| playlist_item(st, p))
}

fn lyrics(r: &str) -> Option<Value> {
    Some(match r {
        "track/1" => json!({
            "synced": [
                {"time_ms": 0, "text": "First light on the demo line"},
                {"time_ms": 400, "text": "Every sample where it should be"},
                {"time_ms": 800, "text": ""}
            ],
            "plain": "First light on the demo line\nEvery sample where it should be"
        }),
        "track/2" => json!({"plain": "A second take\nA little slower"}),
        "track/3" => json!({"instrumental": true}),
        _ => return None,
    })
}

fn details(st: &State, r: &str) -> Option<Value> {
    if r == "artist/1" {
        return Some(json!({
            "biography": {
                "text": "Demo Ensemble records short test pieces for ricercar.\n\nEvery track lasts one second.",
                "source": "Demo Music"
            },
            "related": [
                {"title": "Albums", "items": ALBUMS.iter().map(|a| album_item(st, a)).collect::<Vec<_>>()},
                {"title": "Top tracks", "items": radio_tracks(st, "", &[], 3)}
            ],
            "facts": [{"label": "Formed", "value": "2021"}, {"label": "Label", "value": "Demo Records"}]
        }));
    }
    let n: u32 = r.strip_prefix("album/")?.parse().ok()?;
    let (_, _, year) = ALBUMS.iter().find(|a| a.0 == n)?;
    Some(json!({
        "related": [{"title": "Similar albums", "items": children(st, &format!("similar/album/{n}"))?}],
        "facts": [{"label": "Label", "value": "Demo Records"}, {"label": "Released", "value": year.to_string()}]
    }))
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Serve `t<n>.flac` (with Content-Length), `art/<n>.jpg` and a login page.
/// URLs carry `?exp=<unix>&tok=<t>`; an expired or "gone" URL answers 410.
fn serve(listener: TcpListener, dir: PathBuf, gone: Arc<Mutex<HashSet<String>>>) {
    for stream in listener.incoming().flatten() {
        let dir = dir.clone();
        let gone = gone.clone();
        std::thread::spawn(move || {
            let mut stream = stream;
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            if reader.read_line(&mut first).is_err() {
                return;
            }
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).is_err() || h.trim().is_empty() {
                    break;
                }
            }
            let target = first.split_whitespace().nth(1).unwrap_or("/").to_string();
            let head_only = first.starts_with("HEAD");
            let (path, query) = target.split_once('?').unwrap_or((&target, ""));
            let q: HashMap<&str, &str> = query
                .split('&')
                .filter_map(|kv| kv.split_once('='))
                .collect();
            let expired = q
                .get("exp")
                .and_then(|e| e.parse::<i64>().ok())
                .is_some_and(|e| e < now());
            let is_gone = q
                .get("tok")
                .is_some_and(|t| gone.lock().unwrap().contains(*t));
            let (status, ctype, body): (&str, &str, Vec<u8>) = if path == "/login" {
                (
                    "200 OK",
                    "text/plain; charset=utf-8",
                    b"Demo Music sign-in: the code is DEMO\n".to_vec(),
                )
            } else if expired || is_gone {
                ("410 Gone", "text/plain", Vec::new())
            } else if let Some(f) = path.strip_prefix("/t/") {
                match std::fs::read(dir.join(f)) {
                    Ok(b) => ("200 OK", "audio/flac", b),
                    Err(_) => ("404 Not Found", "text/plain", Vec::new()),
                }
            } else if let Some(f) = path.strip_prefix("/art/") {
                match std::fs::read(dir.join(format!("art-{f}"))) {
                    Ok(b) => ("200 OK", "image/jpeg", b),
                    Err(_) => ("404 Not Found", "text/plain", Vec::new()),
                }
            } else {
                ("404 Not Found", "text/plain", Vec::new())
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            if !head_only {
                let _ = stream.write_all(&body);
            }
        });
    }
}

fn prepare_files(dir: &PathBuf) {
    let _ = std::fs::create_dir_all(dir);
    for t in &TRACKS {
        let p = dir.join(format!("t{}.flac", t.n));
        if !p.exists() {
            let tags = TagInfo {
                title: Some(t.title.into()),
                artist: Some(ARTIST.into()),
                sample_rate: Some(t.rate),
                bits: Some(t.bits),
                ..Default::default()
            };
            ricercar_core::synth::write_flac(&p, &tags).expect("write flac");
        }
    }
    for (n, rgb) in [(1u32, [178u8, 96, 54]), (2, [52, 84, 150])] {
        let p = dir.join(format!("art-{n}.jpg"));
        if !p.exists() {
            let img = image::RgbImage::from_fn(300, 300, |x, y| {
                let k = ((x + y) / 6) as u8;
                image::Rgb(rgb.map(|c| c.saturating_sub(k)))
            });
            let _ = img.save(&p);
        }
    }
}

fn main() {
    let mut opts = Opts {
        protocol: 1,
        auth: true,
        expire_preload: false,
        gone_first: false,
        no_move: false,
        radio_fail: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--protocol" => opts.protocol = args.next().and_then(|v| v.parse().ok()).unwrap_or(1),
            "--no-auth" => opts.auth = false,
            "--expire-preload" => opts.expire_preload = true,
            "--gone-first" => opts.gone_first = true,
            "--no-move" => opts.no_move = true,
            "--radio-fail" => opts.radio_fail = true,
            _ => {}
        }
    }
    let gone = Arc::new(Mutex::new(HashSet::new()));
    let mut st = State {
        opts,
        data_dir: std::env::temp_dir(),
        port: 0,
        favorites: HashSet::new(),
        preloaded: HashSet::new(),
        resolved: HashSet::new(),
        last_player_status: String::new(),
        next_id: 1,
        settings: serde_json::Map::new(),
        playlists: vec![
            Playlist {
                n: 1,
                title: "Demo mix".into(),
                entries: vec![("e1".into(), 1), ("e2".into(), 4), ("e3".into(), 2)],
                editable: true,
            },
            Playlist {
                n: 2,
                title: "Followed picks".into(),
                entries: vec![("e4".into(), 5), ("e5".into(), 3)],
                editable: false,
            },
        ],
        next_entry: 5,
        next_playlist: 3,
    };
    let stdout = std::io::stdout();
    let send = |v: Value| {
        let mut out = stdout.lock();
        let _ = writeln!(out, "{v}");
        let _ = out.flush();
    };
    let mut input = String::new();
    let mut stdin = BufReader::new(std::io::stdin());
    loop {
        input.clear();
        match stdin.read_line(&mut input) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let Ok(msg) = serde_json::from_str::<Value>(&input) else {
            continue;
        };
        let method = msg
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let id = msg.get("id").cloned();
        if method.is_empty() {
            // An answer to one of our requests (remote control).
            st.log(&format!(
                "remote-reply {}",
                msg.get("result")
                    .or(msg.get("error"))
                    .unwrap_or(&Value::Null)
            ));
            continue;
        }
        let Some(id) = id else {
            // Notifications from the host.
            match method.as_str() {
                "settings.changed" => {
                    if let Some(m) = params.get("settings").and_then(Value::as_object) {
                        st.settings = m.clone();
                    }
                    st.log(&format!("settings.changed {}", params["settings"]));
                }
                "locale.changed" => st.log(&format!("locale.changed {}", params["locale"])),
                m if m.starts_with("playback.")
                    && st.setting("report_playback") == Value::Bool(false) => {}
                "player.state" => {
                    let s = params
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if s != st.last_player_status {
                        st.log(&format!("player.state {s}"));
                        st.last_player_status = s;
                    }
                }
                _ => {
                    let r = params.get("ref").and_then(Value::as_str).unwrap_or("");
                    st.log(format!("{method} {r}").trim_end());
                }
            }
            continue;
        };
        let r = params
            .get("ref")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let needs_auth = matches!(
            method.as_str(),
            "browse.root"
                | "browse.list"
                | "search"
                | "item.get"
                | "favorites.set"
                | "track.resolve"
                | "lyrics.get"
                | "item.details"
                | "radio.next"
        ) || method.starts_with("playlists.");
        let result: Result<Value, Value> = if needs_auth && !st.signed_in() {
            Err(err(-32001, "sign in first"))
        } else {
            match method.as_str() {
                "initialize" => {
                    st.data_dir = params["data_dir"]
                        .as_str()
                        .map(PathBuf::from)
                        .unwrap_or(st.data_dir.clone());
                    let cache = params["cache_dir"]
                        .as_str()
                        .map(PathBuf::from)
                        .unwrap_or(st.data_dir.clone());
                    prepare_files(&cache);
                    let l = TcpListener::bind("127.0.0.1:0").expect("bind");
                    st.port = l.local_addr().unwrap().port();
                    let g = gone.clone();
                    std::thread::spawn(move || serve(l, cache, g));
                    st.log(&format!(
                        "initialize {}",
                        params["output"]["device"].as_str().unwrap_or("")
                    ));
                    eprintln!("demo plugin serving on port {}", st.port);
                    if let Some(m) = params.get("settings").and_then(Value::as_object) {
                        st.settings = m.clone();
                    }
                    st.log(&format!("settings {}", params["settings"]));
                    st.log(&format!("locale {}", params["locale"]));
                    st.log(&format!(
                        "greeting {}",
                        st.setting("greeting").as_str().unwrap_or("")
                    ));
                    Ok(json!({
                        "protocol": st.opts.protocol,
                        "plugin": {"id": "demo", "name": "Demo Music", "version": "1.0.0"},
                        "capabilities": {
                            "auth": st.opts.auth, "browse": true, "search": true, "resolve": true,
                            "favorites": true, "reporting": true, "remote_control": true,
                            "library": true, "lyrics": true, "playlist_edit": true,
                            "details": true, "radio": true
                        },
                        "settings": schema(false)
                    }))
                }
                "shutdown" => {
                    st.log("shutdown");
                    send(json!({"jsonrpc": "2.0", "id": id, "result": null}));
                    std::process::exit(0);
                }
                "demo.crash" => std::process::exit(3),
                "demo.remote" => {
                    // Drive the host: the answer is logged when it comes.
                    let rid = format!("demo-{}", st.next_id);
                    st.next_id += 1;
                    send(
                        json!({"jsonrpc": "2.0", "id": rid, "method": params["method"], "params": params["params"]}),
                    );
                    Ok(json!({"sent": rid}))
                }
                "auth.status" => Ok(st.auth_status()),
                "auth.begin" => Ok(json!({
                    "url": format!("{}/login", st.base()),
                    "instructions": "Enter the code DEMO.",
                    "expects_input": true
                })),
                "auth.complete" => {
                    let input = params["input"].as_str().unwrap_or("").trim().to_string();
                    if input.eq_ignore_ascii_case("demo") {
                        let _ =
                            std::fs::write(st.data_dir.join("auth.json"), "{\"user\":\"demo\"}");
                    }
                    st.log("auth.complete");
                    Ok(st.auth_status())
                }
                "auth.sign_out" => {
                    let _ = std::fs::remove_file(st.data_dir.join("auth.json"));
                    Ok(Value::Null)
                }
                "browse.root" => Ok(json!({"sections": [
                    {"ref": "albums", "kind": "folder", "title": "Albums", "browsable": true},
                    {"ref": "playlists", "kind": "folder", "title": "Playlists", "browsable": true},
                    {"ref": "favorites", "kind": "folder", "title": "Favourites", "browsable": true}
                ], "home": [
                    {"ref": "albums", "kind": "folder", "title": "New releases", "browsable": true}
                ]})),
                "browse.list" => match children(&st, &r) {
                    Some(items) => {
                        let offset = params["offset"].as_u64().unwrap_or(0) as usize;
                        let page_size = st.setting("page_size").as_u64().unwrap_or(200);
                        let limit = params["limit"].as_u64().unwrap_or(200).min(page_size) as usize;
                        let total = items.len();
                        let page: Vec<Value> = items.into_iter().skip(offset).take(limit).collect();
                        Ok(
                            json!({"items": page, "total": total, "has_more": offset + limit < total}),
                        )
                    }
                    None => Err(err(-32002, "no such item")),
                },
                "search" => {
                    let q = params["query"].as_str().unwrap_or("").to_lowercase();
                    match q.as_str() {
                        "ratelimit" => Err(
                            json!({"code": -32004, "message": "slow down", "data": {"retry_after": 1}}),
                        ),
                        "offline" => Err(err(-32005, "no network")),
                        _ => {
                            let tracks: Vec<Value> = TRACKS
                                .iter()
                                .filter(|t| {
                                    t.title.to_lowercase().contains(&q)
                                        || ARTIST.to_lowercase().contains(&q)
                                })
                                .map(|t| track_item(&st, t))
                                .collect();
                            let albums: Vec<Value> = ALBUMS
                                .iter()
                                .filter(|a| {
                                    a.1.to_lowercase().contains(&q)
                                        || ARTIST.to_lowercase().contains(&q)
                                })
                                .map(|a| album_item(&st, a))
                                .collect();
                            let by_artist = ARTIST.to_lowercase().contains(&q);
                            let artists: Vec<Value> = if by_artist {
                                vec![artist_item(&st)]
                            } else {
                                Vec::new()
                            };
                            let playlists: Vec<Value> = if by_artist || "demo mix".contains(&q) {
                                children(&st, "playlists").unwrap_or_default()
                            } else {
                                Vec::new()
                            };
                            Ok(json!({"groups": [
                                {"kind": "artist", "items": artists, "has_more": false},
                                {"kind": "album", "items": albums, "has_more": false},
                                {"kind": "playlist", "items": playlists, "has_more": false},
                                {"kind": "track", "items": tracks, "has_more": false}
                            ]}))
                        }
                    }
                }
                "library.albums" | "library.artists" | "library.tracks" | "library.playlists" => {
                    let all: Vec<Value> = match method.as_str() {
                        "library.albums" => ALBUMS.iter().map(|a| album_item(&st, a)).collect(),
                        "library.artists" => vec![artist_item(&st)],
                        "library.playlists" => children(&st, "playlists").unwrap_or_default(),
                        _ => TRACKS.iter().map(|t| track_item(&st, t)).collect(),
                    };
                    let offset = params["offset"].as_u64().unwrap_or(0) as usize;
                    let limit = params["limit"].as_u64().unwrap_or(200).min(200) as usize;
                    let total = all.len();
                    let page: Vec<Value> = all.into_iter().skip(offset).take(limit).collect();
                    Ok(json!({"items": page, "total": total, "has_more": offset + limit < total}))
                }
                "item.get" => find_item(&st, &r).ok_or_else(|| err(-32002, "no such item")),
                "lyrics.get" => {
                    st.log(&format!("lyrics.get {r}"));
                    lyrics(&r).ok_or_else(|| err(-32002, "no lyrics"))
                }
                "item.details" => {
                    st.log(&format!("item.details {r}"));
                    details(&st, &r).ok_or_else(|| err(-32002, "no details"))
                }
                "radio.next" => {
                    let seed = params["seed"].as_str().unwrap_or("").to_string();
                    let exclude: Vec<String> = params["exclude"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default();
                    let limit = params["limit"].as_u64().unwrap_or(10) as usize;
                    st.log(&format!(
                        "radio.next {seed} exclude={} limit={limit}",
                        exclude.join(",")
                    ));
                    if st.opts.radio_fail {
                        Err(err(-32003, "no radio in your plan"))
                    } else {
                        Ok(json!({ "items": radio_tracks(&st, &seed, &exclude, limit) }))
                    }
                }
                "playlists.create" => {
                    let name = params["name"].as_str().unwrap_or("").trim().to_string();
                    if name.is_empty() {
                        Err(err(-32602, "name"))
                    } else {
                        st.log(&format!("playlists.create {name}"));
                        let n = st.next_playlist;
                        st.next_playlist += 1;
                        st.playlists.push(Playlist {
                            n,
                            title: name,
                            entries: Vec::new(),
                            editable: true,
                        });
                        Ok(playlist_item(&st, st.playlists.last().unwrap()))
                    }
                }
                "playlists.rename" => {
                    let name = params["name"].as_str().unwrap_or("").to_string();
                    st.log(&format!("playlists.rename {r} {name}"));
                    st.editable(&r).map(|p| {
                        p.title = name;
                        Value::Null
                    })
                }
                "playlists.delete" => {
                    st.log(&format!("playlists.delete {r}"));
                    st.editable(&r).map(|_| ()).map(|()| {
                        st.playlists.retain(|p| format!("playlist/{}", p.n) != r);
                        Value::Null
                    })
                }
                "playlists.add" => {
                    let refs: Vec<String> = params["items"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default();
                    st.log(&format!("playlists.add {r} {}", refs.join(",")));
                    let ns: Option<Vec<u32>> = refs
                        .iter()
                        .map(|x| {
                            let n: u32 = x.strip_prefix("track/")?.parse().ok()?;
                            TRACKS.iter().any(|t| t.n == n).then_some(n)
                        })
                        .collect();
                    match ns {
                        None => Err(err(-32602, "unknown track")),
                        Some(ns) => {
                            let entries: Vec<(String, u32)> =
                                ns.into_iter().map(|n| (st.entry(), n)).collect();
                            st.editable(&r).map(|p| {
                                p.entries.extend(entries);
                                Value::Null
                            })
                        }
                    }
                }
                "playlists.remove" => {
                    let entries: Vec<String> = params["entries"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default();
                    st.log(&format!("playlists.remove {r} {}", entries.join(",")));
                    st.editable(&r).map(|p| {
                        p.entries.retain(|(e, _)| !entries.contains(e));
                        Value::Null
                    })
                }
                "playlists.move" if st.opts.no_move => Err(err(-32601, "method not found")),
                "playlists.move" => {
                    let entry = params["entry"].as_str().unwrap_or("").to_string();
                    let to = params["to"].as_u64().unwrap_or(0) as usize;
                    st.log(&format!("playlists.move {r} {entry} {to}"));
                    st.editable(&r).and_then(|p| {
                        let from = p
                            .entries
                            .iter()
                            .position(|(e, _)| *e == entry)
                            .ok_or_else(|| err(-32002, "no such entry"))?;
                        let e = p.entries.remove(from);
                        let to = to.min(p.entries.len());
                        p.entries.insert(to, e);
                        Ok(Value::Null)
                    })
                }
                "favorites.set" => {
                    st.log(&format!("favorites.set {r} {}", params["on"]));
                    if params["on"].as_bool().unwrap_or(false) {
                        st.favorites.insert(r.clone());
                    } else {
                        st.favorites.remove(&r);
                    }
                    Ok(Value::Null)
                }
                "track.resolve" => {
                    let purpose = params["purpose"].as_str().unwrap_or("play").to_string();
                    st.log(&format!("track.resolve {r} {purpose}"));
                    let t = r
                        .strip_prefix("track/")
                        .and_then(|n| n.parse::<u32>().ok())
                        .and_then(|n| TRACKS.iter().find(|t| t.n == n));
                    match t {
                        None => Err(err(-32002, "no such track")),
                        Some(t) if !t.playable => Err(err(-32003, "not in your plan")),
                        Some(t) => {
                            let first_preload =
                                purpose == "preload" && st.preloaded.insert(r.clone());
                            let exp = if st.opts.expire_preload && first_preload {
                                now() - 1
                            } else {
                                now() + 600
                            };
                            let tok = format!("{}-{}", t.n, st.next_id);
                            st.next_id += 1;
                            if st.opts.gone_first && st.resolved.insert(r.clone()) {
                                gone.lock().unwrap().insert(tok.clone());
                            }
                            Ok(json!({
                                "url": format!("{}/t/t{}.flac?exp={exp}&tok={tok}", st.base(), t.n),
                                "expires_at": exp,
                                "duration_ms": 1000,
                                "format": {"sample_rate": t.rate, "bits": t.bits, "channels": 2, "codec": "flac"},
                                "replaygain": {"track_gain": -3.5, "track_peak": 0.9},
                                "live": false,
                                // Album 2 goes through a relay of the plugin.
                                "delivery": if t.album == 2 { "proxied" } else { "direct" }
                            }))
                        }
                    }
                }
                _ => Err(err(-32601, "method not found")),
            }
        };
        let reply = match result {
            Ok(v) => json!({"jsonrpc": "2.0", "id": id, "result": v}),
            Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": e}),
        };
        send(reply);
        // Choices that depend on the account: declared again once it is
        // known, and whenever it changes.
        if matches!(
            method.as_str(),
            "initialize" | "auth.complete" | "auth.sign_out"
        ) && (method != "initialize" || st.signed_in())
        {
            send(json!({"jsonrpc": "2.0", "method": "settings.declared",
                "params": {"settings": schema(st.signed_in())}}));
            st.log(&format!("settings.declared {}", st.signed_in()));
        }
    }
}
