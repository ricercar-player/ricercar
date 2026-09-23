use std::collections::HashMap;

use zbus::zvariant::OwnedValue;

const DEST: &str = "org.mpris.MediaPlayer2.ricercar";
const PATH: &str = "/org/mpris/MediaPlayer2";

fn conn() -> zbus::blocking::Connection {
    match zbus::blocking::Connection::session() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("no session bus: {e}");
            std::process::exit(1);
        }
    }
}

fn call<T>(c: &zbus::blocking::Connection, member: &str, body: &T)
where
    T: serde::Serialize + zbus::zvariant::DynamicType,
{
    match c.call_method(Some(DEST), PATH, Some("org.mpris.MediaPlayer2.Player"), member, body) {
        Ok(_) => {}
        Err(e) => {
            eprintln!("{member}: {e}");
            std::process::exit(1);
        }
    }
}

fn prop(c: &zbus::blocking::Connection, iface: &str, name: &str) -> Option<OwnedValue> {
    c.call_method(
        Some(DEST),
        PATH,
        Some("org.freedesktop.DBus.Properties"),
        "Get",
        &(iface, name),
    )
    .ok()
    .and_then(|m| m.body().deserialize::<(OwnedValue,)>().ok())
    .map(|(v,)| v)
}

fn vs(v: &OwnedValue) -> String {
    if let Ok(s) = String::try_from(v.clone()) {
        return s;
    }
    if let Ok(a) = Vec::<String>::try_from(v.clone()) {
        return a.join(", ");
    }
    format!("{v:?}")
}

fn str_prop(c: &zbus::blocking::Connection, iface: &str, name: &str) -> String {
    prop(c, iface, name)
        .and_then(|v| String::try_from(v).ok())
        .unwrap_or_default()
}

fn metadata(c: &zbus::blocking::Connection) -> HashMap<String, OwnedValue> {
    prop(c, "org.mpris.MediaPlayer2.Player", "Metadata")
        .and_then(|v| HashMap::<String, OwnedValue>::try_from(v).ok())
        .unwrap_or_default()
}

fn main() {
    let mut args = std::env::args().skip(1);
    let cmd = args.next();
    let c = conn();
    match cmd.as_deref() {
        None | Some("status") => {
            let status = str_prop(&c, "org.mpris.MediaPlayer2.Player", "PlaybackStatus");
            let md = metadata(&c);
            let get = |k: &str| -> String {
                md.get(k)
                    .map(|v| vs(v))
                    .unwrap_or_default()
            };
            let pos = prop(&c, "org.mpris.MediaPlayer2.Player", "Position")
                .and_then(|v| i64::try_from(v).ok())
                .unwrap_or(0);
            println!("{status}  {} — {}  [{pos}]", get("xesam:artist"), get("xesam:title"));
        }
        Some("play") => call(&c, "Play", &()),
        Some("pause") => call(&c, "Pause", &()),
        Some("toggle") => call(&c, "PlayPause", &()),
        Some("stop") => call(&c, "Stop", &()),
        Some("next") => call(&c, "Next", &()),
        Some("prev") => call(&c, "Previous", &()),
        Some("open") => {
            let uri = args.next().unwrap_or_default();
            call(&c, "OpenURI", &(uri.as_str(),));
        }
        Some("seek") => {
            let secs: i64 = args
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or_default();
            call(&c, "Seek", &(secs * 1_000_000,));
        }
        Some("volume") => {
            match args.next().and_then(|s| s.parse::<f64>().ok()) {
                Some(v) => {
                    let _ = c.call_method(
                        Some(DEST),
                        PATH,
                        Some("org.freedesktop.DBus.Properties"),
                        "Set",
                        &("org.mpris.MediaPlayer2.Player", "Volume", OwnedValue::try_from(v).unwrap()),
                    );
                }
                None => {
                    let v = prop(&c, "org.mpris.MediaPlayer2.Player", "Volume")
                        .and_then(|v| f64::try_from(v).ok())
                        .unwrap_or(1.0);
                    println!("{v:.2}");
                }
            }
        }
        Some("metadata") => {
            for (k, v) in metadata(&c) {
                println!("{k}: {}", vs(&v));
            }
        }
        Some(other) => {
            eprintln!("unknown command: {other}\ncommands: status play pause toggle stop next prev open seek volume metadata");
            std::process::exit(2);
        }
    }
}
