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
//!
//! Everything interesting is appended to `<data_dir>/calls.log`, one line
//! per event, so tests can check what the host did.

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
const PLAYLIST: [u32; 3] = [1, 4, 2];

struct Opts {
    protocol: u64,
    auth: bool,
    expire_preload: bool,
    gone_first: bool,
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
    })
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
        "playlists" => vec![json!({
            "ref": "playlist/1", "kind": "playlist", "title": "Demo mix",
            "subtitle": "3 tracks", "art": format!("{}/art/1.jpg", st.base()), "browsable": true,
        })],
        "favorites" => tracks(&|t| st.favorites.contains(&format!("track/{}", t.n))),
        "artist/1" => ALBUMS.iter().map(|a| album_item(st, a)).collect(),
        "playlist/1" => PLAYLIST
            .iter()
            .filter_map(|n| TRACKS.iter().find(|t| t.n == *n))
            .map(|t| track_item(st, t))
            .collect(),
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
    None
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
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--protocol" => opts.protocol = args.next().and_then(|v| v.parse().ok()).unwrap_or(1),
            "--no-auth" => opts.auth = false,
            "--expire-preload" => opts.expire_preload = true,
            "--gone-first" => opts.gone_first = true,
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
        );
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
                    Ok(json!({
                        "protocol": st.opts.protocol,
                        "plugin": {"id": "demo", "name": "Demo Music", "version": "1.0.0"},
                        "capabilities": {
                            "auth": st.opts.auth, "browse": true, "search": true, "resolve": true,
                            "favorites": true, "reporting": true, "remote_control": true,
                            "library": true
                        }
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
                ]})),
                "browse.list" => match children(&st, &r) {
                    Some(items) => {
                        let offset = params["offset"].as_u64().unwrap_or(0) as usize;
                        let limit = params["limit"].as_u64().unwrap_or(200).min(200) as usize;
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
                            Ok(json!({"groups": [
                                {"kind": "album", "items": albums, "has_more": false},
                                {"kind": "track", "items": tracks, "has_more": false}
                            ]}))
                        }
                    }
                }
                "library.albums" | "library.artists" | "library.tracks" => {
                    let all: Vec<Value> = match method.as_str() {
                        "library.albums" => ALBUMS.iter().map(|a| album_item(&st, a)).collect(),
                        "library.artists" => vec![artist_item(&st)],
                        _ => TRACKS.iter().map(|t| track_item(&st, t)).collect(),
                    };
                    let offset = params["offset"].as_u64().unwrap_or(0) as usize;
                    let limit = params["limit"].as_u64().unwrap_or(200).min(200) as usize;
                    let total = all.len();
                    let page: Vec<Value> = all.into_iter().skip(offset).take(limit).collect();
                    Ok(json!({"items": page, "total": total, "has_more": offset + limit < total}))
                }
                "item.get" => find_item(&st, &r).ok_or_else(|| err(-32002, "no such item")),
                "favorites.set" => {
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
                                "live": false
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
    }
}
