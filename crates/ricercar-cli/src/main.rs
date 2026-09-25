//! `ricercar-cli` — control a running ricercar over MPRIS.

use std::collections::HashMap;

use zbus::zvariant::OwnedValue;

const DEST: &str = "org.mpris.MediaPlayer2.ricercar";
const PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";

const HELP: &str = "usage: ricercar-cli <command>

  status              what is playing (default)
  play | pause | toggle | stop | next | prev
  open URI            play a file:// or http(s):// URI now
  seek [+|-]SECONDS   relative seek (e.g. seek +30, seek -10)
  volume [0..1]       get or set the volume
  shuffle [on|off]    get or set shuffle
  repeat [none|track|playlist]
  metadata            dump the MPRIS metadata";

fn conn() -> zbus::blocking::Connection {
    zbus::blocking::Connection::session().unwrap_or_else(|e| {
        eprintln!("no session bus: {e}");
        std::process::exit(1);
    })
}

fn call<T>(c: &zbus::blocking::Connection, member: &str, body: &T)
where
    T: serde::Serialize + zbus::zvariant::DynamicType,
{
    if let Err(e) = c.call_method(Some(DEST), PATH, Some(PLAYER), member, body) {
        eprintln!("{member}: {e}\n(is ricercar running?)");
        std::process::exit(1);
    }
}

fn prop(c: &zbus::blocking::Connection, name: &str) -> Option<OwnedValue> {
    c.call_method(
        Some(DEST),
        PATH,
        Some("org.freedesktop.DBus.Properties"),
        "Get",
        &(PLAYER, name),
    )
    .ok()
    .and_then(|m| m.body().deserialize::<(OwnedValue,)>().ok())
    .map(|(v,)| v)
}

fn set_prop(c: &zbus::blocking::Connection, name: &str, v: OwnedValue) {
    if let Err(e) = c.call_method(
        Some(DEST),
        PATH,
        Some("org.freedesktop.DBus.Properties"),
        "Set",
        &(PLAYER, name, v),
    ) {
        eprintln!("{name}: {e}");
        std::process::exit(1);
    }
}

fn vs(v: &OwnedValue) -> String {
    if let Ok(s) = String::try_from(v.clone()) {
        return s;
    }
    if let Ok(a) = Vec::<String>::try_from(v.clone()) {
        return a.join(", ");
    }
    if let Ok(n) = i64::try_from(v.clone()) {
        return n.to_string();
    }
    if let Ok(n) = i32::try_from(v.clone()) {
        return n.to_string();
    }
    format!("{v:?}")
}

fn metadata(c: &zbus::blocking::Connection) -> HashMap<String, OwnedValue> {
    prop(c, "Metadata")
        .and_then(|v| HashMap::<String, OwnedValue>::try_from(v).ok())
        .unwrap_or_default()
}

fn mmss(us: i64) -> String {
    let s = us.max(0) / 1_000_000;
    format!("{}:{:02}", s / 60, s % 60)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let cmd = args.next();
    if matches!(cmd.as_deref(), Some("-h" | "--help" | "help")) {
        println!("{HELP}");
        return;
    }
    let c = conn();
    match cmd.as_deref() {
        None | Some("status") => {
            let status = prop(&c, "PlaybackStatus")
                .map(|v| vs(&v))
                .unwrap_or_else(|| {
                    eprintln!("ricercar is not running");
                    std::process::exit(1);
                });
            let md = metadata(&c);
            let get = |k: &str| md.get(k).map(vs).unwrap_or_default();
            let pos = prop(&c, "Position")
                .and_then(|v| i64::try_from(v).ok())
                .unwrap_or(0);
            let len = md
                .get("mpris:length")
                .and_then(|v| i64::try_from(v.clone()).ok())
                .unwrap_or(0);
            println!("{status}: {} — {}", get("xesam:artist"), get("xesam:title"));
            if !get("xesam:album").is_empty() {
                println!("  {}", get("xesam:album"));
            }
            println!("  {} / {}", mmss(pos), mmss(len));
        }
        Some("play") => call(&c, "Play", &()),
        Some("pause") => call(&c, "Pause", &()),
        Some("toggle") => call(&c, "PlayPause", &()),
        Some("stop") => call(&c, "Stop", &()),
        Some("next") => call(&c, "Next", &()),
        Some("prev") => call(&c, "Previous", &()),
        Some("open") => {
            let Some(mut uri) = args.next() else {
                eprintln!("open needs a URI or a path");
                std::process::exit(2);
            };
            if !uri.contains("://")
                && let Ok(abs) = std::fs::canonicalize(&uri)
            {
                uri = format!("file://{}", abs.display());
            }
            call(&c, "OpenUri", &(uri.as_str(),));
        }
        Some("seek") => {
            let secs: f64 = args.next().and_then(|s| s.parse().ok()).unwrap_or_default();
            call(&c, "Seek", &((secs * 1_000_000.0) as i64,));
        }
        Some("volume") => match args.next().and_then(|s| s.parse::<f64>().ok()) {
            Some(v) => set_prop(&c, "Volume", OwnedValue::from(v.clamp(0.0, 1.0))),
            None => {
                let v = prop(&c, "Volume")
                    .and_then(|v| f64::try_from(v).ok())
                    .unwrap_or(1.0);
                println!("{v:.2}");
            }
        },
        Some("shuffle") => match args.next().as_deref() {
            Some("on") => set_prop(&c, "Shuffle", OwnedValue::from(true)),
            Some("off") => set_prop(&c, "Shuffle", OwnedValue::from(false)),
            _ => println!(
                "{}",
                prop(&c, "Shuffle")
                    .and_then(|v| bool::try_from(v).ok())
                    .map(|b| if b { "on" } else { "off" })
                    .unwrap_or("?")
            ),
        },
        Some("repeat") => match args.next() {
            Some(m) => {
                let v = match m.as_str() {
                    "track" | "one" => "Track",
                    "playlist" | "all" => "Playlist",
                    _ => "None",
                };
                set_prop(
                    &c,
                    "LoopStatus",
                    OwnedValue::try_from(zbus::zvariant::Value::from(v)).unwrap(),
                );
            }
            None => println!(
                "{}",
                prop(&c, "LoopStatus").map(|v| vs(&v)).unwrap_or_default()
            ),
        },
        Some("metadata") => {
            let mut md: Vec<_> = metadata(&c).into_iter().collect();
            md.sort_by(|a, b| a.0.cmp(&b.0));
            for (k, v) in md {
                println!("{k}: {}", vs(&v));
            }
        }
        Some(other) => {
            eprintln!("unknown command: {other}\n\n{HELP}");
            std::process::exit(2);
        }
    }
}
