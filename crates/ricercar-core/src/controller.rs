//! Playback controller: owns the queue, drives the audio engine, mirrors its
//! state for every front-end (UI, MPRIS, UPnP) and publishes change events.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::{Duration, Instant};

use ricercar_audio::TrackOpts;
use ricercar_audio::player::{ChainInfo, EngineEvent, PlayerHandle, TransportStatus};
use serde::{Deserialize, Serialize};

use crate::config::ReplayGain;
use crate::library::{Library, Track};
use crate::meta;

/// Metadata about a playable item (library track, pushed stream, radio).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrackInfo {
    pub uri: String,
    /// Local file backing the item, when there is one.
    pub path: Option<String>,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub album_id: Option<String>,
    pub duration_ms: u64,
    /// Cover hint: a local track path or an http(s) image URL.
    pub cover: Option<String>,
    pub track_no: Option<u32>,
    pub year: Option<i32>,
    pub genre: Option<String>,
    pub sample_rate: Option<u32>,
    pub bits: Option<u8>,
    pub codec: Option<String>,
    pub rg_track_gain: Option<f32>,
    pub rg_track_peak: Option<f32>,
    pub rg_album_gain: Option<f32>,
    pub rg_album_peak: Option<f32>,
    pub mb_album_id: Option<String>,
    /// Endless stream (internet radio): no duration, no gapless successor.
    pub live: bool,
}

impl From<&Track> for TrackInfo {
    fn from(t: &Track) -> Self {
        TrackInfo {
            uri: t.uri.clone(),
            path: Some(t.path.clone()),
            title: t.title.clone(),
            artist: t.artist.clone(),
            album: t.album.clone(),
            album_artist: t.album_artist.clone(),
            album_id: Some(t.album_id.clone()),
            duration_ms: t.duration_ms,
            cover: Some(t.path.clone()),
            track_no: t.track,
            year: t.year,
            genre: t.genre.clone(),
            sample_rate: t.sample_rate,
            bits: t.bits,
            codec: t.codec.clone(),
            rg_track_gain: t.rg_track_gain,
            rg_track_peak: t.rg_track_peak,
            rg_album_gain: t.rg_album_gain,
            rg_album_peak: t.rg_album_peak,
            mb_album_id: t.mb_album_id.clone(),
            live: false,
        }
    }
}

impl From<Track> for TrackInfo {
    fn from(t: Track) -> Self {
        TrackInfo::from(&t)
    }
}

impl TrackInfo {
    /// Minimal info for a bare URI (MPRIS OpenUri, UPnP without DIDL).
    pub fn from_uri(uri: &str) -> TrackInfo {
        if let Some(path) = meta::uri_to_path(uri) {
            let tags = meta::read_tags(&path);
            let p = path.to_string_lossy().into_owned();
            return TrackInfo {
                uri: uri.into(),
                path: Some(p.clone()),
                title: tags.title.unwrap_or_default(),
                artist: tags.artist,
                album: tags.album,
                album_artist: tags.album_artist,
                duration_ms: tags.duration_ms,
                cover: Some(p),
                track_no: tags.track,
                year: tags.year,
                sample_rate: tags.sample_rate,
                bits: tags.bits,
                codec: tags.codec,
                ..Default::default()
            };
        }
        let name = uri
            .split(['?', '#'])
            .next()
            .unwrap_or(uri)
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or(uri);
        TrackInfo {
            uri: uri.into(),
            title: meta::percent_decode(name),
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueueItem {
    /// Stable identity within the session (URIs may repeat in a queue).
    pub id: u64,
    pub info: TrackInfo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Repeat {
    #[default]
    Off,
    All,
    One,
}

impl Repeat {
    pub fn cycle(self) -> Repeat {
        match self {
            Repeat::Off => Repeat::All,
            Repeat::All => Repeat::One,
            Repeat::One => Repeat::Off,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Queue owned by us (local library playback).
    Local,
    /// Driven by a remote UPnP control point.
    Remote,
}

/// Where the current queue came from (UI "playing from", ReplayGain auto).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PlayContext {
    #[default]
    None,
    Album(String),
    Artist(String),
    Playlist(i64),
    Radio,
    Mix,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueAt {
    Next,
    End,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CtlEvent {
    TrackChanged,
    StatusChanged(TransportStatus),
    QueueChanged,
    VolumeChanged,
    Seeked(u64),
    /// A play was long enough to count (history, scrobblers).
    Played(TrackInfo),
    StreamTitle(String),
    Error(String),
}

#[derive(Debug, Clone)]
pub struct CtlState {
    pub status: TransportStatus,
    pub queue: Vec<QueueItem>,
    pub current: Option<usize>,
    pub shuffle: bool,
    pub repeat: Repeat,
    pub origin: Origin,
    pub context: PlayContext,
    pub pos_ms: u64,
    pub dur_ms: u64,
    pub chain: ChainInfo,
    pub volume: u32,
    pub muted: bool,
    pub stream_title: Option<String>,
    /// Bumped on every queue mutation.
    pub queue_rev: u64,
}

impl CtlState {
    pub fn current_item(&self) -> Option<&QueueItem> {
        self.current.and_then(|i| self.queue.get(i))
    }

    pub fn track(&self) -> Option<TrackInfo> {
        self.current_item().map(|q| q.info.clone())
    }

    pub fn current_uri(&self) -> Option<String> {
        self.current_item().map(|q| q.info.uri.clone())
    }

    pub fn has_next(&self) -> bool {
        match self.current {
            Some(i) => {
                i + 1 < self.queue.len() || (self.repeat == Repeat::All && !self.queue.is_empty())
            }
            None => !self.queue.is_empty(),
        }
    }

    pub fn has_prev(&self) -> bool {
        self.current.is_some_and(|i| i > 0) || self.repeat == Repeat::All
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Session {
    queue: Vec<TrackInfo>,
    current: Option<usize>,
    pos_ms: u64,
    volume: u32,
    shuffle: bool,
    repeat: Repeat,
    #[serde(default)]
    context: PlayContext,
}

#[derive(Default)]
struct EventHub {
    subs: Mutex<Vec<Sender<CtlEvent>>>,
}

impl EventHub {
    fn subscribe(&self) -> Receiver<CtlEvent> {
        let (tx, rx) = mpsc::channel();
        self.subs.lock().unwrap().push(tx);
        rx
    }
    fn publish(&self, ev: CtlEvent) {
        self.subs
            .lock()
            .unwrap()
            .retain(|s| s.send(ev.clone()).is_ok());
    }
}

/// Engine-side bookkeeping, shared with the bridge thread.
#[derive(Default)]
struct Pending {
    /// Item handed to the engine as gapless successor.
    armed: Option<(u64, String)>,
    /// Item we asked the engine to load (for auto-skip on failure).
    loading: Option<u64>,
    /// Seek to apply once the given item starts (session restore).
    seek_on_start: Option<(u64, u64)>,
    failures: u32,
    replaygain: ReplayGain,
}

pub struct Controller {
    pub lib: Arc<Library>,
    player: Arc<Mutex<PlayerHandle>>,
    pub state: Arc<Mutex<CtlState>>,
    pending: Arc<Mutex<Pending>>,
    events: Arc<EventHub>,
    device_name: RwLock<String>,
    next_id: AtomicU64,
    alive: Arc<AtomicBool>,
    session_path: Arc<Mutex<Option<PathBuf>>>,
    /// Restored session waiting for the first "play".
    resume_at: Mutex<Option<u64>>,
    rg_preamp: Mutex<f32>,
}

fn chain_stub(device: &str) -> ChainInfo {
    ChainInfo {
        device: device.into(),
        device_kind: ricercar_audio::device::classify(device),
        format: None,
        container: None,
        volume: 100,
        bit_perfect: true,
    }
}

/// xorshift64*: good enough for queue shuffles, no extra dependency.
fn shuffle_in_place<T>(v: &mut [T]) {
    let mut s = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E3779B97F4A7C15)
        | 1;
    for i in (1..v.len()).rev() {
        s ^= s >> 12;
        s ^= s << 25;
        s ^= s >> 27;
        let j = (s.wrapping_mul(0x2545F4914F6CDD1D) % (i as u64 + 1)) as usize;
        v.swap(i, j);
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
                queue: Vec::new(),
                current: None,
                shuffle: false,
                repeat: Repeat::Off,
                origin: Origin::Local,
                context: PlayContext::None,
                pos_ms: 0,
                dur_ms: 0,
                chain: chain_stub(device),
                volume: 100,
                muted: false,
                stream_title: None,
                queue_rev: 0,
            })),
            pending: Arc::new(Mutex::new(Pending::default())),
            events: Arc::new(EventHub::default()),
            device_name: RwLock::new(device.into()),
            next_id: AtomicU64::new(1),
            alive: Arc::new(AtomicBool::new(true)),
            session_path: Arc::new(Mutex::new(None)),
            resume_at: Mutex::new(None),
            rg_preamp: Mutex::new(0.0),
        };
        ctl.spawn_bridge();
        ctl
    }

    pub fn subscribe(&self) -> Receiver<CtlEvent> {
        self.events.subscribe()
    }

    pub fn lock(&self) -> MutexGuard<'_, CtlState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn player(&self) -> MutexGuard<'_, PlayerHandle> {
        self.player.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn new_items(&self, infos: Vec<TrackInfo>) -> Vec<QueueItem> {
        infos
            .into_iter()
            .map(|info| QueueItem {
                id: self.next_id.fetch_add(1, Ordering::Relaxed),
                info,
            })
            .collect()
    }

    // ------------------------------------------------------------ engine bridge

    /// Mirror engine events into CtlState, keep the gapless successor armed,
    /// count plays and persist the session.
    fn spawn_bridge(&self) {
        let alive = self.alive.clone();
        let player = self.player.clone();
        let state = self.state.clone();
        let pending = self.pending.clone();
        let events = self.events.clone();
        let lib = self.lib.clone();
        let session_path = self.session_path.clone();
        std::thread::Builder::new()
            .name("ricercar-ctl".into())
            .spawn(move || {
                let mut counted_serial = 0u64;
                let mut play_serial = 0u64;
                let mut listened_ms = 0u64;
                let mut last_pos = 0u64;
                let mut last_save = Instant::now();
                let mut saved_rev = u64::MAX;
                while alive.load(Ordering::Relaxed) {
                    let sub = player.lock().unwrap_or_else(|e| e.into_inner()).subscribe();
                    while alive.load(Ordering::Relaxed) {
                        let ev = match sub.0.recv_timeout(Duration::from_millis(500)) {
                            Ok(e) => Some(e),
                            Err(mpsc::RecvTimeoutError::Disconnected) => break,
                            Err(mpsc::RecvTimeoutError::Timeout) => None,
                        };
                        let bridge = Bridge {
                            player: &player,
                            state: &state,
                            pending: &pending,
                            events: &events,
                        };
                        match ev {
                            Some(EngineEvent::TrackStarted { uri, format }) => {
                                play_serial += 1;
                                listened_ms = 0;
                                last_pos = 0;
                                bridge.on_track_started(&uri, format);
                            }
                            Some(EngineEvent::Position { pos_ms, dur_ms }) => {
                                // Count real listening time, not seeks.
                                let delta = pos_ms.saturating_sub(last_pos);
                                if delta < 2_000 {
                                    listened_ms += delta;
                                }
                                last_pos = pos_ms;
                                let played = {
                                    let mut st = bridge.lock_state();
                                    st.pos_ms = pos_ms;
                                    if let Some(d) = dur_ms.filter(|d| *d > 0) {
                                        st.dur_ms = d;
                                    }
                                    let dur = st.dur_ms;
                                    let threshold = (dur / 2).min(240_000);
                                    (counted_serial != play_serial
                                        && dur > 30_000
                                        && listened_ms >= threshold)
                                        .then(|| st.track())
                                        .flatten()
                                };
                                if let Some(info) = played {
                                    counted_serial = play_serial;
                                    if let Some(p) = &info.path {
                                        lib.record_play(p);
                                    }
                                    events.publish(CtlEvent::Played(info));
                                }
                            }
                            Some(EngineEvent::Status { status }) => bridge.on_status(status),
                            Some(EngineEvent::Error { message }) => bridge.on_error(message),
                            Some(EngineEvent::StreamTitle { title }) => {
                                bridge.lock_state().stream_title = Some(title.clone());
                                events.publish(CtlEvent::StreamTitle(title));
                            }
                            Some(EngineEvent::TrackEnded { .. }) | None => {}
                        }
                        let save_due = last_save.elapsed() >= Duration::from_secs(10);
                        if save_due {
                            last_save = Instant::now();
                            let rev = {
                                let st = bridge.lock_state();
                                st.queue_rev ^ st.pos_ms / 5000
                            };
                            if rev != saved_rev {
                                saved_rev = rev;
                                if let Some(p) = session_path.lock().unwrap().clone() {
                                    save_session_to(&state, &p);
                                }
                            }
                        }
                    }
                }
            })
            .expect("spawn bridge");
    }

    fn bridge(&self) -> Bridge<'_> {
        Bridge {
            player: &self.player,
            state: &self.state,
            pending: &self.pending,
            events: &self.events,
        }
    }

    pub fn device_name(&self) -> String {
        self.device_name.read().unwrap().clone()
    }

    /// Switch output device; playback continues where it was.
    pub fn set_device(&self, name: &str) {
        if name == self.device_name() {
            return;
        }
        let (resume, pos, vol, muted) = {
            let st = self.lock();
            (
                st.status != TransportStatus::Stopped,
                st.pos_ms,
                st.volume,
                st.muted,
            )
        };
        self.player().stop();
        let handle = ricercar_audio::player::spawn_player(name);
        handle.set_volume(vol);
        handle.set_mute(muted);
        handle.set_gain(*self.rg_preamp.lock().unwrap());
        *self.player() = handle;
        *self.device_name.write().unwrap() = name.into();
        {
            let mut st = self.lock();
            st.chain = chain_stub(name);
            st.status = TransportStatus::Stopped;
        }
        *self.pending.lock().unwrap() = Pending::default();
        if resume {
            let id = self.lock().current_item().map(|q| q.id);
            if let Some(id) = id {
                self.pending.lock().unwrap().seek_on_start = Some((id, pos));
                self.bridge().load_current();
            }
        }
    }

    // ------------------------------------------------------------ queue: play

    /// Replace the queue and start playing `start`.
    pub fn play_tracks(&self, infos: Vec<TrackInfo>, start: usize, ctx: PlayContext) {
        if infos.is_empty() {
            return;
        }
        let items = self.new_items(infos);
        {
            let mut st = self.lock();
            let start = start.min(items.len() - 1);
            st.queue = items;
            st.current = Some(start);
            if st.shuffle {
                let first = st.queue.remove(start);
                shuffle_in_place(&mut st.queue);
                st.queue.insert(0, first);
                st.current = Some(0);
            }
            st.origin = Origin::Local;
            st.context = ctx;
            st.queue_rev += 1;
        }
        *self.resume_at.lock().unwrap() = None;
        self.events.publish(CtlEvent::QueueChanged);
        self.bridge().load_current();
    }

    /// Replace the queue with a shuffled copy and play it.
    pub fn play_shuffled(&self, mut infos: Vec<TrackInfo>, ctx: PlayContext) {
        shuffle_in_place(&mut infos);
        self.play_tracks(infos, 0, ctx);
    }

    pub fn play_library_tracks(&self, tracks: &[Track], start: usize, ctx: PlayContext) {
        self.play_tracks(tracks.iter().map(TrackInfo::from).collect(), start, ctx);
    }

    pub fn enqueue(&self, infos: Vec<TrackInfo>, at: EnqueueAt) {
        if infos.is_empty() {
            return;
        }
        let items = self.new_items(infos);
        let start_now = {
            let mut st = self.lock();
            let was_empty = st.queue.is_empty();
            let pos = match (at, st.current) {
                (EnqueueAt::Next, Some(i)) => (i + 1).min(st.queue.len()),
                _ => st.queue.len(),
            };
            st.queue.splice(pos..pos, items);
            st.queue_rev += 1;
            if st.origin == Origin::Remote {
                st.origin = Origin::Local;
            }
            let idle = st.status == TransportStatus::Stopped;
            if was_empty {
                st.current = Some(0);
            }
            was_empty && idle
        };
        self.events.publish(CtlEvent::QueueChanged);
        if start_now {
            self.bridge().load_current();
        } else {
            self.bridge().rearm();
        }
    }

    pub fn play_index(&self, i: usize) {
        {
            let mut st = self.lock();
            if i >= st.queue.len() {
                return;
            }
            st.current = Some(i);
        }
        *self.resume_at.lock().unwrap() = None;
        self.bridge().load_current();
    }

    pub fn play_id(&self, id: u64) {
        let idx = self.lock().queue.iter().position(|q| q.id == id);
        if let Some(i) = idx {
            self.play_index(i);
        }
    }

    pub fn remove_ids(&self, ids: &[u64]) {
        let (reload, stop) = {
            let mut st = self.lock();
            let cur_id = st.current_item().map(|q| q.id);
            let playing = st.status != TransportStatus::Stopped;
            let cur_idx = st.current.unwrap_or(0);
            st.queue.retain(|q| !ids.contains(&q.id));
            st.queue_rev += 1;
            match cur_id {
                Some(cid) if ids.contains(&cid) => {
                    // The playing item went away: continue with what took its place.
                    if cur_idx < st.queue.len() {
                        st.current = Some(cur_idx);
                        (playing, false)
                    } else {
                        st.current = (!st.queue.is_empty()).then(|| st.queue.len() - 1);
                        (false, playing)
                    }
                }
                Some(cid) => {
                    st.current = st.queue.iter().position(|q| q.id == cid);
                    (false, false)
                }
                None => (false, false),
            }
        };
        self.events.publish(CtlEvent::QueueChanged);
        if reload {
            self.bridge().load_current();
        } else if stop {
            self.player().stop();
        } else {
            self.bridge().rearm();
        }
    }

    pub fn move_item(&self, from: usize, to: usize) {
        {
            let mut st = self.lock();
            if from >= st.queue.len() || to >= st.queue.len() || from == to {
                return;
            }
            let cur_id = st.current_item().map(|q| q.id);
            let item = st.queue.remove(from);
            st.queue.insert(to, item);
            st.current = cur_id.and_then(|c| st.queue.iter().position(|q| q.id == c));
            st.queue_rev += 1;
        }
        self.events.publish(CtlEvent::QueueChanged);
        self.bridge().rearm();
    }

    /// Drop everything after the current item.
    pub fn clear_upcoming(&self) {
        {
            let mut st = self.lock();
            let keep = st.current.map(|i| i + 1).unwrap_or(0);
            st.queue.truncate(keep);
            st.queue_rev += 1;
        }
        self.events.publish(CtlEvent::QueueChanged);
        self.bridge().rearm();
    }

    pub fn clear_queue(&self) {
        self.player().stop();
        {
            let mut st = self.lock();
            st.queue.clear();
            st.current = None;
            st.context = PlayContext::None;
            st.queue_rev += 1;
            st.pos_ms = 0;
            st.dur_ms = 0;
        }
        *self.resume_at.lock().unwrap() = None;
        self.events.publish(CtlEvent::QueueChanged);
        self.events.publish(CtlEvent::TrackChanged);
    }

    // ------------------------------------------------------------ modes

    pub fn set_shuffle(&self, on: bool) {
        {
            let mut st = self.lock();
            if st.shuffle == on {
                return;
            }
            st.shuffle = on;
            if on {
                let from = st.current.map(|i| i + 1).unwrap_or(0);
                if from < st.queue.len() {
                    shuffle_in_place(&mut st.queue[from..]);
                }
            } else if st.context != PlayContext::None && st.origin == Origin::Local {
                // Restore a natural order: album/disc/track, else keep as is.
                let cur_id = st.current_item().map(|q| q.id);
                let from = st.current.map(|i| i + 1).unwrap_or(0);
                let mut rest: Vec<QueueItem> = st.queue.drain(from..).collect();
                rest.sort_by(|a, b| {
                    (&a.info.album_id, a.info.track_no).cmp(&(&b.info.album_id, b.info.track_no))
                });
                st.queue.extend(rest);
                st.current = cur_id.and_then(|c| st.queue.iter().position(|q| q.id == c));
            }
            st.queue_rev += 1;
        }
        self.events.publish(CtlEvent::QueueChanged);
        self.bridge().rearm();
    }

    pub fn set_repeat(&self, r: Repeat) {
        self.lock().repeat = r;
        self.events.publish(CtlEvent::QueueChanged);
        self.bridge().rearm();
    }

    // ------------------------------------------------------------ transport

    pub fn next(&self) {
        let target = {
            let st = self.lock();
            match st.current {
                Some(i) if i + 1 < st.queue.len() => Some(i + 1),
                Some(_) if st.repeat == Repeat::All && !st.queue.is_empty() => Some(0),
                None if !st.queue.is_empty() => Some(0),
                _ => None,
            }
        };
        if let Some(i) = target {
            self.play_index(i);
        }
    }

    pub fn prev(&self) {
        let (pos, target) = {
            let st = self.lock();
            let t = match st.current {
                Some(i) if i > 0 => Some(i - 1),
                Some(_) if st.repeat == Repeat::All && !st.queue.is_empty() => {
                    Some(st.queue.len() - 1)
                }
                _ => None,
            };
            (st.pos_ms, t)
        };
        match target {
            Some(i) if pos <= 3000 => self.play_index(i),
            _ => self.seek_ms(0),
        }
    }

    pub fn toggle(&self) {
        let status = self.lock().status;
        match status {
            TransportStatus::Playing => self.pause(),
            TransportStatus::Paused => self.resume(),
            TransportStatus::Stopped => self.play(),
        }
    }

    /// Play from the current state: resume, restart the current item, or
    /// start the queue.
    pub fn play(&self) {
        let (status, has_current, empty) = {
            let st = self.lock();
            (st.status, st.current.is_some(), st.queue.is_empty())
        };
        match status {
            TransportStatus::Paused => self.resume(),
            TransportStatus::Playing => {}
            TransportStatus::Stopped if empty => {}
            TransportStatus::Stopped => {
                if !has_current {
                    self.lock().current = Some(0);
                }
                if let Some(pos) = self.resume_at.lock().unwrap().take() {
                    let id = self.lock().current_item().map(|q| q.id);
                    if let Some(id) = id {
                        self.pending.lock().unwrap().seek_on_start = Some((id, pos));
                    }
                }
                self.bridge().load_current();
            }
        }
    }

    pub fn pause(&self) {
        self.player().pause();
    }

    pub fn resume(&self) {
        self.player().resume();
    }

    pub fn stop(&self) {
        self.player().stop();
        let mut st = self.lock();
        st.pos_ms = 0;
    }

    pub fn seek_ms(&self, ms: u64) {
        let stopped = self.lock().status == TransportStatus::Stopped;
        if stopped {
            // Seeking a restored session: remember it for the first play.
            if self.resume_at.lock().unwrap().is_some() {
                *self.resume_at.lock().unwrap() = Some(ms);
                self.lock().pos_ms = ms;
            }
            return;
        }
        self.player().seek_ms(ms);
        self.lock().pos_ms = ms;
        self.events.publish(CtlEvent::Seeked(ms));
    }

    pub fn seek_relative(&self, delta_ms: i64) {
        let (pos, dur) = {
            let st = self.lock();
            (st.pos_ms as i64, st.dur_ms as i64)
        };
        let target = (pos + delta_ms).max(0);
        let target = if dur > 0 {
            target.min(dur - 500)
        } else {
            target
        };
        self.seek_ms(target.max(0) as u64);
    }

    pub fn set_volume(&self, percent: u32) {
        let percent = percent.min(100);
        self.lock().volume = percent;
        self.player().set_volume(percent);
        self.events.publish(CtlEvent::VolumeChanged);
    }

    pub fn set_muted(&self, muted: bool) {
        self.lock().muted = muted;
        self.player().set_mute(muted);
        self.events.publish(CtlEvent::VolumeChanged);
    }

    /// ReplayGain mode and preamp; applies from the next loaded track.
    pub fn set_replaygain(&self, mode: ReplayGain, preamp_db: f32) {
        self.pending.lock().unwrap().replaygain = mode;
        let pre = if mode == ReplayGain::Off {
            0.0
        } else {
            preamp_db
        };
        self.player().set_gain(pre);
        self.rg_preamp.lock().unwrap().clone_from(&pre);
    }

    // ------------------------------------------------------------ remote (UPnP)

    /// A control point took over the renderer (SetAVTransportURI).
    pub fn set_remote(&self, uri: &str, info: Option<TrackInfo>) {
        let mut info = info.unwrap_or_else(|| TrackInfo::from_uri(uri));
        info.uri = uri.into();
        let items = self.new_items(vec![info]);
        {
            let mut st = self.lock();
            st.queue = items;
            st.current = Some(0);
            st.origin = Origin::Remote;
            st.context = PlayContext::None;
            st.queue_rev += 1;
        }
        *self.resume_at.lock().unwrap() = None;
        self.events.publish(CtlEvent::QueueChanged);
        self.bridge().load_current();
    }

    /// Queue the next track of a remote playlist (SetNextAVTransportURI).
    pub fn remote_next(&self, uri: &str, info: Option<TrackInfo>) {
        {
            let mut st = self.lock();
            let cur = st.current.unwrap_or(0);
            // Keep the list short in long remote sessions: [current, next].
            if cur > 0 {
                st.queue.drain(..cur);
                st.current = Some(0);
            }
            st.queue.truncate(1);
            if !uri.is_empty() {
                let mut info = info.unwrap_or_else(|| TrackInfo::from_uri(uri));
                info.uri = uri.into();
                let items = self.new_items(vec![info]);
                st.queue.extend(items);
            }
            st.queue_rev += 1;
        }
        self.events.publish(CtlEvent::QueueChanged);
        self.bridge().rearm();
    }

    // ------------------------------------------------------------ covers

    pub fn cover_for(&self, uri: &str) -> Option<(Vec<u8>, String)> {
        if let Some(path) = meta::uri_to_path(uri) {
            return meta::cover_bytes(&path);
        }
        let hint = self
            .lock()
            .queue
            .iter()
            .find(|q| q.info.uri == uri)
            .and_then(|q| q.info.cover.clone());
        hint.as_deref().and_then(fetch_cover)
    }

    // ------------------------------------------------------------ session

    /// Enable session persistence and restore a previous session (paused).
    pub fn enable_session(&self, path: PathBuf, restore: bool) {
        *self.session_path.lock().unwrap() = Some(path.clone());
        if !restore {
            return;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        let Ok(s) = serde_json::from_str::<Session>(&text) else {
            return;
        };
        let items = self.new_items(s.queue);
        {
            let mut st = self.lock();
            st.current = s.current.filter(|i| *i < items.len());
            st.dur_ms = st
                .current
                .and_then(|i| items.get(i))
                .map(|q| q.info.duration_ms)
                .unwrap_or(0);
            st.queue = items;
            st.pos_ms = s.pos_ms;
            st.volume = s.volume.min(100);
            st.shuffle = s.shuffle;
            st.repeat = s.repeat;
            st.context = s.context;
            st.queue_rev += 1;
        }
        self.player().set_volume(s.volume.min(100));
        *self.resume_at.lock().unwrap() = Some(s.pos_ms);
        self.events.publish(CtlEvent::QueueChanged);
        self.events.publish(CtlEvent::TrackChanged);
    }

    pub fn save_session(&self) {
        if let Some(p) = self.session_path.lock().unwrap().clone() {
            save_session_to(&self.state, &p);
        }
    }

    pub fn shutdown(&self) {
        self.save_session();
        self.player().stop();
        self.alive.store(false, Ordering::Relaxed);
    }
}

fn save_session_to(state: &Mutex<CtlState>, path: &std::path::Path) {
    let s = {
        let st = state.lock().unwrap_or_else(|e| e.into_inner());
        if st.origin == Origin::Remote {
            return;
        }
        Session {
            queue: st.queue.iter().map(|q| q.info.clone()).collect(),
            current: st.current,
            pos_ms: st.pos_ms,
            volume: st.volume,
            shuffle: st.shuffle,
            repeat: st.repeat,
            context: st.context.clone(),
        }
    };
    if let Ok(json) = serde_json::to_vec(&s)
        && let Err(e) = crate::config::write_atomic(path, &json)
    {
        tracing::warn!("save session: {e}");
    }
}

/// Borrowed view used by both the controller and its bridge thread.
struct Bridge<'a> {
    player: &'a Mutex<PlayerHandle>,
    state: &'a Mutex<CtlState>,
    pending: &'a Mutex<Pending>,
    events: &'a EventHub,
}

impl Bridge<'_> {
    /// ReplayGain for an item; album gain only when the album plays in order.
    fn opts_for(&self, info: &TrackInfo, mode: ReplayGain) -> TrackOpts {
        let album_ctx = matches!(self.lock_state().context, PlayContext::Album(_));
        let album = match mode {
            ReplayGain::Off => return TrackOpts::default(),
            ReplayGain::Track => false,
            ReplayGain::Album => true,
            ReplayGain::Auto => album_ctx,
        };
        let (gain, peak) = if album && info.rg_album_gain.is_some() {
            (info.rg_album_gain, info.rg_album_peak)
        } else {
            (info.rg_track_gain, info.rg_track_peak)
        };
        TrackOpts {
            gain_db: gain,
            peak,
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, CtlState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn player(&self) -> MutexGuard<'_, PlayerHandle> {
        self.player.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn load_current(&self) {
        let item = {
            let mut st = self.lock_state();
            st.pos_ms = 0;
            st.stream_title = None;
            st.dur_ms = st.current_item().map(|q| q.info.duration_ms).unwrap_or(0);
            st.current_item().cloned()
        };
        let Some(item) = item else { return };
        let opts = {
            let mut p = self.pending.lock().unwrap();
            p.armed = None;
            p.loading = Some(item.id);
            self.opts_for(&item.info, p.replaygain)
        };
        self.player().load_with(&item.info.uri, opts);
        self.events.publish(CtlEvent::TrackChanged);
    }

    /// Hand the engine the item that should follow the current one.
    fn rearm(&self) {
        let want = {
            let st = self.lock_state();
            if st.status == TransportStatus::Stopped {
                None
            } else {
                st.current.and_then(|i| {
                    let cur = &st.queue[i];
                    if cur.info.live {
                        return None;
                    }
                    let next = match st.repeat {
                        Repeat::One => Some(cur),
                        Repeat::All => st.queue.get(i + 1).or(st.queue.first()),
                        Repeat::Off => st.queue.get(i + 1),
                    };
                    next.map(|q| (q.id, q.info.uri.clone(), q.info.clone()))
                })
            }
        };
        let mut p = self.pending.lock().unwrap();
        if p.armed.as_ref().map(|a| (a.0, &a.1)) != want.as_ref().map(|w| (w.0, &w.1)) {
            // Never hand an empty successor to an idle engine: it would be a no-op anyway.
            if want.is_some() || p.armed.is_some() {
                let (uri, opts) = match &want {
                    Some((_, uri, info)) => (uri.clone(), self.opts_for(info, p.replaygain)),
                    None => (String::new(), TrackOpts::default()),
                };
                self.player().enqueue_next_with(uri, opts);
            }
            p.armed = want.map(|(id, uri, _)| (id, uri));
        }
    }

    fn on_track_started(&self, uri: &str, format: Option<ricercar_audio::PcmFormat>) {
        let (armed, seek) = {
            let mut p = self.pending.lock().unwrap();
            p.loading = None;
            p.failures = 0;
            (p.armed.take(), p.seek_on_start.take())
        };
        let started_id = {
            let mut st = self.lock_state();
            let by_armed = armed
                .filter(|(_, u)| u == uri)
                .and_then(|(id, _)| st.queue.iter().position(|q| q.id == id));
            let idx = by_armed.or_else(|| {
                st.current
                    .filter(|&i| st.queue.get(i).is_some_and(|q| q.info.uri == uri))
                    .or_else(|| st.queue.iter().position(|q| q.info.uri == uri))
            });
            if idx.is_some() {
                st.current = idx;
            }
            if format.is_some() {
                st.chain.format = format;
            }
            st.pos_ms = 0;
            st.stream_title = None;
            st.dur_ms = st.current_item().map(|q| q.info.duration_ms).unwrap_or(0);
            st.current_item().map(|q| q.id)
        };
        if let (Some((id, pos)), Some(sid)) = (seek, started_id)
            && id == sid
            && pos > 0
        {
            self.player().seek_ms(pos);
        }
        self.events.publish(CtlEvent::TrackChanged);
        self.rearm();
    }

    fn on_status(&self, status: TransportStatus) {
        {
            let mut st = self.lock_state();
            if st.status == status {
                return;
            }
            st.status = status;
            if status == TransportStatus::Stopped {
                st.pos_ms = 0;
            }
        }
        if status == TransportStatus::Stopped {
            self.pending.lock().unwrap().armed = None;
        }
        self.events.publish(CtlEvent::StatusChanged(status));
        if status != TransportStatus::Stopped {
            self.rearm();
        }
    }

    fn on_error(&self, message: String) {
        tracing::warn!("engine: {message}");
        self.events.publish(CtlEvent::Error(message));
        // A track that fails to open is skipped (bounded, to avoid spinning
        // through a queue of unreachable URLs).
        let skip = {
            let mut p = self.pending.lock().unwrap();
            match p.loading.take() {
                Some(_) if p.failures < 5 => {
                    p.failures += 1;
                    true
                }
                _ => false,
            }
        };
        if skip {
            let next = {
                let st = self.lock_state();
                st.current.filter(|i| i + 1 < st.queue.len()).map(|i| i + 1)
            };
            if let Some(i) = next {
                self.lock_state().current = Some(i);
                self.load_current();
            }
        }
    }
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
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return None;
    }
    let resp = ureq::get(url).timeout(Duration::from_secs(8)).call().ok()?;
    let mime = resp
        .header("content-type")
        .unwrap_or("image/jpeg")
        .to_string();
    let mut buf = Vec::new();
    use std::io::Read;
    resp.into_reader()
        .take(16 * 1024 * 1024)
        .read_to_end(&mut buf)
        .ok()?;
    Some((buf, mime))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(n: u32) -> TrackInfo {
        TrackInfo {
            uri: format!("file:///nonexistent/{n}.flac"),
            title: format!("t{n}"),
            track_no: Some(n),
            album_id: Some("a".into()),
            ..Default::default()
        }
    }

    fn ctl() -> Controller {
        Controller::new(Arc::new(Library::in_memory().unwrap()), "null")
    }

    fn titles(c: &Controller) -> Vec<String> {
        c.lock()
            .queue
            .iter()
            .map(|q| q.info.title.clone())
            .collect()
    }

    #[test]
    fn queue_mutations_keep_current() {
        let c = ctl();
        c.enqueue((1..=3).map(info).collect(), EnqueueAt::End);
        assert_eq!(c.lock().current, Some(0));
        c.lock().current = Some(1);
        c.enqueue(vec![info(9)], EnqueueAt::Next);
        assert_eq!(titles(&c), ["t1", "t2", "t9", "t3"]);
        c.move_item(3, 0);
        assert_eq!(titles(&c), ["t3", "t1", "t2", "t9"]);
        assert_eq!(c.lock().current_item().unwrap().info.title, "t2");
        let id = c.lock().queue[0].id;
        c.remove_ids(&[id]);
        assert_eq!(c.lock().current, Some(1));
        c.clear_upcoming();
        assert_eq!(titles(&c), ["t1", "t2"]);
        c.clear_queue();
        assert!(c.lock().queue.is_empty());
    }

    #[test]
    fn shuffle_keeps_current_first_and_restores_order() {
        let c = ctl();
        c.enqueue((1..=30).map(info).collect(), EnqueueAt::End);
        c.lock().context = PlayContext::Album("a".into());
        c.lock().current = Some(4);
        c.set_shuffle(true);
        let t = titles(&c);
        assert_eq!(&t[..5], ["t1", "t2", "t3", "t4", "t5"]);
        assert_ne!(
            &t[5..],
            (6..=30).map(|n| format!("t{n}")).collect::<Vec<_>>()
        );
        c.set_shuffle(false);
        assert_eq!(
            titles(&c),
            (1..=30).map(|n| format!("t{n}")).collect::<Vec<_>>()
        );
    }

    #[test]
    fn next_prev_and_repeat() {
        let c = ctl();
        c.enqueue((1..=2).map(info).collect(), EnqueueAt::End);
        let st = c.lock().clone();
        assert!(st.has_next() && !st.has_prev());
        c.lock().current = Some(1);
        assert!(!c.lock().has_next());
        c.set_repeat(Repeat::All);
        assert!(c.lock().has_next());
        assert_eq!(Repeat::Off.cycle(), Repeat::All);
    }

    #[test]
    fn remote_next_keeps_two_items() {
        let c = ctl();
        c.set_remote("http://x/a.flac", None);
        c.remote_next("http://x/b.flac", None);
        assert_eq!(titles(&c), ["a.flac", "b.flac"]);
        c.lock().current = Some(1);
        c.remote_next("http://x/c%20d.flac", None);
        assert_eq!(titles(&c), ["b.flac", "c d.flac"]);
        assert_eq!(c.lock().origin, Origin::Remote);
    }

    #[test]
    fn session_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("session.json");
        let c = ctl();
        c.enable_session(p.clone(), false);
        c.enqueue((1..=3).map(info).collect(), EnqueueAt::End);
        c.set_repeat(Repeat::One);
        {
            let mut st = c.lock();
            st.current = Some(2);
            st.pos_ms = 42_000;
        }
        c.save_session();
        let c2 = ctl();
        c2.enable_session(p, true);
        let st = c2.lock().clone();
        assert_eq!(st.queue.len(), 3);
        assert_eq!(st.current, Some(2));
        assert_eq!(st.pos_ms, 42_000);
        assert_eq!(st.repeat, Repeat::One);
    }
}
