use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use ricercar_audio::player::{ChainInfo, EngineEvent, PlayerHandle, TransportStatus};

use crate::library::{Library, Track};
use crate::meta;

/// Metadata about a track, for UI, MPRIS and UPnP events.
#[derive(Debug, Clone)]
pub struct TrackInfo {
    pub uri: String,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration_ms: u64,
    /// Cover hint: file path (local) or http url (remote), when known.
    pub cover: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Queue owned by us (local library playback).
    Local,
    /// Driven by a remote UPnP control point.
    Remote,
}

#[derive(Debug, Clone)]
pub struct CtlState {
    pub status: TransportStatus,
    pub current_uri: Option<String>,
    pub meta_map: HashMap<String, TrackInfo>,
    pub queue: Vec<Track>,
    pub queue_index: Option<usize>,
    pub origin: Origin,
    pub pos_ms: u64,
    pub dur_ms: u64,
    pub chain: ChainInfo,
    pub volume: u32,
}

impl CtlState {
    pub fn track(&self) -> Option<TrackInfo> {
        self.current_uri
            .as_ref()
            .and_then(|u| self.meta_map.get(u).cloned())
    }
}

pub struct Controller {
    pub lib: Arc<Library>,
    player: Arc<Mutex<PlayerHandle>>,
    pub state: Arc<Mutex<CtlState>>,
    device_name: RwLock<String>,
    alive: Arc<AtomicBool>,
}

fn chain_stub(device: &str) -> ChainInfo {
    ChainInfo {
        device: device.into(),
        device_kind: ricercar_audio::DeviceKind::Null,
        format: None,
        container: None,
        volume: 100,
        bit_perfect: true,
    }
}

impl Controller {
    pub fn new(lib: Arc<Library>, device: &str) -> Controller {
        let handle = ricercar_audio::player::spawn_player(device);
        let ctl = Controller {
            lib,
            player: Arc::new(Mutex::new(handle)),
            state: Arc::new(Mutex::new(CtlState {
                status: TransportStatus::Stopped,
                current_uri: None,
                meta_map: HashMap::new(),
                queue: Vec::new(),
                queue_index: None,
                origin: Origin::Local,
                pos_ms: 0,
                dur_ms: 0,
                chain: chain_stub(device),
                volume: 100,
            })),
            device_name: RwLock::new(device.into()),
            alive: Arc::new(AtomicBool::new(true)),
        };
        ctl.spawn_bridge();
        ctl
    }

    /// Mirror engine events into CtlState and arm the next local queue item
    /// early enough for gapless playback.
    fn spawn_bridge(&self) {
        let alive = self.alive.clone();
        let player = self.player.clone();
        let state = self.state.clone();
        std::thread::Builder::new()
            .name("ricercar-ctl".into())
            .spawn(move || {
                let mut armed_for: Option<String> = None;
                while alive.load(Ordering::Relaxed) {
                    let sub = player.lock().unwrap().subscribe();
                    let mut last_arm = std::time::Instant::now();
                    while alive.load(Ordering::Relaxed) {
                        let ev = match sub.0.recv_timeout(Duration::from_millis(300)) {
                            Ok(e) => Some(e),
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                            Err(_) => None,
                        };
                        match ev {
                            Some(EngineEvent::TrackStarted { uri, format }) => {
                                let mut st = state.lock().unwrap();
                                if !st.meta_map.contains_key(&uri) {
                                    if let Some(info) = track_info_for(&uri) {
                                        st.meta_map.insert(uri.clone(), info);
                                    }
                                }
                                st.current_uri = Some(uri);
                                if format.is_some() {
                                    st.chain.format = format;
                                }
                                st.pos_ms = 0;
                                armed_for = None;
                            }
                            Some(EngineEvent::Position { pos_ms, dur_ms }) => {
                                let mut st = state.lock().unwrap();
                                st.pos_ms = pos_ms;
                                if let Some(d) = dur_ms {
                                    st.dur_ms = d;
                                }
                            }
                            Some(EngineEvent::Status { status }) => {
                                let mut st = state.lock().unwrap();
                                st.status = status;
                                if status == TransportStatus::Stopped {
                                    armed_for = None;
                                }
                            }
                            _ => {}
                        }
                        if last_arm.elapsed() >= Duration::from_millis(300) {
                            last_arm = std::time::Instant::now();
                            let arm = {
                                let st = state.lock().unwrap();
                                let playing = st.status == TransportStatus::Playing;
                                let cur = st.current_uri.clone();
                                match (playing, st.origin, &cur, &armed_for, st.queue_index) {
                                    (true, Origin::Local, Some(cur), armed, Some(idx))
                                        if armed.as_deref() != Some(cur.as_str()) =>
                                    {
                                        st.queue.get(idx + 1).filter(|_| {
                                            st.dur_ms == 0 || st.pos_ms + 12_000 >= st.dur_ms
                                        })
                                        .map(|t| t.uri.clone())
                                    }
                                    _ => None,
                                }
                            };
                            if let Some(uri) = arm {
                                armed_for = cur_track_uri(&state);
                                player.lock().unwrap().enqueue_next(uri);
                            }
                        }
                    }
                }
            })
            .expect("spawn bridge");
    }

    pub fn device_name(&self) -> String {
        self.device_name.read().unwrap().clone()
    }

    pub fn set_device(&self, name: &str) {
        self.stop();
        let handle = ricercar_audio::player::spawn_player(name);
        *self.player.lock().unwrap() = handle;
        *self.device_name.write().unwrap() = name.into();
        self.state.lock().unwrap().chain = chain_stub(name);
    }

    // ---- local playback ----

    pub fn play_tracks(&self, tracks: Vec<Track>, start: usize) {
        let Some(first) = tracks.get(start).cloned() else {
            return;
        };
        {
            let mut st = self.state.lock().unwrap();
            st.meta_map.insert(
                first.uri.clone(),
                TrackInfo {
                    uri: first.uri.clone(),
                    title: first.title.clone(),
                    artist: first.artist.clone(),
                    album: first.album.clone(),
                    duration_ms: first.duration_ms,
                    cover: Some(first.path.clone()),
                },
            );
            st.queue = tracks;
            st.queue_index = Some(start);
            st.origin = Origin::Local;
        }
        self.player.lock().unwrap().load(&first.uri);
    }

    pub fn play_index(&self, i: usize) {
        let uri = {
            let st = self.state.lock().unwrap();
            st.queue.get(i).map(|t| t.uri.clone())
        };
        if let Some(uri) = uri {
            self.state.lock().unwrap().queue_index = Some(i);
            self.player.lock().unwrap().load(uri);
        }
    }

    pub fn next(&self) {
        let idx = {
            let st = self.state.lock().unwrap();
            st.queue_index.filter(|i| i + 1 < st.queue.len()).map(|i| i + 1)
        };
        if let Some(i) = idx {
            let uri = self.state.lock().unwrap().queue[i].uri.clone();
            self.state.lock().unwrap().queue_index = Some(i);
            self.player.lock().unwrap().load(uri);
        }
    }

    pub fn prev(&self) {
        let (cur_ms, idx) = {
            let st = self.state.lock().unwrap();
            (st.pos_ms, st.queue_index)
        };
        let target = match idx {
            Some(i) if cur_ms > 3000 => Some(i),
            Some(i) if i > 0 => Some(i - 1),
            Some(i) => Some(i),
            None => None,
        };
        if let Some(i) = target {
            let uri = self.state.lock().unwrap().queue[i].uri.clone();
            self.player.lock().unwrap().load(uri);
        }
    }

    pub fn toggle(&self) {
        let status = self.state.lock().unwrap().status;
        match status {
            TransportStatus::Playing => self.player.lock().unwrap().pause(),
            TransportStatus::Paused => self.player.lock().unwrap().resume(),
            TransportStatus::Stopped => {}
        }
    }

    pub fn pause(&self) {
        self.player.lock().unwrap().pause();
    }
    pub fn resume(&self) {
        self.player.lock().unwrap().resume();
    }

    pub fn stop(&self) {
        self.player.lock().unwrap().stop();
        let mut st = self.state.lock().unwrap();
        st.queue_index = None;
        st.current_uri = None;
    }

    pub fn seek_ms(&self, ms: u64) {
        self.player.lock().unwrap().seek_ms(ms);
    }

    pub fn set_volume(&self, percent: u32) {
        self.player.lock().unwrap().set_volume(percent);
        self.state.lock().unwrap().volume = percent;
    }

    // ---- remote (UPnP-driven) ----

    /// A control point took over the renderer.
    pub fn set_remote(&self, uri: &str, info: Option<TrackInfo>) {
        {
            let mut st = self.state.lock().unwrap();
            st.origin = Origin::Remote;
            st.queue_index = None;
            let info = info.unwrap_or_else(|| TrackInfo {
                uri: uri.into(),
                title: uri.rsplit('/').next().unwrap_or(uri).into(),
                artist: None,
                album: None,
                duration_ms: 0,
                cover: None,
            });
            st.meta_map.insert(uri.into(), info);
            st.current_uri = Some(uri.into());
        }
        self.player.lock().unwrap().load(uri);
    }

    /// Queue the next track of a remote playlist (SetNextAVTransportURI).
    pub fn remote_next(&self, uri: &str, info: Option<TrackInfo>) {
        if let Some(info) = info {
            let mut st = self.state.lock().unwrap();
            st.meta_map.insert(uri.into(), info);
        }
        self.player.lock().unwrap().enqueue_next(uri);
    }

    pub fn cover_for(&self, uri: &str) -> Option<(Vec<u8>, String)> {
        if let Some(path) = meta::uri_to_path(uri) {
            return meta::cover_bytes(&path);
        }
        let hint = self
            .state
            .lock()
            .unwrap()
            .meta_map
            .get(uri)
            .and_then(|t| t.cover.clone());
        hint.as_deref().and_then(fetch_cover)
    }
}

fn cur_track_uri(state: &Mutex<CtlState>) -> Option<String> {
    state.lock().unwrap().current_uri.clone()
}

fn track_info_for(uri: &str) -> Option<TrackInfo> {
    let path = meta::uri_to_path(uri)?;
    let tags = meta::read_tags(&path);
    Some(TrackInfo {
        uri: uri.into(),
        title: tags.title.unwrap_or_default(),
        artist: tags.artist,
        album: tags.album,
        duration_ms: tags.duration_ms,
        cover: Some(path.display().to_string()),
    })
}

fn fetch_cover(url: &str) -> Option<(Vec<u8>, String)> {
    if url.starts_with("file:") || std::path::Path::new(url).exists() {
        let p = if url.starts_with("file:") {
            meta::uri_to_path(url)?
        } else {
            std::path::PathBuf::from(url)
        };
        return meta::cover_bytes(&p);
    }
    let resp = ureq::get(url).call().ok()?;
    let mime = resp
        .header("content-type")
        .unwrap_or("image/jpeg")
        .to_string();
    let mut buf = Vec::new();
    resp.into_reader().read_to_end(&mut buf).ok()?;
    Some((buf, mime))
}
