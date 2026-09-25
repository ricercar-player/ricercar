//! org.mpris.MediaPlayer2 on the session bus: media keys, desktop widgets,
//! `playerctl` and `ricercar-cli` all drive the same Controller.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ricercar_audio::TransportStatus;
use ricercar_core::covers::CoverCache;
use ricercar_core::{Controller, CtlEvent, Repeat, TrackInfo};
use zbus::interface;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedValue, Value};

pub const BUS_NAME: &str = "org.mpris.MediaPlayer2.ricercar";
pub const PATH: &str = "/org/mpris/MediaPlayer2";

type Hook = Arc<dyn Fn() + Send + Sync>;

/// Optional integration points supplied by the front-end.
#[derive(Clone, Default)]
pub struct MprisOptions {
    /// Resolves `mpris:artUrl` for local files (a cached JPEG thumbnail).
    pub covers: Option<Arc<CoverCache>>,
    /// Bring the window to the front (`Raise`).
    pub on_raise: Option<Hook>,
    /// Quit the application (`Quit`).
    pub on_quit: Option<Hook>,
}

pub struct MprisPlayer {
    ctl: Arc<Controller>,
    covers: Option<Arc<CoverCache>>,
}

fn track_id(id: Option<u64>) -> ObjectPath<'static> {
    match id {
        Some(id) => ObjectPath::from_string_unchecked(format!("/org/ricercar/track/{id}")),
        None => ObjectPath::from_static_str_unchecked("/org/mpris/MediaPlayer2/TrackList/NoTrack"),
    }
}

fn val(v: impl Into<Value<'static>>) -> OwnedValue {
    OwnedValue::try_from(v.into()).expect("plain values never hold fds")
}

impl MprisPlayer {
    fn art_url(&self, t: &TrackInfo) -> Option<String> {
        let hint = t.cover.as_deref()?;
        if hint.starts_with("http://") || hint.starts_with("https://") {
            return Some(hint.to_string());
        }
        let key = t.album_id.clone().unwrap_or_else(|| hint.to_string());
        let thumb = self
            .covers
            .as_ref()?
            .thumb(&key, std::path::Path::new(hint), 512)?;
        Some(ricercar_core::meta::file_uri(&thumb))
    }
}

#[interface(name = "org.mpris.MediaPlayer2.Player")]
impl MprisPlayer {
    fn play(&self) {
        self.ctl.play();
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
        self.ctl.seek_relative(offset / 1000);
    }
    fn set_position(&self, track: ObjectPath<'_>, position: i64) {
        let cur = track_id(self.ctl.lock().current_item().map(|q| q.id));
        if track.as_str() == cur.as_str() && position >= 0 {
            self.ctl.seek_ms(position as u64 / 1000);
        }
    }
    #[zbus(name = "OpenUri")]
    fn open_uri(&self, uri: &str) {
        self.ctl.play_tracks(
            vec![TrackInfo::from_uri(uri)],
            0,
            ricercar_core::PlayContext::None,
        );
    }

    #[zbus(signal)]
    async fn seeked(emitter: &SignalEmitter<'_>, position: i64) -> zbus::Result<()>;

    #[zbus(property)]
    fn playback_status(&self) -> String {
        match self.ctl.lock().status {
            TransportStatus::Playing => "Playing",
            TransportStatus::Paused => "Paused",
            TransportStatus::Stopped => "Stopped",
        }
        .into()
    }
    #[zbus(property)]
    fn loop_status(&self) -> String {
        match self.ctl.lock().repeat {
            Repeat::Off => "None",
            Repeat::All => "Playlist",
            Repeat::One => "Track",
        }
        .into()
    }
    #[zbus(property)]
    fn set_loop_status(&self, v: String) {
        self.ctl.set_repeat(match v.as_str() {
            "Track" => Repeat::One,
            "Playlist" => Repeat::All,
            _ => Repeat::Off,
        });
    }
    #[zbus(property)]
    fn shuffle(&self) -> bool {
        self.ctl.lock().shuffle
    }
    #[zbus(property)]
    fn set_shuffle(&self, v: bool) {
        self.ctl.set_shuffle(v);
    }
    #[zbus(property)]
    fn metadata(&self) -> HashMap<String, OwnedValue> {
        let (id, info, title) = {
            let st = self.ctl.lock();
            (
                st.current_item().map(|q| q.id),
                st.track(),
                st.stream_title.clone(),
            )
        };
        let mut m = HashMap::new();
        m.insert("mpris:trackid".into(), val(track_id(id)));
        let Some(t) = info else { return m };
        m.insert("xesam:url".into(), val(t.uri.clone()));
        // Internet radio: the stream title is the song, the station the album.
        match (&title, t.live) {
            (Some(song), true) => {
                m.insert("xesam:title".into(), val(song.clone()));
                m.insert("xesam:album".into(), val(t.title.clone()));
            }
            _ => {
                m.insert("xesam:title".into(), val(t.title.clone()));
            }
        }
        if let Some(a) = &t.artist {
            m.insert("xesam:artist".into(), val(vec![a.clone()]));
        }
        if let Some(a) = &t.album_artist {
            m.insert("xesam:albumArtist".into(), val(vec![a.clone()]));
        }
        if let Some(al) = &t.album {
            m.insert("xesam:album".into(), val(al.clone()));
        }
        if let Some(n) = t.track_no {
            m.insert("xesam:trackNumber".into(), val(n as i32));
        }
        if let Some(g) = &t.genre {
            m.insert("xesam:genre".into(), val(vec![g.clone()]));
        }
        if t.duration_ms > 0 {
            m.insert("mpris:length".into(), val(t.duration_ms as i64 * 1000));
        }
        if let Some(url) = self.art_url(&t) {
            m.insert("mpris:artUrl".into(), val(url));
        }
        m
    }
    #[zbus(property)]
    fn can_go_next(&self) -> bool {
        self.ctl.lock().has_next()
    }
    #[zbus(property)]
    fn can_go_previous(&self) -> bool {
        !self.ctl.lock().queue.is_empty()
    }
    #[zbus(property)]
    fn can_play(&self) -> bool {
        !self.ctl.lock().queue.is_empty()
    }
    #[zbus(property)]
    fn can_pause(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn can_seek(&self) -> bool {
        let st = self.ctl.lock();
        st.status != TransportStatus::Stopped && st.dur_ms > 0
    }
    #[zbus(property)]
    fn can_control(&self) -> bool {
        true
    }
    #[zbus(property(emits_changed_signal = "false"))]
    fn position(&self) -> i64 {
        self.ctl.lock().pos_ms as i64 * 1000
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
    fn rate(&self) -> f64 {
        1.0
    }
    #[zbus(property)]
    fn set_rate(&self, _v: f64) {}
    #[zbus(property)]
    fn volume(&self) -> f64 {
        let st = self.ctl.lock();
        if st.muted {
            0.0
        } else {
            st.volume as f64 / 100.0
        }
    }
    #[zbus(property)]
    fn set_volume(&self, value: f64) {
        self.ctl
            .set_volume((value * 100.0).round().clamp(0.0, 100.0) as u32);
    }
}

pub struct MprisRoot {
    on_raise: Option<Hook>,
    on_quit: Option<Hook>,
}

#[interface(name = "org.mpris.MediaPlayer2")]
impl MprisRoot {
    fn raise(&self) {
        if let Some(f) = &self.on_raise {
            f();
        }
    }
    fn quit(&self) {
        if let Some(f) = &self.on_quit {
            f();
        }
    }

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
        [
            "audio/flac",
            "audio/x-flac",
            "audio/wav",
            "audio/x-wav",
            "audio/aiff",
            "audio/mpeg",
            "audio/ogg",
            "audio/opus",
            "audio/mp4",
            "audio/aac",
        ]
        .map(String::from)
        .to_vec()
    }
    #[zbus(property)]
    fn can_quit(&self) -> bool {
        self.on_quit.is_some()
    }
    #[zbus(property)]
    fn can_raise(&self) -> bool {
        self.on_raise.is_some()
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
    fn has_track_list(&self) -> bool {
        false
    }
}

/// Publish on the session bus; returns the connection (must stay alive).
pub fn serve(ctl: Arc<Controller>) -> zbus::Result<zbus::blocking::Connection> {
    serve_with(ctl, MprisOptions::default())
}

pub fn serve_with(
    ctl: Arc<Controller>,
    opts: MprisOptions,
) -> zbus::Result<zbus::blocking::Connection> {
    zbus::blocking::connection::Builder::session()?
        .name(BUS_NAME)?
        .serve_at(
            PATH,
            MprisRoot {
                on_raise: opts.on_raise,
                on_quit: opts.on_quit,
            },
        )?
        .serve_at(
            PATH,
            MprisPlayer {
                ctl,
                covers: opts.covers,
            },
        )?
        .build()
}

/// Forward controller events as PropertiesChanged / Seeked signals.
pub fn spawn_event_loop(
    conn: zbus::blocking::Connection,
    ctl: Arc<Controller>,
    quit: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    let events = ctl.subscribe();
    std::thread::Builder::new()
        .name("ricercar-mpris".into())
        .spawn(move || {
            let Ok(iface) = conn.object_server().interface::<_, MprisPlayer>(PATH) else {
                return;
            };
            while !quit.load(Ordering::Relaxed) {
                let ev = match events.recv_timeout(Duration::from_millis(500)) {
                    Ok(ev) => ev,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(_) => break,
                };
                let p = iface.get();
                let em = iface.signal_emitter();
                let r = zbus::block_on(async {
                    match ev {
                        CtlEvent::TrackChanged | CtlEvent::StreamTitle(_) => {
                            p.metadata_changed(em).await?;
                            p.can_seek_changed(em).await?;
                            p.can_go_next_changed(em).await?;
                            p.can_go_previous_changed(em).await?;
                            p.can_play_changed(em).await
                        }
                        CtlEvent::StatusChanged(_) => {
                            p.playback_status_changed(em).await?;
                            p.can_seek_changed(em).await
                        }
                        CtlEvent::VolumeChanged => p.volume_changed(em).await,
                        CtlEvent::Seeked(ms) => MprisPlayer::seeked(em, ms as i64 * 1000).await,
                        CtlEvent::QueueChanged => {
                            p.loop_status_changed(em).await?;
                            p.shuffle_changed(em).await?;
                            p.can_go_next_changed(em).await?;
                            p.can_go_previous_changed(em).await?;
                            p.can_play_changed(em).await
                        }
                        CtlEvent::Played(_) | CtlEvent::Error(_) => Ok(()),
                    }
                });
                if let Err(e) = r {
                    tracing::debug!("mpris signal: {e}");
                }
            }
        })
        .expect("spawn mpris loop")
}
