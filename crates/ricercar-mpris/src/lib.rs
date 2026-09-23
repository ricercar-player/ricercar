use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ricercar_audio::TransportStatus;
use ricercar_core::Controller;
use zbus::zvariant::{ObjectPath, OwnedValue, Value};
use zbus::interface;

pub const BUS_NAME: &str = "org.mpris.MediaPlayer2.ricercar";
pub const PATH: &str = "/org/mpris/MediaPlayer2";

pub struct MprisPlayer {
    ctl: Arc<Controller>,
}

fn track_id(uri: Option<&str>) -> ObjectPath<'static> {
    let uri = uri.unwrap_or("");
    let mut h: u64 = 0xcbf29ce484222325;
    for b in uri.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    ObjectPath::from_string_unchecked(format!("/ricercar/track/{}", h % 1_000_000_007))
}

fn meta_value(v: impl Into<Value<'static>>) -> OwnedValue {
    OwnedValue::try_from(v.into()).unwrap()
}

#[interface(name = "org.mpris.MediaPlayer2.Player")]
impl MprisPlayer {
    fn play(&self) {
        let st = self.ctl.state.lock().unwrap().status;
        match st {
            TransportStatus::Paused => self.ctl.resume(),
            TransportStatus::Stopped => {}
            TransportStatus::Playing => {}
        }
    }
    fn pause(&self) {
        self.ctl.pause();
    }
    fn play_pause(&self) {
        self.ctl.toggle();
    }
    fn stop(&self) {
        self.ctl.stop();
    }
    fn next(&self) {
        self.ctl.next();
    }
    fn previous(&self) {
        self.ctl.prev();
    }
    fn seek(&self, offset: i64) {
        let ms = (offset / 1000).max(0) as u64;
        self.ctl.seek_ms(ms);
    }
    fn set_position(&self, _track_id: ObjectPath<'_>, position: i64) {
        self.ctl.seek_ms((position / 1000).max(0) as u64);
    }
    #[zbus(name = "OpenURI")]
    fn open_uri(&self, uri: &str) {
        self.ctl.set_remote(uri, None);
    }

    #[zbus(property)]
    fn playback_status(&self) -> String {
        match self.ctl.state.lock().unwrap().status {
            TransportStatus::Playing => "Playing",
            TransportStatus::Paused => "Paused",
            TransportStatus::Stopped => "Stopped",
        }
        .into()
    }
    #[zbus(property)]
    fn loop_status(&self) -> String {
        "None".into()
    }
    #[zbus(property)]
    fn shuffle(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn metadata(&self) -> HashMap<String, OwnedValue> {
        let mut m = HashMap::new();
        let st = self.ctl.state.lock().unwrap();
        let uri = st.current_uri.clone();
        m.insert("mpris:trackid".into(), meta_value(track_id(uri.as_deref())));
        if let Some(uri) = &uri {
            m.insert("xesam:url".into(), meta_value(uri.clone()));
            if let Some(t) = st.meta_map.get(uri) {
                m.insert("xesam:title".into(), meta_value(t.title.clone()));
                if let Some(a) = &t.artist {
                    m.insert("xesam:artist".into(), meta_value(vec![a.clone()]));
                }
                if let Some(al) = &t.album {
                    m.insert("xesam:album".into(), meta_value(al.clone()));
                }
                m.insert(
                    "mpris:length".into(),
                    meta_value(t.duration_ms as i64 * 1000),
                );
                if let Some(c) = &t.cover {
                    let url = if c.starts_with('/') {
                        format!("file://{c}")
                    } else {
                        c.clone()
                    };
                    m.insert("mpris:artUrl".into(), meta_value(url));
                }
            }
        }
        m
    }
    #[zbus(property)]
    fn can_go_next(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn can_go_previous(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn can_play(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn can_pause(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn can_seek(&self) -> bool {
        self.ctl.state.lock().unwrap().status != TransportStatus::Stopped
    }
    #[zbus(property)]
    fn can_control(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn position(&self) -> i64 {
        self.ctl.state.lock().unwrap().pos_ms as i64 * 1000
    }
    #[zbus(property)]
    fn minimum_rate(&self) -> f64 {
        1.0
    }
    #[zbus(property)]
    fn maximum_rate(&self) -> f64 {
        1.0
    }
    #[zbus(property)]
    fn volume(&self) -> f64 {
        self.ctl.state.lock().unwrap().volume as f64 / 100.0
    }
    #[zbus(property)]
    fn set_volume(&self, value: f64) {
        self.ctl.set_volume((value * 100.0).clamp(0.0, 100.0) as u32);
    }
}

pub struct MprisRoot;

#[interface(name = "org.mpris.MediaPlayer2")]
impl MprisRoot {
    fn raise(&self) {}
    fn quit(&self) {}

    #[zbus(property)]
    fn identity(&self) -> String {
        "ricercar".into()
    }
    #[zbus(property)]
    fn desktop_entry(&self) -> String {
        "ricercar".into()
    }
    #[zbus(property)]
    fn supported_uri_schemes(&self) -> Vec<String> {
        vec!["file".into(), "http".into(), "https".into()]
    }
    #[zbus(property)]
    fn supported_media_types(&self) -> Vec<String> {
        vec![
            "audio/flac".into(),
            "audio/wav".into(),
            "audio/mpeg".into(),
            "audio/ogg".into(),
            "audio/mp4".into(),
        ]
    }
    #[zbus(property)]
    fn can_quit(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn fullscreen(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn can_set_fullscreen(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn can_raise(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn has_track_list(&self) -> bool {
        false
    }
}

/// Publish on the session bus; returns the connection (must stay alive).
pub fn serve(ctl: Arc<Controller>) -> zbus::Result<zbus::blocking::Connection> {
    let conn = zbus::blocking::Connection::session()?;
    conn.request_name(BUS_NAME)?;
    conn.object_server().at(PATH, MprisRoot)?;
    conn.object_server().at(
        PATH,
        MprisPlayer {
            ctl: ctl.clone(),
        },
    )?;
    Ok(conn)
}

/// Poll player state and emit PropertiesChanged for changed properties.
pub fn spawn_event_loop(
    conn: zbus::blocking::Connection,
    quit: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let player = match conn.object_server().interface::<_, MprisPlayer>(PATH) {
            Ok(p) => p,
            Err(_) => return,
        };
        let snap = |p: &MprisPlayer| {
            (
                p.playback_status(),
                p.metadata(),
                p.volume(),
                p.can_seek(),
            )
        };
        let mut last = snap(&player.get());
        while !quit.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(500));
            let p = player.get();
            let cur = snap(&p);
            let em = player.signal_emitter();
            if cur.0 != last.0 {
                let _ = zbus::block_on(MprisPlayer::playback_status_changed(&p, em));
            }
            if cur.1 != last.1 {
                let _ = zbus::block_on(MprisPlayer::metadata_changed(&p, em));
            }
            if cur.2 != last.2 {
                let _ = zbus::block_on(MprisPlayer::volume_changed(&p, em));
            }
            if cur.3 != last.3 {
                let _ = zbus::block_on(MprisPlayer::can_seek_changed(&p, em));
            }
            last = cur;
        }
    })
}
